// crates/dbbackup-cli/src/commands/run.rs
//! S05/S03/S06 walking skeleton 배선: config → PostgresSource → 파이프라인 → LocalDest.
//! 슬라이스 1 범위: Postgres 소스 + Local 목적지만 실제 배선. S3/SSH는 명확한 메시지로 미배선.
//!
//! 봉투 경로(키스토어 연동):
//! - 키스토어를 로드하고 KEK를 unlock(무인: DBBACKUP_MASTER_PASSWORD env, 또는 DPAPI Machine-scope).
//! - 백업마다 per-backup DEK를 발급 → KEK로 감싼 WrapSet을 매니페스트에 동봉 → 지속 서명키로 서명.
//! - payload 암호화는 그 per-backup DEK로 수행(파이프라인 인코딩 체인).
//!
//! 편차/한계(슬라이스 1):
//! - DB 접속 비밀번호는 PGPASSWORD 환경변수로 받는다(EncField 시크릿 unwrap 전체 플로우는 후속 슬라이스).

use std::path::Path;
use std::sync::Arc;

use dbbackup_config::Config;
use dbbackup_core::{ChecksumAlgo, DestKind, Engine, ImmutabilityCapability, ObjectLockMode};
use dbbackup_dest_s3::{S3Cfg, S3Dest, S3Provider};
use dbbackup_dest_ssh::{HostKeyPolicy, SshAuth, SshDest};
use dbbackup_pipeline::{
    assert_job_immutability, fan_out, run_artifact, write_signed_manifest_with_wrapset,
    BackupSource, Destination, DumpPlanPart, ImmutabilityKind, LocalDest, Manifest, PipelineCfg,
    RequiredImmutability,
};
use dbbackup_secrets::{Dek, Keystore};
use dbbackup_source_mysql::{
    binary::MysqldumpBinaryMatrix, dump::MySqlConnCfg, meta::MysqlCliMetaConn, MySqlSource,
};
use dbbackup_source_pg::{version::BundledToolMatrix, PgConnCfg, PostgresSource};
use ed25519_dalek::SigningKey;
use zeroize::Zeroizing;

use crate::cli::RunArgs;
use crate::commands::keystore_util;

