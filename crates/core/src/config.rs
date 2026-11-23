//! User settings: `active_window_days`, how recent activity must be for a project to count as
//! active (default 7), `max_git_admin_bytes`, how much of its own content one git directory may
//! hold and still be removed after a backup (default 64 MiB), and two opt-ins,
//! `trust_partial_process_check` and `accept_incomplete_coverage`; and `protected_paths`, extra
//! roots that are never removed (add-only).
//!
//! A config file that exists but cannot be understood is an error. It never falls back to a
//! default, because a silent fallback could shorten the window and make projects look idle.

use crate::error::{Error, Result};
use serde::Deserialize;
use std::path::{Path, PathBuf};

/// Default for `active_window_days`.
pub const DEFAULT_ACTIVE_WINDOW_DAYS: u64 = 7;

/// Default for `max_git_admin_bytes`: 64 MiB.
pub const DEFAULT_MAX_GIT_ADMIN_BYTES: u64 = 64 * 1024 * 1024;

/// Where a value came from, for reports.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConfigSource {
    /// Built-in default (no config file found).
    Default,
    /// Read from this file.
    File(PathBuf),
}

/// The settings.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    /// A project is active when it was touched within this many days.
    pub active_window_days: u64,
    /// Treat a partial process check (some processes unreadable) as sufficient to remove things.
    /// Off unless the user turns it on.
    pub trust_partial_process_check: bool,
    /// When the scan could not read some folders or skipped mount points, still offer
    /// repositories for removal after a backup (each reason says what was not read). Off unless
    /// the user turns it on: by default every repository's dependents are then unknown and it
    /// asks.
    pub accept_incomplete_coverage: bool,
    /// Most allocated bytes of its own files (everything but object files) one git directory may
    /// hold and still be removed after a backup; more asks.
    pub max_git_admin_bytes: u64,
    /// Extra protected roots (absolute or `~/`-relative): nothing inside or containing one is
    /// ever removed. Add-only: no key removes a built-in root.
    pub protected_paths: Vec<String>,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            active_window_days: DEFAULT_ACTIVE_WINDOW_DAYS,
            trust_partial_process_check: false,
            accept_incomplete_coverage: false,
            max_git_admin_bytes: DEFAULT_MAX_GIT_ADMIN_BYTES,
            protected_paths: Vec::new(),
        }
    }
}

/// The key an earlier version used for the opposite opt-in (strict coverage, now the default).
const RETIRED_COVERAGE_KEY: &str = "require_full_coverage";
/// The key an earlier build used to have `scan` verify object stores with git. There is no
/// verification of object data any more: every object-store file is to be backed up.
const RETIRED_VERIFY_KEY: &str = "verify_objects";

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawConfig {
    active_window_days: Option<u64>,
    trust_partial_process_check: Option<bool>,
    accept_incomplete_coverage: Option<bool>,
    max_git_admin_bytes: Option<u64>,
    protected_paths: Option<Vec<String>>,
    /// Read only to refuse it with a message naming its replacement, whatever its value.
    require_full_coverage: Option<toml::Value>,
    /// Read only to refuse it with a message saying why, whatever its value.
    verify_objects: Option<toml::Value>,
}

/// `<platform config dir>/disk-cleaner/config.toml`, if the platform config dir can be found.
pub fn default_config_path() -> Option<PathBuf> {
    let base: PathBuf = if cfg!(windows) {
        PathBuf::from(std::env::var_os("APPDATA")?)
    } else if cfg!(target_os = "macos") {
        PathBuf::from(std::env::var_os("HOME")?)
            .join("Library")
            .join("Application Support")
    } else if let Some(x) = std::env::var_os("XDG_CONFIG_HOME").filter(|v| !v.is_empty()) {
        PathBuf::from(x)
    } else {
        PathBuf::from(std::env::var_os("HOME")?).join(".config")
    };
    Some(base.join("disk-cleaner").join("config.toml"))
}

/// Parse config text. `origin` is only used in error messages.
pub fn parse(text: &str, origin: &Path) -> Result<Config> {
    let raw: RawConfig = toml::from_str(text).map_err(|e| Error::Config {
        path: origin.to_path_buf(),
        message: e.to_string(),
    })?;
    if raw.require_full_coverage.is_some() {
        return Err(Error::Config {
            path: origin.to_path_buf(),
            message: format!(
                "{RETIRED_COVERAGE_KEY} is no longer a setting: incomplete coverage now makes \
                 repositories ask by default; remove the key, or set \
                 accept_incomplete_coverage = true (or pass --accept-incomplete-coverage) to \
                 offer them for removal despite folders the scan could not read"
            ),
        });
    }
    if raw.verify_objects.is_some() {
        return Err(Error::Config {
            path: origin.to_path_buf(),
            message: format!(
                "{RETIRED_VERIFY_KEY} is no longer a setting: this tool defines no verification \
                 of object data, and every object-store file is to be backed up before a \
                 repository is removed; remove the key"
            ),
        });
    }
    let protected_paths = raw.protected_paths.unwrap_or_default();
    for p in &protected_paths {
        crate::protect::validate_config_path(p).map_err(|e| Error::Config {
            path: origin.to_path_buf(),
            message: format!("protected_paths: {e}"),
        })?;
    }
    Ok(Config {
        active_window_days: raw.active_window_days.unwrap_or(DEFAULT_ACTIVE_WINDOW_DAYS),
        trust_partial_process_check: raw.trust_partial_process_check.unwrap_or(false),
        accept_incomplete_coverage: raw.accept_incomplete_coverage.unwrap_or(false),
        max_git_admin_bytes: raw
            .max_git_admin_bytes
            .unwrap_or(DEFAULT_MAX_GIT_ADMIN_BYTES),
        protected_paths,
    })
}

