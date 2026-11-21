//! Every native file dialog of the app is built here (click test CT-F3). Each one starts in the
//! user's documents folder, or the home folder when there is none, on every system; never in
//! the folder the app was started from (an AppImage starts in its own read-only mount). Nothing
//! remembers a last folder: the start is worked out again for each dialog, and nothing is
//! written.

use std::path::PathBuf;
use tauri::{AppHandle, Runtime};
use tauri_plugin_dialog::{DialogExt, FileDialogBuilder};

/// The first candidate that is an absolute path to an existing folder: `documents`, else
/// `home`. `None` when neither is.
pub fn start_dir_from(documents: Option<PathBuf>, home: Option<PathBuf>) -> Option<PathBuf> {
    [documents, home]
        .into_iter()
        .flatten()
        .find(|p| p.is_absolute() && p.is_dir())
}

/// Where every dialog starts on this machine: the documents folder (Linux: XDG_DOCUMENTS_DIR
/// of user-dirs.dirs; macOS: ~/Documents; Windows: the Documents known folder), else home.
/// These are the folders tauri's own `document_dir` and `home_dir` name (same crate).
pub fn start_dir() -> Option<PathBuf> {
    start_dir_from(dirs::document_dir(), dirs::home_dir())
}

/// A file dialog with `title`, starting in `start_dir()`. When no start folder is known the
/// system's own default is left (the dialog is never pointed anywhere else).
pub fn file_dialog<R: Runtime>(app: &AppHandle<R>, title: &str) -> FileDialogBuilder<R> {
    let dialog = app.dialog().file().set_title(title);
    match start_dir() {
        Some(dir) => dialog.set_directory(dir),
        None => dialog,
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    /// The code part of a source file (its test module, if any, left out).
    fn code(text: &str) -> &str {
        text.split("#[cfg(test)]").next().unwrap_or("")
    }

    /// Every native dialog of the app is built in this module (the e2e hooks bypass the dialogs,
    /// so the end-to-end suite cannot see a dialog that skips the start folder).
    #[test]
    fn every_dialog_is_built_here() {
        let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut builders = 0;
        for e in std::fs::read_dir(&src).unwrap() {
            let p = e.unwrap().path();
            if p.extension().and_then(|x| x.to_str()) != Some("rs") {
                continue;
            }
            let text = std::fs::read_to_string(&p).unwrap();
            let name = p.file_name().unwrap().to_string_lossy().to_string();
            let n = code(&text).matches(".dialog()").count();
            if name == "dialogs.rs" {
                builders += n;
            } else {
                assert_eq!(n, 0, "{name} opens a dialog outside dialogs.rs");
            }
        }
        assert_eq!(builders, 1, "dialogs.rs builds every dialog in one place");
        let commands = std::fs::read_to_string(src.join("commands.rs")).unwrap();
        assert_eq!(
            code(&commands).matches("dialogs::file_dialog(").count(),
            3,
            "the scan folders, open and export folder dialogs"
        );
    }

    /// Click-test N2: a save dialog asks "replace?" for a taken name (GTK always, through rfd;
    /// Windows and macOS by default), and the app never replaces a file (NEG-6), so the app has
    /// no save dialog at all. Export picks a folder and the app names the file.
    #[test]
    fn no_dialog_can_offer_to_replace_a_file() {
        let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        for e in std::fs::read_dir(&src).unwrap() {
            let p = e.unwrap().path();
            if p.extension().and_then(|x| x.to_str()) != Some("rs") {
                continue;
            }
            let text = std::fs::read_to_string(&p).unwrap();
            let c = code(&text);
            for word in ["save_file", "set_file_name"] {
                assert!(!c.contains(word), "{} uses {word}", p.display());
            }
        }
        let commands = std::fs::read_to_string(src.join("commands.rs")).unwrap();
        let c = code(&commands);
        let export = &c[c.find("fn pick_export_folder").expect("the export folder dialog")..];
        let export = &export[..export.find("\n}\n").unwrap()];
        assert!(export.contains(".blocking_pick_folder()"), "{export}");
        assert!(c.contains("export_to_dir(&dir"), "the app names the exported file");
    }
}
