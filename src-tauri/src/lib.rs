//! Tauri v2 application entry point for the Jonggrang Tunnel desktop app.
//!
//! The builder is bootstrapped here so the same `run()` can serve both the
//! desktop binary (src/main.rs) and a future mobile entry point.
//!
//! The `#[tauri::command]` handlers (start/stop/status/list) live in
//! [`commands`] and are registered into `invoke_handler` below; the shared
//! [`tunnel::TunnelManager`] is held as Tauri managed state, and an exit hook
//! guarantees every spawned `ssh` child is reaped on app shutdown.

/// Pure tunnel-spec parsing and local-port allocation (task-003) plus the SSH
/// lifecycle manager (task-004). Kept free of `tauri` deps so its `#[cfg(test)]`
/// suite runs under plain `cargo test`.
pub mod tunnel;

/// Tauri command bridge (task-005): serde DTOs + `#[tauri::command]` handlers
/// that drive the shared [`tunnel::TunnelManager`] and emit live status events.
pub mod commands;

use std::sync::{Arc, Mutex};

use tunnel::{SharedTunnelManager, TunnelManager};

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    // The lifecycle manager is shared between the command handlers (as Tauri
    // managed state) and the exit hook below, so it lives behind Arc<Mutex<..>>.
    let manager: SharedTunnelManager = Arc::new(Mutex::new(TunnelManager::new()));
    let teardown = Arc::clone(&manager);

    tauri::Builder::default()
        .manage(manager)
        .invoke_handler(tauri::generate_handler![
            commands::start_tunnel,
            commands::stop_tunnel,
            commands::tunnel_status,
            commands::list_forwards,
            commands::open_dashboard,
        ])
        .build(tauri::generate_context!())
        .expect("error while building tauri application")
        // `build` + `run(callback)` (instead of plain `run`) lets us observe the
        // exit lifecycle. On exit we `stop()` the manager so every `ssh -L` child
        // is killed and reaped; `ForwardProcess`'s `Drop` is the RAII backstop,
        // but stopping explicitly here covers the normal shutdown path.
        .run(move |_app_handle, event| {
            if let tauri::RunEvent::Exit = event {
                if let Ok(mut mgr) = teardown.lock() {
                    mgr.stop();
                }
            }
        });
}
