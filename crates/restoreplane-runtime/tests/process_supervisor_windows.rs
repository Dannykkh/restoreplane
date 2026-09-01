#![cfg(windows)]

use std::fs;
use std::io::{Read, Write};
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use restoreplane_runtime::{
    hash_executable, LaunchSpec, ProcessSupervisor, MAX_TRUSTED_DEPENDENCIES,
    MAX_TRUSTED_DEPENDENCY_PATH_UNITS,
};
use tempfile::TempDir;
use windows::core::PCWSTR;
use windows::Win32::Foundation::{CloseHandle, WAIT_OBJECT_0};
use windows::Win32::System::JobObjects::OpenJobObjectW;
use windows::Win32::System::SystemServices::JOB_OBJECT_QUERY;
use windows::Win32::System::Threading::{OpenProcess, WaitForSingleObject, PROCESS_SYNCHRONIZE};

const MODE_ENV: &str = "RESTOREPLANE_RUNTIME_TEST_MODE";
const CHILD_PID_PATH_ENV: &str = "RESTOREPLANE_RUNTIME_CHILD_PID_PATH";
const FLOOD_BYTES_ENV: &str = "RESTOREPLANE_RUNTIME_FLOOD_BYTES";
const SECRET_ENV: &str = "RESTOREPLANE_RUNTIME_SECRET";
const OUTPUT_SECRET: &str = "cycle52-secret-canary";

#[test]
fn suspended_launch_assigns_the_entire_process_tree_to_a_kill_on_close_job() {
    let temp = TempDir::new().expect("temp directory");
    let executable = std::env::current_exe().expect("test executable");
    let expected_sha256 = hash_executable(&executable).expect("helper hash");
    let child_pid_path = temp.path().join("child.pid");
    let spec = LaunchSpec::new(executable, temp.path().to_path_buf(), expected_sha256)
        .expect("launch spec")
        .with_args(["--exact", "helper_process", "--nocapture"])
        .expect("helper args")
        .with_environment([
            (MODE_ENV, "tree-parent"),
            (
                CHILD_PID_PATH_ENV,
                child_pid_path.to_str().expect("UTF-8 temp path"),
            ),
        ])
        .expect("restricted environment");

    let mut process = ProcessSupervisor::launch(&spec).expect("supervised launch");
    assert!(process.id() > 0);
    assert!(process.start_time_100ns() > 0);
    assert!(
        process.job_key().starts_with("Local\\RestorePlane.Job."),
        "persisted job key must be the actual reopenable kernel object name"
    );
    let job_name = wide_null(process.job_key());
    let reopened =
        unsafe { OpenJobObjectW(JOB_OBJECT_QUERY, false, PCWSTR::from_raw(job_name.as_ptr())) }
            .expect("reopen named job while supervisor owns it");
    let _ = unsafe { CloseHandle(reopened) };
    let child_pid = wait_for_child_pid(&child_pid_path);
    let child_handle =
        unsafe { OpenProcess(PROCESS_SYNCHRONIZE, false, child_pid) }.expect("open grandchild");

    let exit = process.terminate().expect("terminate process tree");
    assert_eq!(exit.process_id(), process.id());
    assert_eq!(
        unsafe { WaitForSingleObject(child_handle, 5_000) },
        WAIT_OBJECT_0,
        "grandchild must be reaped with the job"
    );
    let _ = unsafe { CloseHandle(child_handle) };
}

