//! Tauri 커맨드 — 프론트가 호출하는 백엔드 진입점.
//!
//! 증분1 범위: 읽기 전용 대시보드. config(잡/DB/목적지 정의) + catalog(최신 백업/검증)만 읽어
//! DashboardData(프론트 types.ts와 camelCase로 형태 일치)를 만든다. 액션(init/run/restore 등)은
//! 후속 증분에서 dbbackup.exe 사이드카로 배선한다.

use dbbackup_catalog::{latest_backup_per_job, open_reader_conn, BackupRecord};
use dbbackup_config::schema::{Config, Destination, Job};
use dbbackup_core::{DestKind, ObjectLockMode};
use dbbackup_secrets::Keystore;
use serde::Serialize;

use crate::paths::{config_path, keystore_path};

#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct DestArtifactView {
    dest_name: String,
    dest_kind: String,
    immutability: String,
    retention_until: Option<String>,
    status: String,
}

#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct BackupView {
    id: String,
    job: String,
    source: String,
    engine: String,
    started_at: String,
    finished_at: Option<String>,
    status: String,
    plaintext_bytes: Option<i64>,
    cipher_bytes: Option<i64>,
    manifest_signed: bool,
    last_verify_level: Option<i64>,
    last_verify_result: Option<String>,
    artifacts: Vec<DestArtifactView>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct JobSummaryView {
    name: String,
    source: String,
    engine: String,
    schedule: String,
    required_immutability: String,
    destinations: Vec<String>,
    last_backup: Option<BackupView>,
    health: String,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DashboardData {
    config_present: bool,
    keystore_present: bool,
    schema_version: Option<u32>,
    jobs: Vec<JobSummaryView>,
    recent: Vec<BackupView>,
    overall: String,
}

/// 목적지 불변성 등급을 config 정의에서 유도한다(catalog artifacts.immutability는 현재 NULL).
fn immutability_of(dest: &Destination) -> &'static str {
    match dest.kind {
        DestKind::Local => "none",
        DestKind::Ssh => "server_append_only",
        DestKind::S3 => match dest.object_lock.as_ref().map(|c| c.mode) {
            Some(ObjectLockMode::Governance) => "object_lock_governance",
            Some(ObjectLockMode::Compliance) => "object_lock_compliance",
            _ => "none",
        },
    }
}

fn artifacts_for_job(job: &Job, config: &Config, backup_status: &str) -> Vec<DestArtifactView> {
    job.destinations
        .iter()
        .filter_map(|name| config.destinations.iter().find(|d| &d.name == name))
        .map(|d| DestArtifactView {
            dest_name: d.name.clone(),
            dest_kind: match d.kind {
                DestKind::Local => "local",
                DestKind::Ssh => "ssh",
                DestKind::S3 => "s3",
            }
            .to_string(),
            immutability: immutability_of(d).to_string(),
            retention_until: None,
            status: if backup_status == "success" {
                "uploaded".to_string()
            } else {
                backup_status.to_string()
            },
        })
        .collect()
}

fn engine_str(e: dbbackup_core::Engine) -> &'static str {
    match e {
        dbbackup_core::Engine::Postgresql => "postgresql",
        dbbackup_core::Engine::Mysql => "mysql",
        dbbackup_core::Engine::Mariadb => "mariadb",
    }
}

/// 잡 건강도: 실패/미백업=danger·warn, 미서명·미검증=warn, 서명+검증통과=ok.
fn health_of(rec: Option<&BackupRecord>) -> &'static str {
    match rec {
        None => "warn",
        Some(r) if r.status == "failed" => "danger",
        Some(r) if r.status == "running" => "warn",
        Some(r) if !r.manifest_signed => "warn",
        Some(r) if r.last_verify_result.as_deref() != Some("pass") => "warn",
        Some(_) => "ok",
    }
}

fn to_backup_view(rec: &BackupRecord, artifacts: Vec<DestArtifactView>) -> BackupView {
    BackupView {
        id: rec.id.clone(),
        job: rec.job.clone(),
        source: rec.source.clone(),
        engine: rec.engine.clone(),
        started_at: rec.started_at.clone(),
        finished_at: rec.finished_at.clone(),
        status: rec.status.clone(),
        plaintext_bytes: rec.plaintext_bytes,
        cipher_bytes: rec.cipher_bytes,
        manifest_signed: rec.manifest_signed,
        last_verify_level: rec.last_verify_level,
        last_verify_result: rec.last_verify_result.clone(),
        artifacts,
    }
}

fn overall_of(jobs: &[JobSummaryView]) -> &'static str {
    if jobs.is_empty() {
        return "empty";
    }
    if jobs.iter().any(|j| j.health == "danger") {
        "danger"
    } else if jobs.iter().any(|j| j.health == "warn") {
        "warn"
    } else {
        "ok"
    }
}

#[tauri::command]
pub fn dashboard() -> Result<DashboardData, String> {
    let cfg_path = config_path();
    let config_present = cfg_path.exists();

    if !config_present {
        return Ok(DashboardData {
            config_present: false,
            keystore_present: false,
            schema_version: None,
            jobs: vec![],
            recent: vec![],
            overall: "empty".to_string(),
        });
    }

    let config = Config::load(&cfg_path).map_err(|e| format!("config 로드 실패: {e}"))?;
    let keystore_present = Keystore::exists(&keystore_path(&cfg_path));

    // catalog는 조회 캐시(신뢰 불가). 없으면 잡 정의만으로 대시보드를 구성한다.
    let db_path = config.policy.spool_dir.join("catalog.db");
    let records: Vec<BackupRecord> = if db_path.exists() {
        let conn = open_reader_conn(&db_path).map_err(|e| format!("catalog 열기 실패: {e}"))?;
        latest_backup_per_job(&conn).map_err(|e| format!("catalog 조회 실패: {e}"))?
    } else {
        vec![]
    };

    let db_engine = |alias: &str| {
        config
            .databases
            .iter()
            .find(|d| d.alias == alias)
            .map(|d| engine_str(d.engine))
            .unwrap_or("postgresql")
    };

    let jobs: Vec<JobSummaryView> = config
        .jobs
        .iter()
        .map(|job| {
            let rec = records.iter().find(|r| r.job == job.name);
            let last_backup = rec.map(|r| {
                let arts = artifacts_for_job(job, &config, &r.status);
                to_backup_view(r, arts)
            });
            JobSummaryView {
                name: job.name.clone(),
                source: job.source.clone(),
                engine: db_engine(&job.source).to_string(),
                schedule: if job.schedule.trim().is_empty() {
                    "예약 없음".to_string()
                } else {
                    job.schedule.clone()
                },
                required_immutability: format!("{:?}", job.required_immutability).to_lowercase(),
                destinations: job.destinations.clone(),
                last_backup,
                health: health_of(rec).to_string(),
            }
        })
        .collect();

    let recent: Vec<BackupView> = jobs.iter().filter_map(|j| j.last_backup.clone()).collect();

    let overall = overall_of(&jobs).to_string();

    Ok(DashboardData {
        config_present: true,
        keystore_present,
        schema_version: Some(config.meta.schema_version),
        jobs,
        recent,
        overall,
    })
}
