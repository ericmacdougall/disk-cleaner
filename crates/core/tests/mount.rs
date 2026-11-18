//! AC-1: `--one-file-system` does not cross mount points.
//!
//! Needs a real second filesystem, so it only runs when `DC_TEST_MOUNT_DIR` names an empty
//! directory that is a mount point (CI mounts a tmpfs there on Linux). Without the variable the
//! test is a no-op and says so; it is not counted as verification.

mod common;

use common::*;
use disk_cleaner_core::{scan, ScanOptions};
use std::path::PathBuf;

#[test]
fn one_file_system_does_not_cross_a_mount_point() {
    let mount = match std::env::var_os("DC_TEST_MOUNT_DIR") {
        Some(m) => PathBuf::from(m),
        None => {
            eprintln!("DC_TEST_MOUNT_DIR not set: mount test skipped (not verified here)");
            return;
        }
    };
    let root = mount.parent().expect("mount dir has a parent").to_path_buf();
    // A 4 MiB file on the other filesystem, 1 KiB on the root filesystem.
    write_bytes(&mount.join("big.bin"), 4 * 1024 * 1024);
    write_bytes(&root.join("small.bin"), 1024);

    let mut crossing = ScanOptions::new(vec![root.clone()]);
    crossing.collect_facts = false;
    let all = scan(&crossing).unwrap();
    assert!(all.totals.allocated_bytes >= 4 * 1024 * 1024);
    assert_eq!(all.totals.mount_points_skipped, 0);

    let mut one_fs = crossing.clone();
    one_fs.one_file_system = true;
    let limited = scan(&one_fs).unwrap();
    assert!(
        limited.totals.allocated_bytes < 1024 * 1024,
        "mounted data must not be counted, got {}",
        limited.totals.allocated_bytes
    );
    assert_eq!(limited.totals.mount_points_skipped, 1);
}
