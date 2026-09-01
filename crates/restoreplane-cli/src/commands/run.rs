// crates/restoreplane-cli/src/commands/run.rs
//! S05/S03/S06 walking skeleton 배선: config → PostgresSource → 파이프라인 → LocalDest.
//! 슬라이스 1 범위: Postgres 소스 + Local 목적지만 실제 배선. S3/SSH는 명확한 메시지로 미배선.
//!
//! 봉투 경로(키스토어 연동):
//! - 키스토어를 로드하고 KEK를 unlock(무인: RESTOREPLANE_MASTER_PASSWORD env, 또는 DPAPI Machine-scope).
//! - 백업마다 per-backup DEK를 발급 → KEK로 감싼 WrapSet을 매니페스트에 동봉 → 지속 서명키로 서명.
//! - payload 암호화는 그 per-backup DEK로 수행(파이프라인 인코딩 체인).
//!
//! config 시크릿(EncField) 복호:
//! - KEK unlock 직후 `DekEncryptor(KEK, FIELD_WRAP_GEN)`를 만들어 config의 `enc:<gen>:<b64>` 필드
//!   (DB 비밀번호 / S3 access key)를 메모리에서만 복호한다. 평문은 Zeroizing으로 들고 다닌다.
//! - 환경변수(PGPASSWORD/MYSQL_PWD/AWS_*)가 **설정돼 있으면 그쪽이 우선**한다 — 운영 임시 우회 및
//!   기존 배선 하위호환. 설정되지 않았으면 config EncField를 복호하고, 복호 실패는 **요란히 실패**한다
//!   (잘못된 키스토어/세대를 조용히 env로 폴백해 감추지 않는다).

use std::path::Path;
use std::sync::Arc;

use ed25519_dalek::SigningKey;
use restoreplane_catalog::{upsert_backup_from_manifest, CatalogWriterHandle};
use restoreplane_config::Config;
use restoreplane_core::{
    ChecksumAlgo, DestKind, Engine, ImmutabilityCapability, NotifyChannel, ObjectLockMode,
};
use restoreplane_dest_s3::{S3Cfg, S3Dest, S3Provider};
use restoreplane_dest_ssh::{HostKeyPolicy, SshAuth, SshDest};
use restoreplane_pipeline::{
    assert_job_immutability, fan_out, run_artifact, write_signed_manifest_with_wrapset,
    BackupSource, Destination, DumpPlanPart, ImmutabilityKind, LocalDest, Manifest, PipelineCfg,
    RequiredImmutability,
};
use restoreplane_scheduler::{
    parse_cron_subset, run_scheduled_job, CompositeSink, JobResult, JobRunner, SchedulerContext,
    EXIT_SUCCESS,
};
use restoreplane_secrets::{Dek, DekEncryptor, Keystore};
use restoreplane_source_mysql::{
    binary::{ClientKind, MysqldumpBinaryMatrix},
    dump::MySqlConnCfg,
    meta::MysqlCliMetaConn,
    MySqlSource,
};
use restoreplane_source_pg::{version::BundledToolMatrix, PgConnCfg, PostgresSource};
use zeroize::Zeroizing;

use crate::cli::RunArgs;
use crate::commands::keystore_util;

/// config 필드 암호화 세대. GUI(apps/console config_edit)가 쓰는 값과 같아야 한다 —
/// 달라지면 rotate-key 대상 식별이 어긋난다(core::EncField P2-1).
pub const FIELD_WRAP_GEN: u32 = 1;

pub fn run(config_path: &Path, args: RunArgs) -> anyhow::Result<i32> {
    if !args.dry_run {
        crate::commands::direct_execution_guard::enforce()?;
    }
    run_after_execution_authority_check(config_path, args)
}

