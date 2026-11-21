//! Where native file dialogs start (click test CT-F3). RED STUB: this reproduces the behaviour
//! of 0112370, where no dialog sets a start folder, so the dialog opens in the process's
//! working folder (for an AppImage, its own read-only mount). The fix replaces it.

use std::path::PathBuf;

/// The start folder chosen from the candidates. Today: none.
pub fn start_dir_from(_documents: Option<PathBuf>, _home: Option<PathBuf>) -> Option<PathBuf> {
    None
}

/// Where every dialog starts on this machine. Today: none.
pub fn start_dir() -> Option<PathBuf> {
    start_dir_from(dirs::document_dir(), dirs::home_dir())
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
            "the folder, open and save dialogs"
        );
    }
}
