//! Settings: the two plan settings in core's config file, and the app's own scan settings.
//!
//! The core config file is the one the CLI reads without `--config`. Saving changes only the keys
//! the user changed (comments and other keys are kept), refuses when the file changed since it
//! was loaded, and replaces the file atomically. The protocol, step by step:
//!
//! 1. take an exclusive lock on `config.toml.lock` beside the file (another app window that is
//!    saving makes this one refuse);
//! 2. refuse when `config.toml` or `config.toml.bak` is a symlink or not a regular file;
//! 3. read the current text: when it already holds the requested values, succeed without
//!    writing; when it differs from the loaded text, or core cannot parse it, refuse;
//! 4. write the new text to a temp file (`create_new`, the original's permissions on Unix) and
//!    sync it;
//! 5. write the loaded text to `config.toml.bak` the same way (temp, then rename);
//! 6. re-read `config.toml` and refuse if it changed;
//! 7. rename the temp over `config.toml`: the commit point. Any failure before it leaves the
//!    config unchanged and removes the temp;
//! 8. sync the folder (Unix); a failure there reports "saved; could not confirm it is on disk".
//!
//! Limitation: an editor that does not take the lock and writes between steps 6 and 7 loses
//! that write; `config.toml.bak` holds the text this app loaded, not that editor's change.

use crate::sanitize::display_name;
use disk_cleaner_core::config::parse as parse_core;
use disk_cleaner_core::Config;
use serde::{Deserialize, Serialize};
use std::fs::File;
use std::io::{Read, Write};
use std::ops::RangeInclusive;
use std::path::{Path, PathBuf};

/// Allowed `active_window_days` values in the app.
pub const WINDOW_DAYS: RangeInclusive<u64> = 1..=3650;
/// Allowed `max_git_admin_bytes` values in the app: 1 MiB to 1 TiB.
pub const GIT_ADMIN_BYTES: RangeInclusive<u64> = (1 << 20)..=(1 << 40);

/// Why loading or saving failed. Messages are display-safe.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SettingsError {
    /// A value is out of range or not a number.
    Invalid(String),
    /// Shortening the active window needs the user's confirmation.
    NeedsConfirmation { old: u64, new: u64 },
    /// Not saved: the file changed, is a symlink, cannot be parsed, or is locked.
    Refused(String),
    /// Not saved: a read or write failed before the commit point.
    Io(String),
}

impl std::fmt::Display for SettingsError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SettingsError::Invalid(m) | SettingsError::Refused(m) | SettingsError::Io(m) => {
                f.write_str(m)
            }
            SettingsError::NeedsConfirmation { old, new } => write!(
                f,
                "shortening the active window from {old} to {new} days makes more projects \
                 look idle; confirm to save"
            ),
        }
    }
}

impl std::error::Error for SettingsError {}

/// The config file as Settings loaded it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Loaded {
    pub path: PathBuf,
    /// The file's text, or `None` when there was no file.
    pub text: Option<String>,
    pub config: Config,
}

/// What Settings shows.
#[derive(Debug, Clone, Serialize)]
pub struct SettingsView {
    pub path: String,
    pub file_exists: bool,
    pub active_window_days: u64,
    pub active_window_in_range: bool,
    pub max_git_admin_bytes: u64,
    pub max_git_admin_in_range: bool,
    /// Read only here, with their source.
    pub trust_partial_process_check: bool,
    pub accept_incomplete_coverage: bool,
    pub source: String,
}

impl Loaded {
    pub fn view(&self) -> SettingsView {
        let c = &self.config;
        SettingsView {
            path: display_name(&self.path.to_string_lossy()),
            file_exists: self.text.is_some(),
            active_window_days: c.active_window_days,
            active_window_in_range: WINDOW_DAYS.contains(&c.active_window_days),
            max_git_admin_bytes: c.max_git_admin_bytes,
            max_git_admin_in_range: GIT_ADMIN_BYTES.contains(&c.max_git_admin_bytes),
            trust_partial_process_check: c.trust_partial_process_check,
            accept_incomplete_coverage: c.accept_incomplete_coverage,
            source: if self.text.is_some() {
                display_name(&self.path.to_string_lossy())
            } else {
                "defaults (no config file yet)".to_string()
            },
        }
    }

