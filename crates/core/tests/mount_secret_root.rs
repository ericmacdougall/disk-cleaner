//! PR review 3 of goodqa#16529, F1: a scan root inside a secret directory that is walked on its
//! own (another file system under `--one-file-system`, so the scan keeps both roots) stays
//! secret. Every member under it is a secret `keep` piece and is never hashed, whichever order
//! the roots are given in.
//!
//! Needs real second file systems, so it only runs when `DC_TEST_SECRET_MOUNT_ROOT` names a
//! directory in which `secrets/mnt` and `.aws/mnt` are empty mount points of other file systems
//! (CI mounts two tmpfs there on Linux). Without the variable the test is a no-op and says so; it
//! is not counted as verification.
#![cfg(unix)]

mod common;

use common::*;
use disk_cleaner_core::pieces::PieceOptions;
use disk_cleaner_core::{plan, scan, Lane, PlanOptions, ScanOptions};
use std::path::{Path, PathBuf};

#[test]
fn a_mounted_root_inside_a_secret_directory_stays_secret() {
    use std::os::unix::fs::MetadataExt;
    let base = match std::env::var_os("DC_TEST_SECRET_MOUNT_ROOT") {
        Some(m) => std::fs::canonicalize(PathBuf::from(m)).unwrap(),
        None => {
            eprintln!(
                "DC_TEST_SECRET_MOUNT_ROOT not set: secret mount test skipped (not verified here)"
            );
            return;
        }
    };
    let dev = |p: &Path| std::fs::symlink_metadata(p).unwrap().dev();
    for secret in ["secrets", ".aws"] {
        let inner = base.join(secret).join("mnt");
        assert_ne!(
            dev(&inner),
            dev(&base),
            "{} must be a mount",
            inner.display()
        );
        write_bytes(&inner.join("a.bin"), 100 * 1024);
        write_bytes(&inner.join("b.bin"), 100 * 1024);
    }
    write_bytes(&base.join("loose").join("c.bin"), 100 * 1024);
    write_bytes(&base.join("loose").join("d.bin"), 100 * 1024);
    std::thread::sleep(std::time::Duration::from_millis(1100));
    let inners = [
        base.join("secrets").join("mnt"),
        base.join(".aws").join("mnt"),
    ];
    let orders = [
        vec![base.clone(), inners[0].clone(), inners[1].clone()],
        vec![inners[1].clone(), inners[0].clone(), base.clone()],
    ];
    for roots in orders {
        let mut o = ScanOptions::new(roots.clone());
        o.facts = network_everywhere();
        o.one_file_system = true;
        o.lanes = Some(PieceOptions::default());
        let res = scan(&o).unwrap();
        let survey = res.pieces.as_ref().expect("pieces");
        for inner in &inners {
            let mut seen = 0;
            for p in &survey.pieces {
                for m in &p.members {
                    let path = Path::new(&p.path).join(&m.0);
                    if path.starts_with(inner) {
                        seen += 1;
                        assert!(
                            p.secret.is_some(),
                            "{roots:?}: {} not secret",
                            path.display()
                        );
                    }
                }
            }
            assert_eq!(seen, 2, "{roots:?}: members under {}", inner.display());
        }
        for b in &survey.blobs {
            for c in &b.copies {
                assert!(
                    !inners.iter().any(|i| Path::new(&c.path).starts_with(i)),
                    "{roots:?}: {} was hashed",
                    c.path
                );
            }
        }
        let p = plan(&res, &PlanOptions::new(7, far_future()));
        let lanes = p.lanes.as_ref().expect("lanes");
        for lp in &lanes.pieces {
            let sp = &survey.pieces[lp.scan_piece];
            if inners.iter().any(|i| Path::new(&sp.path).starts_with(i)) {
                assert_eq!(lp.lane, Lane::Keep, "{roots:?}: {}", sp.path);
                assert!(lp.secret.is_some(), "{roots:?}: {}", sp.path);
            }
        }
    }
}
