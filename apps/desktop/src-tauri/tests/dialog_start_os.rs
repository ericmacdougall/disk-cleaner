//! Where the dialogs start on this operating system (click test CT-F3: the Linux AppImage's save
//! dialog opened in its own read-only mount). One test, so this process is the only one that
//! changes its environment and working folder.
// The test makes its own home folder (and, on Linux, a user-dirs file) in a temporary folder.
#![allow(clippy::disallowed_methods)]

use prunedisk_desktop_lib::dialogs::start_dir;

#[test]
fn dialogs_start_in_documents_else_home_never_the_working_folder() {
    let t = tempfile::tempdir().unwrap();
    let work = t.path().join("mount");
    std::fs::create_dir(&work).unwrap();
    // Like an AppImage, which starts in its own mount.
    std::env::set_current_dir(&work).unwrap();
    os_case(t.path());
    let d = start_dir().expect("a start folder");
    assert!(d.is_absolute() && d.is_dir(), "{d:?}");
    assert_ne!(d, std::env::current_dir().unwrap(), "the working folder");
}

#[cfg(target_os = "linux")]
fn os_case(root: &std::path::Path) {
    let home = root.join("home");
    let config = home.join(".config");
    std::fs::create_dir_all(&config).unwrap();
    std::env::set_var("HOME", &home);
    std::env::set_var("XDG_CONFIG_HOME", &config);
    // No user-dirs file and no Documents folder: home.
    assert_eq!(start_dir(), Some(home.clone()), "no documents folder");
    // The user-dirs file names a documents folder that does not exist yet: still home.
    std::fs::write(config.join("user-dirs.dirs"), "XDG_DOCUMENTS_DIR=\"$HOME/Papers\"\n").unwrap();
    assert_eq!(start_dir(), Some(home.clone()), "documents folder named but missing");
    std::fs::create_dir(home.join("Papers")).unwrap();
    assert_eq!(start_dir(), Some(home.join("Papers")), "documents folder from user-dirs");
}

#[cfg(target_os = "macos")]
fn os_case(root: &std::path::Path) {
    let home = root.join("home");
    std::fs::create_dir(&home).unwrap();
    std::env::set_var("HOME", &home);
    assert_eq!(start_dir(), Some(home.clone()), "no Documents folder");
    std::fs::create_dir(home.join("Documents")).unwrap();
    assert_eq!(start_dir(), Some(home.join("Documents")), "~/Documents");
}

#[cfg(windows)]
fn os_case(_root: &std::path::Path) {
    // The known folders cannot be moved for a test: check the choice on the runner's own.
    let docs = dirs::document_dir().filter(|p| p.is_dir());
    let home = dirs::home_dir().expect("a profile folder");
    assert_eq!(start_dir(), Some(docs.unwrap_or(home)), "Documents, else the profile folder");
}