    /// Where the plan's active window came from, worded as the CLI words it.
    pub fn window_source(&self) -> String {
        if self.text.is_some() {
            format!("config {}", self.path.display())
        } else {
            "default".to_string()
        }
    }
}

fn shown(p: &Path) -> String {
    display_name(&p.to_string_lossy())
}

/// Read a file's text, or `None` when it does not exist.
fn read_optional(path: &Path) -> Result<Option<String>, SettingsError> {
    match File::open(path) {
        Ok(mut f) => {
            let mut s = String::new();
            f.read_to_string(&mut s)
                .map_err(|e| SettingsError::Io(format!("cannot read {}: {e}", shown(path))))?;
            Ok(Some(s))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(SettingsError::Io(format!(
            "cannot read {}: {e}",
            shown(path)
        ))),
    }
}

/// Load the config file at `path` (core's default path in the app).
pub fn load_core(path: &Path) -> Result<Loaded, SettingsError> {
    let text = read_optional(path)?;
    let config = match &text {
        Some(t) => parse_core(t, path).map_err(|e| {
            SettingsError::Refused(display_name(&format!(
                "{e}; fix or remove the file by hand, the app will not overwrite it"
            )))
        })?,
        None => Config::default(),
    };
    Ok(Loaded {
        path: path.to_path_buf(),
        text,
        config,
    })
}

/// Core's default config path, the one the CLI reads without `--config`.
pub fn default_core_path() -> Result<PathBuf, SettingsError> {
    disk_cleaner_core::config::default_config_path()
        .ok_or_else(|| SettingsError::Io("cannot find this system's settings folder".to_string()))
}

/// The values the user changed (unchanged keys are `None` and are not written).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
pub struct CoreChanges {
    pub active_window_days: Option<u64>,
    pub max_git_admin_bytes: Option<u64>,
}

impl CoreChanges {
    /// Check ranges, and that a shorter window was confirmed.
    pub fn validate(&self, current: &Config, confirm_shorten: bool) -> Result<(), SettingsError> {
        if let Some(d) = self.active_window_days {
            if !WINDOW_DAYS.contains(&d) {
                return Err(SettingsError::Invalid(format!(
                    "the active window must be {} to {} days",
                    WINDOW_DAYS.start(),
                    WINDOW_DAYS.end()
                )));
            }
            if d < current.active_window_days && !confirm_shorten {
                return Err(SettingsError::NeedsConfirmation {
                    old: current.active_window_days,
                    new: d,
                });
            }
        }
        if let Some(b) = self.max_git_admin_bytes {
            if !GIT_ADMIN_BYTES.contains(&b) {
                return Err(SettingsError::Invalid(
                    "the git folder limit must be 1 MiB to 1 TiB".to_string(),
                ));
            }
        }
        Ok(())
    }

    fn held_by(&self, c: &Config) -> bool {
        self.active_window_days
            .is_none_or(|d| d == c.active_window_days)
            && self
                .max_git_admin_bytes
                .is_none_or(|b| b == c.max_git_admin_bytes)
    }

    fn is_empty(&self) -> bool {
        self.active_window_days.is_none() && self.max_git_admin_bytes.is_none()
    }
}

/// The text with only the changed keys set; everything else (comments, other keys) kept.
pub fn edit_text(old: Option<&str>, changes: &CoreChanges) -> Result<String, SettingsError> {
    let mut doc: toml_edit::DocumentMut = old
        .unwrap_or("")
        .parse()
        .map_err(|e| SettingsError::Refused(format!("the config file is not valid TOML: {e}")))?;
    let to_i64 = |v: u64| {
        i64::try_from(v).map_err(|_| SettingsError::Invalid("value too large".to_string()))
    };
    if let Some(d) = changes.active_window_days {
        set_int(&mut doc, "active_window_days", to_i64(d)?);
    }
    if let Some(b) = changes.max_git_admin_bytes {
        set_int(&mut doc, "max_git_admin_bytes", to_i64(b)?);
    }
    Ok(doc.to_string())
}

/// Set a top-level integer, keeping the old value's spacing and trailing comment.
fn set_int(doc: &mut toml_edit::DocumentMut, key: &str, v: i64) {
    let mut new = toml_edit::Value::from(v);
    if let Some(old) = doc.get(key).and_then(|i| i.as_value()) {
        *new.decor_mut() = old.decor().clone();
    }
    doc[key] = toml_edit::Item::Value(new);
}

/// What a save did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SaveOutcome {
    /// Written and the folder synced.
    Saved(Loaded),
    /// The file already held these values: nothing was written.
    Unchanged(Loaded),
    /// Written (the rename happened) but the folder sync failed.
    SavedUnconfirmed(Loaded, String),
}

