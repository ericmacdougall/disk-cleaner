//! Storage through rclone (DESIGN 5). rclone owns the credentials; the remote is an argument.
//! Every command runs with `--ask-password=false`, never with `-v`/`--dump` (no credential or
//! header ever printed), in its own process group so a timeout kills it and its children
//! (NEG-10), and its stderr is redacted before it reaches any output.

use crate::redact::redact_text;
use std::io::{Read, Write};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Hashes the listing reported for an object.
#[derive(Debug, Clone, Default)]
pub struct Listing {
    pub size: u64,
    pub hashes: std::collections::BTreeMap<String, String>,
}

/// An rclone binary and a remote root such as `b2:bucket/prefix` or a local directory.
#[derive(Debug, Clone)]
pub struct Rclone {
    pub bin: PathBuf,
    pub root: String,
    pub timeout: Duration,
}

fn kill_group(child: &Child) {
    let pid = child.id() as i32;
    if let Some(p) = rustix::process::Pid::from_raw(pid) {
        let _ = rustix::process::kill_process_group(p, rustix::process::Signal::KILL);
    }
}

impl Rclone {
    pub fn object(&self, rel: &str) -> String {
        let r = self.root.trim_end_matches('/');
        format!("{r}/{rel}")
    }

    fn base(&self) -> Command {
        use std::os::unix::process::CommandExt;
        let mut c = Command::new(&self.bin);
        c.process_group(0);
        c.stdin(Stdio::null());
        c
    }

    fn err(&self, what: &str, stderr: &[u8]) -> String {
        let s = String::from_utf8_lossy(stderr);
        let last: Vec<&str> = s.lines().rev().take(3).collect();
        format!(
            "rclone {what} failed: {}",
            redact_text(&last.into_iter().rev().collect::<Vec<_>>().join(" | "))
        )
    }

    /// Run with a deadline, collecting stdout and stderr.
    fn run(&self, mut cmd: Command, what: &str) -> Result<Vec<u8>, String> {
        cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
        let mut child = cmd
            .spawn()
            .map_err(|e| format!("cannot start rclone: {e}"))?;
        let mut out = child.stdout.take().expect("piped");
        let mut errp = child.stderr.take().expect("piped");
        let t_out = std::thread::spawn(move || {
            let mut v = Vec::new();
            let _ = out.read_to_end(&mut v);
            v
        });
        let t_err = std::thread::spawn(move || {
            let mut v = Vec::new();
            let _ = errp.read_to_end(&mut v);
            v
        });
        let start = Instant::now();
        let status = loop {
            if let Some(s) = child.try_wait().map_err(|e| e.to_string())? {
                break s;
            }
            if start.elapsed() > self.timeout {
                kill_group(&child);
                let _ = child.wait();
                return Err(format!(
                    "rclone {what} timed out after {:?} and was killed",
                    self.timeout
                ));
            }
            std::thread::sleep(Duration::from_millis(20));
        };
        let stdout = t_out.join().unwrap_or_default();
        let stderr = t_err.join().unwrap_or_default();
        if !status.success() {
            return Err(self.err(what, &stderr));
        }
        Ok(stdout)
    }

