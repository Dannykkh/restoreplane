//! 백업 실행/검증 사이드카 — 완성된 restoreplane.exe를 자식 프로세스로 띄우고 stdout/stderr를
//! 줄 단위로 프론트에 이벤트("run:log"/"run:done")로 흘린다.
//!
//! 왜 사이드카인가: run/verify는 덤프 도구 호출·스트리밍 암호화·팬아웃 등 완성된 파이프라인을
//! 그대로 재사용해야 한다(재구현 금지). config 편집처럼 순수 라이브러리 호출이 아니라 장시간
//! 실행+진행 로그가 필요하므로 인프로세스가 아니라 사이드카 스트리밍이 맞다.
//!
//! 검증 한계: 실제 백업은 라이브 DB/keystore/목적지가 필요하다. --dry-run은 keystore 언락 이전에
//! 계획만 출력하고 종료하므로 인프라 없이도 사이드카 배선을 검증할 수 있다(run.rs의 dry_run 분기).

use std::io::{BufRead, BufReader};
use std::path::PathBuf;
use std::process::{Command, Stdio};

use serde::Serialize;
use tauri::{AppHandle, Emitter};

use crate::paths::config_path;

/// 동봉 배포에서 restoreplane.exe는 콘솔 실행파일과 같은 디렉토리에 있다(dev: target/<profile>).
fn restoreplane_bin() -> PathBuf {
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            let cand = dir.join("restoreplane.exe");
            if cand.exists() {
                return cand;
            }
        }
    }
    PathBuf::from("restoreplane.exe") // PATH 폴백
}

#[derive(Clone, Serialize)]
struct LogLine {
    line: String,
    stream: &'static str, // "stdout" | "stderr"
}

#[derive(Clone, Serialize)]
struct RunDone {
    code: Option<i32>,
    ok: bool,
}

/// restoreplane 서브커맨드를 자식 프로세스로 띄우고 stdout/stderr를 run:log 이벤트로 스트리밍한다.
/// 커맨드는 즉시 반환하고(비블로킹), 종료 시 run:done을 방출한다. run/verify/restore 공용.
fn spawn_stream(app: AppHandle, args: Vec<String>) -> Result<(), String> {
    let bin = restoreplane_bin();
    let cfg = config_path();

    let mut cmd = Command::new(&bin);
    cmd.arg("--config").arg(&cfg);
    cmd.args(&args);
    cmd.stdout(Stdio::piped()).stderr(Stdio::piped());

    let mut child = cmd
        .spawn()
        .map_err(|e| format!("restoreplane 실행 실패({}): {e}", bin.display()))?;

    let stdout = child.stdout.take();
    let stderr = child.stderr.take();

    let a1 = app.clone();
    let h_out = std::thread::spawn(move || {
        if let Some(out) = stdout {
            for line in BufReader::new(out).lines().map_while(Result::ok) {
                let _ = a1.emit(
                    "run:log",
                    LogLine {
                        line,
                        stream: "stdout",
                    },
                );
            }
        }
    });
    let a2 = app.clone();
    let h_err = std::thread::spawn(move || {
        if let Some(err) = stderr {
            for line in BufReader::new(err).lines().map_while(Result::ok) {
                let _ = a2.emit(
                    "run:log",
                    LogLine {
                        line,
                        stream: "stderr",
                    },
                );
            }
        }
    });

    std::thread::spawn(move || {
        let _ = h_out.join();
        let _ = h_err.join();
        let (code, ok) = match child.wait() {
            Ok(s) => (s.code(), s.success()),
            Err(_) => (None, false),
        };
        let _ = app.emit("run:done", RunDone { code, ok });
    });

    Ok(())
}

/// 백업 잡 실행. dry_run=true면 계획만(인프라 불필요). run:log/run:done 구독.
#[tauri::command]
pub fn run_job(app: AppHandle, source: String, dry_run: bool) -> Result<(), String> {
    let mut args = vec!["run".into(), "--db".into(), source];
    if dry_run {
        args.push("--dry-run".into());
    }
    spawn_stream(app, args)
}

fn restore_args(
    backup: &str,
    from_destination: &str,
    to: Option<&str>,
    decrypt_only: bool,
    with_recovery_code: bool,
) -> Result<Vec<String>, String> {
    if !decrypt_only {
        return Err(
            "RESTORE_DECRYPT_ONLY_REQUIRED: 실제 DB 대상 복원은 아직 지원하지 않습니다.".into(),
        );
    }
    let output_path = to
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| {
            "RESTORE_OUTPUT_REQUIRED: 평문 덤프를 기록할 산출 경로가 필요합니다.".to_string()
        })?;
    let mut args = vec![
        "restore".into(),
        "--backup".into(),
        backup.into(),
        "--from-destination".into(),
        from_destination.into(),
        "--to".into(),
        output_path.into(),
        "--decrypt-only".into(),
    ];
    if with_recovery_code {
        args.push("--with-recovery-code".into());
    }
    Ok(args)
}

/// 백업 복원. 현재 공개 경로는 서명 게이트 뒤 평문 덤프를 만드는 decrypt-only만 허용한다.
/// with_recovery_code면 RESTOREPLANE_RECOVERY_CODE 무인 경로를 쓴다(복원 측 CLI 규약).
#[tauri::command]
pub fn restore_backup(
    app: AppHandle,
    backup: String,
    from_destination: String,
    to: Option<String>,
    decrypt_only: bool,
    with_recovery_code: bool,
) -> Result<(), String> {
    let args = restore_args(
        &backup,
        &from_destination,
        to.as_deref(),
        decrypt_only,
        with_recovery_code,
    )?;
    spawn_stream(app, args)
}

/// Windows Task Scheduler 등록/해제(schtasks). 관리자 권한·유효 config가 필요할 수 있다.
#[tauri::command]
pub fn register_schedule(app: AppHandle, job: String, remove: bool) -> Result<(), String> {
    let mut args = vec!["schedule".into(), "--job".into(), job];
    if remove {
        args.push("--remove".into());
    }
    spawn_stream(app, args)
}

#[cfg(test)]
mod tests {
    use super::restore_args;

    #[test]
    fn restore_sidecar_contract_allows_only_decrypt_only_with_output_path() {
        let non_decrypt = restore_args("backup-1", "s3-primary", Some("dump.sql"), false, false)
            .expect_err("database-target restore must not be exposed before its safety gate");
        assert!(non_decrypt.contains("RESTORE_DECRYPT_ONLY_REQUIRED"));

        let missing_output = restore_args("backup-1", "s3-primary", None, true, false)
            .expect_err("decrypt-only restore requires an explicit output path");
        assert!(missing_output.contains("RESTORE_OUTPUT_REQUIRED"));

        let args = restore_args("backup-1", "s3-primary", Some("dump.sql"), true, true).unwrap();
        assert!(args.iter().any(|arg| arg == "--decrypt-only"));
        assert!(args.iter().any(|arg| arg == "--with-recovery-code"));
        assert!(args.windows(2).any(|pair| pair == ["--to", "dump.sql"]));
    }
}
