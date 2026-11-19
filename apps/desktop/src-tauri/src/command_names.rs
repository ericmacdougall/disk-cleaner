// The commands the webview may call (spec AC-12). One list: build.rs puts it in the app
// manifest, capabilities/default.json grants exactly `allow-<name>` for each, and a test
// checks both and the `generate_handler!` list in lib.rs against it.
pub const COMMANDS: &[&str] = &[
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
