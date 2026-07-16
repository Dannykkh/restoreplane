// S09 9.7 — 검증 주기 정책. L1/L2는 매 백업 직후 자동(비용 거의 없음), L3는 주 1회 + 스키마
// 변경 감지 시 즉시. 순수 로직이라 인프라 없이 단위 테스트 가능.
//
// 섹션 원본은 time::OffsetDateTime을 쓰지만 순수성/무의존을 위해 unix epoch 초(i64)로 모델링한다.

use crate::verify::VerifyLevel;

/// L3 주기 임계(초). 7일.
pub const WEEKLY_SECONDS: i64 = 7 * 24 * 3600;

pub struct CadenceInput<'a> {
    pub now_unix: i64,
    /// 이번 매니페스트 schema_hash.
    pub current_schema_hash: Option<&'a str>,
    /// 직전 성공 백업의 schema_hash.
    pub prev_schema_hash: Option<&'a str>,
    /// 이 job의 마지막 L3 성공 시각(unix). None이면 여태 L3 이력 없음.
    pub last_l3_unix: Option<i64>,
}

/// 이번 백업에 대해 지금 수행할 검증 레벨 집합.
pub fn levels_due(input: &CadenceInput) -> Vec<VerifyLevel> {
    use VerifyLevel::*;
    // L1/L2는 항상.
    let mut due = vec![L1, L2];

    // 스키마 변경 감지 → 즉시 L3.
    let schema_changed = match (input.current_schema_hash, input.prev_schema_hash) {
        (Some(c), Some(p)) => c != p,
        (Some(_), None) => true, // 첫 백업/이전 기록 없음 → L3로 한 번 확증
        _ => false,
    };
    // 주간 경과 → L3.
    let weekly_due = match input.last_l3_unix {
        Some(t) => input.now_unix - t >= WEEKLY_SECONDS,
        None => true, // 여태 L3 이력 없음
    };
    if schema_changed || weekly_due {
        due.push(L3);
    }
    due
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::verify::VerifyLevel::*;

    fn has(v: &[VerifyLevel], l: VerifyLevel) -> bool {
        v.contains(&l)
    }

    #[test]
    fn l1_l2_always_present() {
        let due = levels_due(&CadenceInput {
            now_unix: 1000,
            current_schema_hash: Some("a"),
            prev_schema_hash: Some("a"),
            last_l3_unix: Some(1000), // 방금 L3 → 주간 미도래
        });
        assert!(has(&due, L1) && has(&due, L2));
        assert!(!has(&due, L3), "스키마 동일 + 주간 미도래면 L3 없음");
    }

    #[test]
    fn schema_change_triggers_l3() {
        let due = levels_due(&CadenceInput {
            now_unix: 1000,
            current_schema_hash: Some("b"),
            prev_schema_hash: Some("a"),
            last_l3_unix: Some(1000),
        });
        assert!(has(&due, L3), "스키마 변경 시 즉시 L3");
    }

    #[test]
    fn weekly_elapsed_triggers_l3() {
        let due = levels_due(&CadenceInput {
            now_unix: 1000 + WEEKLY_SECONDS,
            current_schema_hash: Some("a"),
            prev_schema_hash: Some("a"),
            last_l3_unix: Some(1000),
        });
        assert!(has(&due, L3), "주간 경과 시 L3");
    }

    #[test]
    fn no_l3_history_triggers_l3() {
        let due = levels_due(&CadenceInput {
            now_unix: 1000,
            current_schema_hash: Some("a"),
            prev_schema_hash: Some("a"),
            last_l3_unix: None,
        });
        assert!(has(&due, L3), "L3 이력 없으면 L3");
    }

    #[test]
    fn stable_schema_recent_l3_no_l3() {
        let due = levels_due(&CadenceInput {
            now_unix: 1000 + WEEKLY_SECONDS - 1,
            current_schema_hash: Some("a"),
            prev_schema_hash: Some("a"),
            last_l3_unix: Some(1000),
        });
        assert_eq!(due, vec![L1, L2], "안정 스키마 + 주간 직전이면 L1/L2만");
    }
}
