// Connected Backup Platform Windows service and local IPC boundary.

pub mod application;
pub mod backup;
pub mod cutover;
pub mod diagnostics;
pub mod host;
pub mod identity_config;
pub mod ipc;
pub mod operations;
#[cfg(windows)]
mod package_certification;
pub mod recovery;
mod recovery_point;
pub mod repair;
#[cfg(windows)]
mod restore_runtime;
pub mod retention;
pub mod retention_runtime;
pub mod scheduler;
#[cfg(windows)]
pub mod scm;
pub mod secrets;
#[cfg(windows)]
mod startup;
pub mod support_bundle;
pub mod telemetry_archive;
mod trusted_tool;
pub mod update;
