//! dbbackup 콘솔 — Tauri v2 백엔드 엔트리.
mod commands;
mod config_edit;
mod paths;
mod setup;

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .invoke_handler(tauri::generate_handler![
            commands::dashboard,
            setup::keystore_status,
            setup::init_keystore,
            config_edit::add_database,
            config_edit::list_databases,
        ])
        .run(tauri::generate_context!())
        .expect("Tauri 애플리케이션 실행 실패");
}