#[test]
fn relative_path_and_hash_mismatch_are_rejected_before_process_creation() {
    let temp = TempDir::new().expect("temp directory");
    let relative = LaunchSpec::new("helper.exe".into(), temp.path().to_path_buf(), [0_u8; 32])
        .expect_err("relative executable must fail");
    assert_eq!(relative.code(), "PROCESS_SPEC_INVALID");

    let executable = std::env::current_exe().expect("test executable");
    let mut wrong_hash = hash_executable(&executable).expect("helper hash");
    wrong_hash[0] ^= 0xff;
    let marker = temp.path().join("unexpected.marker");
    let spec = LaunchSpec::new(executable, temp.path().to_path_buf(), wrong_hash)
        .expect("structurally valid spec")
        .with_args(["--exact", "helper_process", "--nocapture"])
        .expect("helper args")
        .with_environment([
            (MODE_ENV, "write-marker"),
            (
                CHILD_PID_PATH_ENV,
                marker.to_str().expect("UTF-8 marker path"),
            ),
        ])
        .expect("restricted environment");
    let error = ProcessSupervisor::launch(&spec).expect_err("hash mismatch must fail");
    assert_eq!(error.code(), "PROCESS_EXECUTABLE_NOT_TRUSTED");
    assert!(!marker.exists(), "untrusted executable must never start");

    let executable = std::env::current_exe().expect("test executable");
    let expected_sha256 = hash_executable(&executable).expect("helper hash");
    let duplicate_environment =
        LaunchSpec::new(executable, temp.path().to_path_buf(), expected_sha256)
            .expect("launch spec")
            .with_environment([("Path", "first"), ("PATH", "second")])
            .expect_err("Windows environment names are case-insensitive");
    assert_eq!(duplicate_environment.code(), "PROCESS_SPEC_INVALID");
}

#[test]
fn trusted_dependency_hash_mismatch_is_rejected_before_process_creation() {
    let temp = TempDir::new().expect("temp directory");
    let executable = std::env::current_exe().expect("test executable");
    let expected_sha256 = hash_executable(&executable).expect("helper hash");
    let dependency = temp.path().join("provider-runtime.dll");
    fs::write(&dependency, b"certified dependency bytes").expect("write dependency fixture");
    let mut wrong_dependency_sha256 = hash_executable(&dependency).expect("dependency hash");
    wrong_dependency_sha256[0] ^= 0xff;
    let marker = temp.path().join("unexpected-dependency-launch.marker");
    let spec = LaunchSpec::new(executable, temp.path().to_path_buf(), expected_sha256)
        .expect("launch spec")
        .with_trusted_dependencies([(dependency, wrong_dependency_sha256)])
        .expect("structurally valid dependency")
        .with_args(["--exact", "helper_process", "--nocapture"])
        .expect("helper args")
        .with_environment([
            (MODE_ENV, "write-marker"),
            (
                CHILD_PID_PATH_ENV,
                marker.to_str().expect("UTF-8 marker path"),
            ),
        ])
        .expect("restricted environment");

    let error = ProcessSupervisor::launch(&spec).expect_err("dependency mismatch must fail");
    assert_eq!(error.code(), "PROCESS_DEPENDENCY_NOT_TRUSTED");
    assert!(!marker.exists(), "untrusted dependency must block launch");
}

#[test]
fn supervised_child_holds_trusted_dependency_read_lock_until_drop() {
    let temp = TempDir::new().expect("temp directory");
    let executable = std::env::current_exe().expect("test executable");
    let expected_sha256 = hash_executable(&executable).expect("helper hash");
    let dependency = temp.path().join("delay-loaded-provider.dll");
    fs::write(&dependency, b"locked dependency bytes").expect("write dependency fixture");
    let dependency_sha256 = hash_executable(&dependency).expect("dependency hash");
    let spec = LaunchSpec::new(executable, temp.path().to_path_buf(), expected_sha256)
        .expect("launch spec")
        .with_trusted_dependencies([(dependency.clone(), dependency_sha256)])
        .expect("trusted dependency")
        .with_args(["--exact", "helper_process", "--nocapture"])
        .expect("helper args")
        .with_environment([(MODE_ENV, "tree-child")])
        .expect("restricted environment");

    let mut process = ProcessSupervisor::launch(&spec).expect("supervised launch");
    assert!(
        process
            .wait_timeout(Duration::from_millis(50))
            .expect("observe dependency-lock child")
            .is_none(),
        "dependency-lock helper must still be running"
    );
    assert!(
        fs::remove_file(&dependency).is_err(),
        "FILE_SHARE_READ-only dependency handle must deny delete while child is supervised"
    );
    assert!(
        fs::write(&dependency, b"replacement dependency bytes").is_err(),
        "FILE_SHARE_READ-only dependency handle must deny overwrite while child is supervised"
    );
    let renamed = temp.path().join("replaced-provider.dll");
    assert!(
        fs::rename(&dependency, &renamed).is_err(),
        "FILE_SHARE_READ-only dependency handle must deny replacement while child is supervised"
    );

    drop(process);
    fs::remove_file(&dependency).expect("dependency lock releases after supervised child drop");
}

