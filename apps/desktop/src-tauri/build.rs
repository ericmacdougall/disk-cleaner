// The app manifest lists every command the webview may call. A command that is not listed here
// and granted in capabilities/default.json cannot be invoked from the page.
const COMMANDS: &[&str] = &["app_info"];

fn main() {
    tauri_build::try_build(
        tauri_build::Attributes::new()
            .app_manifest(tauri_build::AppManifest::new().commands(COMMANDS)),
    )
    .expect("tauri-build failed");
}
