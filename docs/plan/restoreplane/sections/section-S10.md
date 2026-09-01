# S10 — Scheduler(schtasks) + Overlap Guard + 알림

> [!IMPORTANT]
> SUPERSEDED: 이 섹션의 Plan별 Windows Task Scheduler 설계는 더 이상 production 계약이 아니다.
> 현재 계약은 singleton 서비스의 durable scheduler이며 [ADR-001](../../../adr/ADR-001-execution-authority.md)과
> [Program-owned backup engine](../../program-owned-backup-engine.md)을 따른다.

> 의존: S05(Pipeline — `run_job` 진입점), S08(Catalog — `try_acquire_run`/`RunGuard`, `runs` 부분
> UNIQUE 인덱스). 레이어 4. 관련 리뷰: rust-stack.md §9(schtasks vs windows-service, StartWhenAvailable),
> architecture.md P1-5(overlap guard), red-team.md C1(DPAPI/최소권한 계정), M6(로그 시크릿 리댁션).

## Background

이 시스템은 "무인(unattended) 스케줄 실행"이 랜섬웨어 방어 스토리의 필수 전제다(`plan.md` 구현
슬라이스 4). S10은 그 무인 실행을 **Windows Task Scheduler**에 등록하고, 등록된 작업이 안전하게
반복 실행되도록 세 가지를 보장한다: (1) 재부팅 등으로 놓친 실행이 자동으로 따라잡히고(catch-up),
(2) 따라잡기 실행이 아직 실행 중인 인스턴스와 절대 겹치지 않으며(overlap guard), (3) 실행 결과가
시크릿 유출 없이 로그/이벤트/선택적 webhook·email로 가시화된다(알림).

### 왜 schtasks(Task Scheduler)이고 상주 서비스(`windows-service`)가 아닌가

`rust-stack.md` §9의 결론을 그대로 채택한다. 요구사항은 "특정 시각에 실행하고, 그 시각에 기계가
꺼져 있었다면 켜지는 즉시 따라잡는다"(catch-up)이다. Windows Task Scheduler는 이 요구사항을 위해
설계된 OS 네이티브 컴포넌트이며 `StartWhenAvailable` 플래그 하나로 catch-up을 얻는다.

반대로 `windows-service` 크레이트로 상주 서비스를 만들면:
- 서비스는 "항상 켜져 있는 백그라운드 프로세스" 모델이지, "정해진 시각에 실행" 모델이 아니다.
  "매일 03:00에 실행하고, 03:00에 기계가 꺼져 있었으면 부팅 즉시 실행"을 얻으려면 서비스 내부에
  **직접 타이머 + 마지막 실행시각 영속화 + 부팅시 지연시간 계산 로직**을 새로 구현해야 한다 —
  이는 Task Scheduler가 이미 검증된 방식으로 제공하는 기능을 재발명(reinvent)하는 것이다.
- 서비스 크래시/재시작 정책, 계정 권한 분리, 이벤트 로그 연동도 Task Scheduler가 `<RestartOnFailure>`,
  `<Principal>`, 자체 이력(History) 탭으로 이미 제공한다.
- `windows-service`는 "지속적 백그라운드 작업"(예: WAL 연속 아카이빙, 상시 API 서버)에는 여전히
  적합한 도구이지만, 이 프로젝트의 백업 실행은 본질적으로 "주기적으로 시작해서 끝나는 배치 작업"이므로
  Task Scheduler가 정확히 맞는 도구다. **이 섹션은 `windows-service`를 사용하지 않는다.**
  향후 상주 에이전트(예: 실시간 상태 API)가 필요해지면 별도 섹션에서 검토한다.

### 세 가지 방어선의 관계

| 방어선 | 담당 | 막는 것 |
|---|---|---|
| Task Scheduler `MultipleInstancesPolicy=IgnoreNew` | 이 섹션(XML) | OS 스케줄러 자신이 같은 태스크를 중복 기동하는 것 |
| OS 네임드 뮤텍스(`named-lock`) | S08(재사용) | 같은 호스트의 다른 프로세스(수동 `restoreplane run` 등)가 겹쳐 도는 것 |
| `runs` 부분 UNIQUE 인덱스(SQLite) | S08(재사용) | 위 두 방어선이 모두 우회된 최후의 DB 레벨 방어 |

S10은 새 overlap guard를 만들지 않는다. **S08이 이미 구현한 `try_acquire_run`/`RunGuard`를 그대로
소비**하고, Task Scheduler XML 설정과 스케줄러 진입점(exit code, 알림) 배선만 추가한다
(architecture.md P1-5 요구를 정확히 재사용으로 충족).

### 무인 실행의 시크릿 경로 (요약, 구현은 S02 소관)

Task Scheduler가 프로세스를 시작시키는 것과 그 프로세스가 DEK를 여는 것은 별개 계층이다.

1. **계정 로그온**: Task Scheduler가 `restoreplane.exe run --job X`를 어떤 Windows 계정으로 실행할지.
   → 이 섹션이 XML `<Principal>`로 결정한다. **최소권한 전용 서비스 계정**(SYSTEM 아님, C1 대응)을
   쓰고, 가능하면 비밀번호를 저장하지 않는 **S4U 로그온**을 기본값으로 한다(아래 참고).
2. **DEK 언랩**: 그 프로세스가 실제 백업 암호화 키를 여는 것. → **S02**가 DPAPI Machine-scope +
   비커밋 keyfile-entropy로 이미 구현했다(C1 3중 방어: 최소권한 계정 + keyfile ACL + 비커밋).
   S10은 이 경로를 호출만 하며 키 자료를 전혀 다루지 않는다.

이 둘을 분리해서 이해하는 것이 중요하다: 계정 로그온 방식이 안전해도 DPAPI 경로가 약하면 C1이
재발하고, 반대로 DPAPI가 완벽해도 계정 로그온에 평문 비밀번호가 노출되면 새로운 구멍이 생긴다.

## Requirements

### 기능 요구사항 (FR)
- **FR1** `schtasks /Create /XML`으로 태스크를 등록한다. XML은 다음을 반드시 포함한다:
  `StartWhenAvailable=true`(catch-up), `RunLevel=HighestAvailable`, `MultipleInstancesPolicy=IgnoreNew`,
  트리거별 `RestartOnFailure`(간격/횟수는 `[policy.retry]`에서 가져옴).
- **FR2** 로그온 방식은 기본 **S4U**(비밀번호 미저장, "로그온 여부와 무관하게 실행"을 비밀번호 없이
  달성)로 하고, 환경상 S4U가 불가능한 경우(계정에 "배치 작업으로 로그온" 권한 부여 불가 등)에만
  옵트인으로 Password 로그온 경로를 제공한다(잔여 리스크는 Risk & Rollback에 문서화).
- **FR3** `Job.schedule`(S01 config, cron 유사 문자열)을 파싱해 Task Scheduler `CalendarTrigger`로
  변환한다. v1은 `"M H * * *"`(매일) / `"M H * * D"`(매주 특정 요일) 두 패턴만 지원하고, 그 외
  표현식은 명확한 에러로 거부한다(조용한 오해석 금지).
- **FR4** `restoreplane schedule --job X`는 **멱등**해야 한다 — 이미 동일 파라미터로 등록되어 있으면
  재등록(`/Create /F`)을 실행하되 "변경 없음"으로 로그하고, 파라미터가 바뀐 경우만 실제 갱신으로
  기록한다.
- **FR5** `restoreplane schedule --job X --remove`는 `schtasks /Delete /TN ... /F`로 태스크를 제거한다.
- **FR6** 스케줄된(그리고 수동) `run` 진입점은 실행 전 S08 `try_acquire_run`을 반드시 통과해야 한다.
  획득 실패(`AlreadyRunning`)는 **실패가 아니라 스킵**으로 취급하고, 프로세스는 **exit code 0**으로
  종료한다(Task Scheduler의 `RestartOnFailure`가 정상적으로 실행 중인 이전 인스턴스를 방해하며 재시도
  폭주를 일으키지 않도록 하기 위함). 스킵은 로그(WARN)와 알림 채널에 별도로 기록되어 가시성을 확보한다
  (Task Scheduler의 "마지막 실행 결과"만으로는 스킵이 보이지 않으므로 앱 레벨 가시성이 필수).
- **FR7** 실패 알림은 `NotificationSink` 트레이트로 추상화한다. **기본값은 로그 파일 + Windows
  Event Log이며 항상 켜져 있다.** webhook/email은 `[notify]` config로 옵트인하는 추가 채널이다.
  webhook 전송 실패가 로그/이벤트 알림 자체를 막아서는 안 된다(채널 간 격리).
