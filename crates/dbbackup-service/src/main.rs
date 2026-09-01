#[cfg(windows)]
fn main() -> windows_service::Result<()> {
    dbbackup_service::scm::run_dispatcher()
}

#[cfg(not(windows))]
fn main() {
    eprintln!("dbbackup-service is supported on Windows only");
    std::process::exit(1);
}
