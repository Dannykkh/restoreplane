mod lifecycle;
#[cfg(windows)]
mod pipe_runtime;
mod singleton;

pub use lifecycle::{
    ClaimRuntime, HostConfig, HostError, HostPhase, HostRuntime, ServiceHealth, ServiceHealthProbe,
    ServiceHost, ShutdownReason,
};
#[cfg(windows)]
pub use pipe_runtime::PipeHostRuntime;
pub use singleton::MaintenanceAuthority;