#[test]
fn streaming_child_holds_trusted_dependency_read_lock_until_drop() {
    let temp = TempDir::new().expect("temp directory");
    let executable = std::env::current_exe().expect("test executable");
    let expected_sha256 = hash_executable(&executable).expect("helper hash");
    let dependency = temp.path().join("streaming-delay-loaded-provider.dll");
    fs::write(&dependency, b"streaming dependency bytes").expect("write dependency fixture");
    let dependency_sha256 = hash_executable(&dependency).expect("dependency hash");
    let spec = LaunchSpec::new(executable, temp.path().to_path_buf(), expected_sha256)
        .expect("launch spec")
        .with_trusted_dependencies([(dependency.clone(), dependency_sha256)])
        .expect("trusted dependency")
        .with_args(["--exact", "helper_process", "--nocapture"])
        .expect("helper args")
        .with_environment([(MODE_ENV, "tree-child")])
        .expect("restricted environment");

    let mut process =
        ProcessSupervisor::launch_streaming(&spec).expect("streaming supervised launch");
    assert!(
        process
            .wait_timeout(Duration::from_millis(50))
            .expect("observe streaming dependency-lock child")
            .is_none(),
        "streaming dependency-lock helper must still be running"
    );
    assert!(
        fs::remove_file(&dependency).is_err(),
        "streaming supervision must retain the dependency lock"
    );
    drop(process);
    fs::remove_file(&dependency).expect("streaming dependency lock releases after drop");
}

#[test]
fn completed_child_retains_dependency_lock_until_supervisor_drop() {
    let temp = TempDir::new().expect("temp directory");
    let executable = std::env::current_exe().expect("test executable");
    let expected_sha256 = hash_executable(&executable).expect("helper hash");
    let dependency = temp.path().join("completed-provider.dll");
    fs::write(&dependency, b"completed dependency bytes").expect("write dependency fixture");
    let dependency_sha256 = hash_executable(&dependency).expect("dependency hash");
    let spec = LaunchSpec::new(executable, temp.path().to_path_buf(), expected_sha256)
        .expect("launch spec")
        .with_trusted_dependencies([(dependency.clone(), dependency_sha256)])
        .expect("trusted dependency")
        .with_args(["--exact", "helper_process", "--nocapture"])
        .expect("helper args")
        .with_environment([(MODE_ENV, "stream-once")])
        .expect("restricted environment");

    let mut process = ProcessSupervisor::launch(&spec).expect("supervised launch");
    let exit = process
        .wait_timeout(Duration::from_secs(5))
        .expect("wait for completed child")
        .expect("child exits");
    assert_eq!(exit.exit_code(), 0);
    assert!(
        fs::remove_file(&dependency).is_err(),
        "completed supervisor must retain locks until its owned job lifetime ends on drop"
    );
    drop(process);
    fs::remove_file(&dependency).expect("completed supervisor releases lock on drop");
}

#[test]
fn supervised_child_holds_dependency_parent_path_binding_until_drop() {
    let temp = TempDir::new().expect("temp directory");
    let provider_directory = temp.path().join("provider-directory");
    fs::create_dir(&provider_directory).expect("create provider directory");
    let dependency = provider_directory.join("provider.dll");
    fs::write(&dependency, b"path-bound dependency bytes").expect("write dependency fixture");
    let dependency_sha256 = hash_executable(&dependency).expect("dependency hash");
    let executable = std::env::current_exe().expect("test executable");
    let expected_sha256 = hash_executable(&executable).expect("helper hash");
    let spec = LaunchSpec::new(executable, temp.path().to_path_buf(), expected_sha256)
        .expect("launch spec")
        .with_trusted_dependencies([(dependency, dependency_sha256)])
        .expect("trusted dependency")
        .with_args(["--exact", "helper_process", "--nocapture"])
        .expect("helper args")
        .with_environment([(MODE_ENV, "tree-child")])
        .expect("restricted environment");

    let mut process = ProcessSupervisor::launch(&spec).expect("supervised launch");
    assert!(
        process
            .wait_timeout(Duration::from_millis(50))
            .expect("observe path-binding child")
            .is_none(),
        "path-binding helper must still be running"
    );
    let moved_directory = temp.path().join("moved-provider-directory");
    assert!(
        fs::rename(&provider_directory, &moved_directory).is_err(),
        "supervisor must deny ancestor rename that would rebind dependency paths"
    );
    drop(process);
    fs::rename(&provider_directory, &moved_directory)
        .expect("dependency ancestor binding releases on drop");
}

