//! PruneDisk desktop backend. Read-only: it scans and shows; it never deletes, moves or backs
//! up. The only process it starts is its own executable as the analysis worker (worker.rs).
#![deny(unsafe_code)]

pub mod command_names;
pub mod commands;
#[cfg(feature = "e2e")]
mod e2e;
pub mod state;
pub mod worker;

use commands::AppState;
use state::Inner;

/// Start the app, or run as the analysis worker when started with `--analysis-worker`.
pub fn main_entry() {
    if let Some(args) = worker::worker_args_from(std::env::args_os()) {
        worker::worker_main(&args);
    }
    run();
}

/// Start the app.
pub fn run() {
    let inner = Inner::new(None);
    #[cfg(feature = "e2e")]
    e2e_open(&inner);
    let on_exit = std::sync::Arc::clone(&inner);
    #[cfg(feature = "e2e")]
    let on_setup = std::sync::Arc::clone(&inner);
    let app = tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .setup(move |_app| {
            #[cfg(feature = "e2e")]
            e2e_start(_app.handle(), &on_setup);
            Ok(())
        })
        .manage(AppState(inner))
        .invoke_handler(tauri::generate_handler![
            commands::app_info,
            commands::list_volumes,
            commands::choose_folders,
            commands::open_scan_file,
            commands::export_plan,
            commands::start_scan,
            commands::cancel_scan,
            commands::current_state,
            commands::tree_page,
            commands::tree_toggle,
            commands::tree_sort,
            commands::tree_focus,
            commands::tree_tiles,
            commands::top_files,
            commands::dev_summary,
            commands::dev_page,
            commands::session_folders,
            commands::get_settings,
            commands::save_settings,
        ])
        .build(tauri::generate_context!())
        .expect("PruneDisk could not start");
    app.run(move |_, event| {
        #[cfg(feature = "e2e")]
        e2e_log(&event);
        if let tauri::RunEvent::Exit = event {
            // End any analysis worker and the git commands it started.
            on_exit.shutdown();
        }
    });
}

/// Test-only (the `e2e` feature, never in release bundles): open the scan file named by
/// `PRUNEDISK_E2E_SCAN` at start, through the normal import path.
#[cfg(feature = "e2e")]
fn e2e_open(inner: &Inner) {
    if let Some(p) = std::env::var_os("PRUNEDISK_E2E_SCAN") {
        if let Err(e) = inner.import_file(std::path::Path::new(&p)) {
            eprintln!("e2e: cannot open the scan file: {e}");
        }
    }
}

/// Test-only (the `e2e` feature): scan the folders named by `PRUNEDISK_E2E_START` at start,
/// through the same offer and Start as the chooser, with git facts.
#[cfg(feature = "e2e")]
fn e2e_start(app: &tauri::AppHandle, inner: &std::sync::Arc<Inner>) {
    use tauri::Emitter;
    let Some(roots) = e2e::start_roots() else {
        return;
    };
    let roots: Vec<String> = roots
        .iter()
        .map(|r| r.to_string_lossy().into_owned())
        .collect();
    inner.offer_roots(roots.iter().map(String::as_str));
    let handle = app.clone();
    let emit: state::Emit = std::sync::Arc::new(move |e: &state::ScanEvent| {
        let _ = handle.emit("scan", e);
    });
    let settings = state::ScanSettings {
        collect_git_facts: true,
    };
    match inner.start_scan(&roots, settings, emit) {
        Ok(job) => eprintln!("e2e: started scan job {job:?}"),
        Err(e) => eprintln!("e2e: cannot start the scan: {e}"),
    }
}

/// Test-only (the `e2e` feature): one stderr line per window or exit event of the run loop.
#[cfg(feature = "e2e")]
fn e2e_log(event: &tauri::RunEvent) {
    use tauri::{RunEvent, WindowEvent};
    let what = match event {
        RunEvent::Exit => "exit".to_string(),
        RunEvent::ExitRequested { code, .. } => format!("exit requested (code {code:?})"),
        RunEvent::WindowEvent {
            label,
            event: WindowEvent::CloseRequested { .. },
            ..
        } => format!("window {label} close requested"),
        RunEvent::WindowEvent {
            label,
            event: WindowEvent::Destroyed,
            ..
        } => format!("window {label} destroyed"),
        _ => return,
    };
    eprintln!("e2e: event {what}");
}

#[cfg(test)]
mod tests {
    use super::command_names::COMMANDS;

    /// Spec AC-12: the registered command list is exactly this list.
    #[test]
    fn the_command_list_is_the_specified_one() {
        let spec = [
            "app_info",
            "list_volumes",
            "choose_folders",
            "open_scan_file",
            "export_plan",
            "start_scan",
            "cancel_scan",
            "current_state",
            "tree_page",
            "tree_toggle",
            "tree_sort",
            "tree_focus",
            "tree_tiles",
            "top_files",
            "dev_summary",
            "dev_page",
            "session_folders",
            "get_settings",
            "save_settings",
        ];
        assert_eq!(COMMANDS, spec);
    }

    #[test]
    fn the_handler_registers_exactly_the_command_list() {
        let src = include_str!("lib.rs");
        let start = src.find("generate_handler![").unwrap();
        let end = start + src[start..].find(']').unwrap();
        let registered: Vec<&str> = src[start..end]
            .split("commands::")
            .skip(1)
            .map(|s| s.trim_end_matches(|c: char| !c.is_alphanumeric() && c != '_'))
            .map(|s| s.split(|c: char| !c.is_alphanumeric() && c != '_').next().unwrap())
            .collect();
        assert_eq!(registered, COMMANDS);
    }

    #[test]
    fn capabilities_grant_exactly_the_commands_and_event_listening() {
        let cap: serde_json::Value =
            serde_json::from_str(include_str!("../capabilities/default.json")).unwrap();
        let mut granted: Vec<String> = cap["permissions"]
            .as_array()
            .unwrap()
            .iter()
            .map(|p| p.as_str().unwrap().to_string())
            .collect();
        granted.sort();
        let mut want: Vec<String> = COMMANDS
            .iter()
            .map(|c| format!("allow-{}", c.replace('_', "-")))
            .chain(["core:event:allow-listen".into(), "core:event:allow-unlisten".into()])
            .collect();
        want.sort();
        assert_eq!(granted, want);
    }

    /// Spec AC-12: the content security policy, exactly.
    #[test]
    fn the_csp_is_the_specified_one() {
        let conf: serde_json::Value =
            serde_json::from_str(include_str!("../tauri.conf.json")).unwrap();
        assert_eq!(
            conf["app"]["security"]["csp"].as_str().unwrap(),
            "default-src 'self'; script-src 'self'; style-src 'self'; img-src 'self' data:; \
             connect-src ipc: http://ipc.localhost; object-src 'none'; base-uri 'none'; \
             form-action 'none'; frame-ancestors 'none'"
        );
    }

    /// The webview keeps no browsing data on disk (docs/DESKTOP.md, "Files the desktop app
    /// writes"); the macOS bundle job checks what an idle launch writes.
    #[test]
    fn the_window_runs_the_webview_in_incognito_mode() {
        let conf: serde_json::Value =
            serde_json::from_str(include_str!("../tauri.conf.json")).unwrap();
        let windows = conf["app"]["windows"].as_array().unwrap();
        assert!(!windows.is_empty());
        for w in windows {
            assert_eq!(w["incognito"].as_bool(), Some(true), "{w}");
        }
    }
}