impl SaveOutcome {
    pub fn loaded(&self) -> &Loaded {
        match self {
            SaveOutcome::Saved(l)
            | SaveOutcome::Unchanged(l)
            | SaveOutcome::SavedUnconfirmed(l, _) => l,
        }
    }
}

/// Test hooks into the save protocol.
#[derive(Default)]
pub struct SaveHooks<'a> {
    /// Runs before step 6 (the re-read).
    pub before_recheck: Option<&'a dyn Fn()>,
    /// Runs between steps 6 and 7; an error stands for a failed rename.
    pub before_commit: Option<&'a dyn Fn() -> std::io::Result<()>>,
    /// Makes step 8 fail.
    pub fail_dir_sync: bool,
}

fn sibling(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path
        .file_name()
        .map(|n| n.to_os_string())
        .unwrap_or_default();
    name.push(suffix);
    path.with_file_name(name)
}

/// Refuse a symlink or anything but a regular file (absent is fine).
fn check_plain(path: &Path) -> Result<(), SettingsError> {
    match std::fs::symlink_metadata(path) {
        Ok(md) if md.file_type().is_symlink() => Err(SettingsError::Refused(format!(
            "{} is a symbolic link; the app does not write through links",
            shown(path)
        ))),
        Ok(md) if !md.is_file() => Err(SettingsError::Refused(format!(
            "{} is not a regular file",
            shown(path)
        ))),
        Ok(_) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(SettingsError::Io(format!(
            "cannot check {}: {e}",
            shown(path)
        ))),
    }
}

/// Step 1: create (if needed) and lock `<file>.lock`. The lock file stays on disk by design;
/// the lock is released when the returned handle is dropped, or by the OS if the app dies.
#[allow(clippy::disallowed_methods)] // creates the lock file, listed in scripts/readonly-allow.txt
pub fn lock_config(path: &Path) -> Result<File, SettingsError> {
    let lock = sibling(path, ".lock");
    check_plain(&lock)?;
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&lock)
        .map_err(|e| {
            SettingsError::Io(format!("not saved: cannot create {}: {e}", shown(&lock)))
        })?;
    match file.try_lock() {
        Ok(()) => Ok(file),
        Err(std::fs::TryLockError::WouldBlock) => Err(SettingsError::Refused(
            "not saved: another PruneDisk window is saving settings; try again".to_string(),
        )),
        Err(std::fs::TryLockError::Error(e)) => Err(SettingsError::Io(format!(
            "not saved: cannot lock {}: {e}",
            shown(&lock)
        ))),
    }
}

