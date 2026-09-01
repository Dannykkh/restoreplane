// src/register.rs — schtasks /Create /Query /Delete 프로세스 호출(FR1/FR2/FR4/FR5).
use crate::redact::scrub;
use crate::xml::{render_task_xml, TaskXmlParams};
use regex::Regex;
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

pub fn task_name(job: &str) -> String {
    format!("{TASK_FOLDER}\\{job}")
}

/// UTF-8 XML 문자열을 Task Scheduler가 기대하는 UTF-16LE + BOM으로 파일에 쓴다(gotcha, §10.2).
async fn write_task_xml_file(path: &Path, xml_utf8: &str) -> Result<(), SchedulerError> {
    let mut bytes: Vec<u8> = vec![0xFF, 0xFE]; // UTF-16LE BOM
    for unit in xml_utf8.encode_utf16() {
        bytes.extend_from_slice(&unit.to_le_bytes());
    }
    tokio::fs::write(path, bytes)
        .await
        .map_err(SchedulerError::Io)
}

/// 태스크를 등록(또는 갱신)한다. FR2: LogonType=S4U 기본 경로는 /RP(비밀번호)를 절대 넘기지 않는다 —
/// 저장할 비밀번호 자체가 없다.
pub async fn register_task(
    params: &TaskXmlParams,
    xml_dir: &Path,
) -> Result<PathBuf, SchedulerError> {
    let xml = render_task_xml(params);
    let xml_path = xml_dir.join(format!("{}.xml", sanitize_filename(&params.job_name)));
    write_task_xml_file(&xml_path, &xml).await?;

    let tn = task_name(&params.job_name);
    let mut cmd = Command::new("schtasks");
    cmd.arg("/Create")
        .arg("/XML")
        .arg(&xml_path)
        .arg("/TN")
        .arg(&tn)
        // /RU만으로 계정이 지정되고, S4U는 계정의 배치 로그온 권한만으로 동작한다.
        .arg("/RU")
        .arg(&params.service_account)
        .arg("/F");

    let output = cmd.output().await.map_err(SchedulerError::Spawn)?;
    if !output.status.success() {
        let stderr = scrub(&String::from_utf8_lossy(&output.stderr));
        return Err(SchedulerError::SchtasksFailed {
            task_name: tn,
            stderr,
        });
    }
    tracing::info!(job = %params.job_name, xml = %xml_path.display(), "Task Scheduler 등록 완료");
    Ok(xml_path)
}

#[derive(Debug, PartialEq, Eq)]
pub enum RegisterOutcome {
    Registered,
    Unchanged,
}

/// 등록 전 현재 상태와 비교해 실제로 바뀐 경우만 재등록한다(FR4, 멱등).
pub async fn register_idempotent(
    params: &TaskXmlParams,
    xml_dir: &Path,
) -> Result<RegisterOutcome, SchedulerError> {
    let new_xml = render_task_xml(params);
    if let Ok(Some(existing_xml)) = query_task_xml(&params.job_name).await {
        // Task Scheduler는 /Query 시 자동 필드 추가/재정렬/인코딩 변환을 하므로 raw XML 동등비교는
        // 신뢰할 수 없다. 우리가 제어하는 의미 필드만 뽑아 서명(signature)으로 비교한다(FR4).
        if task_signature(&existing_xml) == task_signature(&new_xml) {
            tracing::info!(job = %params.job_name, "스케줄 변경 없음 — 재등록 스킵");
            return Ok(RegisterOutcome::Unchanged);
        }
    }
    register_task(params, xml_dir).await?;
    Ok(RegisterOutcome::Registered)
}

/// `/Query /XML`로 현재 등록된 XML을 읽어온다. 태스크가 없으면 Ok(None).
pub async fn query_task_xml(job: &str) -> Result<Option<String>, SchedulerError> {
    let tn = task_name(job);
    let output = Command::new("schtasks")
        .arg("/Query")
        .arg("/TN")
        .arg(&tn)
        .arg("/XML")
        .output()
        .await
        .map_err(SchedulerError::Spawn)?;
    if !output.status.success() {
        return Ok(None); // 존재하지 않음 — schtasks가 "찾을 수 없음" 류 에러를 stderr로 반환
    }
    Ok(Some(String::from_utf8_lossy(&output.stdout).to_string()))
}

pub async fn remove_task(job: &str) -> Result<(), SchedulerError> {
    let tn = task_name(job);
    let output = Command::new("schtasks")
        .arg("/Delete")
        .arg("/TN")
        .arg(&tn)
        .arg("/F")
        .output()
        .await
        .map_err(SchedulerError::Spawn)?;
    if !output.status.success() {
        let stderr = scrub(&String::from_utf8_lossy(&output.stderr));
        return Err(SchedulerError::SchtasksFailed {
            task_name: tn,
            stderr,
        });
    }
    tracing::info!(job, "Task Scheduler 태스크 제거 완료");
    Ok(())
}

/// 우리가 제어하는 의미 필드만 뽑아 재등록 필요 여부를 판정하기 위한 서명. Task Scheduler가 붙이는
/// 자동 필드(SecurityDescriptor, IdleSettings 기본값 등)와 재정렬/공백을 무시한다.
fn task_signature(xml: &str) -> String {
    let grab = |pat: &str| -> String {
        Regex::new(pat)
            .ok()
            .and_then(|r| r.captures(xml))
            .and_then(|c| c.get(1))
            .map(|m| m.as_str().trim().to_string())
            .unwrap_or_default()
    };
    // 인수 비교는 인용부호 인코딩 차이(&quot; vs ")를 정규화한다.
    let args = grab(r"(?s)<Arguments>(.*?)</Arguments>")
        .replace("&quot;", "")
        .replace('"', "");
    // StartBoundary는 날짜가 아니라 시각(hh:mm:ss)만 비교(반복 앵커).
    let start_time = grab(r"<StartBoundary>[0-9-]+T([0-9:]+)");
    let schedule = if xml.contains("<ScheduleByDay") {
        "day".to_string()
    } else {
        format!("week:{}", grab(r"(?s)<DaysOfWeek>\s*<([A-Za-z]+)"))
    };
    [
        grab(r"(?s)<Command>(.*?)</Command>").to_lowercase(),
        args,
        grab(r"<LogonType>(.*?)</LogonType>"),
        grab(r"<RunLevel>(.*?)</RunLevel>"),
        grab(r"<MultipleInstancesPolicy>(.*?)</MultipleInstancesPolicy>"),
        grab(r"<StartWhenAvailable>(.*?)</StartWhenAvailable>"),
        grab(r"(?s)<RestartOnFailure>.*?<Interval>(.*?)</Interval>"),
        grab(r"(?s)<RestartOnFailure>.*?<Count>(.*?)</Count>"),
        start_time,
        schedule,
    ]
    .join("|")
}

fn sanitize_filename(job: &str) -> String {
    job.chars()
        .map(|c| {
            if c.is_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect()
}
