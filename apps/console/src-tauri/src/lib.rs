//! dbbackup 콘솔 — Tauri v2 백엔드 엔트리.
mod commands;

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .invoke_handler(tauri::generate_handler![commands::dashboard])
        .run(tauri::generate_context!())
        .expect("Tauri 애플리케이션 실행 실패");
}