fn run_after_execution_authority_check(config_path: &Path, args: RunArgs) -> anyhow::Result<i32> {
    let config = Config::load(config_path)?;
    restoreplane_config::validate(&config).map_err(|errs| anyhow::anyhow!("{errs:?}"))?;

    let jobs: Vec<_> = config
        .jobs
        .iter()
        .filter(|j| j.enabled)
        .filter(|j| args.db.as_deref().is_none_or(|db| j.source == db))
        .filter(|j| args.job.as_deref().is_none_or(|name| j.name == name))
        .cloned()
        .collect();

    if jobs.is_empty() {
        anyhow::bail!("no matching enabled jobs for --db={:?}", args.db);
    }

    // 모든 cron을 실행 준비 전에 검증한다. 다중 job 중 뒤쪽 cron 오류 때문에 앞쪽 백업만
    // 실행된 뒤 중단되는 부분 실행을 방지한다.
    let jobs = jobs
        .into_iter()
        .map(|job| {
            let schedule = parse_cron_subset(&job.schedule)
                .map_err(|e| anyhow::anyhow!("job '{}'의 schedule 파싱 실패: {e}", job.name))?;
            Ok((job, schedule))
        })
        .collect::<anyhow::Result<Vec<_>>>()?;

    if args.dry_run {
        for (job, _) in &jobs {
            println!(
                "[dry-run] would run job '{}': {} -> {:?}",
                job.name, job.source, job.destinations
            );
        }
        return Ok(EXIT_SUCCESS);
    }

    // 키스토어 로드 + KEK unlock(무인: env 비번 또는 DPAPI) + 지속 서명키 복구.
    let ks_path = keystore_util::keystore_path(config_path);
    if !Keystore::exists(&ks_path) {
        anyhow::bail!(
            "키스토어가 없습니다({}) — 먼저 `restoreplane init`을 실행하세요.",
            ks_path.display()
        );
    }
    let keystore = Keystore::load(&ks_path)?;
    let cred = keystore_util::resolve_unlock_credential(false, false)?;
    let kek = keystore
        .unlock_kek(&cred)
        .map_err(|e| anyhow::anyhow!("KEK unlock 실패(자격증명 확인): {e}"))?;
    let signing_key = keystore
        .signing_key(&kek)
        .map_err(|e| anyhow::anyhow!("서명키 복구 실패: {e}"))?;

    // config 필드 복호 어댑터 — KEK로 직접 파생한다(GUI가 등록 시 쓴 것과 동일한 세대).
    let field_enc = DekEncryptor::new(kek.clone(), FIELD_WRAP_GEN);

    // 카탈로그 싱글 라이터 — GUI 대시보드와 scheduler overlap guard가 공유하는 경로.
    std::fs::create_dir_all(&config.policy.spool_dir)?;
    let catalog_db_path = config.policy.spool_dir.join("catalog.db");
    let catalog = CatalogWriterHandle::spawn(catalog_db_path.clone())?;

    let notify = build_notification_sink(&config, &field_enc);

    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    rt.block_on(async move {
        let scheduler = SchedulerContext {
            catalog_writer: catalog,
            catalog_db_path,
            notify,
        };

        let mut exit_code = EXIT_SUCCESS;
        for (job, schedule) in &jobs {
            let runner = CliJobRunner {
                config: &config,
                job,
                keystore: &keystore,
                kek: &kek,
                signing_key: &signing_key,
                field_enc: &field_enc,
                catalog: &scheduler.catalog_writer,
            };
            let job_exit_code = run_scheduled_job(&job.name, schedule, &scheduler, &runner).await;
            exit_code = exit_code.max(job_exit_code);
        }
        Ok::<_, anyhow::Error>(exit_code)
    })
}

fn build_notification_sink(config: &Config, field_enc: &DekEncryptor) -> CompositeSink {
    let webhook_url_plaintext = match config.notify.as_ref() {
        Some(notify) if notify.channel == NotifyChannel::Webhook => notify
            .webhook_url
            .as_ref()
            .and_then(|field| match field_enc.unwrap_field(field) {
                Ok(url) => Some(url.to_string()),
                Err(e) => {
                    tracing::warn!(
                        error = %restoreplane_scheduler::scrub(&e.to_string()),
                        "선택적 webhook URL 복호 실패 — webhook만 비활성화하고 기본 알림으로 계속"
                    );
                    None
                }
            }),
        _ => None,
    };
    CompositeSink::default_plus_config(config.notify.as_ref(), webhook_url_plaintext)
}

/// 카탈로그 artifacts.dest_kind 문자열(스키마 어휘와 동일해야 rebuild 결과와 겹쳐 쓸 수 있다).
fn dest_kind_str(map: &std::collections::HashMap<String, DestKind>, name: &str) -> &'static str {
    match map.get(name) {
        Some(DestKind::Local) => "local",
        Some(DestKind::S3) => "s3",
        Some(DestKind::Ssh) => "ssh",
        None => "unknown",
    }
}

