//! Tauri 커맨드 — 프론트가 호출하는 백엔드 진입점.
//!
//! 증분1 범위: 읽기 전용 대시보드. config(잡/DB/목적지 정의) + catalog(최신 백업/검증)만 읽어
//! DashboardData(프론트 types.ts와 camelCase로 형태 일치)를 만든다. 액션(init/run/restore 등)은
//! 후속 증분에서 restoreplane.exe 사이드카로 배선한다.

use std::collections::HashMap;

use restoreplane_catalog::{
    artifacts_for_backup, latest_backup_per_job, latest_run_per_job, open_reader_conn,
    ArtifactRecord, BackupRecord, LatestRunRecord,
};
use restoreplane_config::schema::Config;
use restoreplane_secrets::Keystore;
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
    last_attempt_status: Option<String>,
    last_attempt_started_at: Option<String>,
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

fn to_artifact_view(record: &ArtifactRecord) -> DestArtifactView {
    DestArtifactView {
        dest_name: record.dest_name.clone(),
        dest_kind: record.dest_kind.clone(),
        immutability: record
            .immutability
            .clone()
            .unwrap_or_else(|| "unknown".to_string()),
        retention_until: record.retention_until.clone(),
        status: record.status.clone(),
    }
}

fn engine_str(e: restoreplane_core::Engine) -> &'static str {
    match e {
        restoreplane_core::Engine::Postgresql => "postgresql",
        restoreplane_core::Engine::Mysql => "mysql",
        restoreplane_core::Engine::Mariadb => "mariadb",
        restoreplane_core::Engine::Sqlserver => "sqlserver",
        restoreplane_core::Engine::Sqlite => "sqlite",
    }
}

/// 잡 건강도: 최신 실행 실패/부분 성공을 과거 성공 backup보다 먼저 평가한다.
fn health_of(rec: Option<&BackupRecord>, latest_attempt: Option<&LatestRunRecord>) -> &'static str {
    match latest_attempt.map(|attempt| attempt.status.as_str()) {
        Some("failed" | "partial") => return "danger",
        Some("running" | "coalesced") => return "warn",
        Some("success") | None => {}
        Some(_) => return "warn",
    }
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
    let (records, attempts, artifacts_by_backup): (
        Vec<BackupRecord>,
        Vec<LatestRunRecord>,
        HashMap<String, Vec<ArtifactRecord>>,
    ) = if db_path.exists() {
        let conn = open_reader_conn(&db_path).map_err(|e| format!("catalog 열기 실패: {e}"))?;
        let records =
            latest_backup_per_job(&conn).map_err(|e| format!("catalog 조회 실패: {e}"))?;
        let attempts =
            latest_run_per_job(&conn).map_err(|e| format!("실행 이력 조회 실패: {e}"))?;
        let mut artifacts_by_backup = HashMap::new();
        for record in &records {
            let artifacts = artifacts_for_backup(&conn, &record.id)
                .map_err(|e| format!("artifact 조회 실패: {e}"))?;
            artifacts_by_backup.insert(record.id.clone(), artifacts);
        }
        (records, attempts, artifacts_by_backup)
    } else {
        (vec![], vec![], HashMap::new())
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
            let attempt = attempts.iter().find(|r| r.job == job.name);
            let last_backup = rec.map(|r| {
                let arts = artifacts_by_backup
                    .get(&r.id)
                    .map(|records| records.iter().map(to_artifact_view).collect())
                    .unwrap_or_default();
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
                last_attempt_status: attempt.map(|value| value.status.clone()),
                last_attempt_started_at: attempt.map(|value| value.started_at.clone()),
                health: health_of(rec, attempt).to_string(),
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

#[cfg(test)]
mod tests {
    use super::*;
    use restoreplane_catalog::{ArtifactRecord, LatestRunRecord};

    fn successful_backup() -> BackupRecord {
        BackupRecord {
            id: "backup-ok".into(),
            job: "nightly".into(),
            source: "primary".into(),
            engine: "postgresql".into(),
            started_at: "2026-07-28T03:00:00Z".into(),
            finished_at: Some("2026-07-28T03:05:00Z".into()),
            status: "success".into(),
            plaintext_bytes: Some(1_000),
            cipher_bytes: Some(300),
            manifest_signed: true,
            last_verify_level: Some(1),
            last_verify_result: Some("pass".into()),
        }
    }

    #[test]
    fn latest_failed_attempt_overrides_older_successful_backup_health() {
        let backup = successful_backup();
        let attempt = LatestRunRecord {
            id: "run-failed".into(),
            job: "nightly".into(),
            started_at: "2026-07-29T03:00:00Z".into(),
            status: "failed".into(),
        };

        assert_eq!(health_of(Some(&backup), Some(&attempt)), "danger");
    }

    #[test]
    fn artifact_view_preserves_unknown_evidence_instead_of_using_config_claims() {
        let artifact = ArtifactRecord {
            backup_id: "backup-ok".into(),
            dest_name: "s3-primary".into(),
            dest_kind: "s3".into(),
            object_key: "nightly/backup-ok.enc".into(),
            object_checksum: Some("sha256:abc".into()),
            immutability: None,
            retention_until: None,
            status: "committed".into(),
            uploaded_at: Some("2026-07-29T03:05:00Z".into()),
        };

        let view = to_artifact_view(&artifact);

        assert_eq!(view.dest_name, "s3-primary");
        assert_eq!(view.immutability, "unknown");
        assert_eq!(view.retention_until, None);
        assert_eq!(view.status, "committed");
    }
}