pub fn run(config_path: &Path, args: RunArgs) -> anyhow::Result<()> {
    let config = Config::load(config_path)?;
    dbbackup_config::validate(&config).map_err(|errs| anyhow::anyhow!("{errs:?}"))?;

    let jobs: Vec<_> = config
        .jobs
        .iter()
        .filter(|j| j.enabled)
        .filter(|j| args.db.as_deref().map_or(true, |db| j.source == db))
        .cloned()
        .collect();

    if jobs.is_empty() {
        anyhow::bail!("no matching enabled jobs for --db={:?}", args.db);
    }

    if args.dry_run {
        for job in &jobs {
            println!(
                "[dry-run] would run job '{}': {} -> {:?}",
                job.name, job.source, job.destinations
            );
        }
        return Ok(());
    }

    // 키스토어 로드 + KEK unlock(무인: env 비번 또는 DPAPI) + 지속 서명키 복구.
    let ks_path = keystore_util::keystore_path(config_path);
    if !Keystore::exists(&ks_path) {
        anyhow::bail!(
            "키스토어가 없습니다({}) — 먼저 `dbbackup init`을 실행하세요.",
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

    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    rt.block_on(async move {
        for job in &jobs {
            execute_job(&config, job, &keystore, &kek, &signing_key).await?;
        }
        Ok::<_, anyhow::Error>(())
    })
}

async fn execute_job(
    config: &Config,
    job: &dbbackup_config::schema::Job,
    keystore: &Keystore,
    kek: &Dek,
    signing_key: &SigningKey,
) -> anyhow::Result<()> {
    // 1) 소스 DB 해석.
    let db = config
        .databases
        .iter()
        .find(|d| d.alias == job.source)
        .ok_or_else(|| anyhow::anyhow!("job '{}'의 소스 '{}'를 찾을 수 없음", job.name, job.source))?;

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
                dests.push(build_s3_dest(dest).await?);
            }
            DestKind::Ssh => {
                dests.push(build_ssh_dest(dest)?);
            }
        }
    }
    if dests.is_empty() {
        anyhow::bail!("job '{}'에 배선 가능한 목적지가 없음", job.name);
    }

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

    // 4) 엔진별 BackupSource 구성. 비밀번호는 엔진 관례 env로 받는다(슬라이스 한계 — 위 주석 참조).
    let source: Box<dyn BackupSource> = match db.engine {
        Engine::Postgresql => {
            let password = std::env::var("PGPASSWORD").map_err(|_| {
                anyhow::anyhow!("postgresql run은 DB 비밀번호를 PGPASSWORD 환경변수로 요구합니다")
            })?;
            let conn = PgConnCfg {
                host: db.host.clone(),
                port: db.port,
                user: db.user.clone(),
                password: Zeroizing::new(password),
                admin_database: "postgres".to_string(),
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
            // 비밀번호는 MYSQL_PWD env로만 흐른다(R10 — argv 평문 노출 금지).
            let password = std::env::var("MYSQL_PWD").map_err(|_| {
                anyhow::anyhow!("mysql/mariadb run은 DB 비밀번호를 MYSQL_PWD 환경변수로 요구합니다")
            })?;
            let conn = MySqlConnCfg {
                host: db.host.clone(),
                port: db.port,
                user: db.user.clone(),
                password: Zeroizing::new(password.clone()),
                database: db.dbname.clone(),
            };
            // 메타 조회 seam: `mysql` CLI(PATH). sqlx 미도입 편차 — meta.rs 주석 참조.
            let meta = MysqlCliMetaConn {
                client: std::path::PathBuf::from("mysql"),
                host: db.host.clone(),
                port: db.port,
                user: db.user.clone(),
                password: Zeroizing::new(password),
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
    };

    // 5) plan → 각 파트를 봉투 경로로 흘려보낸다(per-backup DEK → KEK-wrap → 서명 매니페스트).
    let plan = source.plan().await?;

    let mut parts: Vec<DumpPlanPart> = Vec::new();
    if let Some(g) = plan.globals_part {
        parts.push(g);
    }
    parts.extend(plan.db_parts);

    for part in &parts {
        let dek = Dek::generate();
        let wrap_set = keystore
            .wrap_dek(kek, &dek, 1)
            .map_err(|e| anyhow::anyhow!("DEK wrap 실패: {e}"))?;
        let spool_path = spool_dir.join(format!("{}.spool", part.backup_id));
        let artifact = source.spawn(part)?;
        let commit = run_artifact(artifact, spool_path, dek, &cfg).await?;

        let manifest = Manifest::from_commit(part, db.engine, &commit, 1);
        let (manifest_path, _signed) =
            write_signed_manifest_with_wrapset(&commit.spool_path, manifest, wrap_set, signing_key)?;

        let outcomes = fan_out(&commit, dests.clone(), &cfg).await;
        let ok = outcomes.iter().filter(|o| o.result.is_ok()).count();
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

    Ok(())
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

/// S3 목적지 배선. 자격증명은 (기존 DB 비번 편차와 동일하게) 환경변수로 받는다 —
/// AWS_ACCESS_KEY_ID/AWS_SECRET_ACCESS_KEY. config EncField unwrap 전체 플로우는 후속 슬라이스.
/// 배선 직후 **매 run 자가테스트**(deny-check + 락 상태 단언)를 수행하고, 실패(과권한/미잠금/전송오류)면
/// 백업을 시작하지 않는다(S06 불변식 (a) — 자가테스트 Ok일 때만 run 진행).
async fn build_s3_dest(
    dest: &dbbackup_config::schema::Destination,
) -> anyhow::Result<Arc<dyn Destination>> {
    let name = dest.name.clone();
    let access_key_id = std::env::var("AWS_ACCESS_KEY_ID").map_err(|_| {
        anyhow::anyhow!("s3 목적지 '{name}'는 AWS_ACCESS_KEY_ID 환경변수(append-only writer 신원)를 요구합니다")
    })?;
    let secret_access_key = std::env::var("AWS_SECRET_ACCESS_KEY")
        .map_err(|_| anyhow::anyhow!("s3 목적지 '{name}'는 AWS_SECRET_ACCESS_KEY 환경변수를 요구합니다"))?;

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
        region: dest.region.clone().unwrap_or_else(|| "us-east-1".to_string()),
        bucket: dest
            .bucket
            .clone()
            .ok_or_else(|| anyhow::anyhow!("s3 목적지 '{name}'에 bucket이 없음"))?,
        prefix: dest.prefix.clone(),
        access_key_id,
        secret_access_key,
        object_lock_mode,
        retention_days,
        // config에 per-dest dwell이 없어 0으로 둔다(가드 통과). dwell 검증은 자가테스트 경고로도 노출.
        min_dwell_days: 0,
        checksum_algo,
        // Compliance opt-in은 환경변수로 명시(M5 — 되돌릴 수 없는 긴 보존의 오설정 방지).
        compliance_opt_in: std::env::var("DBBACKUP_S3_COMPLIANCE_OPT_IN").is_ok(),
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
/// passphrase/password는 환경변수(DBBACKUP_SSH_PASSPHRASE/DBBACKUP_SSH_PASSWORD)로 받는다 —
/// config EncField unwrap 전체 플로우는 후속 슬라이스(기존 DB 비번 편차와 정합).
fn build_ssh_dest(
    dest: &dbbackup_config::schema::Destination,
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
    let fpr = dest
        .host_key_fingerprint
        .clone()
        .ok_or_else(|| anyhow::anyhow!("ssh 목적지 '{name}'에 host_key_fingerprint가 없음(MITM 방어 필수)"))?;
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
        let passphrase = std::env::var("DBBACKUP_SSH_PASSPHRASE").ok().map(Zeroizing::new);
        SshAuth::PublicKey { identity_pem: Zeroizing::new(pem), passphrase }
    } else {
        let password = std::env::var("DBBACKUP_SSH_PASSWORD").map_err(|_| {
            anyhow::anyhow!("ssh 목적지 '{name}'는 개인키(private_key_path) 또는 DBBACKUP_SSH_PASSWORD를 요구합니다")
        })?;
        SshAuth::Password { password: Zeroizing::new(password) }
    };

    // append_only는 서버측 보장을 config로 표현할 수단이 아직 없어 false(None caps). 서버 append-only가
    // 외부 보장될 때만 true로 올려야 하며, 그 전까지 ObjectLock 요구 Job은 이 목적지에서 실패한다(R9).
    let ssh = SshDest::new(name, host, port, user, policy, auth, remote_dir, false);
    Ok(Arc::new(ssh))
}
