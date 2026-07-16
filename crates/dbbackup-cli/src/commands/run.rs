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
use dbbackup_core::{DestKind, Engine};
use dbbackup_pipeline::{
    fan_out, run_artifact, write_signed_manifest_with_wrapset, BackupSource, Destination,
    DumpPlanPart, LocalDest, Manifest, PipelineCfg,
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

    // 2) 목적지 해석 — local만 실제 배선, 나머지는 미배선 안내.
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
            DestKind::S3 | DestKind::Ssh => {
                tracing::warn!(dest = %name, kind = ?dest.kind, "이 목적지 종류는 아직 배선되지 않음(슬라이스 2+)");
                println!("[skip] 목적지 '{name}'({:?})는 아직 배선되지 않았습니다.", dest.kind);
            }
        }
    }
    if dests.is_empty() {
        anyhow::bail!("job '{}'에 배선 가능한 local 목적지가 없음", job.name);
    }

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