#[test]
fn trusted_dependency_spec_rejects_relative_and_duplicate_paths() {
    let temp = TempDir::new().expect("temp directory");
    let executable = std::env::current_exe().expect("test executable");
    let expected_sha256 = hash_executable(&executable).expect("helper hash");
    let dependency = temp.path().join("duplicate-provider.dll");
    fs::write(&dependency, b"dependency bytes").expect("write dependency fixture");
    let dependency_sha256 = hash_executable(&dependency).expect("dependency hash");
    let base = LaunchSpec::new(
        executable.clone(),
        temp.path().to_path_buf(),
        expected_sha256,
    )
    .expect("launch spec");

    let relative = base
        .clone()
        .with_trusted_dependencies([("provider.dll", dependency_sha256)])
        .expect_err("relative dependency path must fail");
    assert_eq!(relative.code(), "PROCESS_SPEC_INVALID");

    let duplicate = base
        .with_trusted_dependencies([
            (dependency.clone(), dependency_sha256),
            (dependency, dependency_sha256),
        ])
        .expect_err("duplicate dependency path must fail");
    assert_eq!(duplicate.code(), "PROCESS_SPEC_INVALID");
}

#[test]
fn trusted_dependency_spec_enforces_the_public_count_bound() {
    assert_eq!(MAX_TRUSTED_DEPENDENCIES, 512);
    let temp = TempDir::new().expect("temp directory");
    let executable = std::env::current_exe().expect("test executable");
    let expected_sha256 = hash_executable(&executable).expect("helper hash");
    let mut dependencies = Vec::new();
    for index in 0..=MAX_TRUSTED_DEPENDENCIES {
        let path = temp.path().join(format!("provider-{index}.dll"));
        fs::write(&path, index.to_le_bytes()).expect("write bounded dependency fixture");
        let digest = hash_executable(&path).expect("dependency hash");
        dependencies.push((path, digest));
    }
    let error = LaunchSpec::new(executable, temp.path().to_path_buf(), expected_sha256)
        .expect("launch spec")
        .with_trusted_dependencies(dependencies)
        .expect_err("dependency count above the public bound must fail");
    assert_eq!(error.code(), "PROCESS_SPEC_INVALID");
}

#[test]
fn trusted_dependency_spec_enforces_the_public_path_bound() {
    assert_eq!(MAX_TRUSTED_DEPENDENCY_PATH_UNITS, 32_767);
    let temp = TempDir::new().expect("temp directory");
    let executable = std::env::current_exe().expect("test executable");
    let expected_sha256 = hash_executable(&executable).expect("helper hash");
    let oversized = std::path::PathBuf::from(format!(
        "C:\\{}",
        "x".repeat(MAX_TRUSTED_DEPENDENCY_PATH_UNITS)
    ));

    let error = LaunchSpec::new(executable, temp.path().to_path_buf(), expected_sha256)
        .expect("launch spec")
        .with_trusted_dependencies([(oversized, [0_u8; 32])])
        .expect_err("dependency path above the public UTF-16-unit bound must fail");
    assert_eq!(error.code(), "PROCESS_SPEC_INVALID");
}

