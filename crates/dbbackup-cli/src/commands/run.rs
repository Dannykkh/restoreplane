// crates/dbbackup-cli/src/commands/run.rs
//! S05/S03/S06 walking skeleton 배선: config → PostgresSource → 파이프라인 → LocalDest.
//! 슬라이스 1 범위: Postgres 소스 + Local 목적지만 실제 배선. S3/SSH는 명확한 메시지로 미배선.
//!
//! 봉투 경로(키스토어 연동):
//! - 키스토어를 로드하고 KEK를 unlock(무인: DBBACKUP_MASTER_PASSWORD env, 또는 DPAPI Machine-scope).
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

use dbbackup_catalog::{upsert_backup_from_manifest, CatalogWriterHandle};
use dbbackup_config::Config;
use dbbackup_core::{ChecksumAlgo, DestKind, Engine, ImmutabilityCapability, ObjectLockMode};
use dbbackup_dest_s3::{S3Cfg, S3Dest, S3Provider};
use dbbackup_dest_ssh::{HostKeyPolicy, SshAuth, SshDest};
use dbbackup_pipeline::{
    assert_job_immutability, fan_out, run_artifact, write_signed_manifest_with_wrapset,
    BackupSource, Destination, DumpPlanPart, ImmutabilityKind, LocalDest, Manifest, PipelineCfg,
    RequiredImmutability,
};
use dbbackup_secrets::{Dek, DekEncryptor, Keystore};
use dbbackup_source_mysql::{
    binary::MysqldumpBinaryMatrix, dump::MySqlConnCfg, meta::MysqlCliMetaConn, MySqlSource,
};
use dbbackup_source_pg::{version::BundledToolMatrix, PgConnCfg, PostgresSource};
use ed25519_dalek::SigningKey;
use zeroize::Zeroizing;

use crate::cli::RunArgs;
use crate::commands::keystore_util;

/// config 필드 암호화 세대. GUI(apps/console config_edit)가 쓰는 값과 같아야 한다 —
/// 달라지면 rotate-key 대상 식별이 어긋난다(core::EncField P2-1).
pub const FIELD_WRAP_GEN: u32 = 1;

pub fn run(config_path: &Path, args: RunArgs) -> anyhow::Result<()> {
    let config = Config::load(config_path)?;
    dbbackup_config::validate(&config).map_err(|errs| anyhow::anyhow!("{errs:?}"))?;

    let jobs: Vec<_> = config
        .jobs
        .iter()
        .filter(|j| j.enabled)
        .filter(|j| args.db.as_deref().is_none_or(|db| j.source == db))
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

    // config 필드 복호 어댑터 — KEK로 직접 파생한다(GUI가 등록 시 쓴 것과 동일한 세대).
    let field_enc = DekEncryptor::new(kek.clone(), FIELD_WRAP_GEN);

    // 카탈로그 싱글 라이터 — GUI 대시보드가 읽는 것과 같은 경로(spool_dir/catalog.db).
    std::fs::create_dir_all(&config.policy.spool_dir)?;
    let catalog = CatalogWriterHandle::spawn(config.policy.spool_dir.join("catalog.db"))?;

    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    rt.block_on(async move {
        for job in &jobs {
            execute_job(
                &config,
                job,
                &keystore,
                &kek,
                &signing_key,
                &field_enc,
                &catalog,
            )
            .await?;
        }
        Ok::<_, anyhow::Error>(())
    })
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
    field: Option<&dbbackup_core::EncField>,
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

async fn execute_job(
    config: &Config,
    job: &dbbackup_config::schema::Job,
    keystore: &Keystore,
    kek: &Dek,
    signing_key: &SigningKey,
    field_enc: &DekEncryptor,
    catalog: &CatalogWriterHandle,
) -> anyhow::Result<()> {
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
            };
            // 메타 조회 seam: `mysql` CLI(PATH). sqlx 미도입 편차 — meta.rs 주석 참조.
            let meta = MysqlCliMetaConn {
                client: std::path::PathBuf::from("mysql"),
                host: db.host.clone(),
                port: db.port,
                user: db.user.clone(),
                password,
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
        let manifest_for_catalog = manifest.clone();
        let (manifest_path, _signed) = write_signed_manifest_with_wrapset(
            &commit.spool_path,
            manifest,
            wrap_set,
            signing_key,
        )?;

        let outcomes = fan_out(&commit, dests.clone(), &cfg).await;
        let ok = outcomes.iter().filter(|o| o.result.is_ok()).count();

        // 커밋된 목적지만 카탈로그 캐시에 반영한다(대시보드/조회용). 카탈로그는 **캐시일 뿐**이라
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
                    "카탈로그 캐시 반영 실패(백업은 성공 — `dbbackup catalog rebuild`로 복구 가능)"
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

/// S3 목적지 배선. append-only writer 자격증명은 config EncField를 복호해 쓰고,
/// AWS_ACCESS_KEY_ID/AWS_SECRET_ACCESS_KEY env가 설정돼 있으면 그쪽이 우선한다.
/// 배선 직후 **매 run 자가테스트**(deny-check + 락 상태 단언)를 수행하고, 실패(과권한/미잠금/전송오류)면
/// 백업을 시작하지 않는다(S06 불변식 (a) — 자가테스트 Ok일 때만 run 진행).
async fn build_s3_dest(
    dest: &dbbackup_config::schema::Destination,
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
        let passphrase = std::env::var("DBBACKUP_SSH_PASSPHRASE")
            .ok()
            .map(Zeroizing::new);
        SshAuth::PublicKey {
            identity_pem: Zeroizing::new(pem),
            passphrase,
        }
    } else {
        let password = std::env::var("DBBACKUP_SSH_PASSWORD").map_err(|_| {
            anyhow::anyhow!("ssh 목적지 '{name}'는 개인키(private_key_path) 또는 DBBACKUP_SSH_PASSWORD를 요구합니다")
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
    use dbbackup_core::SecretEncryptor;
    use dbbackup_secrets::Credential;

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
        let got =
            resolve_secret(&field_enc, Some(&field), "DBBACKUP_TEST_UNSET_PW", "test").unwrap();
        assert_eq!(&*got, "s3cr3t-db-pw");
    }

    /// 잘못된 KEK로는 조용히 통과하지 않는다(폴백 없이 실패해야 함).
    #[test]
    fn wrong_kek_fails_loudly() {
        let field = DekEncryptor::new(Dek::generate(), FIELD_WRAP_GEN)
            .wrap("pw")
            .unwrap();
        let other = DekEncryptor::new(Dek::generate(), FIELD_WRAP_GEN);
        let err =
            resolve_secret(&other, Some(&field), "DBBACKUP_TEST_UNSET_PW2", "test").unwrap_err();
        assert!(err.to_string().contains("복호 실패"), "{err}");
    }

    /// env가 설정돼 있으면 운영 우회로 그쪽이 우선한다.
    #[test]
    fn env_overrides_config_field() {
        let key = "DBBACKUP_TEST_OVERRIDE_PW";
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
