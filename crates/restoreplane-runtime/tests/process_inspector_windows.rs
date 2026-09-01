#![cfg(windows)]

use std::sync::Arc;

use restoreplane_catalog::{ClaimRequest, OperationWorkSpec, OperationalStore, ProcessStartSpec};
use restoreplane_domain::{OperationRunId, WorkItemId};
use restoreplane_runtime::{
    hash_executable, snapshot_spawned_process, LaunchSpec, ProcessInspector, ProcessLauncher,
    ProcessObservation, ProcessProbe, SystemRuntimeClock, WindowsProcessInspector,
    WindowsProcessLauncher,
};
use tempfile::TempDir;

const MODE_ENV: &str = "RESTOREPLANE_INSPECTOR_TEST_MODE";

#[test]
fn inspector_distinguishes_matching_owned_reused_and_exited_processes() {
    let temp = TempDir::new().expect("temp directory");
    let db_path = temp.path().join("operational.db");
    let store = OperationalStore::open(db_path).expect("open operational store");
    let repository = store.runtime();
    let clock = Arc::new(SystemRuntimeClock);
    let now_ms = restoreplane_runtime::RuntimeClock::now_ms(clock.as_ref());
    repository
        .enqueue_operation_work(OperationWorkSpec::new(
            OperationRunId::new(),
            WorkItemId::new(),
            "tool_rescan",
            "service",
            "process-inspector",
            "{}",
            now_ms,
        ))
        .expect("enqueue operation");
    let claim = repository
        .claim_next(ClaimRequest::new(
            "worker-inspect",
            "boot-inspect",
            now_ms,
            5_000,
        ))
        .expect("claim operation")
        .expect("due operation");
    repository
        .start_claim(&claim, now_ms + 1)
        .expect("start claim");

    let executable = std::env::current_exe().expect("test executable");
    let digest = hash_executable(&executable).expect("hash executable");
    let launch = LaunchSpec::new(executable.clone(), temp.path().to_path_buf(), digest)
        .expect("launch spec")
        .with_args(["--exact", "helper_process", "--nocapture"])
        .expect("helper args")
        .with_environment([(MODE_ENV, "wait")])
        .expect("restricted environment");
    let launcher = WindowsProcessLauncher;
    let mut process = launcher.launch(&launch).expect("launch owned process");
    let identity = process.identity().clone();
    let recorded_at_ms = restoreplane_runtime::RuntimeClock::now_ms(clock.as_ref());
    let process_instance_id = uuid::Uuid::new_v4().to_string();
    repository
        .record_process_start(ProcessStartSpec::new(
            process_instance_id.clone(),
            claim.clone(),
            "native_tool",
            identity.process_id(),
            identity.process_start_time_ms(),
            "boot-inspect",
            identity.job_key(),
            executable.to_string_lossy().into_owned(),
            Some(hex_digest(&digest)),
            recorded_at_ms,
        ))
        .expect("record process");
    repository
        .mark_process_running(&claim, &process_instance_id, recorded_at_ms)
        .expect("mark process running");
    let candidate = repository
        .expired_claims(now_ms + 5_001, 10)
        .expect("scan expired process")
        .pop()
        .expect("candidate");
    let recorded = candidate.process().expect("recorded process");
    let probe = ProcessProbe::from(recorded);
    let inspector = WindowsProcessInspector;
    assert_eq!(
        inspector.inspect(&probe).expect("inspect owned process"),
        ProcessObservation::MatchingOwned
    );

    let reused = ProcessProbe::new(
        recorded.process_id(),
        recorded.process_start_time_ms() + 1,
        recorded.job_object_key(),
    )
    .expect("reused probe");
    assert_eq!(
        inspector.inspect(&reused).expect("inspect reused PID"),
        ProcessObservation::Reused
    );

    process.terminate().expect("terminate helper");
    assert_eq!(
        inspector.inspect(&probe).expect("inspect exited process"),
        ProcessObservation::Exited
    );
}

#[test]
fn inspector_preserves_live_unowned_child_and_confirms_its_eventual_exit() {
    let executable = std::env::current_exe().expect("test executable");
    let mut child = std::process::Command::new(&executable)
        .args(["--exact", "helper_process", "--nocapture"])
        .env(MODE_ENV, "wait")
        .spawn()
        .expect("spawn unowned child");
    let snapshot = snapshot_spawned_process(i64::from(child.id())).expect("exact OS identity");
    assert!(snapshot.executable_path().is_absolute());
    let probe = ProcessProbe::new(
        snapshot.process_id(),
        snapshot.process_start_time_ms(),
        format!(r"Local\RestorePlane.Job.Unowned.{}", uuid::Uuid::new_v4()),
    )
    .expect("unowned probe");
    let inspector = WindowsProcessInspector;

    assert_eq!(
        inspector.inspect(&probe).expect("inspect live child"),
        ProcessObservation::MatchingUnowned
    );

    child.kill().expect("kill helper");
    child.wait().expect("reap helper");
    assert!(matches!(
        inspector.inspect(&probe).expect("inspect exited child"),
        ProcessObservation::Exited | ProcessObservation::Missing
    ));
}

#[test]
fn helper_process() {
    if std::env::var(MODE_ENV).as_deref() == Ok("wait") {
        loop {
            std::thread::sleep(std::time::Duration::from_secs(1));
        }
    }
}

fn hex_digest(digest: &[u8; 32]) -> String {
    use std::fmt::Write;

    let mut encoded = String::with_capacity(64);
    for byte in digest {
        let _ = write!(encoded, "{byte:02x}");
    }
    encoded
}
