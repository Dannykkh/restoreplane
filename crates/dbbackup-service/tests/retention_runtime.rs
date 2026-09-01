use std::sync::Arc;

use dbbackup_catalog::OperationalStore;
use dbbackup_pipeline::DestinationDeleteV2;
use dbbackup_runtime::{
    RetentionDeleteOperatorResolverV2, RetentionDeleteResolutionErrorV2, RuntimeClock,
};
use dbbackup_service::retention_runtime::{RetentionRuntimeTick, ServiceRetentionRuntime};
use tempfile::TempDir;

struct FixedClock(i64);

impl RuntimeClock for FixedClock {
    fn now_ms(&self) -> i64 {
        self.0
    }
}

struct UnusedResolver;

impl RetentionDeleteOperatorResolverV2 for UnusedResolver {
    fn resolve(
        &self,
        _destination_fingerprint: &str,
        _delete_secret_ref_id: Option<&str>,
    ) -> Result<Box<dyn DestinationDeleteV2>, RetentionDeleteResolutionErrorV2> {
        Err(
            RetentionDeleteResolutionErrorV2::terminal("UNEXPECTED_RETENTION_DELETE")
                .expect("static resolution error"),
        )
    }
}

#[tokio::test(flavor = "current_thread")]
async fn service_retention_runtime_owns_bounded_maintenance_and_only_its_delete_queue() {
    let temp_dir = TempDir::new().expect("temp dir");
    let store = OperationalStore::open(temp_dir.path().join("operational.sqlite3"))
        .expect("open operational store");
    let service = ServiceRetentionRuntime::new(
        store,
        Arc::new(UnusedResolver),
        Arc::new(FixedClock(1_000)),
        "retention-worker-test",
        "retention-boot-test",
        10_000,
        64,
    )
    .expect("create retention runtime");

    let report = service
        .maintenance_once(1_000)
        .expect("bounded empty maintenance");
    assert_eq!(report.candidates_evaluated(), 0);
    assert_eq!(
        service.delete_once().await.expect("empty queue tick"),
        RetentionRuntimeTick::Idle
    );
}