/// config의 EncField를 복호해 평문 시크릿을 얻는다. env가 설정돼 있으면 그것을 우선한다(운영 우회).
/// 둘 다 없거나 복호가 실패하면 요란히 실패한다 — 조용한 폴백은 잘못된 키를 감춘다.
fn resolve_secret(
    field_enc: &DekEncryptor,
    field: Option<&restoreplane_core::EncField>,
    env_key: &str,
    what: &str,
) -> anyhow::Result<Zeroizing<String>> {
    if let Ok(v) = std::env::var(env_key) {
        return Ok(Zeroizing::new(v));
    }
    let field = field.ok_or_else(|| {
        anyhow::anyhow!(
            "{what}: config에 암호화된 값이 없고 {env_key} 환경변수도 설정되지 않았습니다"
        )
    })?;
    field_enc.unwrap_field(field).map_err(|e| {
        anyhow::anyhow!("{what}: config 시크릿 복호 실패(키스토어/wrap 세대 불일치 가능): {e}")
    })
}

struct CliJobRunner<'a> {
    config: &'a Config,
    job: &'a restoreplane_config::schema::Job,
    keystore: &'a Keystore,
    kek: &'a Dek,
    signing_key: &'a SigningKey,
    field_enc: &'a DekEncryptor,
    catalog: &'a CatalogWriterHandle,
}

#[async_trait::async_trait]
impl JobRunner for CliJobRunner<'_> {
    async fn run(&self, job: &str) -> anyhow::Result<JobResult> {
        if job != self.job.name {
            anyhow::bail!(
                "scheduler가 요청한 job '{job}'과 runner job '{}'이 다릅니다",
                self.job.name
            );
        }
        execute_job(
            self.config,
            self.job,
            self.keystore,
            self.kek,
            self.signing_key,
            self.field_enc,
            self.catalog,
        )
        .await
    }
}

