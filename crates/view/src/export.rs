//! Plan export: the plan JSON written to a new file, never over an existing one.

use crate::sanitize::display_name;
use disk_cleaner_core::Plan;
use std::io::Write;
use std::path::{Path, PathBuf};

/// Why an export failed (display-safe).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExportError {
    /// The target exists; nothing was written.
    Exists(String),
    Io(String),
}

impl std::fmt::Display for ExportError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ExportError::Exists(m) | ExportError::Io(m) => f.write_str(m),
        }
    }
}

impl std::error::Error for ExportError {}

/// Write `plan` as pretty JSON to `path`, which must not exist (`create_new`). A file this call
/// created and could not finish is removed again; nothing else is touched.
#[allow(clippy::disallowed_methods)] // create_new only, listed in scripts/readonly-allow.txt
pub fn write_plan_new(path: &Path, plan: &Plan) -> Result<(), ExportError> {
    let shown = display_name(&path.to_string_lossy());
    let text = serde_json::to_vec_pretty(plan)
        .map_err(|e| ExportError::Io(format!("cannot encode the plan: {e}")))?;
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .map_err(|e| {
            if e.kind() == std::io::ErrorKind::AlreadyExists {
                ExportError::Exists(format!("{shown} already exists; nothing was written"))
            } else {
                ExportError::Io(format!("cannot create {shown}: {e}; nothing was written"))
            }
        })?;
    let done = file
        .write_all(&text)
        .and_then(|_| file.write_all(b"\n"))
        .and_then(|_| file.sync_all());
    if let Err(e) = done {
        drop(file);
        // Only the file this call just created.
        let _ = std::fs::remove_file(path);
        return Err(ExportError::Io(format!(
            "cannot write {shown}: {e}; nothing was kept"
        )));
    }
    Ok(())
}

/// How many names `export_to_dir` tries before it gives up: the stamped name, then `-2` up to
/// this. A folder that already holds this many plans for one second is refused.
pub const EXPORT_NAME_TRIES: u32 = 1000;

/// `YYYYMMDD-HHMMSS` (UTC) for unix seconds.
pub fn stamp(unix: i64) -> String {
    let days = unix.div_euclid(86_400);
    let secs = unix.rem_euclid(86_400);
    // Civil date from days since 1970-01-01 (Howard Hinnant's algorithm).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!(
        "{y:04}{m:02}{d:02}-{:02}{:02}{:02}",
        secs / 3600,
        secs / 60 % 60,
        secs % 60
    )
}

/// Export into `dir` under `prunedisk-plan-<stamp>.json`, or `-2`, `-3`... when taken. The app
/// names the file, so no dialog ever asks to replace one (click-test N2). A name that is taken
/// by anything (a file, a folder, a symlink, even a dangling one) is skipped, never followed or
/// replaced; `write_plan_new`'s create-new open stays the final guard against a name taken in
/// between (two exports in the same second). Nothing outside `dir` is written.
pub fn export_to_dir(dir: &Path, plan: &Plan, now: i64) -> Result<PathBuf, ExportError> {
    export_to_dir_within(dir, plan, now, EXPORT_NAME_TRIES)
}

fn export_to_dir_within(
    dir: &Path,
    plan: &Plan,
    now: i64,
    tries: u32,
) -> Result<PathBuf, ExportError> {
    let shown = display_name(&dir.to_string_lossy());
    if !dir.is_dir() {
        return Err(ExportError::Io(format!(
            "{shown} is not a folder; nothing was written"
        )));
    }
    let base = format!("prunedisk-plan-{}", stamp(now));
    for n in 1..=tries {
        let name = if n == 1 {
            format!("{base}.json")
        } else {
            format!("{base}-{n}.json")
        };
        let path = dir.join(name);
        // Taken by anything, including a symlink to nowhere: next name. (On Windows a folder
        // with the name fails create-new as "access denied", not "exists".)
        if path.symlink_metadata().is_ok() {
            continue;
        }
        match write_plan_new(&path, plan) {
            Ok(()) => return Ok(path),
            Err(ExportError::Exists(_)) => continue,
            Err(e) => return Err(e),
        }
    }
    Err(ExportError::Io(format!(
        "no free file name for the plan in {shown} ({tries} names tried); nothing was written"
    )))
}

