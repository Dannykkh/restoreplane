// S08 8.1 — 싱글 라이터 액터. 전용 OS 스레드가 유일한 writer 커넥션을 독점 소유하고,
// 나머지 코드는 채널로 클로저를 보내 순차 실행시킨다(동시 쓰기 원천 차단, FR2).
//
// 섹션 원본은 tokio mpsc/oneshot을 쓰지만, 카탈로그 경로는 동기로 충분하므로 std 채널만 사용해
// tokio 런타임 의존을 없앤다(테스트 단순화, 슬라이스 1 편차).

use std::path::PathBuf;
use std::sync::mpsc::{self, Sender};
use std::thread;

use rusqlite::Connection;

use crate::conn::open_writer_conn;

type WriteJob = Box<dyn FnOnce(&mut Connection) + Send>;

/// writer 스레드로의 핸들. Clone 가능(여러 호출자가 같은 단일 writer에 순차 전송).
#[derive(Clone)]
pub struct CatalogWriterHandle {
    tx: Sender<WriteJob>,
}

impl CatalogWriterHandle {
    /// 전용 스레드를 띄우고 db_path의 유일한 writer 커넥션을 그 스레드가 소유하게 한다.
    /// 커넥션 오픈 실패는 여기서(spawn 시점) 즉시 Err로 전파한다.
    pub fn spawn(db_path: PathBuf) -> anyhow::Result<Self> {
        let (tx, rx) = mpsc::channel::<WriteJob>();
        let (ready_tx, ready_rx) = mpsc::channel::<Result<(), String>>();

        thread::Builder::new()
            .name("catalog-writer".into())
            .spawn(move || {
                let mut conn = match open_writer_conn(&db_path) {
                    Ok(c) => {
                        let _ = ready_tx.send(Ok(()));
                        c
                    }
                    Err(e) => {
                        let _ = ready_tx.send(Err(e.to_string()));
                        return;
                    }
                };
                // 채널이 닫힐 때까지(모든 핸들 drop) 작업을 순차 실행.
                while let Ok(job) = rx.recv() {
                    job(&mut conn);
                }
            })?;

        match ready_rx.recv() {
            Ok(Ok(())) => Ok(Self { tx }),
            Ok(Err(e)) => Err(anyhow::anyhow!("catalog writer 커넥션 오픈 실패: {e}")),
            Err(_) => Err(anyhow::anyhow!("catalog writer 스레드가 시작 직후 종료됨")),
        }
    }

    /// 단일 writer 커넥션 위에서 클로저를 실행하고 결과를 돌려받는다(호출자는 블로킹 대기).
    pub fn run<F, T>(&self, job: F) -> anyhow::Result<T>
    where
        F: FnOnce(&mut Connection) -> anyhow::Result<T> + Send + 'static,
        T: Send + 'static,
    {
        let (reply_tx, reply_rx) = mpsc::channel::<anyhow::Result<T>>();
        self.tx
            .send(Box::new(move |conn| {
                let _ = reply_tx.send(job(conn));
            }))
            .map_err(|_| anyhow::anyhow!("catalog writer 채널이 닫힘"))?;
        reply_rx
            .recv()
            .map_err(|_| anyhow::anyhow!("catalog writer가 응답 없이 종료됨"))?
    }
}
