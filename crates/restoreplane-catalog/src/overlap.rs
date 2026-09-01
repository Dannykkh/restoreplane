// S08 8.6 — overlap guard(이중 방어 + stale 회수).
//   1차: OS 네임드 뮤텍스로 acquire 임계구간을 직렬화(같은 호스트 동시 기동 빠른 실패).
//   2차: runs(job) WHERE status='running' 부분 UNIQUE 인덱스로 run 전체 구간을 DB 레벨에서 방어.
//        (크래시로 running row가 남으면 MAX_RUN_SECONDS 초과 시 stale로 회수)
//
// 섹션 원본은 뮤텍스를 run 전체 구간 동안 보유하지만, NamedLockGuard가 NamedLock을 빌리는
// self-referential 문제가 있어 여기서는 뮤텍스를 acquire 임계구간에서만 보유한다. run 전체 구간의
// 단일성은 DB 부분 UNIQUE 인덱스(권위 소스)가 보장하므로 방어력은 동일하다(슬라이스 1 편차, 문서화).

use named_lock::NamedLock;
use rusqlite::params;
use uuid::Uuid;

use crate::writer::CatalogWriterHandle;

/// 정체된 running row 인정 기준(초). run은 대개 수십 분 내 끝나므로 넉넉히 잡는다.
pub const MAX_RUN_SECONDS: i64 = 6 * 3600;

#[derive(Debug, thiserror::Error)]
pub enum OverlapError {
    #[error("job '{0}'이 이미 실행 중(overlap guard)")]
    AlreadyRunning(String),
    #[error("OS 네임드 뮤텍스 획득 실패: {0}")]
    MutexUnavailable(String),
    #[error(transparent)]
    Db(#[from] anyhow::Error),
}

/// 실행 중인 run에 대한 핸들. 정상 종료 시 finish()로 상태를 명시적으로 마감한다.
#[derive(Debug)]
pub struct RunGuard {
    run_id: String,
    job: String,
    finished: bool,
}

impl RunGuard {
    pub fn run_id(&self) -> &str {
        &self.run_id
    }

    /// 정상/실패 종료를 명시 기록한다. Drop에서 async가 불가하므로 반드시 명시 호출한다.
    pub fn finish(mut self, writer: &CatalogWriterHandle, status: &str) -> anyhow::Result<()> {
        let run_id = self.run_id.clone();
        let status = status.to_string();
        writer.run(move |conn| {
            conn.execute(
                "UPDATE runs SET status=?1 WHERE id=?2",
                params![status, run_id],
            )?;
            Ok(())
        })?;
        self.finished = true;
        Ok(())
    }
}

impl Drop for RunGuard {
    fn drop(&mut self) {
        if !self.finished {
            // finish() 없이 드롭(패닉/크래시). DB는 'running'으로 남고 다음 시도 때 stale 회수된다.
            tracing::warn!(
                job = %self.job,
                run_id = %self.run_id,
                "RunGuard가 finish() 없이 드롭됨 — 다음 실행 시 stale로 회수됨"
            );
        }
    }
}

fn short_hash(s: &str) -> String {
    blake3::hash(s.as_bytes()).to_hex()[..16].to_string()
}

/// job에 대한 실행권을 획득한다. 이미 실행 중이면 OverlapError::AlreadyRunning.
pub fn try_acquire_run(writer: &CatalogWriterHandle, job: &str) -> Result<RunGuard, OverlapError> {
    // 1차: 뮤텍스로 acquire 임계구간 직렬화(같은 호스트 동시 기동 빠른 실패).
    let lock_name = format!("restoreplane-run-{}", short_hash(job));
    let named =
        NamedLock::create(&lock_name).map_err(|e| OverlapError::MutexUnavailable(e.to_string()))?;
    let _mutex = match named.try_lock() {
        Ok(g) => g,
        Err(_) => return Err(OverlapError::AlreadyRunning(job.to_string())),
    };

    // 2차: 뮤텍스 보유 중 DB 삽입(stale 회수 후 부분 UNIQUE 인덱스가 최종 판정).
    let run_id = Uuid::new_v4().to_string();
    let run_id_db = run_id.clone();
    let job_db = job.to_string();

    let inserted = writer.run(move |conn| {
        let tx = conn.transaction()?;
        // 정체된 running row 회수.
        tx.execute(
            "UPDATE runs SET status='coalesced'
             WHERE job=?1 AND status='running'
               AND (unixepoch('now') - unixepoch(started_at)) > ?2",
            params![job_db, MAX_RUN_SECONDS],
        )?;
        // 실제 삽입. 다른 running row가 남아 있으면 부분 UNIQUE 인덱스가 막는다.
        let res = tx.execute(
            "INSERT INTO runs (id, job, started_at, status)
             VALUES (?1, ?2, strftime('%Y-%m-%d %H:%M:%S','now'), 'running')",
            params![run_id_db, job_db],
        );
        match res {
            Ok(_) => {
                tx.commit()?;
                Ok(true)
            }
            Err(rusqlite::Error::SqliteFailure(e, _))
                if e.code == rusqlite::ErrorCode::ConstraintViolation =>
            {
                let _ = tx.rollback();
                Ok(false)
            }
            Err(e) => Err(e.into()),
        }
    })?;
    // _mutex는 여기서 스코프를 벗어나며 해제된다.

    if inserted {
        Ok(RunGuard {
            run_id,
            job: job.to_string(),
            finished: false,
        })
    } else {
        Err(OverlapError::AlreadyRunning(job.to_string()))
    }
}