async fn execute_job(
    config: &Config,
    job: &restoreplane_config::schema::Job,
    keystore: &Keystore,
    kek: &Dek,
    signing_key: &SigningKey,
    field_enc: &DekEncryptor,
    catalog: &CatalogWriterHandle,
) -> anyhow::Result<JobResult> {
    // 1) 소스 DB 해석.
    let db = config
        .databases
        .iter()
        .find(|d| d.alias == job.source)
        .ok_or_else(|| {
            anyhow::anyhow!("job '{}'의 소스 '{}'를 찾을 수 없음", job.name, job.source)
        })?;

    // 2) 목적지 해석 — kind별로 Local/S3/SSH를 실제 배선(옵션 C 팬아웃).
    let mut dests: Vec<Arc<dyn Destination>> = Vec::new();
    for name in &job.destinations {
        let dest = config
            .destinations
            .iter()
            .find(|d| &d.name == name)
            .ok_or_else(|| anyhow::anyhow!("목적지 '{name}'를 config에서 찾을 수 없음"))?;
        match dest.kind {
            DestKind::Local => {
                let path = dest
                    .path
                    .clone()
                    .ok_or_else(|| anyhow::anyhow!("local 목적지 '{name}'에 path가 없음"))?;
                dests.push(Arc::new(LocalDest::new(dest.name.clone(), path)));
            }
            DestKind::S3 => {
                dests.push(build_s3_dest(dest, field_enc).await?);
            }
            DestKind::Ssh => {
                dests.push(build_ssh_dest(dest)?);
            }
        }
    }
    if dests.is_empty() {
        anyhow::bail!("job '{}'에 배선 가능한 목적지가 없음", job.name);
    }

    // 2a) 카탈로그 artifacts.dest_kind 기록용 이름→종류 맵(config 정의가 정본).
    let kind_by_name: std::collections::HashMap<String, DestKind> = config
        .destinations
        .iter()
        .map(|d| (d.name.clone(), d.kind))
        .collect();

    // 2b) Job 불변성 assert(P1-2/R12): required_immutability를 목적지 능력에 대해 검증. 미충족이면
    // 팬아웃 전에 **요란히 실패**한다(Local/SSH만으로 ObjectLock을 요구하는 오인 구성을 배선 시점에 차단).
    let required = map_required_immutability(job.required_immutability);
    assert_job_immutability(required, &dests).map_err(|e| {
        anyhow::anyhow!(
            "job '{}'의 required_immutability={:?}를 만족하는 목적지가 없음(랜섬웨어 방어 오인 방지): {e}",
            job.name,
            job.required_immutability
        )
    })?;

    // 3) 스풀 디렉토리 + 파이프라인 설정.
    let spool_dir = &config.policy.spool_dir;
    std::fs::create_dir_all(spool_dir)?;
    let mut cfg = PipelineCfg {
        zstd_level: 3,
        max_parallel_dumps: config.policy.max_parallel_dumps as usize,
        min_free_disk: config.policy.min_free_disk_mb.saturating_mul(1024 * 1024),
        upload_retries: config.policy.retry.attempts,
        ..PipelineCfg::default()
    };
    // R9(S04): MySQL 소스면 DDL-race stderr 패턴을 파이프라인 fatal_patterns에 병합한다
    // (exit=0이어도 부분 스키마 덤프를 전체 실패로 강등하는 호출 지점).
    if matches!(db.engine, Engine::Mysql | Engine::Mariadb) {
        cfg.fatal_patterns.extend(MySqlSource::fatal_patterns());
    }

    // 4) 엔진별 BackupSource 구성. 비밀번호는 config EncField를 복호해 쓰고, 엔진 관례 env가
    //    설정돼 있으면 그쪽이 우선한다(resolve_secret 주석 참조).
    let source: Box<dyn BackupSource> = match db.engine {
        Engine::Postgresql => {
            let password = resolve_secret(
                field_enc,
                Some(&db.password),
                "PGPASSWORD",
                &format!("database '{}' 비밀번호", db.alias),
            )?;
            let conn = PgConnCfg {
                host: db.host.clone(),
                port: db.port,
                user: db.user.clone(),
                password,
                admin_database: "postgres".to_string(),
                tls_mode: "preferred".to_string(),
            };
            let matrix = BundledToolMatrix::from_base_dir(Path::new("tools/pg"));
            Box::new(PostgresSource {
                job: job.name.clone(),
                alias: db.alias.clone(),
                engine_alias: db.alias.clone(),
                conn,
                registered_databases: vec![db.dbname.clone()],
                matrix,
            })
        }
        Engine::Mysql | Engine::Mariadb => {
            // 자식 프로세스로는 MYSQL_PWD env로만 흐른다(R10 — argv 평문 노출 금지).
            let password = resolve_secret(
                field_enc,
                Some(&db.password),
                "MYSQL_PWD",
                &format!("database '{}' 비밀번호", db.alias),
            )?;
            let conn = MySqlConnCfg {
                host: db.host.clone(),
                port: db.port,
                user: db.user.clone(),
                password: password.clone(),
                database: db.dbname.clone(),
                tls_mode: "preferred".to_owned(),
            };
            // 메타 조회 seam: `mysql` CLI(PATH). sqlx 미도입 편차 — meta.rs 주석 참조.
            let meta = MysqlCliMetaConn {
                client: std::path::PathBuf::from("mysql"),
                expected_sha256: None,
                trusted_dependencies: Vec::new(),
                host: db.host.clone(),
                port: db.port,
                user: db.user.clone(),
                password,
                client_kind: if db.engine == Engine::Mariadb {
                    ClientKind::MariaDb
                } else {
                    ClientKind::OracleMySql
                },
                tls_mode: "preferred".to_owned(),
            };
            let matrix = MysqldumpBinaryMatrix::from_base_dir(Path::new("tools/mysql"));
            Box::new(MySqlSource::new(
                job.name.clone(),
                db.alias.clone(),
                Box::new(meta),
                conn,
                matrix,
                true, // capture_binlog: 앵커 공백은 영구 재현 불가(R8)
            ))
        }
        Engine::Sqlserver | Engine::Sqlite => {
            anyhow::bail!(
                "ENGINE_REQUIRES_SERVICE_CONNECTION_PROFILE: SQL Server/SQLite Wave 1은 \
                 legacy host/user/password config가 아니라 v2 service Connection을 사용해야 합니다"
            )
        }
    };

    // 5) plan → 각 파트를 봉투 경로로 흘려보낸다(per-backup DEK → KEK-wrap → 서명 매니페스트).
    let plan = source.plan().await?;

    let mut parts: Vec<DumpPlanPart> = Vec::new();
    if let Some(g) = plan.globals_part {
        parts.push(g);
    }
    parts.extend(plan.db_parts);

    let mut primary_backup_id = None;
    let mut failed_destinations = std::collections::BTreeSet::new();

    for part in &parts {
        let dek = Dek::generate();
        let wrap_set = keystore
            .wrap_dek(kek, &dek, 1)
            .map_err(|e| anyhow::anyhow!("DEK wrap 실패: {e}"))?;
        let spool_path = spool_dir.join(format!("{}.spool", part.backup_id));
        let artifact = source.spawn(part)?;
        let commit = run_artifact(artifact, spool_path, dek, &cfg).await?;

        let manifest = Manifest::from_commit(part, db.engine, &commit, 1);
        let manifest_for_catalog = manifest.clone();
        let (manifest_path, _signed) = write_signed_manifest_with_wrapset(
            &commit.spool_path,
            manifest,
            wrap_set,
            signing_key,
        )?;

        let outcomes = fan_out(&commit, dests.clone(), &cfg).await;
        let ok = outcomes.iter().filter(|o| o.result.is_ok()).count();
        primary_backup_id = Some(part.backup_id.to_string());
        failed_destinations.extend(
            outcomes
                .iter()
                .filter(|o| o.result.is_err())
                .map(|o| o.dest_name.clone()),
        );

        // 암호문+서명 매니페스트가 모두 커밋된 목적지만 카탈로그 캐시에 반영한다(대시보드/조회용).
        // 카탈로그는 **캐시일 뿐**이라
        // 여기서 실패해도 백업 자체는 성공으로 둔다 — 매니페스트가 정본이고 rebuild로 복구된다.
        for outcome in outcomes.iter().filter(|o| o.result.is_ok()) {
            let kind = dest_kind_str(&kind_by_name, &outcome.dest_name);
            if let Err(e) = upsert_backup_from_manifest(
                catalog,
                manifest_for_catalog.clone(),
                outcome.dest_name.clone(),
                kind.to_string(),
            ) {
                tracing::warn!(
                    dest = %outcome.dest_name,
                    error = %e,
                    "카탈로그 캐시 반영 실패(백업은 성공 — `restoreplane catalog rebuild`로 복구 가능)"
                );
            }
        }
        println!(
            "job '{}' part {}: 스풀={} 매니페스트={} 목적지 {}/{} 성공",
            job.name,
            part.backup_id,
            commit.spool_path.display(),
            manifest_path.display(),
            ok,
            outcomes.len()
        );
        if ok == 0 {
            anyhow::bail!("part {}: 모든 목적지 업로드 실패", part.backup_id);
        }
    }

    let backup_id = primary_backup_id
        .ok_or_else(|| anyhow::anyhow!("job '{}'의 덤프 plan이 비어 있음", job.name))?;
    Ok(summarize_job_result(backup_id, failed_destinations))
}