/// Load the config.
///
/// With `explicit` (the `--config` flag) the file must exist. Without it the default location is
/// used when the file exists; a missing default file means defaults. Any file that is present
/// must parse.
pub fn load(explicit: Option<&Path>) -> Result<(Config, ConfigSource)> {
    let (path, must_exist) = match explicit {
        Some(p) => (p.to_path_buf(), true),
        None => match default_config_path() {
            Some(p) => (p, false),
            None => return Ok((Config::default(), ConfigSource::Default)),
        },
    };
    match std::fs::read_to_string(&path) {
        Ok(text) => Ok((parse(&text, &path)?, ConfigSource::File(path))),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound && !must_exist => {
            Ok((Config::default(), ConfigSource::Default))
        }
        Err(e) => Err(Error::Config {
            path,
            message: format!("cannot read config file: {e}"),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_is_seven_days() {
        assert_eq!(Config::default().active_window_days, 7);
        assert_eq!(
            parse("", Path::new("c.toml")).unwrap().active_window_days,
            7
        );
    }

    #[test]
    fn reads_the_value() {
        let c = parse("active_window_days = 14\n", Path::new("c.toml")).unwrap();
        assert_eq!(c.active_window_days, 14);
    }

    #[test]
    fn malformed_files_are_errors_not_defaults() {
        for bad in [
            "active_window_days = \"seven\"",
            "active_window_days = -3",
            "active_window_days = 1.5",
            "active_window_days = ",
            "this is not toml",
            "active_window_day = 3",
        ] {
            let e = parse(bad, Path::new("c.toml")).expect_err(bad);
            assert!(e.to_string().contains("c.toml"), "{e}");
        }
    }

    #[test]
    fn protected_paths_add_only() {
        // The key adds roots; it is validated like an override.
        let c = parse(
            "protected_paths = [\"/data/agents\", \"~/notes\"]\n",
            Path::new("c.toml"),
        )
        .unwrap();
        assert_eq!(c.protected_paths, vec!["/data/agents", "~/notes"]);
        assert!(Config::default().protected_paths.is_empty());
        for bad in ["\"\"", "\"rel/x\"", "\"~\"", "\"/\"", "\"~/\""] {
            let e =
                parse(&format!("protected_paths = [{bad}]"), Path::new("c.toml")).expect_err(bad);
            assert!(e.to_string().contains("protected_paths"), "{e}");
        }
        // No key removes a built-in root: the only keys are additions, and an unprotect-style
        // key is refused as unknown.
        for key in ["unprotected_paths", "protected_roots", "protect_builtin"] {
            assert!(parse(&format!("{key} = []"), Path::new("c.toml")).is_err());
        }
        // Resolving with the extras keeps every built-in root and adds the configured ones.
        let tmp = tempfile::tempdir().unwrap();
        let home = std::fs::canonicalize(tmp.path()).unwrap();
        let ctx = crate::protect::ScanContext {
            passwd_home: Some(home.clone()),
            euid: Some(1000),
            home_env: Some(home.to_string_lossy().into_owned()),
            extra_paths: c.protected_paths.clone(),
            ..Default::default()
        };
        let pc = crate::protect::resolve(&ctx).unwrap();
        for (rel, _) in crate::protect::BUILTIN_PROTECTED {
            let want = home.join(rel).to_string_lossy().into_owned();
            assert!(pc.roots.iter().any(|r| r.spelled == want), "{rel} missing");
        }
        assert!(pc.roots.iter().any(|r| r.spelled == "/data/agents"));
        assert!(pc.roots.iter().any(|r| r.spelled.ends_with("/notes")));
        // A configured path equal to the home stops the scan.
        let mut ctx2 = ctx.clone();
        ctx2.extra_paths = vec!["~/".to_string()];
        assert!(crate::protect::resolve(&ctx2).is_err());
    }

    #[test]
    fn explicit_missing_file_is_an_error() {
        let dir = std::env::temp_dir()
            .join("dc-config-test-missing-xyz")
            .join("nope.toml");
        assert!(load(Some(&dir)).is_err());
    }
}