/// A temp file beside `target`, created with `create_new`, holding `text`, synced.
#[allow(clippy::disallowed_methods)] // only called from the listed save functions
fn write_temp(target: &Path, text: &str, perms_from: Option<&Path>) -> std::io::Result<PathBuf> {
    let mut last_err = None;
    for attempt in 0..16u32 {
        let tmp = sibling(target, &format!(".{}.{attempt}.tmp", std::process::id()));
        let mut opts = std::fs::OpenOptions::new();
        opts.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o600);
        }
        let mut f = match opts.open(&tmp) {
            Ok(f) => f,
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                last_err = Some(e);
                continue;
            }
            Err(e) => return Err(e),
        };
        let res = (|| {
            #[cfg(unix)]
            {
                if let Some(src) = perms_from {
                    if let Ok(md) = std::fs::metadata(src) {
                        f.set_permissions(md.permissions())?;
                    }
                }
            }
            #[cfg(not(unix))]
            {
                let _ = perms_from;
            }
            f.write_all(text.as_bytes())?;
            f.sync_all()
        })();
        if let Err(e) = res {
            drop(f);
            let _ = std::fs::remove_file(&tmp);
            return Err(e);
        }
        return Ok(tmp);
    }
    Err(last_err.unwrap_or_else(|| std::io::Error::other("no free temp name")))
}

/// Save the changed keys of the core config file (steps 1 to 8 above).
#[allow(clippy::disallowed_methods)] // listed in scripts/readonly-allow.txt
pub fn save_core_config(
    loaded: &Loaded,
    changes: &CoreChanges,
    confirm_shorten: bool,
    hooks: &SaveHooks<'_>,
) -> Result<SaveOutcome, SettingsError> {
    let path = &loaded.path;
    let bak = sibling(path, ".bak");
    changes.validate(&loaded.config, confirm_shorten)?;
    let dir = path
        .parent()
        .ok_or_else(|| SettingsError::Io("not saved: the config path has no folder".into()))?;
    std::fs::create_dir_all(dir)
        .map_err(|e| SettingsError::Io(format!("not saved: cannot create {}: {e}", shown(dir))))?;
    // 1
    let _lock = lock_config(path)?;
    // 2
    check_plain(path)?;
    check_plain(&bak)?;
    // 3
    let current = read_optional(path)?;
    let current_config = match &current {
        Some(t) => parse_core(t, path).map_err(|e| {
            SettingsError::Refused(display_name(&format!(
                "not saved: {e}; the app will not overwrite a file core cannot read"
            )))
        })?,
        None => Config::default(),
    };
    if changes.is_empty() || (current.is_some() && changes.held_by(&current_config)) {
        return Ok(SaveOutcome::Unchanged(Loaded {
            path: path.clone(),
            text: current,
            config: current_config,
        }));
    }
    if current != loaded.text {
        return Err(SettingsError::Refused(format!(
            "not saved: {} changed since Settings opened it; reopen Settings to see the new \
             values",
            shown(path)
        )));
    }
    let new_text = edit_text(current.as_deref(), changes)?;
    let new_config = parse_core(&new_text, path).map_err(|e| {
        SettingsError::Refused(display_name(&format!(
            "not saved: the new text is invalid: {e}"
        )))
    })?;
    if !changes.held_by(&new_config) {
        return Err(SettingsError::Refused(
            "not saved: the new text does not hold the new values".to_string(),
        ));
    }
    // 4
    let tmp = write_temp(path, &new_text, current.as_ref().map(|_| path.as_path()))
        .map_err(|e| SettingsError::Io(format!("not saved: cannot write a temp file: {e}")))?;
    let fail = |msg: String| {
        let _ = std::fs::remove_file(&tmp);
        SettingsError::Io(msg)
    };
    // 5
    if let Some(old) = &current {
        let bak_tmp = write_temp(&bak, old, Some(path))
            .map_err(|e| fail(format!("not saved: cannot write the backup copy: {e}")))?;
        if let Err(e) = std::fs::rename(&bak_tmp, &bak) {
            let _ = std::fs::remove_file(&bak_tmp);
            return Err(fail(format!(
                "not saved: cannot write {}: {e}",
                shown(&bak)
            )));
        }
    }
    // 6
    if let Some(h) = hooks.before_recheck {
        h();
    }
    let again = read_optional(path).map_err(|e| fail(e.to_string()))?;
    if again != current {
        let _ = std::fs::remove_file(&tmp);
        return Err(SettingsError::Refused(format!(
            "not saved: {} changed while saving; reopen Settings",
            shown(path)
        )));
    }
    // 7
    let commit = match hooks.before_commit {
        Some(h) => h(),
        None => Ok(()),
    }
    .and_then(|_| std::fs::rename(&tmp, path));
    if let Err(e) = commit {
        return Err(fail(format!(
            "not saved: cannot replace {}: {e}",
            shown(path)
        )));
    }
    let saved = Loaded {
        path: path.clone(),
        text: Some(new_text),
        config: new_config,
    };
    // 8
    match sync_dir(dir, hooks.fail_dir_sync) {
        Ok(()) => Ok(SaveOutcome::Saved(saved)),
        Err(e) => Ok(SaveOutcome::SavedUnconfirmed(
            saved,
            format!("saved; could not confirm it is on disk ({e})"),
        )),
    }
}