- **FR8** 모든 구조적 로그는 기록 직전 시크릿 리댁션을 거친다. DEK/KEK/복구코드/`enc:` 필드/
  DSN(연결문자열)이 어떤 로그 라인에도 원문으로 남지 않는다(M6). 리댁션은 (a) 명시적 `scrub()` 호출
  지점과 (b) 파일 writer를 감싸는 방어적 레이어 두 겹으로 적용한다.
- **FR9** 매 `run` 종료 시(성공/실패/부분성공 모두) **RPO notice**를 stdout과 로그에 출력한다: 해당
  job의 마지막 성공 백업 이후 경과 시간과, 스케줄 간격 대비 초과 여부.

### 비기능 요구사항 (NFR)
- 대상 실행 환경은 Windows 10/11, Windows Server 2019+ (Task Scheduler 2.0/XML `version="1.4"` 지원
  범위 내).
- schtasks 호출 실패/타임아웃은 명확한 에러로 전파되고, stderr는 로그 기록 전 리댁션을 거친다.
- 이 섹션은 새 overlap guard 코드를 작성하지 않는다(S08 재사용, 중복 구현 금지).

## Dependencies

- **S05(Pipeline)** — `restoreplane-pipeline::run_job(job: &str, ctx) -> Result<RunOutcome, PipelineError>`을
  호출만 한다. 실제 덤프/암호화/업로드 로직은 전혀 이 섹션의 관심사가 아니다.
- **S08(Catalog)** — `restoreplane-catalog::overlap::{try_acquire_run, RunGuard, OverlapError}`를 그대로
  가져다 쓴다. 또한 RPO notice 계산을 위해 `backups` 테이블 읽기 전용 조회(`open_reader_conn`)를
  사용한다.
- **S02(SecretManager)** — 직접 의존하지 않는다. 다만 S05가 내부적으로 unattended 경로에서 S02의
  DPAPI unwrap을 호출하므로, 이 섹션의 계정/권한 설계(§ 배경)가 S02의 C1 대응(최소권한 계정,
  keyfile ACL)과 같은 서비스 계정을 가리키도록 문서적으로 정합을 맞춘다.
- **S01(Config/CLI)** — `Job{name, schedule, destinations, ...}`, `Policy.retry{attempts, backoff_secs}`,
  `Notify{channel, webhook_url, email_to}`를 읽기만 한다. `restoreplane schedule` CLI 인자
  (`ScheduleArgs{job, remove}`)는 S01이 이미 정의했고, 이 섹션은 그 핸들러 로직만 구현한다.
- **S11(보안 하드닝)** — 서비스 계정의 실제 프로비저닝(계정 생성, "배치 작업으로 로그온" 권한 부여,
  install 디렉터리/keyfile ACL)은 운영 절차로 S11 범위에 속한다. 이 섹션은 그 계정의 **이름**만
  config로 받아 XML에 채워 넣는다.
- **이 섹션이 신규 생성하는 크레이트**: `restoreplane-scheduler` (`sections/index.md`/S01 §1에서 이미
  예고된 이름).

## Reference Libraries

| 크레이트 | 버전 | 역할 | 비고 |
|---|---|---|---|
| `windows` | 0.58.x | Windows Event Log(`ReportEventW`) 호출 | `Win32_System_EventLog`, `Win32_Foundation` feature. `[확인 필요]` 정확한 API 시그니처는 버전별로 다를 수 있음 |
| `regex` | 1.x | 로그 리댁션 패턴(`enc:`, DSN, 키워드=값) | 컴파일 타임 `Lazy`/`OnceLock`로 1회 컴파일 |
| `reqwest` | 0.12.x (`rustls-tls`, `json`) | webhook 알림 전송 | `default-features = false` + rustls로 OpenSSL 전이 의존 회피(H6 정합) |
| `async-trait` | 0.1.x | `NotificationSink` async 트레이트 | S08과 동일 패턴 |
| `time` | 0.3.x | RFC3339 타임스탬프, RPO 경과시간 계산 | S08과 동일 |
| `thiserror` | 1.x | `SchedulerError`, `OverlapError`(S08 재사용) | |
| `tokio` (`process`, `fs`, `rt-multi-thread`) | 1.x | `schtasks.exe` 비동기 스폰, XML 파일 쓰기 | |
| (dev) `quick-xml` | 0.36.x | 생성된 XML의 round-trip 파싱 검증(런타임 의존 아님) | XML은 수기 템플릿+이스케이프로 생성, 파싱 검증에만 사용 |

> `[확인 필요]`: `windows` 0.58 `EventLog` 모듈의 `RegisterEventSourceW`/`ReportEventW` 정확한
> 시그니처(버전별 변동 가능). 메시지 DLL 미등록 시 Event Viewer에 "설명을 찾을 수 없음" 디스클레이머가
> 붙는 것은 알려진 동작이며 기능에는 영향 없음(Risk & Rollback 참고).

## Implementation

### 모듈 구조

```
crates/restoreplane-scheduler/
  Cargo.toml
  src/
    lib.rs          // SchedulerContext, 공개 API
    cron.rs         // "M H * * *" / "M H * * D" 서브셋 파서 -> ScheduleSpec
    xml.rs          // Task Scheduler XML 템플릿 생성 (수기 문자열 + 이스케이프)
    register.rs      // schtasks /Create /Query /Delete 프로세스 호출
    overlap.rs        // S08 RunGuard 어댑터: exit code 결정 + skip 가시화
    notify.rs          // NotificationSink + Log/EventLog/Webhook/EmailPlaceholder Sink
    redact.rs           // scrub(), RedactingWriter (M6)
    rpo.rs               // RPO notice 계산 + 출력
    entrypoint.rs         // run_scheduled_job() — 위 전부를 배선하는 유일한 진입점
  tests/
    cron_parser.rs
    xml_roundtrip.rs
    redact.rs
    overlap_skip_exit_code.rs
    rpo_notice.rs
```

```toml
# crates/restoreplane-scheduler/Cargo.toml
[package]
name = "restoreplane-scheduler"
version.workspace = true
edition.workspace = true

[dependencies]
restoreplane-core = { path = "../restoreplane-core" }
restoreplane-config = { path = "../restoreplane-config" }
restoreplane-catalog = { path = "../restoreplane-catalog" }
restoreplane-pipeline = { path = "../restoreplane-pipeline" }
anyhow = { workspace = true }
thiserror = { workspace = true }
tracing = { workspace = true }
time = { version = "0.3", features = ["formatting", "parsing", "serde-well-known"] }
regex = "1"
async-trait = "0.1"
tokio = { version = "1", features = ["process", "fs", "rt-multi-thread"] }
reqwest = { version = "0.12", default-features = false, features = ["json", "rustls-tls"] }

[target.'cfg(windows)'.dependencies]
windows = { version = "0.58", features = ["Win32_System_EventLog", "Win32_Foundation"] }

[dev-dependencies]
quick-xml = "0.36"
tempfile = "3"
```

### 10.1 cron 서브셋 파서 (cron.rs, FR3)

