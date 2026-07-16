// crates/dbbackup-cli/src/commands/run.rs
//! S05/S03/S06 walking skeleton 배선: config → PostgresSource → 파이프라인 → LocalDest.
//! 슬라이스 1 범위: Postgres 소스 + Local 목적지만 실제 배선. S3/SSH는 명확한 메시지로 미배선.
//!
//! 편차/한계(슬라이스 1):
//! - DB 접속 비밀번호는 PGPASSWORD 환경변수로 받는다(EncField 시크릿 unwrap 전체 플로우는 후속 슬라이스).
//! - 매니페스트 서명키는 config에 핀되기 전이라 임시 키페어로 서명한다(공개키는 매니페스트에 동봉).

use std::path::Path;
use std::sync::Arc;

use dbbackup_config::Config;
use dbbackup_core::{DestKind, Engine};
use dbbackup_pipeline::{
    fan_out, run_artifact, write_signed_manifest, BackupSource, Destination, DumpPlanPart,
    LocalDest, Manifest, PipelineCfg,
};
use dbbackup_secrets::{generate_signing_keypair, Dek};
use dbbackup_source_pg::{version::BundledToolMatrix, PgConnCfg, PostgresSource};
use zeroize::Zeroizing;

use crate::cli::RunArgs;

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

    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    rt.block_on(async move {
        for job in &jobs {
            execute_job(&config, job).await?;
        }
        Ok::<_, anyhow::Error>(())
    })
}

async fn execute_job(config: &Config, job: &dbbackup_config::schema::Job) -> anyhow::Result<()> {
    // 1) 소스 DB 해석.
    let db = config
        .databases
        .iter()
        .find(|d| d.alias == job.source)
        .ok_or_else(|| anyhow::anyhow!("job '{}'의 소스 '{}'를 찾을 수 없음", job.name, job.source))?;

    if db.engine != Engine::Postgresql {
        anyhow::bail!("슬라이스 1은 postgresql 소스만 배선됨(요청: {:?})", db.engine);
    }

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
    let cfg = PipelineCfg {
        zstd_level: 3,
        max_parallel_dumps: config.policy.max_parallel_dumps as usize,
        min_free_disk: config.policy.min_free_disk_mb.saturating_mul(1024 * 1024),
        upload_retries: config.policy.retry.attempts,
        ..PipelineCfg::default()
    };

    // 4) PostgresSource 구성. 비밀번호는 PGPASSWORD env로(슬라이스 1 한계 — 위 주석 참조).
    let password = std::env::var("PGPASSWORD").map_err(|_| {
        anyhow::anyhow!("슬라이스 1 run은 DB 비밀번호를 PGPASSWORD 환경변수로 요구합니다")
    })?;
    let conn = PgConnCfg {
        host: db.host.clone(),
        port: db.port,
        user: db.user.clone(),
        password: Zeroizing::new(password),
        admin_database: "postgres".to_string(),
    };
    let matrix = BundledToolMatrix::from_base_dir(Path::new("tools/pg"));
    let source = PostgresSource {
        job: job.name.clone(),
        alias: db.alias.clone(),
        engine_alias: db.alias.clone(),
        conn,
        registered_databases: vec![db.dbname.clone()],
        matrix,
    };

    // 5) plan → 각 파트를 파이프라인으로 흘려보낸다.
    let plan = source.plan().await?;
    let (sk, _vk) = generate_signing_keypair(); // 슬라이스 1: 임시 서명키(공개키는 매니페스트 동봉)

    let mut parts: Vec<DumpPlanPart> = Vec::new();
    if let Some(g) = plan.globals_part {
        parts.push(g);
    }
    parts.extend(plan.db_parts);

    for part in &parts {
        let dek = Dek::generate();
        let spool_path = spool_dir.join(format!("{}.spool", part.backup_id));
        let artifact = source.spawn(part)?;
        let commit = run_artifact(artifact, spool_path, dek, &cfg).await?;

        let manifest = Manifest::from_commit(part, db.engine, &commit, 1);
        let (manifest_path, _signed) = write_signed_manifest(&commit.spool_path, manifest, &sk)?;

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
