// Prevents an extra console window on Windows in release builds. DO NOT REMOVE.
// (Harmless on macOS/Linux, which are this app's only targets.)
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

fn main() {
    jonggrang_tunnel_lib::run();
}
