// Prevents an extra console window on Windows in release builds. DO NOT REMOVE.
// (Harmless on macOS/Linux, which are this app's only targets.)
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

// Thin binary shim for the Tauri v2 lib/bin split: all real wiring — the
// `#[tauri::command]` registration, the shared `TunnelManager` managed state,
// and the on-exit teardown hook (task-005) — lives in `run()` in the library
// crate so it is reusable as the (future) mobile entry point.
fn main() {
    jonggrang_tunnel_lib::run();
}
