// No console window on Windows in release builds.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

fn main() {
    // `--analysis-worker` runs the analysis worker instead of the app (worker.rs).
    prunedisk_desktop_lib::main_entry();
}
