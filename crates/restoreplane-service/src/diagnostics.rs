use restoreplane_catalog::{OperationalStore, RepositoryError, ServiceDiagnostics};

pub fn collect_service_diagnostics(
    store: &OperationalStore,
    now_ms: i64,
) -> Result<ServiceDiagnostics, RepositoryError> {
    store.diagnostics().collect(now_ms)
}
