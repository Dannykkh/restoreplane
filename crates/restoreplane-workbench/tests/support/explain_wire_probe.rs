// Test-only exact user-wire handoff counter; every call delegates to the native adapter.
use async_trait::async_trait;
use restoreplane_domain::workbench::{ConnectDescriptor, Mode, TypedCell};
use restoreplane_workbench::driver::{
    DriverCancel, DriverConnection, DriverFactory, DriverFacts, DriverLogin, DriverSummary,
    FileReadLimits, NativeDriverFactory, QueryLimits, RowSink, TransactionCommand,
};
use restoreplane_workbench::WorkbenchError;
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};

#[derive(Clone)]
pub struct Probe {
    watched: Arc<Vec<(blake3::Hash, AtomicUsize)>>,
}
impl Probe {
    pub fn new(statements: &[&str]) -> Self {
        assert!(statements.len() <= 8);
        Self {
            watched: Arc::new(
                statements
                    .iter()
                    .map(|sql| (blake3::hash(sql.as_bytes()), AtomicUsize::new(0)))
                    .collect(),
            ),
        }
    }
    pub fn count(&self, sql: &str) -> usize {
        let hash = blake3::hash(sql.as_bytes());
        self.watched
            .iter()
            .find(|(candidate, _)| *candidate == hash)
            .expect("test wire must be registered before observation")
            .1
            .load(Ordering::SeqCst)
    }
    pub fn factory(&self) -> Arc<dyn DriverFactory> {
        Arc::new(Factory(self.clone()))
    }
    fn observe(&self, sql: &str) {
        let hash = blake3::hash(sql.as_bytes());
        if let Some((_, count)) = self
            .watched
            .iter()
            .find(|(candidate, _)| *candidate == hash)
        {
            count.fetch_add(1, Ordering::SeqCst);
        }
    }
}
struct Factory(Probe);
struct Connection {
    inner: Box<dyn DriverConnection>,
    probe: Probe,
}
#[async_trait]
impl DriverFactory for Factory {
    async fn connect(
        &self,
        descriptor: &ConnectDescriptor,
        login: DriverLogin,
        mode: Mode,
    ) -> Result<Box<dyn DriverConnection>, WorkbenchError> {
        Ok(Box::new(Connection {
            inner: NativeDriverFactory::default()
                .connect(descriptor, login, mode)
                .await?,
            probe: self.0.clone(),
        }))
    }
}
#[async_trait]
impl DriverConnection for Connection {
    fn facts(&self) -> &DriverFacts {
        self.inner.facts()
    }
    fn cancel_handle(&self) -> Arc<dyn DriverCancel> {
        self.inner.cancel_handle()
    }
    async fn set_read_only(&mut self, value: bool) -> Result<(), WorkbenchError> {
        self.inner.set_read_only(value).await
    }
    async fn transaction_read_only(&mut self) -> Result<bool, WorkbenchError> {
        self.inner.transaction_read_only().await
    }
    async fn configure_limits(&mut self, value: std::time::Duration) -> Result<(), WorkbenchError> {
        self.inner.configure_limits(value).await
    }
    async fn transaction_control(
        &mut self,
        command: TransactionCommand,
    ) -> Result<(), WorkbenchError> {
        self.inner.transaction_control(command).await
    }
    async fn execute(
        &mut self,
        sql: &str,
        limits: QueryLimits,
        sink: &mut dyn RowSink,
    ) -> Result<DriverSummary, WorkbenchError> {
        self.probe.observe(sql);
        self.inner.execute(sql, limits, sink).await
    }
    async fn execute_bound(
        &mut self,
        sql: &str,
        parameters: &[TypedCell],
        limits: QueryLimits,
        sink: &mut dyn RowSink,
    ) -> Result<DriverSummary, WorkbenchError> {
        self.probe.observe(sql);
        self.inner
            .execute_bound(sql, parameters, limits, sink)
            .await
    }
    async fn execute_file_read(
        &mut self,
        sql: &str,
        limits: FileReadLimits,
        sink: &mut dyn RowSink,
    ) -> Result<DriverSummary, WorkbenchError> {
        self.probe.observe(sql);
        self.inner.execute_file_read(sql, limits, sink).await
    }
    async fn close(&mut self) -> Result<(), WorkbenchError> {
        self.inner.close().await
    }
}