```rust
// src/cron.rs
use thiserror::Error;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ScheduleSpec {
    Daily { hour: u8, minute: u8 },
    Weekly { hour: u8, minute: u8, day: Weekday },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Weekday {
    Sun, Mon, Tue, Wed, Thu, Fri, Sat,
}

impl Weekday {
    fn from_cron_field(f: &str) -> Result<Self, CronError> {
        match f {
            "0" | "7" => Ok(Weekday::Sun),
            "1" => Ok(Weekday::Mon),
            "2" => Ok(Weekday::Tue),
            "3" => Ok(Weekday::Wed),
            "4" => Ok(Weekday::Thu),
            "5" => Ok(Weekday::Fri),
            "6" => Ok(Weekday::Sat),
            other => Err(CronError::Unsupported(format!("day-of-week '{other}'"))),
        }
    }

    /// Task Scheduler XML의 <DaysOfWeek> 자식 엘리먼트 태그명.
    pub fn xml_tag(&self) -> &'static str {
        match self {
            Weekday::Sun => "Sunday", Weekday::Mon => "Monday", Weekday::Tue => "Tuesday",
            Weekday::Wed => "Wednesday", Weekday::Thu => "Thursday", Weekday::Fri => "Friday",
            Weekday::Sat => "Saturday",
        }
    }
}

#[derive(Debug, Error)]
pub enum CronError {
    #[error("cron 표현식은 정확히 5개 필드가 필요함(분 시 일 월 요일): '{0}'")]
    WrongFieldCount(String),
    #[error("v1은 'M H * * *'(매일) 또는 'M H * * D'(매주 요일 D)만 지원함. 지원 안 되는 표현: {0}")]
    Unsupported(String),
    #[error("숫자 필드 파싱 실패: {0}")]
    InvalidNumber(#[source] std::num::ParseIntError),
}

/// "0 3 * * *" -> Daily{3,0}, "30 3 * * 1" -> Weekly{3,30,Mon}.
/// 리스트(`1,2`)/범위(`1-5`)/스텝(`*/15`)은 v1 스코프 밖 — 명확히 거부한다(FR3).
pub fn parse_cron_subset(expr: &str) -> Result<ScheduleSpec, CronError> {
    let fields: Vec<&str> = expr.split_whitespace().collect();
    if fields.len() != 5 {
        return Err(CronError::WrongFieldCount(expr.to_string()));
    }
    let [minute, hour, day_of_month, month, day_of_week] = [fields[0], fields[1], fields[2], fields[3], fields[4]];

    if day_of_month != "*" || month != "*" {
        return Err(CronError::Unsupported(format!("일/월 필드는 '*'만 지원: '{expr}'")));
    }
    let m: u8 = minute.parse().map_err(CronError::InvalidNumber)?;
    let h: u8 = hour.parse().map_err(CronError::InvalidNumber)?;
    if m > 59 || h > 23 {
        return Err(CronError::Unsupported(format!("분/시 범위 초과: '{expr}'")));
    }

    if day_of_week == "*" {
        Ok(ScheduleSpec::Daily { hour: h, minute: m })
    } else {
        Ok(ScheduleSpec::Weekly { hour: h, minute: m, day: Weekday::from_cron_field(day_of_week)? })
    }
}
```

### 10.2 Task Scheduler XML 템플릿 (xml.rs, FR1)

전체 XML 템플릿(이 문서에서 요구하는 산출물). 실제 생성은 `render_task_xml`이 파라미터를 채워
넣으며, 값은 모두 `escape_xml`을 거친다.

```xml
<?xml version="1.0" encoding="UTF-16"?>
<Task version="1.4" xmlns="http://schemas.microsoft.com/windows/2004/02/mit/task">
  <RegistrationInfo>
    <Description>restoreplane job '{JOB_NAME}' — restoreplane schedule 커맨드로 관리됨. 수기 편집 금지.</Description>
    <URI>\RestorePlane\{JOB_NAME}</URI>
  </RegistrationInfo>
  <Triggers>
    <CalendarTrigger>
      <StartBoundary>{START_BOUNDARY}</StartBoundary>
      <Enabled>true</Enabled>
      {SCHEDULE_BY_BLOCK}
      <RestartOnFailure>
        <Interval>PT{RETRY_BACKOFF_MIN}M</Interval>
        <Count>{RETRY_ATTEMPTS}</Count>
      </RestartOnFailure>
    </CalendarTrigger>
  </Triggers>
  <Principals>
    <Principal id="Author">
      <UserId>{SERVICE_ACCOUNT}</UserId>
      <LogonType>{LOGON_TYPE}</LogonType>
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
  </Settings>
  <Actions Context="Author">
    <Exec>
      <Command>{EXE_PATH}</Command>
      <Arguments>run --job "{JOB_NAME}" --scheduled</Arguments>
      <WorkingDirectory>{WORKING_DIR}</WorkingDirectory>
    </Exec>
  </Actions>
</Task>
```

`{SCHEDULE_BY_BLOCK}`은 `ScheduleSpec`에 따라 둘 중 하나로 치환된다:

```xml
<!-- Daily -->
<ScheduleByDay><DaysInterval>1</DaysInterval></ScheduleByDay>

<!-- Weekly (요일 1개) -->
<ScheduleByWeek>
  <WeeksInterval>1</WeeksInterval>
  <DaysOfWeek><Monday /></DaysOfWeek>
</ScheduleByWeek>
```

```rust
// src/xml.rs
use crate::cron::{ScheduleSpec, Weekday};
use time::OffsetDateTime;

pub struct TaskXmlParams {
    pub job_name: String,
    pub exe_path: String,
    pub working_dir: String,
    pub service_account: String,   // 예: "CORP\\svc-restoreplane" 또는 ".\\svc-restoreplane"(로컬 계정)
    pub logon_type: LogonType,
    pub schedule: ScheduleSpec,
    pub next_start_boundary: OffsetDateTime, // 최초 등록 시각(그 이후는 Task Scheduler가 자체 계산)
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
    fn xml_value(&self) -> &'static str {
        match self { LogonType::S4U => "S4U", LogonType::Password => "Password" }
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
        ScheduleSpec::Daily { .. } => "<ScheduleByDay><DaysInterval>1</DaysInterval></ScheduleByDay>".to_string(),
        ScheduleSpec::Weekly { day, .. } => format!(
            "<ScheduleByWeek><WeeksInterval>1</WeeksInterval><DaysOfWeek><{tag} /></DaysOfWeek></ScheduleByWeek>",
            tag = day.xml_tag()
        ),
    }
}

fn start_boundary_for(spec: &ScheduleSpec, from: OffsetDateTime) -> String {
    let (hour, minute) = match spec {
        ScheduleSpec::Daily { hour, minute } => (*hour, *minute),
        ScheduleSpec::Weekly { hour, minute, .. } => (*hour, *minute),
    };
    // 다음 발생 시각을 대략 계산해 최초 StartBoundary로 사용한다. 정확한 반복은 XML의
    // ScheduleByDay/Week가 담당하므로, 여기서는 "과거가 아닌 가장 가까운 유효 시각"이면 충분하다.
    let mut candidate = from.replace_time(
        time::Time::from_hms(hour, minute, 0).expect("hour/minute은 parse_cron_subset이 범위 검증함"),
    );
    if candidate <= from {
        candidate += time::Duration::days(1);
    }
    candidate.format(&time::format_description::well_known::Rfc3339).expect("RFC3339 포맷 실패 없음")
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
      <RestartOnFailure>
        <Interval>PT{backoff}M</Interval>
        <Count>{attempts}</Count>
      </RestartOnFailure>
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
  </Settings>
  <Actions Context="Author">
    <Exec>
      <Command>{exe}</Command>
      <Arguments>run --job &quot;{job}&quot; --scheduled</Arguments>
      <WorkingDirectory>{wd}</WorkingDirectory>
    </Exec>
  </Actions>
</Task>"#,
        job = escape_xml(&p.job_name),
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
```

> **주의(gotcha)**: Task Scheduler는 XML 파일이 **UTF-16LE + BOM**으로 인코딩되어 있을 것을 기대한다
> (선언부 `encoding="UTF-16"`과 실제 바이트 인코딩이 일치해야 함). `render_task_xml`은 Rust `String`
> (UTF-8)을 반환하므로, 파일로 쓸 때 반드시 UTF-16LE 변환을 거친다(§10.3 `write_task_xml_file`).

### 10.3 등록/조회/삭제 — schtasks 프로세스 호출 (register.rs, FR1/FR2/FR4/FR5)

