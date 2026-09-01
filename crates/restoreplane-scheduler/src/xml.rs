// src/xml.rs — Task Scheduler XML 템플릿 생성(FR1). 수기 문자열 + 이스케이프로 생성한다.
use crate::cron::ScheduleSpec;
use time::OffsetDateTime;

pub struct TaskXmlParams {
    pub job_name: String,
    pub exe_path: String,
    /// 태스크 등록 시 사용한 config의 절대 경로. 예약 실행이 기본 경로로 되돌아가지 않게 고정한다.
    pub config_path: String,
    pub working_dir: String,
    /// 예: "CORP\\svc-restoreplane" 또는 ".\\svc-restoreplane"(로컬 계정).
    pub service_account: String,
    pub logon_type: LogonType,
    pub schedule: ScheduleSpec,
    /// 최초 등록 시각(그 이후 반복은 Task Scheduler가 ScheduleByDay/Week로 자체 계산).
    pub next_start_boundary: OffsetDateTime,
    pub retry_attempts: u32,
    pub retry_backoff_min: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogonType {
    /// 비밀번호 미저장. "로그온 여부와 무관하게 실행"을 비밀번호 없이 달성(FR2 기본값).
    /// 계정에 "배치 작업으로 로그온"(SeBatchLogonRight) 권한이 필요.
    S4U,
    /// 비밀번호를 schtasks에 /RP로 전달해 저장. S4U 불가 환경에서만 옵트인(Risk 참고).
    Password,
}

impl LogonType {
    pub fn xml_value(&self) -> &'static str {
        match self {
            LogonType::S4U => "S4U",
            LogonType::Password => "Password",
        }
    }
}

/// XML 예약문자 이스케이프. job_name/경로가 &, <, >, ", ' 를 포함할 가능성에 대비.
pub fn escape_xml(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

fn schedule_by_block(spec: &ScheduleSpec) -> String {
    match spec {
        ScheduleSpec::Hourly { .. } => concat!(
            "<ScheduleByDay><DaysInterval>1</DaysInterval></ScheduleByDay>",
            "<Repetition><Interval>PT1H</Interval><Duration>P1D</Duration>",
            "<StopAtDurationEnd>false</StopAtDurationEnd></Repetition>"
        )
        .to_owned(),
        ScheduleSpec::Daily { .. } => {
            "<ScheduleByDay><DaysInterval>1</DaysInterval></ScheduleByDay>".to_string()
        }
        ScheduleSpec::Weekly { day, .. } => format!(
            "<ScheduleByWeek><WeeksInterval>1</WeeksInterval><DaysOfWeek><{tag} /></DaysOfWeek></ScheduleByWeek>",
            tag = day.xml_tag()
        ),
    }
}

fn start_boundary_for(spec: &ScheduleSpec, from: OffsetDateTime) -> String {
    let (hour, minute) = match spec {
        ScheduleSpec::Hourly { minute } => (from.hour(), *minute),
        ScheduleSpec::Daily { hour, minute } => (*hour, *minute),
        ScheduleSpec::Weekly { hour, minute, .. } => (*hour, *minute),
    };
    // 중요(gotcha): Task Scheduler의 StartBoundary는 타임존 지정자가 없으면 "로컬 시각"으로 해석한다.
    // RFC3339(…Z/오프셋)로 쓰면 schtasks가 로컬로 변환해 실제 실행 시각이 어긋난다(예: 03:00Z -> 12:00 KST).
    // cron의 시:분은 로컬 시각 의도이므로, 오프셋 없는 naive 로컬 형식으로 그대로 기록한다.
    let time = time::Time::from_hms(hour, minute, 0)
        .expect("hour/minute은 parse_cron_subset이 범위 검증함");
    let dt = time::PrimitiveDateTime::new(from.date(), time);
    let fmt = time::format_description::parse_borrowed::<2>(
        "[year]-[month]-[day]T[hour]:[minute]:[second]",
    )
    .expect("정적 포맷 서술자 파싱 실패 없음");
    dt.format(&fmt).expect("naive datetime 포맷 실패 없음")
}

pub fn render_task_xml(p: &TaskXmlParams) -> String {
    let start_boundary = start_boundary_for(&p.schedule, p.next_start_boundary);
    format!(
        r#"<?xml version="1.0" encoding="UTF-16"?>
<Task version="1.4" xmlns="http://schemas.microsoft.com/windows/2004/02/mit/task">
  <RegistrationInfo>
    <Description>restoreplane job '{job}' -- restoreplane schedule command managed. Do not hand-edit.</Description>
    <URI>\RestorePlane\{job}</URI>
  </RegistrationInfo>
  <Triggers>
    <CalendarTrigger>
      <StartBoundary>{start}</StartBoundary>
      <Enabled>true</Enabled>
      {schedule_block}
    </CalendarTrigger>
  </Triggers>
  <Principals>
    <Principal id="Author">
      <UserId>{account}</UserId>
      <LogonType>{logon}</LogonType>
      <RunLevel>HighestAvailable</RunLevel>
    </Principal>
  </Principals>
  <Settings>
    <MultipleInstancesPolicy>IgnoreNew</MultipleInstancesPolicy>
    <DisallowStartIfOnBatteries>false</DisallowStartIfOnBatteries>
    <StopIfGoingOnBatteries>false</StopIfGoingOnBatteries>
    <AllowHardTerminate>true</AllowHardTerminate>
    <StartWhenAvailable>true</StartWhenAvailable>
    <RunOnlyIfNetworkAvailable>false</RunOnlyIfNetworkAvailable>
    <Enabled>true</Enabled>
    <Hidden>false</Hidden>
    <ExecutionTimeLimit>PT6H</ExecutionTimeLimit>
    <Priority>7</Priority>
    <RestartOnFailure>
      <Interval>PT{backoff}M</Interval>
      <Count>{attempts}</Count>
    </RestartOnFailure>
  </Settings>
  <Actions Context="Author">
    <Exec>
      <Command>{exe}</Command>
      <Arguments>--config &quot;{config}&quot; run --job &quot;{job}&quot; --scheduled</Arguments>
      <WorkingDirectory>{wd}</WorkingDirectory>
    </Exec>
  </Actions>
</Task>"#,
        job = escape_xml(&p.job_name),
        config = escape_xml(&p.config_path),
        start = start_boundary,
        schedule_block = schedule_by_block(&p.schedule),
        backoff = p.retry_backoff_min,
        attempts = p.retry_attempts,
        account = escape_xml(&p.service_account),
        logon = p.logon_type.xml_value(),
        exe = escape_xml(&p.exe_path),
        wd = escape_xml(&p.working_dir),
    )
}
