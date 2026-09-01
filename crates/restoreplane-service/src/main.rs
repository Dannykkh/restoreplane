#[cfg(windows)]
fn main() -> windows_service::Result<()> {
    restoreplane_service::scm::run_dispatcher()
}

#[cfg(not(windows))]
fn main() {
    eprintln!("restoreplane-service is supported on Windows only");
    std::process::exit(1);
}
