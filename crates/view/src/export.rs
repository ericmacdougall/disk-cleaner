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
                ExportError::Io(format!("cannot create {shown}: {e}"))
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
        return Err(ExportError::Io(format!("cannot write {shown}: {e}")));
    }
    Ok(())
}

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

/// Export into `dir` under `prunedisk-plan-<stamp>.json`, or `-2`, `-3`... when taken.
pub fn export_to_dir(dir: &Path, plan: &Plan, now: i64) -> Result<PathBuf, ExportError> {
    let base = format!("prunedisk-plan-{}", stamp(now));
    for n in 1..=1000u32 {
        let name = if n == 1 {
            format!("{base}.json")
        } else {
            format!("{base}-{n}.json")
        };
        let path = dir.join(name);
        match write_plan_new(&path, plan) {
            Ok(()) => return Ok(path),
            Err(ExportError::Exists(_)) => continue,
            Err(e) => return Err(e),
        }
    }
    Err(ExportError::Io(format!(
        "no free file name for the plan in {}",
        display_name(&dir.to_string_lossy())
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
        let taken = dir.path().join(format!("prunedisk-plan-{}-2.json", stamp(EPOCH)));
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
