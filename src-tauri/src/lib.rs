//! Tauri v2 application entry point for the Jonggrang Tunnel desktop app.
//!
//! The builder is bootstrapped here so the same `run()` can serve both the
//! desktop binary (src/main.rs) and a future mobile entry point.
//!
//! Real `#[tauri::command]` handlers (start/stop/status/list) are wired into
//! `invoke_handler` in a later task (task-005); for now the handler list is an
//! empty stub so the app compiles and boots a window.

/// Pure tunnel-spec parsing and local-port allocation (task-003). Kept free of
/// `tauri`/process/network deps so its `#[cfg(test)]` suite runs under plain
/// `cargo test`. The lifecycle manager (task-004) builds on these types.
pub mod tunnel;

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .invoke_handler(tauri::generate_handler![])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
