//! PruneDisk desktop backend. Read-only: it scans and shows; it never deletes, moves or backs up.

use serde::Serialize;

/// What the about line shows.
#[derive(Serialize)]
pub struct AppInfo {
    pub version: &'static str,
    pub schema_version: u32,
    pub read_only: bool,
}

#[tauri::command]
fn app_info() -> AppInfo {
    AppInfo {
        version: env!("CARGO_PKG_VERSION"),
        schema_version: disk_cleaner_core::SCHEMA_VERSION,
        read_only: true,
    }
}

/// Start the app.
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .invoke_handler(tauri::generate_handler![app_info])
        .run(tauri::generate_context!())
        .expect("PruneDisk could not start");
}