```rust
// src/register.rs
use crate::xml::{render_task_xml, TaskXmlParams};
use crate::redact::scrub;
use std::path::{Path, PathBuf};
use thiserror::Error;
use tokio::process::Command;

#[derive(Debug, Error)]
pub enum SchedulerError {
    #[error("schtasks 프로세스 실행 실패: {0}")]
    Spawn(#[source] std::io::Error),
    #[error("schtasks 실패(태스크 '{task_name}'): {stderr}")]
    SchtasksFailed { task_name: String, stderr: String },
    #[error("XML 파일 쓰기 실패: {0}")]
    Io(#[source] std::io::Error),
}

const TASK_FOLDER: &str = "RestorePlane";

fn task_name(job: &str) -> String {
    format!("{TASK_FOLDER}\\{job}")
}

/// UTF-8 XML 문자열을 Task Scheduler가 기대하는 UTF-16LE + BOM으로 파일에 쓴다(gotcha, §10.2).
async fn write_task_xml_file(path: &Path, xml_utf8: &str) -> Result<(), SchedulerError> {
    let mut bytes: Vec<u8> = vec![0xFF, 0xFE]; // UTF-16LE BOM
    for unit in xml_utf8.encode_utf16() {
        bytes.extend_from_slice(&unit.to_le_bytes());
    }
    tokio::fs::write(path, bytes).await.map_err(SchedulerError::Io)
}

/// 태스크를 등록(또는 갱신)한다. FR4: 동일 XML이면 재실행은 사실상 no-op이지만, schtasks 자체는
/// 매번 /Create /F로 덮어쓴다 — "변경 없음" 판정은 register_idempotent가 사전에 diff로 수행한다.
pub async fn register_task(
    params: &TaskXmlParams,
    xml_dir: &Path,
) -> Result<PathBuf, SchedulerError> {
    let xml = render_task_xml(params);
    let xml_path = xml_dir.join(format!("{}.xml", sanitize_filename(&params.job_name)));
    write_task_xml_file(&xml_path, &xml).await?;

    let tn = task_name(&params.job_name);
    let mut cmd = Command::new("schtasks");
    cmd.arg("/Create").arg("/XML").arg(&xml_path).arg("/TN").arg(&tn);
    // FR2: LogonType=S4U 기본 경로는 /RP(비밀번호)를 절대 넘기지 않는다 — 저장할 비밀번호 자체가 없음.
    // /RU만으로 계정이 지정되고, S4U는 계정의 배치 로그온 권한만으로 동작한다.
    cmd.arg("/RU").arg(&params.service_account);
    cmd.arg("/F");

    let output = cmd.output().await.map_err(SchedulerError::Spawn)?;
    if !output.status.success() {
        let stderr = scrub(&String::from_utf8_lossy(&output.stderr));
        return Err(SchedulerError::SchtasksFailed { task_name: tn, stderr });
    }
    tracing::info!(job = %params.job_name, xml = %xml_path.display(), "Task Scheduler 등록 완료");
    Ok(xml_path)
}

/// 등록 전 현재 상태와 비교해 실제로 바뀐 경우만 재등록한다(FR4).
pub async fn register_idempotent(
    params: &TaskXmlParams,
    xml_dir: &Path,
) -> Result<RegisterOutcome, SchedulerError> {
    let new_xml = render_task_xml(params);
    if let Some(existing_xml) = query_task_xml(&params.job_name).await.ok().flatten() {
        if normalize_for_compare(&existing_xml) == normalize_for_compare(&new_xml) {
            tracing::info!(job = %params.job_name, "스케줄 변경 없음 — 재등록 스킵");
            return Ok(RegisterOutcome::Unchanged);
        }
    }
    register_task(params, xml_dir).await?;
    Ok(RegisterOutcome::Registered)
}

#[derive(Debug, PartialEq, Eq)]
pub enum RegisterOutcome { Registered, Unchanged }

/// `/Query /XML`로 현재 등록된 XML을 읽어온다. 태스크가 없으면 Ok(None).
pub async fn query_task_xml(job: &str) -> Result<Option<String>, SchedulerError> {
    let tn = task_name(job);
    let output = Command::new("schtasks")
        .arg("/Query").arg("/TN").arg(&tn).arg("/XML")
        .output().await.map_err(SchedulerError::Spawn)?;
    if !output.status.success() {
        return Ok(None); // 존재하지 않음 — schtasks가 "찾을 수 없음" 류 에러를 stderr로 반환
    }
    Ok(Some(String::from_utf8_lossy(&output.stdout).to_string()))
}

pub async fn remove_task(job: &str) -> Result<(), SchedulerError> {
    let tn = task_name(job);
    let output = Command::new("schtasks")
        .arg("/Delete").arg("/TN").arg(&tn).arg("/F")
        .output().await.map_err(SchedulerError::Spawn)?;
    if !output.status.success() {
        let stderr = scrub(&String::from_utf8_lossy(&output.stderr));
        return Err(SchedulerError::SchtasksFailed { task_name: tn, stderr });
    }
    tracing::info!(job, "Task Scheduler 태스크 제거 완료");
    Ok(())
}

/// 비교 시 자동 생성 필드(예: Task Scheduler가 붙이는 <Version> GUID, 줄바꿈 방식)의
/// 잡음을 제거하기 위한 최소 정규화. 완전한 XML 정규화가 아니라 실용적 diff 목적.
fn normalize_for_compare(xml: &str) -> String {
    xml.chars().filter(|c| !c.is_whitespace()).collect()
}

fn sanitize_filename(job: &str) -> String {
    job.chars().map(|c| if c.is_alphanumeric() || c == '-' || c == '_' { c } else { '_' }).collect()
}
```

> **잔여 리스크(Password 경로)**: 환경상 S4U를 쓸 수 없어 `LogonType::Password`를 선택한 경우,
> `/RP <password>`를 schtasks 커맨드라인 인자로 넘겨야 한다(Task Scheduler XML 자체는 비밀번호를
>담지 않는 보안 설계이기 때문). 이 인자는 짧은 시간 동안 프로세스 커맨드라인으로 노출된다(동일 머신의
> 다른 관리자/도구가 프로세스 목록을 통해 관측 가능). 이는 `schtasks.exe`의 근본적 제약이며, Rust
> 코드가 로그에 남기지 않는 것과는 별개의 잔여 리스크다 — **그래서 FR2가 S4U를 기본값으로 강제**한다.
> 이 경로를 완전히 없애려면 `windows` 크레이트로 Task Scheduler COM API(`ITaskFolder::RegisterTaskDefinition`)를
> 직접 호출해 비밀번호를 프로세스 내부에서만 전달하는 방법이 있다(`[확인 필요]`, 향후 개선 후보로
> Risk & Rollback에 기록).

### 10.4 Overlap Guard 어댑터 — S08 재사용 + exit code 정책 (overlap.rs, FR6)

```rust
// src/overlap.rs
use restoreplane_catalog::overlap::{try_acquire_run, OverlapError, RunGuard};
use restoreplane_catalog::writer::CatalogWriterHandle;

/// 스케줄러 진입점(entrypoint.rs)이 쓰는 exit code. Task Scheduler의 RestartOnFailure는 "exit != 0"을
/// 실패로 간주해 재시도를 건다 — 정상적으로 실행 중인 이전 인스턴스를 건드리지 않기 위해
/// 스킵(overlap)은 의도적으로 0(성공)으로 리턴한다(FR6). 가시성은 로그/알림이 담당한다.
pub const EXIT_SUCCESS: i32 = 0;
pub const EXIT_FAILED: i32 = 1;
pub const EXIT_SKIPPED_OVERLAP: i32 = 0; // 의도적으로 EXIT_SUCCESS와 동일값
pub const EXIT_SCHEDULER_ERROR: i32 = 2;

pub enum AcquireOutcome {
    Acquired(RunGuard),
    Skipped, // 이미 실행 중 — catch-up이 아직 끝나지 않은 인스턴스와 겹침
}

/// try_acquire_run을 호출하고, "이미 실행 중"과 "그 외 오류"를 구분해 로그/의미를 명확히 한다.
pub async fn acquire_or_skip(
    writer: &CatalogWriterHandle,
    job: &str,
) -> Result<AcquireOutcome, OverlapError> {
    match try_acquire_run(writer, job).await {
        Ok(guard) => Ok(AcquireOutcome::Acquired(guard)),
        Err(OverlapError::AlreadyRunning(_)) => {
            tracing::warn!(
                job,
                "overlap guard: 이미 실행 중인 인스턴스가 있어 이번 트리거(catch-up 포함)를 스킵/coalesce함"
            );
            Ok(AcquireOutcome::Skipped)
        }
        Err(e) => Err(e), // 뮤텍스/DB 오류 등은 진짜 오류로 전파(entrypoint가 EXIT_SCHEDULER_ERROR 처리)
    }
}
```

### 10.5 알림 추상화 (notify.rs, FR7)