fn summarize_job_result(
    backup_id: String,
    failed_destinations: std::collections::BTreeSet<String>,
) -> JobResult {
    if failed_destinations.is_empty() {
        JobResult::Success { backup_id }
    } else {
        JobResult::PartialSuccess {
            backup_id,
            failed_destinations: failed_destinations.into_iter().collect(),
        }
    }
}

/// config의 job.required_immutability(core 어휘)를 파이프라인 assert 어휘로 매핑(P1-2/R12).
fn map_required_immutability(cap: ImmutabilityCapability) -> RequiredImmutability {
    match cap {
        ImmutabilityCapability::None => RequiredImmutability::None,
        ImmutabilityCapability::Governance => RequiredImmutability::ObjectLock,
        ImmutabilityCapability::Compliance => RequiredImmutability::ComplianceOnly,
        ImmutabilityCapability::ServerAppendOnly => RequiredImmutability::AppendOnlyOrLock,
    }
}

/// S3 목적지 배선. append-only writer 자격증명은 config EncField를 복호해 쓰고,
/// AWS_ACCESS_KEY_ID/AWS_SECRET_ACCESS_KEY env가 설정돼 있으면 그쪽이 우선한다.
/// 배선 직후 **매 run 자가테스트**(deny-check + 락 상태 단언)를 수행하고, 실패(과권한/미잠금/전송오류)면
/// 백업을 시작하지 않는다(S06 불변식 (a) — 자가테스트 Ok일 때만 run 진행).
async fn build_s3_dest(
    dest: &restoreplane_config::schema::Destination,
    field_enc: &DekEncryptor,
) -> anyhow::Result<Arc<dyn Destination>> {
    let name = dest.name.clone();
    let access_key_id = resolve_secret(
        field_enc,
        dest.access_key_id.as_ref(),
        "AWS_ACCESS_KEY_ID",
        &format!("s3 목적지 '{name}' access key id(append-only writer 신원)"),
    )?;
    let secret_access_key = resolve_secret(
        field_enc,
        dest.secret_access_key.as_ref(),
        "AWS_SECRET_ACCESS_KEY",
        &format!("s3 목적지 '{name}' secret access key"),
    )?;

    let (object_lock_mode, retention_days) = match &dest.object_lock {
        Some(cfg) => (map_object_lock_mode(cfg.mode), cfg.retention_days),
        None => (ImmutabilityKind::None, 0),
    };
    let checksum_algo = dest.checksum_algo.map(|c| match c {
        ChecksumAlgo::Crc32c => "crc32c".to_string(),
        ChecksumAlgo::Sha256 => "sha256".to_string(),
    });

    let s3cfg = S3Cfg {
        endpoint: dest.endpoint.clone().unwrap_or_default(),
        region: dest
            .region
            .clone()
            .unwrap_or_else(|| "us-east-1".to_string()),
        bucket: dest
            .bucket
            .clone()
            .ok_or_else(|| anyhow::anyhow!("s3 목적지 '{name}'에 bucket이 없음"))?,
        prefix: dest.prefix.clone(),
        access_key_id: access_key_id.to_string(),
        secret_access_key: secret_access_key.to_string(),
        object_lock_mode,
        retention_days,
        // config에 per-dest dwell이 없어 0으로 둔다(가드 통과). dwell 검증은 자가테스트 경고로도 노출.
        min_dwell_days: 0,
        checksum_algo,
        // Compliance opt-in은 환경변수로 명시(M5 — 되돌릴 수 없는 긴 보존의 오설정 방지).
        compliance_opt_in: std::env::var("RESTOREPLANE_S3_COMPLIANCE_OPT_IN").is_ok(),
        provider: S3Provider::Aws,
    };
    let s3 = S3Dest::new(name.clone(), s3cfg)
        .map_err(|e| anyhow::anyhow!("s3 목적지 '{name}' 구성 실패: {e}"))?;

    // 매 run 자가테스트(H1/H2). 과권한/미잠금이면 여기서 요란히 실패 → 백업 시작 안 함.
    s3.self_test().await.map_err(|e| {
        anyhow::anyhow!("s3 목적지 '{name}' 자가테스트 실패 — 백업 거부(랜섬웨어 방어 미검증): {e}")
    })?;

    Ok(Arc::new(s3))
}