#[cfg(test)]
#[allow(clippy::disallowed_methods)] // tests write fixtures
mod tests {
    use super::*;
    use crate::dev::{make_plan, PlanInputs};
    use crate::synth::{synthetic_scan, EPOCH, SEED};
    use disk_cleaner_core::{Config, ProcessCheck};

    fn plan() -> Plan {
        let scan = synthetic_scan(20, SEED);
        let inputs = PlanInputs {
            config: Config::default(),
            window_source: "default".into(),
        };
        make_plan(&scan, &inputs, EPOCH, ProcessCheck::none_running())
    }

    #[test]
    fn stamps_are_utc_dates() {
        assert_eq!(stamp(0), "19700101-000000");
        assert_eq!(stamp(EPOCH), "20260101-000000");
        assert_eq!(stamp(951_782_400 + 3661), "20000229-010101");
    }

    #[test]
    fn an_existing_file_is_never_overwritten() {
        let dir = tempfile::tempdir().unwrap();
        let p = plan();
        let first = export_to_dir(dir.path(), &p, EPOCH).unwrap();
        let taken = dir
            .path()
            .join(format!("prunedisk-plan-{}-2.json", stamp(EPOCH)));
        std::fs::write(&taken, b"user data").unwrap();
        let second = export_to_dir(dir.path(), &p, EPOCH).unwrap();
        assert_ne!(first, second);
        assert_ne!(second, taken);
        assert_eq!(std::fs::read(&taken).unwrap(), b"user data");
        let text = std::fs::read_to_string(&second).unwrap();
        let back: Plan = serde_json::from_str(&text).unwrap();
        assert!(back.dry_run);
        assert!(matches!(
            write_plan_new(&first, &p),
            Err(ExportError::Exists(_))
        ));
    }

    /// The names in `dir`, sorted.
    fn names(dir: &Path) -> Vec<String> {
        let mut v: Vec<String> = std::fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        v.sort();
        v
    }

    fn nth(n: u32) -> String {
        if n == 1 {
            format!("prunedisk-plan-{}.json", stamp(EPOCH))
        } else {
            format!("prunedisk-plan-{}-{n}.json", stamp(EPOCH))
        }
    }