```rust
// src/notify.rs
use async_trait::async_trait;
use restoreplane_config::schema::Notify as NotifyConfig;
use restoreplane_core::NotifyChannel;
use time::OffsetDateTime;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunOutcome { Success, PartialSuccess, Failed, SkippedOverlap }

#[derive(Debug, Clone)]
pub struct RunEvent {
    pub job: String,
    pub backup_id: Option<String>,
    pub outcome: RunOutcome,
    pub started_at: OffsetDateTime,
    pub finished_at: Option<OffsetDateTime>,
    /// 이미 scrub()을 거친 메시지만 넣는다 — 호출부 책임(§10.6과 이중 방어).
    pub message: String,
    pub rpo_summary: Option<String>,
}

#[async_trait]
pub trait NotificationSink: Send + Sync {
    async fn notify(&self, event: &RunEvent) -> anyhow::Result<()>;
    fn name(&self) -> &'static str;
}

/// 여러 sink에 순차 통지하되, 하나가 실패해도 나머지를 막지 않는다(FR7 채널 격리).
/// 로그+이벤트는 기본값으로 항상 포함되어야 하며, webhook 실패가 이 둘을 방해해서는 안 된다.
pub struct CompositeSink {
    sinks: Vec<Box<dyn NotificationSink>>,
}

impl CompositeSink {
    pub fn default_plus_config(notify_cfg: Option<&NotifyConfig>) -> Self {
        let mut sinks: Vec<Box<dyn NotificationSink>> = vec![
            Box::new(LogFileSink),
            Box::new(WindowsEventLogSink::new()),
        ];
        if let Some(cfg) = notify_cfg {
            match cfg.channel {
                NotifyChannel::None => {}
                NotifyChannel::Webhook => {
                    if let Some(url_field) = &cfg.webhook_url {
                        sinks.push(Box::new(WebhookSink::from_enc_field(url_field)));
                    } else {
                        tracing::warn!("notify.channel=webhook이나 webhook_url이 비어 있음 — webhook sink 미등록");
                    }
                }
                NotifyChannel::Email => {
                    // 오픈 이슈(plan.md): email 채널 미구현. 조용히 무시하지 않고 매 run마다
                    // 명시적으로 경고해 "설정했는데 실제로는 알림이 안 간다"는 상황을 막는다.
                    sinks.push(Box::new(EmailPlaceholderSink));
                }
            }
        }
        CompositeSink { sinks }
    }

    pub async fn notify_all(&self, event: &RunEvent) {
        for sink in &self.sinks {
            if let Err(e) = sink.notify(event).await {
                // 리댁션 이중 방어: 에러 메시지 자체도 scrub 후 로그(외부 SDK 에러가 시크릿을
                // 담고 있을 가능성, 예: webhook 응답이 URL/토큰을 에코).
                tracing::error!(sink = sink.name(), error = %crate::redact::scrub(&e.to_string()),
                    "알림 채널 전송 실패(다른 채널에는 영향 없음)");
            }
        }
    }
}

/// 실제 파일 기록은 tracing이 이미 수행한다(RedactingWriter 경유, §10.6). 이 sink는 run 종료를
/// 요약 라인으로 한 번 더 명시적으로 남겨 알림 이력을 로그에서 grep하기 쉽게 한다.
pub struct LogFileSink;

#[async_trait]
impl NotificationSink for LogFileSink {
    async fn notify(&self, event: &RunEvent) -> anyhow::Result<()> {
        tracing::info!(
            job = %event.job, outcome = ?event.outcome, backup_id = ?event.backup_id,
            "run 종료 요약: {}", event.message
        );
        Ok(())
    }
    fn name(&self) -> &'static str { "log" }
}

pub struct EmailPlaceholderSink;

#[async_trait]
impl NotificationSink for EmailPlaceholderSink {
    async fn notify(&self, event: &RunEvent) -> anyhow::Result<()> {
        tracing::warn!(job = %event.job,
            "notify.channel=email은 아직 구현되지 않음(plan.md Open Questions) — 이 알림은 전송되지 않았음. \
             log+event 채널만 신뢰할 것");
        Ok(())
    }
    fn name(&self) -> &'static str { "email(placeholder)" }
}

pub struct WebhookSink {
    url: String, // SecretManager로 복호화된 평문 URL. 절대 로그에 출력하지 않는다.
    client: reqwest::Client,
}

impl WebhookSink {
    pub fn from_enc_field(_field: &restoreplane_core::EncField) -> Self {
        // 실제 복호화는 S02 SecretManager 경유(호출부 배선은 entrypoint.rs에서 이미 언랩된
        // 평문을 주입하는 형태로 리팩터링될 수 있음 — 여기서는 인터페이스만 고정).
        todo!("S02 SecretManager 연동 후 EncField -> 평문 URL 언랩 배선")
    }
}

#[async_trait]
impl NotificationSink for WebhookSink {
    async fn notify(&self, event: &RunEvent) -> anyhow::Result<()> {
        let body = serde_json::json!({
            "job": event.job,
            "outcome": format!("{:?}", event.outcome),
            "backup_id": event.backup_id,
            "message": event.message, // 호출부가 이미 scrub한 메시지만 여기 도달(FR7)
            "rpo": event.rpo_summary,
        });
        self.client.post(&self.url).json(&body).send().await?.error_for_status()?;
        Ok(())
    }
    fn name(&self) -> &'static str { "webhook" }
}
```

Windows Event Log sink (`ReportEventW`):

```rust
// src/notify.rs (계속, cfg(windows))
#[cfg(windows)]
mod eventlog {
    use windows::core::PCWSTR;
    use windows::Win32::Foundation::HANDLE;
    use windows::Win32::System::EventLog::{
        DeregisterEventSource, RegisterEventSourceW, ReportEventW,
        EVENTLOG_ERROR_TYPE, EVENTLOG_INFORMATION_TYPE, EVENTLOG_WARNING_TYPE,
    };

    const SOURCE_NAME: &str = "restoreplane";

    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(std::iter::once(0)).collect()
    }

    pub struct EventLogHandle(HANDLE);

    impl EventLogHandle {
        pub fn open() -> anyhow::Result<Self> {
            let name = wide(SOURCE_NAME);
            // 사전에 레지스트리 EventMessageFile이 등록되어 있지 않아도 ReportEventW는 성공한다.
            // Event Viewer에는 "설명을 찾을 수 없음" 문구 + 우리가 넘긴 원문 문자열이 함께 표시됨
            // (기능에는 무해, Risk & Rollback에 개선 항목으로 기록).
            let h = unsafe { RegisterEventSourceW(PCWSTR::null(), PCWSTR(name.as_ptr()))? };
            Ok(EventLogHandle(h))
        }

        pub fn report(&self, level: EventLevel, event_id: u32, message: &str) -> anyhow::Result<()> {
            let msg_wide = wide(message);
            let strings = [PCWSTR(msg_wide.as_ptr())];
            let ev_type = match level {
                EventLevel::Info => EVENTLOG_INFORMATION_TYPE,
                EventLevel::Warning => EVENTLOG_WARNING_TYPE,
                EventLevel::Error => EVENTLOG_ERROR_TYPE,
            };
            unsafe {
                ReportEventW(self.0, ev_type, 0, event_id, None, 0, Some(&strings), None)?;
            }
            Ok(())
        }
    }

    impl Drop for EventLogHandle {
        fn drop(&mut self) {
            unsafe { let _ = DeregisterEventSource(self.0); }
        }
    }

    #[derive(Debug, Clone, Copy)]
    pub enum EventLevel { Info, Warning, Error }
}

pub struct WindowsEventLogSink {
    #[cfg(windows)]
    handle: std::sync::OnceLock<anyhow::Result<eventlog::EventLogHandle>>,
}

impl WindowsEventLogSink {
    pub fn new() -> Self {
        #[cfg(windows)]
        { WindowsEventLogSink { handle: std::sync::OnceLock::new() } }
        #[cfg(not(windows))]
        { WindowsEventLogSink {} }
    }
}

#[async_trait]
impl NotificationSink for WindowsEventLogSink {
    #[cfg(windows)]
    async fn notify(&self, event: &RunEvent) -> anyhow::Result<()> {
        let handle = self.handle.get_or_init(eventlog::EventLogHandle::open);
        let handle = handle.as_ref().map_err(|e| anyhow::anyhow!("{e}"))?;
        let (level, event_id) = match event.outcome {
            RunOutcome::Success => (eventlog::EventLevel::Info, 1000),
            RunOutcome::PartialSuccess => (eventlog::EventLevel::Warning, 1001),
            RunOutcome::Failed => (eventlog::EventLevel::Error, 1002),
            RunOutcome::SkippedOverlap => (eventlog::EventLevel::Warning, 1003),
        };
        // event.message는 이미 scrub()을 거친 문자열만 허용(FR8, 호출부 계약).
        handle.report(level, event_id, &format!("[{}] {}", event.job, event.message))
    }

    #[cfg(not(windows))]
    async fn notify(&self, _event: &RunEvent) -> anyhow::Result<()> {
        tracing::debug!("Windows Event Log는 Windows 대상에서만 동작함(비Windows 빌드는 no-op)");
        Ok(())
    }

    fn name(&self) -> &'static str { "windows-event-log" }
}
```

### 10.6 시크릿 리댁션 (redact.rs, FR8, M6)

