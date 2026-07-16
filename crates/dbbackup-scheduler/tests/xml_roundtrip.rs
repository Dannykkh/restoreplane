// T2~T3 — XML 생성/이스케이프 라운드트립 + 필수 플래그 존재.
use dbbackup_scheduler::cron::{ScheduleSpec, Weekday};
use dbbackup_scheduler::xml::{render_task_xml, LogonType, TaskXmlParams};
use quick_xml::events::Event;
use quick_xml::Reader;
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

/// XML이 well-formed임을 quick-xml 리더로 확인(파싱 성공 = 유효 XML).
fn assert_wellformed(xml: &str) {
    let mut reader = Reader::from_str(xml);
    reader.config_mut().trim_text(true);
    loop {
        match reader.read_event() {
            Ok(Event::Eof) => break,
            Ok(_) => {}
            Err(e) => panic!("XML 파싱 실패: {e}"),
        }
    }
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