fn map_object_lock_mode(mode: ObjectLockMode) -> ImmutabilityKind {
    match mode {
        ObjectLockMode::None => ImmutabilityKind::None,
        ObjectLockMode::Governance => ImmutabilityKind::ObjectLockGovernance,
        ObjectLockMode::Compliance => ImmutabilityKind::ObjectLockCompliance,
    }
}

/// SSH(SFTP) 목적지 배선. host-key 지문은 config 필수(blind-accept 금지, R1). 개인키는 파일 경로,
/// passphrase/password는 환경변수(RESTOREPLANE_SSH_PASSPHRASE/RESTOREPLANE_SSH_PASSWORD)로 받는다 —
/// config EncField unwrap 전체 플로우는 후속 슬라이스(기존 DB 비번 편차와 정합).
fn build_ssh_dest(
    dest: &restoreplane_config::schema::Destination,
) -> anyhow::Result<Arc<dyn Destination>> {
    let name = dest.name.clone();
    let host = dest
        .host
        .clone()
        .ok_or_else(|| anyhow::anyhow!("ssh 목적지 '{name}'에 host가 없음"))?;
    let port = dest.port.unwrap_or(22);
    let user = dest
        .user
        .clone()
        .ok_or_else(|| anyhow::anyhow!("ssh 목적지 '{name}'에 user가 없음"))?;
    let fpr = dest.host_key_fingerprint.clone().ok_or_else(|| {
        anyhow::anyhow!("ssh 목적지 '{name}'에 host_key_fingerprint가 없음(MITM 방어 필수)")
    })?;
    let remote_dir = dest
        .path
        .clone()
        .ok_or_else(|| anyhow::anyhow!("ssh 목적지 '{name}'에 원격 경로(path)가 없음"))?
        .to_string_lossy()
        .into_owned();

    let policy = HostKeyPolicy::new(fpr, format!("{host}:{port}"));

    // 인증: 개인키 파일 우선, 없으면 password 환경변수.
    let auth = if let Some(key_path) = &dest.private_key_path {
        let pem = std::fs::read_to_string(key_path)
            .map_err(|e| anyhow::anyhow!("ssh 개인키 읽기 실패({}): {e}", key_path.display()))?;
        let passphrase = std::env::var("RESTOREPLANE_SSH_PASSPHRASE")
            .ok()
            .map(Zeroizing::new);
        SshAuth::PublicKey {
            identity_pem: Zeroizing::new(pem),
            passphrase,
        }
    } else {
        let password = std::env::var("RESTOREPLANE_SSH_PASSWORD").map_err(|_| {
            anyhow::anyhow!("ssh 목적지 '{name}'는 개인키(private_key_path) 또는 RESTOREPLANE_SSH_PASSWORD를 요구합니다")
        })?;
        SshAuth::Password {
            password: Zeroizing::new(password),
        }
    };

    // append_only는 서버측 보장을 config로 표현할 수단이 아직 없어 false(None caps). 서버 append-only가
    // 외부 보장될 때만 true로 올려야 하며, 그 전까지 ObjectLock 요구 Job은 이 목적지에서 실패한다(R9).
    let ssh = SshDest::new(name, host, port, user, policy, auth, remote_dir, false);
    Ok(Arc::new(ssh))
}

