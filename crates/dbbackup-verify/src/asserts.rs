// S09 9.6 — 검증 단언 헬퍼(순수 로직). 스키마 해시 정규화/row count 대조/버전 갭 판정.
// 라이브 프로덕션 DB는 절대 건드리지 않는다 — 비교 기준은 매니페스트에 캡처된 백업 시점 값이다.

use std::collections::BTreeMap;

use crate::sandbox::parse_major;

/// --schema-only 덤프를 정규화(주석/빈줄/후행공백 제거) 후 blake3. 공백/순서 잡음으로 인한 오탐 제거.
pub fn normalize_and_hash_schema(raw: &str) -> String {
    let normalized: String = raw
        .lines()
        .map(|l| l.trim_end())
        .filter(|l| !l.is_empty() && !l.starts_with("--"))
        .collect::<Vec<_>>()
        .join("\n");
    blake3::hash(normalized.as_bytes()).to_hex().to_string()
}

/// 매니페스트 백업시점 카운트 vs 복원본 카운트 비교. 반환: (일치여부, 노트, 복원본 총행수).
pub fn compare_row_counts(
    manifest_captured: &BTreeMap<String, u64>,
    restored: &BTreeMap<String, u64>,
) -> (bool, String, u64) {
    let total_restored: u64 = restored.values().sum();
    if manifest_captured.is_empty() {
        return (
            true,
            format!("[백업시점 source_row_counts 없음 → row 비교 skip, 복원본 총 {total_restored}행 기록]"),
            total_restored,
        );
    }
    let mut mismatches = Vec::new();
    for (table, &expected) in manifest_captured {
        let got = restored.get(table).copied().unwrap_or(0);
        if got != expected {
            mismatches.push(format!("{table}: expected={expected} got={got}"));
        }
    }
    if mismatches.is_empty() {
        (true, format!("row count 전부 일치({}테이블)", manifest_captured.len()), total_restored)
    } else {
        (false, format!("row count 불일치: {}", mismatches.join(", ")), total_restored)
    }
}

/// 원본과 복원 대상 메이저가 다르면 버전 갭(투명화 대상).
pub fn version_gap(source_version: &str, target_version: &str) -> bool {
    match (parse_major(source_version), parse_major(target_version)) {
        (Ok(a), Ok(b)) => a != b,
        _ => true, // 파싱 불가는 보수적으로 갭으로 표기
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_manifest_counts_skips_compare() {
        let restored = BTreeMap::from([("t".to_string(), 5u64)]);
        let (pass, note, total) = compare_row_counts(&BTreeMap::new(), &restored);
        assert!(pass);
        assert_eq!(total, 5);
        assert!(note.contains("skip"));
    }

    #[test]
    fn matching_counts_pass() {
        let m = BTreeMap::from([("t".to_string(), 3u64)]);
        let r = BTreeMap::from([("t".to_string(), 3u64)]);
        assert!(compare_row_counts(&m, &r).0);
    }

    #[test]
    fn mismatch_fails() {
        let m = BTreeMap::from([("t".to_string(), 3u64)]);
        let r = BTreeMap::from([("t".to_string(), 2u64)]);
        assert!(!compare_row_counts(&m, &r).0);
    }

    #[test]
    fn version_gap_detects_major_diff() {
        assert!(version_gap("PostgreSQL 16.3", "PostgreSQL 15.6"));
        assert!(!version_gap("PostgreSQL 16.3", "16.1"));
    }
}
