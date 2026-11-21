//! The start folder of every native dialog (click test CT-F3): the documents folder when it is
//! an existing folder, else home; never a relative path, a file or a missing folder.
// The tests make their own folders and files in temporary folders.
#![allow(clippy::disallowed_methods)]

use prunedisk_desktop_lib::dialogs::start_dir_from;
use std::path::PathBuf;

#[test]
fn documents_when_it_is_a_folder() {
    let t = tempfile::tempdir().unwrap();
    let docs = t.path().join("Documents");
    std::fs::create_dir(&docs).unwrap();
    assert_eq!(start_dir_from(Some(docs.clone()), Some(t.path().to_path_buf())), Some(docs));
}

#[test]
fn home_when_documents_is_missing_a_file_relative_or_unknown() {
    let t = tempfile::tempdir().unwrap();
    let home = t.path().to_path_buf();
    let docs = t.path().join("Documents");
    assert_eq!(start_dir_from(Some(docs.clone()), Some(home.clone())), Some(home.clone()), "missing");
    std::fs::write(&docs, b"x").unwrap();
    assert_eq!(start_dir_from(Some(docs), Some(home.clone())), Some(home.clone()), "a file");
    assert_eq!(
        start_dir_from(Some(PathBuf::from("Documents")), Some(home.clone())),
        Some(home.clone()),
        "relative"
    );
    assert_eq!(start_dir_from(None, Some(home.clone())), Some(home), "unknown");
}

#[test]
fn none_when_neither_is_a_folder() {
    let t = tempfile::tempdir().unwrap();
    let gone = t.path().join("gone");
    assert_eq!(start_dir_from(Some(gone.clone()), Some(gone)), None);
    assert_eq!(start_dir_from(None, Some(PathBuf::from("relative-home"))), None);
    assert_eq!(start_dir_from(None, None), None);
}
