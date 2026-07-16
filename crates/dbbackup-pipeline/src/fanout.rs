// crates/dbbackup-pipeline/src/fanout.rs
//! S05 §8 — 팬아웃(옵션 C). 커밋된 스풀을 목적지별로 독립 업로드(JoinSet), 부분 성공 벡터 반환(R6).
//! 한 목적지 실패가 다른 목적지를 막지 않는다.

use std::sync::Arc;

use crate::commit::{PipelineCfg, SpoolCommit};
use crate::dest::{upload_resumable, Destination, UploadError, UploadReceipt};

pub struct DestOutcome {
    pub dest_name: String,
    pub result: Result<UploadReceipt, UploadError>,
    pub attempts: u32,
}

/// 커밋된 스풀을 목적지별로 독립 업로드. key는 스풀 파일명을 그대로 쓴다.
pub async fn fan_out(
    commit: &SpoolCommit,
    dests: Vec<Arc<dyn Destination>>,
    cfg: &PipelineCfg,
) -> Vec<DestOutcome> {
    let key = commit
        .spool_path
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "backup.spool".to_string());

    let mut set: tokio::task::JoinSet<DestOutcome> = tokio::task::JoinSet::new();
    for dest in dests {
        let spool = commit.spool_path.clone();
        let key = key.clone();
        let retries = cfg.upload_retries;
        set.spawn(async move {
            let name = dest.name().to_string();
            let mut attempts = 0u32;
            let mut last: Result<UploadReceipt, UploadError> = Err(UploadError::NotAttempted);
            while attempts <= retries {
                attempts += 1;
                match upload_resumable(dest.as_ref(), &spool, &key).await {
                    Ok(r) => {
                        last = Ok(r);
                        break;
                    }
                    Err(e) if e.is_retryable() && attempts <= retries => {
                        let backoff =
                            std::time::Duration::from_millis(50 * (1u64 << (attempts - 1)));
                        tracing::warn!(%name, attempts, ?e, "업로드 재시도");
                        tokio::time::sleep(backoff).await;
                        last = Err(e);
                    }
                    Err(e) => {
                        last = Err(e);
                        break;
                    }
                }
            }
            DestOutcome {
                dest_name: name,
                result: last,
                attempts,
            }
        });
    }

    let mut outcomes = Vec::new();
    while let Some(joined) = set.join_next().await {
        match joined {
            Ok(o) => outcomes.push(o),
            Err(e) => outcomes.push(DestOutcome {
                dest_name: "<panicked>".into(),
                result: Err(UploadError::TaskPanic(e.to_string())),
                attempts: 0,
            }),
        }
    }
    outcomes
}
