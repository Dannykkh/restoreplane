// T2~T3 — XML 생성/이스케이프 라운드트립 + 필수 플래그 존재.
use dbbackup_scheduler::cron::{ScheduleSpec, Weekday};
use dbbackup_scheduler::xml::{render_task_xml, LogonType, TaskXmlParams};
use time::OffsetDateTime;

fn params(job: &str, schedule: ScheduleSpec) -> TaskXmlParams {
    TaskXmlParams {
        job_name: job.to_string(),
        exe_path: r"C:\Program Files\dbbackup\dbbackup.exe".to_string(),
        working_dir: r"C:\ProgramData\dbbackup".to_string(),
        service_account: r".\svc-dbbackup".to_string(),
        logon_type: LogonType::S4U,
        schedule,
        next_start_boundary: OffsetDateTime::now_utc(),
        retry_attempts: 3,
        retry_backoff_min: 10,
    }
}

/// XML well-formedness를 경량 태그-균형 검사로 확인(외부 XML 파서 의존 제거).
/// 열림/닫힘/self-closing 태그의 스택 균형과, 태그 밖 raw '<'/'>' 부재를 검증한다.
/// 특수문자 이스케이프 회귀(special_chars 테스트)를 잡는 것이 목적.
fn assert_wellformed(xml: &str) {
    let bytes = xml.as_bytes();
    let mut stack: Vec<String> = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'<' => {
                let end = xml[i..].find('>').map(|o| i + o).unwrap_or_else(|| {
                    panic!("닫히지 않은 태그(< 뒤에 > 없음) at {i}")
                });
                let inner = &xml[i + 1..end];
                if let Some(rest) = inner.strip_prefix('?') {
                    // 선언 <?xml ... ?> — 무시
                    let _ = rest;
                } else if let Some(name) = inner.strip_prefix('/') {
                    // 닫힘 태그
                    let expected = stack.pop().unwrap_or_else(|| panic!("여는 태그 없이 닫힘: {name}"));
                    assert_eq!(expected, name.trim(), "태그 불균형: {expected} vs {name}");
                } else if inner.ends_with('/') {
                    // self-closing <Foo /> — 스택 변화 없음
                } else {
                    // 여는 태그: 이름만 추출(속성 무시)
                    let name = inner.split_whitespace().next().unwrap_or("").to_string();
                    assert!(!name.is_empty(), "빈 태그명 at {i}");
                    stack.push(name);
                }
                i = end + 1;
            }
            b'>' => panic!("태그 밖의 raw '>' at {i} (이스케이프 누락)"),
            _ => i += 1,
        }
    }
    assert!(stack.is_empty(), "닫히지 않은 태그 남음: {stack:?}");
}

#[test]
fn daily_xml_contains_required_flags() {
    let xml = render_task_xml(&params("nightly", ScheduleSpec::Daily { hour: 3, minute: 0 }));
    assert_wellformed(&xml);
    assert!(xml.contains("<StartWhenAvailable>true</StartWhenAvailable>"));
    assert!(xml.contains("<MultipleInstancesPolicy>IgnoreNew</MultipleInstancesPolicy>"));
    assert!(xml.contains("<RunLevel>HighestAvailable</RunLevel>"));
    assert!(xml.contains("<RestartOnFailure>"));
    // 파라미터 값 반영.
    assert!(xml.contains("<Interval>PT10M</Interval>"));
    assert!(xml.contains("<Count>3</Count>"));
    assert!(xml.contains("<LogonType>S4U</LogonType>"));
    assert!(xml.contains("<ScheduleByDay><DaysInterval>1</DaysInterval></ScheduleByDay>"));
    // S4U 기본 경로는 XML에 비밀번호가 없다.
    assert!(!xml.to_lowercase().contains("password>"));
}

#[test]
fn weekly_xml_uses_dayofweek() {
    let xml = render_task_xml(&params(
        "weekly",
        ScheduleSpec::Weekly { hour: 4, minute: 15, day: Weekday::Wed },
    ));
    assert_wellformed(&xml);
    assert!(xml.contains("<ScheduleByWeek>"));
    assert!(xml.contains("<Wednesday />"));
}

#[test]
fn special_chars_are_escaped_and_wellformed() {
    // job 이름에 XML 예약문자가 들어가도 유효 XML이 생성돼야 한다.
    let xml = render_task_xml(&params(
        "a&b<c>\"d\"'e'",
        ScheduleSpec::Daily { hour: 1, minute: 1 },
    ));
    assert_wellformed(&xml);
    assert!(xml.contains("a&amp;b&lt;c&gt;"));
    // 원문 raw & 는 남지 않는다(&amp;로만 존재).
    assert!(!xml.contains("a&b"));
}

#[test]
fn password_logon_renders_password_type() {
    let mut p = params("pw", ScheduleSpec::Daily { hour: 2, minute: 0 });
    p.logon_type = LogonType::Password;
    let xml = render_task_xml(&p);
    assert!(xml.contains("<LogonType>Password</LogonType>"));
}