```rust
// src/redact.rs
use regex::Regex;
use std::sync::OnceLock;

fn patterns() -> &'static Vec<(Regex, &'static str)> {
    static PATTERNS: OnceLock<Vec<(Regex, &'static str)>> = OnceLock::new();
    PATTERNS.get_or_init(|| vec![
        // enc:<wrap_gen>:<base64> 필드(config 시크릿, S01 EncField 와이어 포맷).
        (Regex::new(r"enc:\d+:[A-Za-z0-9+/=]+").unwrap(), "enc:[REDACTED]"),
        // DSN/연결문자열의 user:pass@host 부분(postgres://, mysql://, s3://, sftp:// 등 공통).
        (Regex::new(r"://[^/\s:@]+:[^/\s@]+@").unwrap(), "://[REDACTED]:[REDACTED]@"),
        // key=value / key: value 형태의 흔한 시크릿 키워드(대소문자 무관).
        (Regex::new(r"(?i)\b(password|passwd|secret|token|dek|kek|recovery[_-]?code|access[_-]?key|private[_-]?key)\s*[:=]\s*\S+").unwrap(),
            "$1=[REDACTED]"),
        // schtasks /RP 인자가 실수로 로그 문자열에 섞여 들어온 경우.
        (Regex::new(r"(?i)/RP\s+\S+").unwrap(), "/RP [REDACTED]"),
    ])
}

/// 로그/알림에 실어 보내기 직전 반드시 통과시킨다(FR8). 명시적 호출 지점.
pub fn scrub(input: &str) -> String {
    let mut out = input.to_string();
    for (re, replacement) in patterns() {
        out = re.replace_all(&out, *replacement).to_string();
    }
    out
}

/// 파일 writer를 감싸는 방어적 레이어(2중 방어). 호출부가 scrub()을 빠뜨려도 마지막 관문에서
/// 걸러진다 — 특히 서드파티 크레이트(DB 드라이버/S3 SDK)의 Display 구현이 DSN을 그대로
/// 노출하는 경우를 잡기 위함(M6).
pub struct RedactingWriter<W: std::io::Write> {
    inner: W,
}

impl<W: std::io::Write> RedactingWriter<W> {
    pub fn new(inner: W) -> Self { RedactingWriter { inner } }
}

impl<W: std::io::Write> std::io::Write for RedactingWriter<W> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let text = String::from_utf8_lossy(buf);
        let scrubbed = scrub(&text);
        self.inner.write_all(scrubbed.as_bytes())?;
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> { self.inner.flush() }
}
```

`tracing_subscriber` 초기화(S01 `init_tracing`을 확장, S11이 최종 정책 확정):

```rust
// src/lib.rs (발췌)
pub fn init_scheduler_tracing(log_path: &std::path::Path) -> anyhow::Result<()> {
    let file = std::fs::OpenOptions::new().create(true).append(true).open(log_path)?;
    let redacting = crate::redact::RedactingWriter::new(file);
    tracing_subscriber::fmt()
        .with_writer(std::sync::Mutex::new(redacting)) // 방어선 2(FR8)
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();
    Ok(())
}
```

### 10.7 RPO Notice (rpo.rs, FR9)

```rust
// src/rpo.rs
use crate::cron::ScheduleSpec;
use restoreplane_catalog::conn::open_reader_conn;
use std::path::Path;
use time::{Duration, OffsetDateTime};

pub struct RpoNotice {
    pub job: String,
    pub last_success_at: Option<OffsetDateTime>,
    pub elapsed_since_last_success: Option<Duration>,
    pub target_interval: Duration,
    pub within_target: bool,
}

fn target_interval_of(spec: &ScheduleSpec) -> Duration {
    match spec {
        ScheduleSpec::Daily { .. } => Duration::days(1),
        ScheduleSpec::Weekly { .. } => Duration::days(7),
    }
}

/// job의 마지막 성공 백업 시각을 카탈로그(읽기 전용)에서 조회해 RPO 경과를 계산한다.
/// 카탈로그는 캐시이므로(S08 H5 원칙) 이 notice는 "정보 제공"용이지 보안 판단에 쓰지 않는다.
pub fn compute_rpo_notice(
    catalog_db_path: &Path,
    job: &str,
    schedule: &ScheduleSpec,
) -> anyhow::Result<RpoNotice> {
    let conn = open_reader_conn(catalog_db_path)?;
    let last: Option<String> = conn.query_row(
        "SELECT started_at FROM backups WHERE job=?1 AND status='success' ORDER BY started_at DESC LIMIT 1",
        rusqlite::params![job],
        |row| row.get(0),
    ).ok();

    let target_interval = target_interval_of(schedule);
    let last_success_at = last.and_then(|s| OffsetDateTime::parse(&s, &time::format_description::well_known::Rfc3339).ok());
    let now = OffsetDateTime::now_utc();
    let elapsed = last_success_at.map(|t| now - t);
    let within_target = match elapsed {
        Some(e) => e <= target_interval + target_interval / 2, // 목표의 1.5배까지 허용(경고 임계값)
        None => false,
    };

    Ok(RpoNotice { job: job.to_string(), last_success_at, elapsed_since_last_success: elapsed, target_interval, within_target })
}

pub fn print_and_log(n: &RpoNotice) {
    let line = match (n.last_success_at, n.within_target) {
        (Some(t), true) => format!(
            "RPO notice: job '{}' — 최근 성공 백업 {} (경과 {}h{}m), 목표 RPO(~{}일) 이내.",
            n.job, t, n.elapsed_since_last_success.unwrap().whole_hours(),
            n.elapsed_since_last_success.unwrap().whole_minutes() % 60, n.target_interval.whole_days(),
        ),
        (Some(t), false) => format!(
            "RPO notice: [경고] job '{}' — 최근 성공 백업 {} 이후 경과 {}h — 목표 RPO(~{}일)를 초과함. \
             지금 랜섬웨어 피해가 발생하면 이 경과 시간만큼 데이터가 유실됩니다.",
            n.job, t, n.elapsed_since_last_success.unwrap().whole_hours(), n.target_interval.whole_days(),
        ),
        (None, _) => format!(
            "RPO notice: [경고] job '{}' — 성공한 백업 이력이 없습니다. 현재 RPO 보장 불가.", n.job
        ),
    };
    println!("{line}");
    if n.within_target { tracing::info!("{line}"); } else { tracing::warn!("{line}"); }
}
```

### 10.8 스케줄러 진입점 — 전부 배선 (entrypoint.rs, FR6/FR7/FR9)

`restoreplane run --scheduled` (Task Scheduler의 `<Actions><Exec><Arguments>`가 호출하는 바로 그 경로)와
수동 `restoreplane run`은 **동일한 진입점**을 공유한다 — overlap guard는 스케줄 여부와 무관하게 항상
적용되어야 하기 때문이다(S08 설계 의도 그대로).