    /// Stream bytes into `rel`. `feed` writes everything to the pipe; the shard bound is the
    /// streaming cutoff so the backend can store a whole-object hash (C6).
    pub fn rcat<F>(&self, rel: &str, cutoff: u64, feed: F) -> Result<(), String>
    where
        F: FnOnce(&mut dyn Write) -> Result<(), String>,
    {
        let mut cmd = self.base();
        cmd.args(["rcat", "--ask-password=false", "--streaming-upload-cutoff"])
            // rclone reads a bare number as KiB; say bytes.
            .arg(format!("{}B", cutoff.max(1)))
            .arg(self.object(rel))
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::piped());
        let mut child = cmd
            .spawn()
            .map_err(|e| format!("cannot start rclone: {e}"))?;
        let mut stdin = child.stdin.take().expect("piped");
        let mut errp = child.stderr.take().expect("piped");
        let t_err = std::thread::spawn(move || {
            let mut v = Vec::new();
            let _ = errp.read_to_end(&mut v);
            v
        });
        // Watchdog: a hung rclone (or a pipe it stops reading) is killed after the timeout.
        let done = Arc::new(AtomicBool::new(false));
        let pid = child.id() as i32;
        let timeout = self.timeout;
        let d2 = done.clone();
        let fired = Arc::new(AtomicBool::new(false));
        let f2 = fired.clone();
        let dog = std::thread::spawn(move || {
            let start = Instant::now();
            while !d2.load(Ordering::SeqCst) {
                if start.elapsed() > timeout {
                    f2.store(true, Ordering::SeqCst);
                    if let Some(p) = rustix::process::Pid::from_raw(pid) {
                        let _ =
                            rustix::process::kill_process_group(p, rustix::process::Signal::KILL);
                    }
                    return;
                }
                std::thread::sleep(Duration::from_millis(50));
            }
        });
        let fed = feed(&mut stdin);
        drop(stdin);
        let status = child.wait().map_err(|e| e.to_string());
        done.store(true, Ordering::SeqCst);
        let _ = dog.join();
        let stderr = t_err.join().unwrap_or_default();
        if fired.load(Ordering::SeqCst) {
            return Err(format!(
                "rclone rcat timed out after {:?} and was killed",
                self.timeout
            ));
        }
        fed?;
        let status = status?;
        if !status.success() {
            return Err(self.err("rcat", &stderr));
        }
        Ok(())
    }

    /// Size and hashes of one object, `None` when it does not exist.
    pub fn stat(&self, rel: &str) -> Result<Option<Listing>, String> {
        let mut cmd = self.base();
        cmd.args([
            "lsjson",
            "--ask-password=false",
            "--hash",
            "--files-only",
            "--no-mimetype",
        ])
        .arg(self.object(rel));
        let out = match self.run(cmd, "lsjson") {
            Ok(o) => o,
            Err(e) if e.contains("directory not found") || e.contains("not found") => {
                return Ok(None)
            }
            Err(e) => return Err(e),
        };
        let v: serde_json::Value =
            serde_json::from_slice(&out).map_err(|e| format!("rclone lsjson output: {e}"))?;
        let Some(first) = v.as_array().and_then(|a| a.first()) else {
            return Ok(None);
        };
        let size = first
            .get("Size")
            .and_then(|s| s.as_u64())
            .ok_or("rclone lsjson gave no size")?;
        let mut hashes = std::collections::BTreeMap::new();
        if let Some(h) = first.get("Hashes").and_then(|h| h.as_object()) {
            for (k, v) in h {
                if let Some(s) = v.as_str() {
                    hashes.insert(k.to_ascii_lowercase(), s.to_ascii_lowercase());
                }
            }
        }
        Ok(Some(Listing { size, hashes }))
    }

    /// Bytes `offset..offset+count` of an object.
    pub fn cat_range(&self, rel: &str, offset: u64, count: u64) -> Result<Vec<u8>, String> {
        let mut cmd = self.base();
        cmd.args(["cat", "--ask-password=false", "--offset"])
            .arg(offset.to_string())
            .arg("--count")
            .arg(count.to_string())
            .arg(self.object(rel));
        let out = self.run(cmd, "cat")?;
        if out.len() as u64 != count {
            return Err(format!(
                "rclone cat returned {} bytes, expected {count}",
                out.len()
            ));
        }
        Ok(out)
    }

    /// The whole object.
    pub fn cat(&self, rel: &str) -> Result<Vec<u8>, String> {
        let mut cmd = self.base();
        cmd.args(["cat", "--ask-password=false"])
            .arg(self.object(rel));
        self.run(cmd, "cat")
    }

    /// Names of objects under `dir` (non-recursive).
    pub fn list(&self, dir: &str) -> Result<Vec<String>, String> {
        let mut cmd = self.base();
        cmd.args(["lsf", "--ask-password=false", "--files-only"])
            .arg(self.object(dir));
        match self.run(cmd, "lsf") {
            Ok(o) => Ok(String::from_utf8_lossy(&o)
                .lines()
                .map(|s| s.to_string())
                .collect()),
            Err(e) if e.contains("not found") => Ok(Vec::new()),
            Err(e) => Err(e),
        }
    }
}