fn sync_dir(dir: &Path, fail: bool) -> std::io::Result<()> {
    if fail {
        return Err(std::io::Error::other("injected"));
    }
    #[cfg(unix)]
    {
        File::open(dir)?.sync_all()
    }
    #[cfg(not(unix))]
    {
        let _ = dir;
        Ok(())
    }
}

/// The app's own scan settings.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AppSettings {
    /// How many of the biggest files to list.
    pub top_files: usize,
    /// Output files at least this many MiB are listed wherever they are.
    pub min_output_mib: u64,
    /// Do not enter other file systems.
    pub one_file_system: bool,
}

impl Default for AppSettings {
    fn default() -> Self {
        AppSettings {
            top_files: 200,
            min_output_mib: 100,
            one_file_system: true,
        }
    }
}

impl AppSettings {
    pub fn validate(&self) -> Result<(), SettingsError> {
        if !(10..=1000).contains(&self.top_files) {
            return Err(SettingsError::Invalid(
                "top files must be 10 to 1,000".to_string(),
            ));
        }
        if !(1..=1_048_576).contains(&self.min_output_mib) {
            return Err(SettingsError::Invalid(
                "the output size must be 1 MiB to 1 TiB".to_string(),
            ));
        }
        Ok(())
    }
}

/// The app settings file beside core's config file.
pub fn app_settings_path(core_path: &Path) -> PathBuf {
    core_path.with_file_name("prunedisk-app.json")
}

/// Load the app settings; a missing or unreadable file gives the defaults and a note.
pub fn load_app_settings(path: &Path) -> (AppSettings, Option<String>) {
    match read_optional(path) {
        Ok(None) => (AppSettings::default(), None),
        Ok(Some(t)) => match serde_json::from_str::<AppSettings>(&t) {
            Ok(s) if s.validate().is_ok() => (s, None),
            _ => (
                AppSettings::default(),
                Some(format!("{} is not valid; using defaults", shown(path))),
            ),
        },
        Err(e) => (AppSettings::default(), Some(e.to_string())),
    }
}

/// Save the app settings (temp file, then rename).
#[allow(clippy::disallowed_methods)] // listed in scripts/readonly-allow.txt
pub fn save_app_settings(path: &Path, s: &AppSettings) -> Result<(), SettingsError> {
    s.validate()?;
    check_plain(path)?;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| {
            SettingsError::Io(format!("not saved: cannot create {}: {e}", shown(dir)))
        })?;
    }
    let text = serde_json::to_string_pretty(s)
        .map_err(|e| SettingsError::Io(format!("not saved: {e}")))?;
    let tmp = write_temp(path, &text, None)
        .map_err(|e| SettingsError::Io(format!("not saved: cannot write a temp file: {e}")))?;
    if let Err(e) = std::fs::rename(&tmp, path) {
        let _ = std::fs::remove_file(&tmp);
        return Err(SettingsError::Io(format!(
            "not saved: cannot replace {}: {e}",
            shown(path)
        )));
    }
    Ok(())
}