```rust
// src/entrypoint.rs
use crate::notify::{CompositeSink, RunEvent, RunOutcome};
use crate::overlap::{acquire_or_skip, AcquireOutcome, EXIT_FAILED, EXIT_SCHEDULER_ERROR, EXIT_SKIPPED_OVERLAP, EXIT_SUCCESS};
use crate::redact::scrub;
use crate::rpo::{compute_rpo_notice, print_and_log};
use restoreplane_catalog::writer::CatalogWriterHandle;
use time::OffsetDateTime;

pub struct SchedulerContext {
    pub catalog_writer: CatalogWriterHandle,
    pub catalog_db_path: std::path::PathBuf,
    pub notify: CompositeSink,
}

/// restoreplane-cli의 `run` 커맨드 핸들러가 호출하는 유일한 진입점.
pub async fn run_scheduled_job(
    job: &str,
    schedule: &crate::cron::ScheduleSpec,
    ctx: &SchedulerContext,
) -> i32 {
    let acquired = match acquire_or_skip(&ctx.catalog_writer, job).await {
        Ok(o) => o,
        Err(e) => {
            tracing::error!(job, error = %scrub(&e.to_string()), "overlap guard 획득 자체가 실패함");
            return EXIT_SCHEDULER_ERROR;
        }
    };

    let guard = match acquired {
        AcquireOutcome::Acquired(g) => g,
        AcquireOutcome::Skipped => {
            let event = RunEvent {
                job: job.to_string(), backup_id: None, outcome: RunOutcome::SkippedOverlap,
                started_at: OffsetDateTime::now_utc(), finished_at: None,
                message: "이미 실행 중인 인스턴스가 있어 이번 트리거를 스킵함(overlap guard)".to_string(),
                rpo_summary: None,
            };
            ctx.notify.notify_all(&event).await;
            return EXIT_SKIPPED_OVERLAP; // 의도적으로 0 — RestartOnFailure 폭주 방지(FR6)
        }
    };

    let started_at = OffsetDateTime::now_utc();
    // S05 소유. 이 섹션은 호출만 한다 — 덤프/암호화/업로드 로직 없음.
    let result = restoreplane_pipeline::run_job(job).await;
    let finished_at = OffsetDateTime::now_utc();

    let (status, outcome, msg) = match &result {
        Ok(restoreplane_pipeline::JobResult::Success { backup_id }) =>
            ("success", RunOutcome::Success, format!("백업 성공(backup_id={backup_id})")),
        Ok(restoreplane_pipeline::JobResult::PartialSuccess { backup_id, failed_destinations }) =>
            ("partial", RunOutcome::PartialSuccess,
             format!("일부 목적지 실패(backup_id={backup_id}, 실패={failed_destinations:?})")),
        Err(e) => ("failed", RunOutcome::Failed, scrub(&format!("백업 실패: {e}"))),
    };

    if let Err(e) = guard.finish(&ctx.catalog_writer, status).await {
        tracing::error!(job, error = %scrub(&e.to_string()), "RunGuard.finish() 실패(runs row가 running으로 남을 수 있음, 다음 실행 시 stale 회수됨)");
    }

    let rpo = compute_rpo_notice(&ctx.catalog_db_path, job, schedule).ok();
    if let Some(n) = &rpo { print_and_log(n); }

    let event = RunEvent {
        job: job.to_string(),
        backup_id: result.as_ref().ok().and_then(|r| r.backup_id().map(str::to_string)),
        outcome, started_at, finished_at: Some(finished_at), message: msg,
        rpo_summary: rpo.map(|n| format!("last_success_elapsed_ok={}", n.within_target)),
    };
    ctx.notify.notify_all(&event).await;

    match outcome {
        RunOutcome::Success | RunOutcome::PartialSuccess => EXIT_SUCCESS,
        RunOutcome::Failed => EXIT_FAILED,
        RunOutcome::SkippedOverlap => unreachable!("이 분기는 위에서 이미 처리됨"),
    }
}
```

## Test Scenarios

1. **cron 서브셋 파싱**: `"0 3 * * *"` → `Daily{3,0}`, `"30 3 * * 1"` → `Weekly{3,30,Mon}`,
   `"*/15 * * * *"`/`"0 3 1 * *"`(일 필드 고정) → `CronError`로 명확히 거부.
2. **XML 생성/이스케이프**: job 이름에 `&`, `<`, `"` 포함 시 `render_task_xml` 결과가 `quick-xml`
   리더로 파싱 성공(잘못된 XML이 생성되지 않음).
3. **XML 필수 필드 존재**: 생성된 XML에 `StartWhenAvailable=true`, `MultipleInstancesPolicy=IgnoreNew`,
   `RunLevel=HighestAvailable`, `RestartOnFailure/Interval`·`Count`가 파라미터와 일치하는 값으로
   존재.
4. **등록 → 재조회 정합성(Windows 전용, 통합)**: `register_task` 후 `query_task_xml`로 재조회한 XML의
   `StartWhenAvailable`/`MultipleInstancesPolicy`/`LogonType`이 등록 파라미터와 일치.
5. **재부팅 후 놓친 실행의 catch-up(수동 QA 체크리스트 + 대체 자동화)**: `StartBoundary`를 과거로
   설정한 태스크를 등록하고 `schtasks /Run /TN`으로 즉시 트리거해 실행이 정상적으로 시작되는지 확인
   (실제 "머신이 꺼져 있다가 켜지는" 시나리오는 CI에서 재현 불가 — VM 재부팅 기반 수동 QA로 별도
   문서화. 여기서는 `StartWhenAvailable=true`가 XML에 정확히 박혀 있음을 자동 테스트로 보증하고,
   그 신뢰를 Task Scheduler 자체 동작에 위임한다).
6. **overlap 방지 — 겹치는 두 실행**: job A로 `acquire_or_skip`을 동시에 두 번 호출 →
   정확히 하나는 `Acquired`, 나머지 하나는 `Skipped`. `restoreplane_pipeline::run_job`이 정확히 1회만
   호출됐는지 mock으로 검증.
7. **overlap 스킵 시 exit code**: `run_scheduled_job`이 `Skipped` 경로를 탈 때 반환값이 정확히
   `EXIT_SKIPPED_OVERLAP`(0)이고, `CompositeSink::notify_all`이 `SkippedOverlap` outcome으로 호출됨.
8. **overlap 스킵이 실패로 오인되지 않음**: 시나리오 7의 로그 라인이 `tracing::warn!` 레벨로
   기록되고 `tracing::error!`가 아님(스킵 ≠ 장애).
9. **리댁션 — enc 필드**: `scrub("password=enc:3:aGVsbG8=")` 결과에 원본 base64가 남지 않음.
10. **리댁션 — DSN**: `scrub("postgres://backup_reader:hunter2@10.0.0.5/app")` 결과에 `hunter2`가
    없고 `[REDACTED]`로 치환됨.
11. **리댁션 — 키워드=값**: `scrub("dek=abcdef0123456789")`, `scrub("Recovery-Code: XXXX-YYYY")` 각각
    치환 확인.
12. **RedactingWriter 방어선 2**: `tracing::info!`로 시크릿 패턴이 섞인 문자열을 직접 로그해도
    실제 파일 바이트에는 패턴이 남지 않음(호출부가 scrub()을 빠뜨린 상황을 시뮬레이션).
13. **RPO notice — 정상**: 마지막 성공이 스케줄 간격(예: 1일)의 1.5배 이내 → `within_target=true`.
14. **RPO notice — 초과**: 마지막 성공이 스케줄 간격의 1.5배 초과 → `within_target=false`, 경고 문구
    출력.
15. **RPO notice — 이력 없음**: 해당 job의 성공 백업이 카탈로그에 전혀 없음 → 명시적 경고
    ("RPO 보장 불가").
16. **멱등 등록(FR4)**: 동일 파라미터로 `register_idempotent` 2회 호출 → 두 번째 호출이
    `RegisterOutcome::Unchanged` 반환, "변경 없음" 로그.
17. **삭제(FR5)**: `remove_task` 후 `query_task_xml`이 `None` 반환.
18. **알림 채널 격리**: `WebhookSink::notify`가 항상 에러를 반환하도록 mock한 상태에서
    `CompositeSink::notify_all` 호출 → `LogFileSink`/`WindowsEventLogSink`의 `notify`는 여전히
    호출됨(webhook 실패가 전파되어 다른 sink를 건너뛰지 않음).
19. **email placeholder 경고**: `notify.channel = "email"` 설정 시 매 run마다 "email 미구현" 경고
    로그가 남고, 예외/패닉 없이 나머지 sink는 정상 동작.

## Implementation Strategy

1. **cron.rs + xml.rs 먼저** — 파일시스템/프로세스 의존 없는 순수 로직. T1~T3을 조기 통과시켜
   Task Scheduler XML의 정확성을 확정한다.
2. **redact.rs 조기 고정** — 다른 모든 모듈(register/notify/entrypoint)이 `scrub()`을 호출하므로
   패턴 세트를 먼저 확정하고 T9~T12로 검증한다.
3. **register.rs** — schtasks 실 프로세스 호출은 Windows 환경에서만 통합 테스트 가능(T4~T5, T16~T17).
   CI가 Windows 러너를 포함하지 않는다면 로컬/스테이징 Windows 머신에서 수동 QA로 보완하고 결과를
   `docs/handoffs/`에 기록.
4. **overlap.rs** — S08의 `try_acquire_run`을 그대로 가져와 exit code 매핑만 씌운다. mock
   `restoreplane_pipeline::run_job`으로 T6~T8을 크로스플랫폼에서 검증.
5. **notify.rs** — LogFileSink/WindowsEventLogSink(기본, 항상 켜짐)를 먼저 완성하고, WebhookSink는
   그 다음, EmailPlaceholderSink는 마지막(T18~T19).
6. **rpo.rs** — S08 카탈로그가 준비된 이후 통합(T13~T15).
7. **entrypoint.rs** — 5~6단계 결과물을 배선. `restoreplane-cli`의 `run` 핸들러(S01이 정의한 `RunArgs`)가
   이 함수를 호출하도록 연결하는 것으로 이 섹션의 구현을 마무리한다.
