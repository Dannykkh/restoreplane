// tests/mysql_anchor.rs — T12/T13: AnchorScanningReader를 mock AsyncRead(&[u8])로 구동한다.
//! 실제 자식 프로세스 없이 바이트 청크 주입만으로 검증(빠른 반복). DB 불필요.

use std::sync::{Arc, Mutex};

use dbbackup_source_mysql::anchor::{AnchorScanningReader, BinlogAnchor, ANCHOR_SCAN_CAP};
use tokio::io::AsyncReadExt;

#[tokio::test]
async fn t12_captures_coordinates_and_passes_bytes_unchanged() {
    let body = "-- MySQL dump\n\
                -- CHANGE MASTER TO MASTER_LOG_FILE='mysql-bin.000042', MASTER_LOG_POS=1234;\n\
                SET @@GLOBAL.GTID_PURGED='3E11FA47-71CA-11E1-9E33-C80AA9429562:1-5';\n\
                CREATE TABLE t (id INT);\n";
    let input = body.as_bytes().to_vec();

    let anchor = Arc::new(Mutex::new(BinlogAnchor::default()));
    let mut reader = AnchorScanningReader::new(&input[..], anchor.clone());

    let mut out = Vec::new();
    reader.read_to_end(&mut out).await.unwrap();

    // 바이트 스트림 자체는 변형 없이 그대로 통과.
    assert_eq!(out, input);

    let a = anchor.lock().unwrap();
    assert!(a
        .binlog_coordinates
        .as_deref()
        .unwrap()
        .contains("mysql-bin.000042"));
    assert!(a
        .binlog_coordinates
        .as_deref()
        .unwrap()
        .contains("MASTER_LOG_POS=1234"));
    assert!(a
        .gtid_set
        .as_deref()
        .unwrap()
        .contains("3E11FA47-71CA-11E1-9E33-C80AA9429562"));
}

#[tokio::test]
async fn t13_marker_beyond_scan_cap_is_not_captured() {
    // 헤더에 좌표 없이 ANCHOR_SCAN_CAP 이후에 유사 텍스트가 등장 → 스캔 상한 초과로 캡처 안 됨.
    let mut input = vec![b'x'; ANCHOR_SCAN_CAP + 4096];
    let late = b"-- CHANGE MASTER TO MASTER_LOG_FILE='late.000001', MASTER_LOG_POS=9;";
    input[ANCHOR_SCAN_CAP + 100..ANCHOR_SCAN_CAP + 100 + late.len()].copy_from_slice(late);

    let anchor = Arc::new(Mutex::new(BinlogAnchor::default()));
    let mut reader = AnchorScanningReader::new(&input[..], anchor.clone());

    let mut out = Vec::new();
    reader.read_to_end(&mut out).await.unwrap();

    // 바이트는 온전히 통과하되, 상한 밖 좌표는 잡히지 않는다(의도된 상한 동작).
    assert_eq!(out.len(), input.len());
    let a = anchor.lock().unwrap();
    assert!(
        a.binlog_coordinates.is_none(),
        "상한 밖 좌표가 잡혔다 — 상한 로직 회귀"
    );
}
