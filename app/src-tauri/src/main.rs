// Prevent an extra console window on Windows in release; no effect on macOS/Linux.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

fn main() {
    claudrvoyant_run();
}

/// Indirection so the bin and the (mobile-capable) lib share one entry point.
fn claudrvoyant_run() {
    claurdvoyant_app_lib::run();
}