8. 전 과정에서 **로그에 시크릿이 새는 새 경로가 생기지 않는지** 코드 리뷰 체크리스트로 점검
   (특히 서드파티 에러 `Display`를 `%e`로 그대로 로그하는 곳이 있으면 리젝트, 반드시 `scrub()` 경유).

## Quality Gate

- `cargo test -p restoreplane-scheduler`: 크로스플랫폼 테스트(T1~T3, T6~T15, T18~T19) green. Windows
  전용 테스트(T4~T5, T16~T17)는 `#[cfg(windows)]`로 게이팅하고 Windows CI 러너/수동 QA에서 별도 확인.
- `cargo clippy --all-targets -- -D warnings`.
- grep 감사: 기본 등록 경로(`register_idempotent`/`register_task`)에 `/RP` 인자가 존재하지 않음(S4U
  기본값 확인). `LogonType::Password` 경로는 명시적 옵트인 함수로만 도달 가능해야 함.
- grep 감사: `tracing::(info|warn|error)!` 호출부 중 서드파티 에러(`%e`/`?e`/`{e}`)를 직접 넣는 곳이
  `scrub()`을 거치지 않고 존재하지 않음(FR8).
- `RunOutcome::SkippedOverlap` → exit code 매핑이 `EXIT_SUCCESS`와 동일 값임을 상수 비교 테스트로
  고정(실수로 값이 갈라지는 회귀 방지).
- Windows Event Log sink가 비Windows 빌드에서도 컴파일되고 no-op으로 동작함(크로스플랫폼 개발환경
  지원, S08과 동일 원칙).

## Risk & Rollback

| 리스크 | 완화 | 롤백 |
|---|---|---|
| S4U 로그온에 필요한 "배치 작업으로 로그온" 권한이 계정에 없어 등록 실패 | `doctor` 커맨드(S01)에 사전 점검 항목 추가 권고, 에러 메시지에 `secpol.msc`/그룹 정책 경로 안내 | `LogonType::Password` 옵트인 경로로 폴백(잔여 리스크는 §10.3에 문서화된 커맨드라인 노출) |
| `schtasks.exe`가 없는 축소 설치본(Server Core 일부 구성) | `doctor`에서 `schtasks.exe` 존재 확인 | `windows` 크레이트로 Task Scheduler COM API(`ITaskService`) 직접 호출로 대체(`[확인 필요]`, 별도 후속 작업) |
| 스킵(overlap)을 exit 0으로 처리해 Task Scheduler 이력만 보면 "항상 성공"으로 보임 | 애플리케이션 로그(WARN) + 알림(log+event 기본)이 실제 가시성을 담당함을 운영 문서에 명시 | 필요 시 스킵 전용 exit code(예: 3)로 바꾸고 `RestartOnFailure`를 트리거별로 세분화하는 대안 검토(현재는 재시도 폭주 방지를 우선) |
| Windows Event Log에 메시지 DLL 미등록으로 Viewer에 디스클레이머 문구가 붙음 | 기능에는 무해, 원문 메시지는 그대로 함께 표시됨 | S01 `init`/설치 스크립트에 `EventMessageFile` 레지스트리 키 등록을 후속 폴리시 항목으로 추가(스코프 밖) |
| webhook URL(`enc:` 필드) 복호화 배선이 아직 S02와 완전히 연결되지 않음(`todo!()`) | `WebhookSink::from_enc_field`가 아직 자리표시자임을 명시, S02 완료 후 배선 마무리 | webhook 미설정 상태에서도 log+event 기본 알림은 항상 동작(FR7 채널 격리로 영향 없음) |
| 리댁션 정규식이 새로운 시크릿 포맷(예: 향후 추가되는 API 키 형식)을 놓칠 가능성 | 방어선 2(`RedactingWriter`)로 이중화, 코드 리뷰 체크리스트로 신규 시크릿 타입 추가 시 패턴 업데이트 의무화 | 패턴 세트만 교체, 인터페이스(`scrub()`) 불변 |
| `named-lock`(S08)의 Windows `Global\` 네임스페이스가 일부 제한된 세션에서 실패할 수 있음 | S08 문서와 동일 완화: 부분 UNIQUE 인덱스가 최종 방어선이므로 뮤텍스 실패가 곧 overlap 허용으로 이어지지 않음 | S08 Risk & Rollback 참고(중복 문서화 대신 참조) |

## Acceptance Criteria

- AC1: `restoreplane schedule --job X`가 XML을 생성해 Task Scheduler에 등록하고, 재조회 시
  `StartWhenAvailable=true`, `MultipleInstancesPolicy=IgnoreNew`, `RunLevel=HighestAvailable`,
  `RestartOnFailure`가 `[policy.retry]` 값과 일치함을 확인한다(T2~T4).
- AC2: 기본 로그온 방식이 S4U이며, 기본 등록 경로 어디에도 비밀번호가 커맨드라인 인자로 전달되지
  않는다(코드 리뷰 grep, FR2).
- AC3: 동일 job의 겹치는 실행 시도 중 정확히 하나만 진행되고, 스킵된 쪽은 exit 0으로 종료해
  `RestartOnFailure`를 유발하지 않으며, 스킵이 로그(WARN)와 알림 채널에 별도로 기록되어 가시성을
  확보한다(T6~T8).
- AC4: 로그 파일/Windows Event Log/webhook 알림 어디에도 DEK/KEK/복구코드/`enc:` 필드/DSN 원문이
  등장하지 않는다(T9~T12, M6).
- AC5: 로그+이벤트 알림은 `[notify]` 설정과 무관하게 항상 동작하고, webhook 전송 실패가 이 둘을
  방해하지 않는다(T18).
- AC6: 매 run 종료 시 RPO notice가 출력되고, 목표 간격(스케줄 기반)을 1.5배 초과하면 경고로
  표시된다(T13~T15).
- AC7: `restoreplane schedule --job X --remove`가 태스크를 제거하고, 재등록 시도 시 파라미터가 동일하면
  "변경 없음"으로 판정되어 불필요한 재등록을 반복하지 않는다(T16~T17).
- AC8: 이 섹션은 `windows-service`를 사용하지 않으며, overlap guard 로직을 새로 구현하지 않고 S08의
  `try_acquire_run`/`RunGuard`를 그대로 재사용한다(코드 리뷰로 확인, 중복 구현 없음).

## Files

| 파일 | 내용 |
|---|---|
| `crates/restoreplane-scheduler/Cargo.toml` | 신규 크레이트 매니페스트 |
| `crates/restoreplane-scheduler/src/lib.rs` | `SchedulerContext`, `init_scheduler_tracing`, 공개 API 재노출 |
| `crates/restoreplane-scheduler/src/cron.rs` | `ScheduleSpec`/`Weekday`, `"M H * * [*|D]"` 서브셋 파서 |
| `crates/restoreplane-scheduler/src/xml.rs` | `TaskXmlParams`, `render_task_xml`, `escape_xml`, `LogonType` |
| `crates/restoreplane-scheduler/src/register.rs` | schtasks `/Create /Query /Delete` 호출, 멱등 등록, UTF-16LE 쓰기 |
| `crates/restoreplane-scheduler/src/overlap.rs` | S08 `RunGuard` 어댑터, exit code 상수/정책 |
| `crates/restoreplane-scheduler/src/notify.rs` | `NotificationSink`, `CompositeSink`, Log/EventLog/Webhook/EmailPlaceholder Sink |
| `crates/restoreplane-scheduler/src/redact.rs` | `scrub()`, `RedactingWriter` |
| `crates/restoreplane-scheduler/src/rpo.rs` | `RpoNotice`, `compute_rpo_notice`, `print_and_log` |
| `crates/restoreplane-scheduler/src/entrypoint.rs` | `run_scheduled_job` — 전 모듈 배선, `restoreplane-cli`의 `run` 핸들러가 호출 |
| `crates/restoreplane-scheduler/tests/cron_parser.rs` | T1 |
| `crates/restoreplane-scheduler/tests/xml_roundtrip.rs` | T2~T3 |
| `crates/restoreplane-scheduler/tests/redact.rs` | T9~T12 |
| `crates/restoreplane-scheduler/tests/overlap_skip_exit_code.rs` | T6~T8 |
| `crates/restoreplane-scheduler/tests/rpo_notice.rs` | T13~T15 |
| `crates/restoreplane-scheduler/tests/register_windows.rs` | T4~T5, T16~T17 (`#[cfg(windows)]`, Windows CI/수동 QA) |

`#tags: scheduler, schtasks, task-scheduler-xml, overlap-guard, startwhenavailable, s4u, redaction, windows-event-log, rpo, rust`