#[test]
fn trusted_dependency_spec_rejects_reparse_path_components() {
    let temp = TempDir::new().expect("temp directory");
    let real_directory = temp.path().join("real-provider-directory");
    let linked_directory = temp.path().join("linked-provider-directory");
    fs::create_dir(&real_directory).expect("create real dependency directory");
    if let Err(error) = std::os::windows::fs::symlink_dir(&real_directory, &linked_directory) {
        if error.raw_os_error() == Some(1314) {
            eprintln!("symbolic-link privilege unavailable; reparse assertion skipped");
            return;
        }
        panic!("create dependency directory symlink: {error}");
    }
    let real_dependency = real_directory.join("provider.dll");
    fs::write(&real_dependency, b"dependency behind reparse component")
        .expect("write dependency fixture");
    let dependency_sha256 = hash_executable(&real_dependency).expect("dependency hash");
    let executable = std::env::current_exe().expect("test executable");
    let expected_sha256 = hash_executable(&executable).expect("helper hash");

    let error = LaunchSpec::new(executable, temp.path().to_path_buf(), expected_sha256)
        .expect("launch spec")
        .with_trusted_dependencies([(linked_directory.join("provider.dll"), dependency_sha256)])
        .expect_err("dependency path containing a reparse component must fail");
    assert_eq!(error.code(), "PROCESS_SPEC_INVALID");
}

#[test]
fn stdout_and_stderr_flood_are_drained_concurrently_into_redacted_bounded_tails() {
    let temp = TempDir::new().expect("temp directory");
    let executable = std::env::current_exe().expect("test executable");
    let expected_sha256 = hash_executable(&executable).expect("helper hash");
    let spec = LaunchSpec::new(executable, temp.path().to_path_buf(), expected_sha256)
        .expect("launch spec")
        .with_args(["--exact", "helper_process", "--nocapture"])
        .expect("helper args")
        .with_environment([
            (MODE_ENV.to_owned(), "flood".to_owned()),
            (FLOOD_BYTES_ENV.to_owned(), (4 * 1024 * 1024).to_string()),
            (SECRET_ENV.to_owned(), OUTPUT_SECRET.to_owned()),
        ])
        .expect("restricted environment")
        .with_output_tail_bytes(4 * 1024)
        .expect("bounded output")
        .with_redacted_output_literals([OUTPUT_SECRET.as_bytes()])
        .expect("redaction");

    let mut process = ProcessSupervisor::launch(&spec).expect("supervised flood launch");
    let exit = process
        .wait_timeout(Duration::from_secs(10))
        .expect("wait")
        .expect("flood helper exits without deadlock");
    assert_eq!(exit.exit_code(), 0);
    for stream in [exit.stdout(), exit.stderr()] {
        assert!(stream.total_bytes() >= 4 * 1024 * 1024);
        assert!(stream.truncated());
        assert!(stream.tail().len() <= 4 * 1024);
        assert!(
            !stream
                .tail()
                .windows(OUTPUT_SECRET.len())
                .any(|window| window == OUTPUT_SECRET.as_bytes()),
            "secret literal must be redacted before retention"
        );
    }
}

#[test]
fn timeout_observation_does_not_drop_ownership_and_explicit_cancel_reaps_the_process() {
    let temp = TempDir::new().expect("temp directory");
    let executable = std::env::current_exe().expect("test executable");
    let expected_sha256 = hash_executable(&executable).expect("helper hash");
    let spec = LaunchSpec::new(executable, temp.path().to_path_buf(), expected_sha256)
        .expect("launch spec")
        .with_args(["--exact", "helper_process", "--nocapture"])
        .expect("helper args")
        .with_environment([(MODE_ENV, "tree-child")])
        .expect("restricted environment");
    let mut process = ProcessSupervisor::launch(&spec).expect("supervised timeout launch");

    assert!(
        process
            .wait_timeout(Duration::from_millis(50))
            .expect("timeout observation")
            .is_none(),
        "timeout must preserve process ownership"
    );
    let exit = process.terminate().expect("cancel and reap");
    assert_eq!(exit.process_id(), process.id());
}

