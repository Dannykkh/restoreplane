// crates/restoreplane-source-mysql/src/anchor.rs
//! S04 §6 — binlog 좌표/GTID 앵커 캡처(R8). --source-data=2/--master-data=2는 좌표를 덤프 본문
//! 상단 주석(`-- CHANGE MASTER TO ...`, `SET @@GLOBAL.GTID_PURGED=...`)으로 남긴다. 암호화 스풀을
//! 사후에 열어 파싱하는 대신, 스트림이 파이프라인으로 흘러가는 동시에 헤더 구간만 훔쳐보는 tee를
//! 쓴다(S05 HashingWriter blake3 tee와 같은 패턴을 읽기 쪽에 적용). 바이트는 절대 변형하지 않는다.

use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};

use tokio::io::{AsyncRead, ReadBuf};

/// 헤더는 보통 수 KB 안에 있으므로 그 이후는 스캔을 끄고 순수 pass-through로 전환한다
/// (수 GB 스트림 전체를 문자열 스캔하지 않기 위한 상한).
pub const ANCHOR_SCAN_CAP: usize = 64 * 1024;

#[derive(Default, Clone, Debug, serde::Serialize)]
pub struct BinlogAnchor {
    /// "CHANGE MASTER TO MASTER_LOG_FILE=..., MASTER_LOG_POS=...;"
    pub binlog_coordinates: Option<String>,
    /// "SET @@GLOBAL.GTID_PURGED='...';"
    pub gtid_set: Option<String>,
}

/// mysqldump stdout을 감싸 바이트를 있는 그대로 통과시키면서, 첫 ANCHOR_SCAN_CAP 바이트에서
/// 좌표/GTID 주석 라인을 찾아 anchor에 채운다(순수 tee — 내용 변형 없음).
pub struct AnchorScanningReader<R> {
    inner: R,
    scan_buf: Vec<u8>,
    scan_done: bool,
    anchor: Arc<Mutex<BinlogAnchor>>,
}

impl<R> AnchorScanningReader<R> {
    pub fn new(inner: R, anchor: Arc<Mutex<BinlogAnchor>>) -> Self {
        Self {
            inner,
            scan_buf: Vec::with_capacity(ANCHOR_SCAN_CAP),
            scan_done: false,
            anchor,
        }
    }
}

impl<R: AsyncRead + Unpin> AsyncRead for AnchorScanningReader<R> {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        let me = self.get_mut();
        let before = buf.filled().len();
        let res = Pin::new(&mut me.inner).poll_read(cx, buf);

        if let Poll::Ready(Ok(())) = &res {
            if !me.scan_done {
                let chunk = &buf.filled()[before..];
                let remaining = ANCHOR_SCAN_CAP.saturating_sub(me.scan_buf.len());
                let take = remaining.min(chunk.len());
                me.scan_buf.extend_from_slice(&chunk[..take]);

                if let Ok(text) = std::str::from_utf8(&me.scan_buf) {
                    let mut anchor = me.anchor.lock().unwrap();
                    if anchor.binlog_coordinates.is_none() {
                        anchor.binlog_coordinates = extract_stmt(text, "CHANGE MASTER TO");
                    }
                    if anchor.gtid_set.is_none() {
                        anchor.gtid_set = extract_stmt(text, "GTID_PURGED=");
                    }
                }

                let both_found = {
                    let a = me.anchor.lock().unwrap();
                    a.binlog_coordinates.is_some() && a.gtid_set.is_some()
                };
                if me.scan_buf.len() >= ANCHOR_SCAN_CAP || both_found {
                    me.scan_done = true;
                    me.scan_buf = Vec::new(); // 스캔 종료 후 메모리 반환
                }
            }
        }
        res
    }
}

/// marker가 담긴 라인 시작부터 종결 `;`까지를 한 문장으로 추출한다. **종결 `;`가 아직 안 보이면
/// None** — 스트림이 청크로 도착할 때 문장이 절단된 채(파일명/POS 누락) 조기 확정되는 것을 막는다.
pub fn extract_stmt(text: &str, marker: &str) -> Option<String> {
    let start = text.find(marker)?;
    let stmt_start = text[..start]
        .rfind(['\n', '\r'])
        .map(|p| p + 1)
        .unwrap_or(0);
    let end = start + text[start..].find(';')? + 1; // 종결자 필수(부분 문장 배제)
    Some(text[stmt_start..end].trim().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_stmt_grabs_full_statement() {
        let text =
            "-- header\n-- CHANGE MASTER TO MASTER_LOG_FILE='bin.0001', MASTER_LOG_POS=154;\nrest";
        let s = extract_stmt(text, "CHANGE MASTER TO").unwrap();
        assert!(s.contains("MASTER_LOG_FILE='bin.0001'"));
        assert!(s.ends_with(';'));
    }

    #[test]
    fn extract_stmt_none_when_marker_absent() {
        assert!(extract_stmt("no coordinates here", "CHANGE MASTER TO").is_none());
    }
}