#[cfg(test)]
mod tests {
    use super::*;
    use restoreplane_config::schema::{
        Database, Destination, Job, Notify, Retention, VerifyPolicy,
    };
    use restoreplane_core::{EncField, NotifyChannel, SecretEncryptor};
    use restoreplane_secrets::Credential;

    fn valid_dry_run_config() -> Config {
        let mut config = Config::empty();
        config.databases.push(Database {
            alias: "app".to_string(),
            engine: Engine::Postgresql,
            host: "127.0.0.1".to_string(),
            port: 5432,
            dbname: "app".to_string(),
            user: "backup_reader".to_string(),
            password: EncField::new(1, b"ciphertext".to_vec()),
            tunnel: None,
            dump_globals: true,
            single_transaction: None,
        });
        config.destinations.push(Destination {
            name: "local".to_string(),
            kind: DestKind::Local,
            endpoint: None,
            region: None,
            bucket: None,
            prefix: String::new(),
            access_key_id: None,
            secret_access_key: None,
            checksum_algo: None,
            object_lock: None,
            tunnel: None,
            host: None,
            port: None,
            user: None,
            private_key_path: None,
            host_key_fingerprint: None,
            path: Some("D:/backup-spool".into()),
            reader: None,
        });
        config.jobs.push(Job {
            name: "nightly".to_string(),
            source: "app".to_string(),
            destinations: vec!["local".to_string()],
            required_immutability: ImmutabilityCapability::None,
            retention: Retention::default(),
            verify: VerifyPolicy::default(),
            schedule: "0 3 * * *".to_string(),
            enabled: true,
        });
        config
    }

    #[test]
    fn job_filter_rejects_unknown_job_instead_of_running_everything() {
        let dir = tempfile::tempdir().unwrap();
        let config_path = dir.path().join("restoreplane.toml");
        valid_dry_run_config().save(&config_path).unwrap();

        let err = run_after_execution_authority_check(
            &config_path,
            RunArgs {
                db: None,
                job: Some("missing".to_string()),
                scheduled: false,
                dry_run: true,
            },
        )
        .unwrap_err();

        assert!(
            err.to_string().contains("no matching enabled jobs"),
            "{err}"
        );
    }

    #[test]
    fn invalid_schedule_is_rejected_before_execution_setup() {
        let dir = tempfile::tempdir().unwrap();
        let config_path = dir.path().join("restoreplane.toml");
        let mut config = valid_dry_run_config();
        config.jobs[0].schedule = "unsupported cron".to_string();
        config.save(&config_path).unwrap();

        let err = run_after_execution_authority_check(
            &config_path,
            RunArgs {
                db: None,
                job: Some("nightly".to_string()),
                scheduled: true,
                dry_run: false,
            },
        )
        .unwrap_err();

        assert!(err.to_string().contains("schedule 파싱 실패"), "{err}");
    }

    #[test]
    fn failed_execution_is_recorded_through_scheduler_entrypoint() {
        let dir = tempfile::tempdir().unwrap();
        let config_path = dir.path().join("restoreplane.toml");
        let mut config = valid_dry_run_config();
        config.policy.spool_dir = dir.path().join("spool");
        config.save(&config_path).unwrap();

        let password = Zeroizing::new("test-master-password".to_string());
        let (keystore, _recovery) =
            Keystore::init(&password, dir.path().join("dpapi.keyfile")).unwrap();
        keystore
            .save(&keystore_util::keystore_path(&config_path))
            .unwrap();

        std::env::set_var(keystore_util::MASTER_PW_ENV, &*password);
        let run_result = run_after_execution_authority_check(
            &config_path,
            RunArgs {
                db: None,
                job: Some("nightly".to_string()),
                scheduled: true,
                dry_run: false,
            },
        );
        std::env::remove_var(keystore_util::MASTER_PW_ENV);

        let exit_code = run_result.unwrap();
        assert_eq!(exit_code, restoreplane_scheduler::EXIT_FAILED);

        let conn =
            restoreplane_catalog::open_reader_conn(&config.policy.spool_dir.join("catalog.db"))
                .unwrap();
        let failed_runs: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM runs WHERE job='nightly' AND status='failed'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(failed_runs, 1);
    }

    #[test]
    fn encrypted_webhook_url_is_unwrapped_for_scheduler_notifications() {
        let field_enc = DekEncryptor::new(Dek::generate(), FIELD_WRAP_GEN);
        let webhook_url = field_enc
            .wrap("https://notify.invalid/restoreplane")
            .unwrap();
        let mut config = Config::empty();
        config.notify = Some(Notify {
            channel: NotifyChannel::Webhook,
            webhook_url: Some(webhook_url),
            email_to: None,
        });

        let sink = build_notification_sink(&config, &field_enc);

        assert!(sink.sink_names().contains(&"webhook"));
    }