    #[test]
    fn the_name_and_its_first_suffix_taken_gives_the_next_one() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(nth(1)), b"one").unwrap();
        std::fs::write(dir.path().join(nth(2)), b"two").unwrap();
        let got = export_to_dir(dir.path(), &plan(), EPOCH).unwrap();
        assert_eq!(got, dir.path().join(nth(3)));
        assert_eq!(std::fs::read(dir.path().join(nth(1))).unwrap(), b"one");
        assert_eq!(std::fs::read(dir.path().join(nth(2))).unwrap(), b"two");
        let mut want = vec![nth(1), nth(2), nth(3)];
        want.sort();
        assert_eq!(names(dir.path()), want);
    }

    #[test]
    fn a_folder_with_the_name_is_skipped_and_left_empty() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join(nth(1))).unwrap();
        let got = export_to_dir(dir.path(), &plan(), EPOCH).unwrap();
        assert_eq!(got, dir.path().join(nth(2)));
        assert!(names(&dir.path().join(nth(1))).is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn a_symlink_with_the_name_is_never_followed() {
        let dir = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        // A dangling link (create-new through it would create the target) and a link to a
        // file outside (writing through it would change that file).
        let missing = outside.path().join("would-be-created");
        let victim = outside.path().join("victim");
        std::fs::write(&victim, b"keep me").unwrap();
        std::os::unix::fs::symlink(&missing, dir.path().join(nth(1))).unwrap();
        std::os::unix::fs::symlink(&victim, dir.path().join(nth(2))).unwrap();
        let got = export_to_dir(dir.path(), &plan(), EPOCH).unwrap();
        assert_eq!(got, dir.path().join(nth(3)));
        assert!(
            missing.symlink_metadata().is_err(),
            "the dangling link's target was created"
        );
        assert_eq!(std::fs::read(&victim).unwrap(), b"keep me");
        assert_eq!(names(outside.path()), vec!["victim".to_string()]);
        assert_eq!(
            std::fs::read_link(dir.path().join(nth(1))).unwrap(),
            missing
        );
    }

    #[test]
    fn two_exports_in_the_same_second_write_two_files() {
        let dir = tempfile::tempdir().unwrap();
        let p = std::sync::Arc::new(plan());
        let path = dir.path().to_path_buf();
        let threads: Vec<_> = (0..8)
            .map(|_| {
                let (p, path) = (std::sync::Arc::clone(&p), path.clone());
                std::thread::spawn(move || export_to_dir(&path, &p, EPOCH).unwrap())
            })
            .collect();
        let mut got: Vec<PathBuf> = threads.into_iter().map(|t| t.join().unwrap()).collect();
        got.sort();
        got.dedup();
        assert_eq!(got.len(), 8, "two exports shared a file: {got:?}");
        for f in &got {
            let back: Plan = serde_json::from_str(&std::fs::read_to_string(f).unwrap()).unwrap();
            assert!(back.dry_run);
        }
        assert_eq!(names(dir.path()).len(), 8);
    }

    #[test]
    fn the_name_search_is_bounded_and_writes_nothing_when_all_are_taken() {
        let dir = tempfile::tempdir().unwrap();
        for n in 1..=3 {
            std::fs::write(dir.path().join(nth(n)), b"taken").unwrap();
        }
        let before = names(dir.path());
        let e = export_to_dir_within(dir.path(), &plan(), EPOCH, 3).unwrap_err();
        assert!(e.to_string().contains("3 names tried"), "{e}");
        assert!(e.to_string().ends_with("nothing was written"), "{e}");
        assert_eq!(names(dir.path()), before);
        assert_eq!(EXPORT_NAME_TRIES, 1000);
    }

    #[cfg(unix)]
    #[test]
    fn a_folder_that_cannot_be_written_gives_a_clear_message_and_no_file() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let ro = dir.path().join("read-only");
        std::fs::create_dir(&ro).unwrap();
        std::fs::set_permissions(&ro, std::fs::Permissions::from_mode(0o555)).unwrap();
        // Root (some containers) ignores the mode: nothing to check there.
        let probe = ro.join("probe");
        if std::fs::write(&probe, b"").is_ok() {
            std::fs::remove_file(&probe).unwrap();
            std::fs::set_permissions(&ro, std::fs::Permissions::from_mode(0o755)).unwrap();
            return;
        }
        let e = export_to_dir(&ro, &plan(), EPOCH).unwrap_err();
        std::fs::set_permissions(&ro, std::fs::Permissions::from_mode(0o755)).unwrap();
        let m = e.to_string();
        assert!(m.starts_with("cannot create "), "{m}");
        assert!(m.ends_with("nothing was written"), "{m}");
        assert!(names(&ro).is_empty());
    }

    #[test]
    fn a_file_picked_as_the_folder_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("not-a-folder");
        std::fs::write(&file, b"x").unwrap();
        let e = export_to_dir(&file, &plan(), EPOCH).unwrap_err();
        assert!(e.to_string().contains("is not a folder"), "{e}");
        assert_eq!(std::fs::read(&file).unwrap(), b"x");
    }

    #[test]
    fn a_missing_folder_is_an_error_not_a_crash() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("no").join("such");
        assert!(matches!(
            export_to_dir(&missing, &plan(), EPOCH),
            Err(ExportError::Io(_))
        ));
    }
}
