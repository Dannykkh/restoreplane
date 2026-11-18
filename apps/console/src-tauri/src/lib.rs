//! restoreplane 콘솔 — Tauri v2 백엔드 엔트리.
mod ipc_client;
mod workbench;
mod workbench_files;
mod workbench_native;

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .manage(std::sync::Arc::new(workbench::DesktopWorkbench::default()))
        .on_window_event(|window, event| {
            if matches!(event, tauri::WindowEvent::Destroyed) {
                use tauri::Manager;
                let state = window
                    .state::<std::sync::Arc<workbench::DesktopWorkbench>>()
                    .inner()
                    .clone();
                let label = window.label().to_owned();
                tauri::async_runtime::spawn(async move {
                    state.window_closed(&label).await;
                });
            }
        })
        .invoke_handler(tauri::generate_handler![
            ipc_client::service_request,
            workbench::workbench_request,
        ])
        .run(tauri::generate_context!())
        .expect("Tauri 애플리케이션 실행 실패");
}