    #[test]
    fn invalid_optional_webhook_keeps_default_notification_sinks() {
        let encrypted_with_other_key = DekEncryptor::new(Dek::generate(), FIELD_WRAP_GEN)
            .wrap("https://notify.invalid/restoreplane")
            .unwrap();
        let field_enc = DekEncryptor::new(Dek::generate(), FIELD_WRAP_GEN);
        let mut config = Config::empty();
        config.notify = Some(Notify {
            channel: NotifyChannel::Webhook,
            webhook_url: Some(encrypted_with_other_key),
            email_to: None,
        });

        let sink = build_notification_sink(&config, &field_enc);
        let names = sink.sink_names();

        assert!(names.contains(&"log"));
        assert!(names.contains(&"windows-event-log"));
        assert!(!names.contains(&"webhook"));
    }

    #[test]
    fn destination_failures_are_reported_as_partial_success_without_duplicates() {
        let failed = ["ssh-archive", "s3-primary", "ssh-archive"]
            .into_iter()
            .map(str::to_string)
            .collect();

        let result = summarize_job_result("backup-1".to_string(), failed);

        match result {
            JobResult::PartialSuccess {
                backup_id,
                failed_destinations,
            } => {
                assert_eq!(backup_id, "backup-1");
                assert_eq!(failed_destinations, ["s3-primary", "ssh-archive"]);
            }
            other => panic!("expected partial success, got {other:?}"),
        }
    }

    /// 폐곡선: GUI/add-db가 KEK로 봉투암호화해 config에 쓴 EncField를,
    /// run이 키스토어에서 다시 unlock한 KEK로 그대로 복호한다.
    #[test]
    fn enc_field_roundtrip_through_keystore() {
        let dir = tempfile::tempdir().unwrap();
        let ks_path = dir.path().join("keystore.json");
        let pw = Zeroizing::new("correct-horse-staple".to_string());
        let (ks, _recovery) = Keystore::init(&pw, dir.path().join("dpapi.keyfile")).unwrap();
        ks.save(&ks_path).unwrap();

        // 등록 측(GUI/add-db): KEK unlock → wrap.
        let write_kek = Keystore::load(&ks_path)
            .unwrap()
            .unlock_kek(&Credential::Password(pw.clone()))
            .unwrap();
        let field = DekEncryptor::new(write_kek, FIELD_WRAP_GEN)
            .wrap("s3cr3t-db-pw")
            .unwrap();
        assert_eq!(field.wrap_gen, FIELD_WRAP_GEN);

        // 실행 측(run): 별도 로드 → 같은 평문.
        let run_kek = Keystore::load(&ks_path)
            .unwrap()
            .unlock_kek(&Credential::Password(pw))
            .unwrap();
        let field_enc = DekEncryptor::new(run_kek, FIELD_WRAP_GEN);
        let got = resolve_secret(
            &field_enc,
            Some(&field),
            "RESTOREPLANE_TEST_UNSET_PW",
            "test",
        )
        .unwrap();
        assert_eq!(&*got, "s3cr3t-db-pw");
    }

    /// 잘못된 KEK로는 조용히 통과하지 않는다(폴백 없이 실패해야 함).
    #[test]
    fn wrong_kek_fails_loudly() {
        let field = DekEncryptor::new(Dek::generate(), FIELD_WRAP_GEN)
            .wrap("pw")
            .unwrap();
        let other = DekEncryptor::new(Dek::generate(), FIELD_WRAP_GEN);
        let err = resolve_secret(&other, Some(&field), "RESTOREPLANE_TEST_UNSET_PW2", "test")
            .unwrap_err();
        assert!(err.to_string().contains("복호 실패"), "{err}");
    }

    /// env가 설정돼 있으면 운영 우회로 그쪽이 우선한다.
    #[test]
    fn env_overrides_config_field() {
        let key = "RESTOREPLANE_TEST_OVERRIDE_PW";
        std::env::set_var(key, "from-env");
        let field = DekEncryptor::new(Dek::generate(), FIELD_WRAP_GEN)
            .wrap("from-config")
            .unwrap();
        let other = DekEncryptor::new(Dek::generate(), FIELD_WRAP_GEN);
        let got = resolve_secret(&other, Some(&field), key, "test").unwrap();
        assert_eq!(&*got, "from-env");
        std::env::remove_var(key);
    }
}