#[test]
fn streaming_launch_exposes_pipes_without_giving_up_the_named_job_fence() {
    let temp = TempDir::new().expect("temp directory");
    let executable = std::env::current_exe().expect("test executable");
    let expected_sha256 = hash_executable(&executable).expect("helper hash");
    let spec = LaunchSpec::new(executable, temp.path().to_path_buf(), expected_sha256)
        .expect("launch spec")
        .with_args(["--exact", "helper_process", "--nocapture"])
        .expect("helper args")
        .with_environment([(MODE_ENV, "stream-once")])
        .expect("restricted environment");

    let mut process =
        ProcessSupervisor::launch_streaming(&spec).expect("supervised streaming launch");
    assert!(process.start_time_ms().expect("Unix process start time") > 0);
    assert_eq!(process.executable_path(), spec.executable());
    assert_eq!(process.executable_sha256(), spec.expected_sha256());
    let job_name = wide_null(process.job_key());
    let reopened =
        unsafe { OpenJobObjectW(JOB_OBJECT_QUERY, false, PCWSTR::from_raw(job_name.as_ptr())) }
            .expect("reopen named job while streaming supervisor owns it");
    let _ = unsafe { CloseHandle(reopened) };

    let mut stdout = process.take_stdout().expect("take stdout pipe");
    let mut stderr = process.take_stderr().expect("take stderr pipe");
    let mut stdout_text = String::new();
    let mut stderr_text = String::new();
    stdout
        .read_to_string(&mut stdout_text)
        .expect("read stdout");
    stderr
        .read_to_string(&mut stderr_text)
        .expect("read stderr");
    let exit = process
        .wait_timeout(Duration::from_secs(5))
        .expect("wait")
        .expect("stream helper exits");

    assert!(stdout_text.contains("stream-stdout"));
    assert!(stderr_text.contains("stream-stderr"));
    assert_eq!(exit.exit_code(), 0);
    assert_eq!(exit.process_id(), process.id());
}

#[test]
fn helper_process() {
    match std::env::var(MODE_ENV).as_deref() {
        Ok("tree-parent") => {
            let child_pid_path = std::env::var(CHILD_PID_PATH_ENV).expect("child PID path");
            let mut child = Command::new(std::env::current_exe().expect("current executable"))
                .args(["--exact", "helper_process", "--nocapture"])
                .env_clear()
                .env(MODE_ENV, "tree-child")
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .expect("spawn grandchild");
            fs::write(child_pid_path, child.id().to_string()).expect("write child PID");
            let _ = child.wait();
        }
        Ok("tree-child") => loop {
            thread::sleep(Duration::from_secs(1));
        },
        Ok("write-marker") => {
            fs::write(
                std::env::var(CHILD_PID_PATH_ENV).expect("marker path"),
                b"started",
            )
            .expect("write marker");
        }
        Ok("flood") => {
            let bytes: usize = std::env::var(FLOOD_BYTES_ENV)
                .expect("flood bytes")
                .parse()
                .expect("numeric flood bytes");
            let secret = std::env::var(SECRET_ENV).expect("secret canary");
            let stdout_secret = secret.clone();
            let stdout = thread::spawn(move || {
                write_flood(std::io::stdout(), b'O', bytes, stdout_secret.as_bytes())
            });
            let stderr = thread::spawn(move || {
                write_flood(std::io::stderr(), b'E', bytes, secret.as_bytes())
            });
            stdout.join().expect("stdout writer");
            stderr.join().expect("stderr writer");
        }
        Ok("stream-once") => {
            std::io::stdout()
                .write_all(b"stream-stdout")
                .expect("write streaming stdout");
            std::io::stderr()
                .write_all(b"stream-stderr")
                .expect("write streaming stderr");
        }
        _ => {}
    }
}

fn write_flood(mut writer: impl Write, byte: u8, total_bytes: usize, secret: &[u8]) {
    let chunk = vec![byte; 16 * 1024];
    let mut remaining = total_bytes;
    while remaining > 0 {
        let length = remaining.min(chunk.len());
        writer.write_all(&chunk[..length]).expect("flood write");
        remaining -= length;
    }
    writer.write_all(secret).expect("secret write");
    writer.flush().expect("flood flush");
}

fn wait_for_child_pid(path: &std::path::Path) -> u32 {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if let Ok(value) = fs::read_to_string(path) {
            return value.trim().parse().expect("child PID");
        }
        assert!(
            Instant::now() < deadline,
            "helper did not publish grandchild PID"
        );
        thread::sleep(Duration::from_millis(20));
    }
}

fn wide_null(value: &str) -> Vec<u16> {
    use std::os::windows::ffi::OsStrExt;

    std::ffi::OsStr::new(value)
        .encode_wide()
        .chain(std::iter::once(0))
        .collect()
}
