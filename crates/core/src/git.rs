//! The only place that runs `git`.
//!
//! Every call goes through [`Git::run`], which returns a typed [`GitError`] for a missing
//! binary, a timeout or a failed command, so a failure can never be mistaken for empty output
//! (principle 2: unknown means keep). Only read-only subcommands are issued by callers; the
//! environment is set so git never takes optional locks and never prompts.

use crate::redact::redact_text;
use std::ffi::OsString;
use std::io::Read;
use std::path::Path;
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

/// Why a git invocation did not produce output.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum GitError {
    /// The binary could not be started (missing, not executable).
    #[error("could not start git: {0}")]
    Spawn(String),
    /// The command ran longer than the timeout and was killed.
    #[error("git timed out after {0:?}")]
    Timeout(Duration),
    /// The command exited with a failure status.
    #[error("git exited with status {code:?}: {stderr}")]
    Failed { code: Option<i32>, stderr: String },
}

/// A git binary plus a per-command timeout.
#[derive(Debug, Clone)]
pub struct Git {
    program: OsString,
    timeout: Duration,
}

impl Default for Git {
    fn default() -> Self {
        Git::new("git", Duration::from_secs(30))
    }
}

/// Environment variables that would redirect git away from the directory we point it at.
const REDIRECT_VARS: [&str; 7] = [
    "GIT_DIR",
    "GIT_WORK_TREE",
    "GIT_INDEX_FILE",
    "GIT_OBJECT_DIRECTORY",
    "GIT_ALTERNATE_OBJECT_DIRECTORIES",
    "GIT_COMMON_DIR",
    "GIT_NAMESPACE",
];

impl Git {
    /// Use `program` (a name on `PATH` or a path) with a per-command timeout.
    pub fn new(program: impl Into<OsString>, timeout: Duration) -> Git {
        Git {
            program: program.into(),
            timeout,
        }
    }

    /// The configured timeout.
    pub fn timeout(&self) -> Duration {
        self.timeout
    }

    /// Run `git <args>` in `cwd` and return stdout (lossy UTF-8).
    ///
    /// `core.fsmonitor` is forced off because a repository's own config could otherwise make
    /// `git status` execute an arbitrary program.
    pub fn run(&self, cwd: &Path, args: &[&str]) -> Result<String, GitError> {
        let mut cmd = Command::new(&self.program);
        cmd.arg("-c")
            .arg("core.fsmonitor=false")
            .arg("-c")
            .arg("gc.auto=0")
            .args(args)
            .current_dir(cwd)
            .env("GIT_OPTIONAL_LOCKS", "0")
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("LC_ALL", "C")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        for var in REDIRECT_VARS {
            cmd.env_remove(var);
        }
        let mut child = cmd
            .spawn()
            .map_err(|e| GitError::Spawn(redact_text(&e.to_string())))?;

        let mut stdout = child.stdout.take();
        let mut stderr = child.stderr.take();
        let out_reader = thread::spawn(move || {
            let mut buf = Vec::new();
            if let Some(s) = stdout.as_mut() {
                let _ = s.read_to_end(&mut buf);
            }
            buf
        });
        let err_reader = thread::spawn(move || {
            let mut buf = Vec::new();
            if let Some(s) = stderr.as_mut() {
                let _ = s.read_to_end(&mut buf);
            }
            buf
        });

        let deadline = Instant::now() + self.timeout;
        let status = loop {
            match child.try_wait() {
                Ok(Some(status)) => break status,
                Ok(None) => {
                    if Instant::now() >= deadline {
                        let _ = child.kill();
                        let _ = child.wait();
                        // Do not join the readers: a grandchild may still hold the pipes.
                        return Err(GitError::Timeout(self.timeout));
                    }
                    thread::sleep(Duration::from_millis(5));
                }
                Err(e) => {
                    let _ = child.kill();
                    return Err(GitError::Spawn(redact_text(&e.to_string())));
                }
            }
        };

        let out = out_reader.join().unwrap_or_default();
        let err = err_reader.join().unwrap_or_default();
        if status.success() {
            Ok(String::from_utf8_lossy(&out).into_owned())
        } else {
            let stderr: String = redact_text(String::from_utf8_lossy(&err).trim())
                .chars()
                .take(300)
                .collect();
            Err(GitError::Failed {
                code: status.code(),
                stderr,
            })
        }
    }
}