#[cfg(test)]
#[allow(clippy::disallowed_methods)] // tests write fixtures
mod tests {
    use super::*;

    const MIB: u64 = 1 << 20;

    fn setup(text: Option<&str>) -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("disk-cleaner").join("config.toml");
        if let Some(t) = text {
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(&p, t).unwrap();
        }
        (dir, p)
    }

    fn days(d: u64) -> CoreChanges {
        CoreChanges {
            active_window_days: Some(d),
            max_git_admin_bytes: None,
        }
    }

    fn temps_left(p: &Path) -> Vec<String> {
        std::fs::read_dir(p.parent().unwrap())
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|n| n.ends_with(".tmp"))
            .collect()
    }

    #[test]
    fn round_trip_keeps_comments_and_other_keys() {
        let original =
            "# my settings\nactive_window_days = 7 # a week\ntrust_partial_process_check = true\n";
        let (_d, p) = setup(Some(original));
        let loaded = load_core(&p).unwrap();
        let out = save_core_config(&loaded, &days(30), false, &SaveHooks::default()).unwrap();
        assert!(matches!(out, SaveOutcome::Saved(_)));
        let text = std::fs::read_to_string(&p).unwrap();
        assert!(text.contains("# my settings"), "{text}");
        assert!(text.contains("active_window_days = 30 # a week"), "{text}");
        assert!(
            text.contains("trust_partial_process_check = true"),
            "{text}"
        );
        let again = load_core(&p).unwrap();
        assert_eq!(again.config.active_window_days, 30);
        assert!(again.config.trust_partial_process_check);
        assert_eq!(
            std::fs::read_to_string(sibling(&p, ".bak")).unwrap(),
            original
        );
        assert!(temps_left(&p).is_empty());
    }

    #[test]
    fn a_missing_file_is_created_with_its_folder() {
        let (_d, p) = setup(None);
        let loaded = load_core(&p).unwrap();
        assert_eq!(loaded.text, None);
        save_core_config(&loaded, &days(14), false, &SaveHooks::default()).unwrap();
        assert_eq!(load_core(&p).unwrap().config.active_window_days, 14);
        assert!(
            !sibling(&p, ".bak").exists(),
            "no backup of a file that did not exist"
        );
    }

    #[test]
    fn ranges_and_shortening_are_checked() {
        let (_d, p) = setup(Some("active_window_days = 30\n"));
        let loaded = load_core(&p).unwrap();
        let h = SaveHooks::default();
        for bad in [0, 3651] {
            assert!(matches!(
                save_core_config(&loaded, &days(bad), true, &h),
                Err(SettingsError::Invalid(_))
            ));
        }
        let big = CoreChanges {
            active_window_days: None,
            max_git_admin_bytes: Some(MIB - 1),
        };
        assert!(matches!(
            save_core_config(&loaded, &big, false, &h),
            Err(SettingsError::Invalid(_))
        ));
        assert_eq!(
            save_core_config(&loaded, &days(7), false, &h).unwrap_err(),
            SettingsError::NeedsConfirmation { old: 30, new: 7 }
        );
        assert_eq!(
            std::fs::read_to_string(&p).unwrap(),
            "active_window_days = 30\n"
        );
        save_core_config(&loaded, &days(7), true, &h).unwrap();
        assert_eq!(load_core(&p).unwrap().config.active_window_days, 7);
    }

    #[test]
    fn an_out_of_range_value_already_in_the_file_is_kept() {
        let (_d, p) = setup(Some(
            "active_window_days = 9000\nmax_git_admin_bytes = 10\n",
        ));
        let loaded = load_core(&p).unwrap();
        let v = loaded.view();
        assert!(!v.active_window_in_range);
        assert!(!v.max_git_admin_in_range);
        let change = CoreChanges {
            active_window_days: None,
            max_git_admin_bytes: Some(64 * MIB),
        };
        save_core_config(&loaded, &change, false, &SaveHooks::default()).unwrap();
        let after = load_core(&p).unwrap();
        assert_eq!(after.config.active_window_days, 9000);
        assert_eq!(after.config.max_git_admin_bytes, 64 * MIB);
    }

    #[test]
    fn an_unparsable_file_is_never_overwritten() {
        let bad = "active_window_days = 7\nrequire_full_coverage = true\n";
        let (_d, p) = setup(Some(bad));
        assert!(matches!(load_core(&p), Err(SettingsError::Refused(_))));
        // Even with a stale loaded copy, save refuses.
        let stale = Loaded {
            path: p.clone(),
            text: Some(bad.to_string()),
            config: Config::default(),
        };
        assert!(matches!(
            save_core_config(&stale, &days(30), false, &SaveHooks::default()),
            Err(SettingsError::Refused(_))
        ));
        assert_eq!(std::fs::read_to_string(&p).unwrap(), bad);
    }

    #[test]
    fn a_second_writer_from_the_same_loaded_text_is_refused() {
        let (_d, p) = setup(Some("active_window_days = 7\n"));
        let a = load_core(&p).unwrap();
        let b = load_core(&p).unwrap();
        save_core_config(&a, &days(30), false, &SaveHooks::default()).unwrap();
        assert!(matches!(
            save_core_config(&b, &days(60), false, &SaveHooks::default()),
            Err(SettingsError::Refused(_))
        ));
        assert_eq!(load_core(&p).unwrap().config.active_window_days, 30);
    }

    #[test]
    fn a_retry_with_values_already_saved_succeeds_without_writing() {
        let (_d, p) = setup(Some("active_window_days = 7\n"));
        let a = load_core(&p).unwrap();
        save_core_config(&a, &days(30), false, &SaveHooks::default()).unwrap();
        let before = std::fs::read_to_string(&p).unwrap();
        // The same request again from the old loaded copy (a lost response, retried).
        let out = save_core_config(&a, &days(30), false, &SaveHooks::default()).unwrap();
        assert!(matches!(out, SaveOutcome::Unchanged(_)));
        assert_eq!(out.loaded().config.active_window_days, 30);
        assert_eq!(std::fs::read_to_string(&p).unwrap(), before);
    }

    #[test]
    fn an_edit_before_the_recheck_is_refused() {
        let (_d, p) = setup(Some("active_window_days = 7\n"));
        let loaded = load_core(&p).unwrap();
        let p2 = p.clone();
        let edit = move || std::fs::write(&p2, "active_window_days = 9\n").unwrap();
        let hooks = SaveHooks {
            before_recheck: Some(&edit),
            ..SaveHooks::default()
        };
        assert!(matches!(
            save_core_config(&loaded, &days(30), false, &hooks),
            Err(SettingsError::Refused(_))
        ));
        assert_eq!(
            std::fs::read_to_string(&p).unwrap(),
            "active_window_days = 9\n"
        );
        assert!(temps_left(&p).is_empty());
    }

    #[test]
    fn an_edit_between_recheck_and_commit_is_lost_as_documented() {
        let (_d, p) = setup(Some("active_window_days = 7\n"));
        let loaded = load_core(&p).unwrap();
        let p2 = p.clone();
        let edit = move || {
            std::fs::write(&p2, "active_window_days = 9\n").unwrap();
            Ok(())
        };
        let hooks = SaveHooks {
            before_commit: Some(&edit),
            ..SaveHooks::default()
        };
        save_core_config(&loaded, &days(30), false, &hooks).unwrap();
        assert_eq!(load_core(&p).unwrap().config.active_window_days, 30);
        // The backup holds the text Settings loaded, not the editor's change.
        assert_eq!(
            std::fs::read_to_string(sibling(&p, ".bak")).unwrap(),
            "active_window_days = 7\n"
        );
    }

    #[test]
    fn a_failed_replace_leaves_the_config_and_no_temp() {
        let (_d, p) = setup(Some("active_window_days = 7\n"));
        let loaded = load_core(&p).unwrap();
        let fail = || Err(std::io::Error::other("injected"));
        let hooks = SaveHooks {
            before_commit: Some(&fail),
            ..SaveHooks::default()
        };
        match save_core_config(&loaded, &days(30), false, &hooks) {
            Err(SettingsError::Io(m)) => assert!(m.starts_with("not saved"), "{m}"),
            other => panic!("{other:?}"),
        }
        assert_eq!(
            std::fs::read_to_string(&p).unwrap(),
            "active_window_days = 7\n"
        );
        assert!(temps_left(&p).is_empty());
    }

    #[test]
    fn a_failed_folder_sync_is_reported_as_unconfirmed() {
        let (_d, p) = setup(Some("active_window_days = 7\n"));
        let loaded = load_core(&p).unwrap();
        let hooks = SaveHooks {
            fail_dir_sync: true,
            ..SaveHooks::default()
        };
        match save_core_config(&loaded, &days(30), false, &hooks).unwrap() {
            SaveOutcome::SavedUnconfirmed(l, m) => {
                assert_eq!(l.config.active_window_days, 30);
                assert!(m.contains("could not confirm"), "{m}");
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn a_held_lock_refuses_the_save() {
        let (_d, p) = setup(Some("active_window_days = 7\n"));
        let loaded = load_core(&p).unwrap();
        let held = lock_config(&p).unwrap();
        assert!(matches!(
            save_core_config(&loaded, &days(30), false, &SaveHooks::default()),
            Err(SettingsError::Refused(_))
        ));
        drop(held);
        save_core_config(&loaded, &days(30), false, &SaveHooks::default()).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn symlinks_are_refused_and_permissions_kept() {
        use std::os::unix::fs::PermissionsExt;
        let (d, p) = setup(Some("active_window_days = 7\n"));
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o640)).unwrap();
        let loaded = load_core(&p).unwrap();
        save_core_config(&loaded, &days(30), false, &SaveHooks::default()).unwrap();
        let mode = std::fs::metadata(&p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o640);

        let target = d.path().join("elsewhere.toml");
        std::fs::write(&target, "active_window_days = 7\n").unwrap();
        let link = d.path().join("linked").join("config.toml");
        std::fs::create_dir_all(link.parent().unwrap()).unwrap();
        std::os::unix::fs::symlink(&target, &link).unwrap();
        let l2 = load_core(&link).unwrap();
        assert!(matches!(
            save_core_config(&l2, &days(30), false, &SaveHooks::default()),
            Err(SettingsError::Refused(_))
        ));
        assert_eq!(
            std::fs::read_to_string(&target).unwrap(),
            "active_window_days = 7\n"
        );

        let bak = sibling(&p, ".bak");
        std::fs::remove_file(&bak).unwrap();
        std::os::unix::fs::symlink(&target, &bak).unwrap();
        let l3 = load_core(&p).unwrap();
        assert!(matches!(
            save_core_config(&l3, &days(60), false, &SaveHooks::default()),
            Err(SettingsError::Refused(_))
        ));
    }

    #[test]
    fn app_settings_round_trip_and_validate() {
        let (_d, p) = setup(None);
        let ap = app_settings_path(&p);
        assert_eq!(load_app_settings(&ap), (AppSettings::default(), None));
        let s = AppSettings {
            top_files: 50,
            min_output_mib: 10,
            one_file_system: false,
        };
        save_app_settings(&ap, &s).unwrap();
        assert_eq!(load_app_settings(&ap).0, s);
        let bad = AppSettings { top_files: 5, ..s };
        assert!(save_app_settings(&ap, &bad).is_err());
        std::fs::write(&ap, "{\"top_files\": 50}").unwrap();
        let (got, note) = load_app_settings(&ap);
        assert_eq!(got, AppSettings::default());
        assert!(note.is_some());
    }
}
