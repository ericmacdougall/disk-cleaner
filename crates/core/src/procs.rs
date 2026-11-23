//! Running processes, as evidence for the plan's per-item rules (see [`crate::evidence`] and
//! docs/DESIGN.md, In use). Read-only. What could not be read is recorded as unread (`None`),
//! never as "nothing there".
//!
//! On Linux the snapshot is read from `/proc` directly: per process its state, parent, flags
//! (`PF_KTHREAD` marks a kernel thread), start time, owner, working directory, command line and
//! open files (`/proc/<pid>/fd`). On macOS it comes from `sysinfo` and one `lsof` run for the
//! scanning user's open files. A process that vanished while it was read is told apart from one
//! that exists but cannot be read by a signal-0 re-probe, which the caller supplies (the core
//! crate forbids unsafe code).

use serde::{Deserialize, Serialize};
use std::path::Path;

/// What kind of process a snapshot entry is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ProcKind {
    /// A running process (readable or not).
    #[default]
    Live,
    /// A kernel thread (Linux `PF_KTHREAD`): never evidence, ignored.
    KernelThread,
    /// Exited, a zombie (`Z`) or dead (`X`), or vanished while read (its `/proc` entry failed
    /// with `ESRCH` or `ENOENT` and a signal-0 re-probe said `ESRCH`): ignored.
    Gone,
}

/// One process in a snapshot. Every field that could not be read is `None`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Proc {
    pub pid: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ppid: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub uid: Option<u32>,
    /// Executable basenames known for it (the executable's, `argv[0]`'s and the kernel's short
    /// name), for recognising agent and container processes.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub names: Vec<String>,
    /// Working directory; `None` when it could not be read.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    /// Command line; `None` when it could not be read or was empty.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cmdline: Option<Vec<String>>,
    /// Absolute paths of open files; `None` when they could not be listed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub open_files: Option<Vec<String>>,
    /// Start time, unix seconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub start_time: Option<i64>,
    #[serde(default)]
    pub kind: ProcKind,
}

impl Proc {
    /// A readable process with this working directory (tests and fixtures).
    pub fn with_cwd(pid: u32, cwd: &str) -> Proc {
        Proc {
            pid,
            cwd: Some(cwd.to_string()),
            cmdline: Some(vec!["proc".to_string()]),
            open_files: Some(Vec::new()),
            names: vec!["proc".to_string()],
            ..Proc::default()
        }
    }

    /// Neither the working directory nor the command line could be read.
    pub fn fully_unknown(&self) -> bool {
        self.kind == ProcKind::Live && self.cwd.is_none() && self.cmdline.is_none()
    }
}

/// Every running process, as read at plan time.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProcessSnapshot {
    #[serde(default)]
    pub procs: Vec<Proc>,
    /// Set when process information could not be read at all: every removing item asks.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl ProcessSnapshot {
    /// No process running, fully known (tests and callers that checked by other means).
    pub fn none_running() -> ProcessSnapshot {
        ProcessSnapshot::default()
    }

    /// Process information could not be read.
    pub fn unknown(reason: impl Into<String>) -> ProcessSnapshot {
        ProcessSnapshot {
            procs: Vec::new(),
            error: Some(reason.into()),
        }
    }

    /// Readable processes with these working directories.
    pub fn from_cwds(cwds: &[&str]) -> ProcessSnapshot {
        ProcessSnapshot {
            procs: cwds
                .iter()
                .enumerate()
                .map(|(i, c)| Proc::with_cwd(10_000 + i as u32, c))
                .collect(),
            error: None,
        }
    }

    /// Readable processes with these working directories plus `hidden` processes whose working
    /// directory and command line are both unreadable.
    pub fn with_hidden(cwds: &[&str], hidden: usize) -> ProcessSnapshot {
        let mut s = ProcessSnapshot::from_cwds(cwds);
        for k in 0..hidden {
            s.procs.push(Proc {
                pid: 20_000 + k as u32,
                ..Proc::default()
            });
        }
        s
    }
}

/// Answer of a signal-0 re-probe of a pid.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Probe {
    /// `ESRCH`: no such process.
    Gone,
    /// It exists (success or `EPERM`).
    Exists,
    /// The probe could not be made.
    Unknown,
}

/// `PF_KTHREAD` in `/proc/<pid>/stat` flags.
pub const PF_KTHREAD: u64 = 0x0020_0000;

/// `/proc/<pid>/stat` fields after the command name: state, parent, flags, start time (clock
/// ticks since boot). The command name may contain spaces and parentheses, so fields are counted
/// from the last `)`.
pub fn parse_stat(text: &str) -> Option<(char, u32, u64, u64)> {
    let close = text.rfind(')')?;
    let rest: Vec<&str> = text[close + 1..].split_whitespace().collect();
    let state = rest.first()?.chars().next()?;
    let ppid = rest.get(1)?.parse().ok()?;
    let flags = rest.get(6)?.parse().ok()?;
    let start = rest.get(19)?.parse().ok()?;
    Some((state, ppid, flags, start))
}

fn comm_of(text: &str) -> Option<String> {
    let open = text.find('(')?;
    let close = text.rfind(')')?;
    (close > open).then(|| text[open + 1..close].to_string())
}

fn strip_deleted(s: &str) -> String {
    s.strip_suffix(" (deleted)").unwrap_or(s).to_string()
}

fn basename(s: &str) -> Option<String> {
    Path::new(s)
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .filter(|n| !n.is_empty())
}

/// Most open files recorded per process.
const MAX_FDS: usize = 4096;
/// Most command-line arguments recorded per process.
const MAX_ARGS: usize = 512;

/// Read one process from a `/proc`-style directory. `boot` is the boot time (unix seconds), for
/// the start time.
pub fn read_proc_entry(
    proc_root: &Path,
    pid: u32,
    boot: Option<i64>,
    probe: &dyn Fn(u32) -> Probe,
) -> Proc {
    let dir = proc_root.join(pid.to_string());
    let mut p = Proc {
        pid,
        ..Proc::default()
    };
    match std::fs::read_to_string(dir.join("stat")) {
        Err(e) => {
            // ESRCH (3) or ENOENT (2): it may have exited while it was read. Only a re-probe
            // that also says ESRCH makes it gone; EPERM means it exists.
            if matches!(e.raw_os_error(), Some(2) | Some(3)) && probe(pid) == Probe::Gone {
                p.kind = ProcKind::Gone;
                return p;
            }
        }
        Ok(text) => {
            if let Some((state, ppid, flags, start)) = parse_stat(&text) {
                p.ppid = Some(ppid);
                if matches!(state, 'Z' | 'X' | 'x') {
                    p.kind = ProcKind::Gone;
                    return p;
                }
                if flags & PF_KTHREAD != 0 {
                    p.kind = ProcKind::KernelThread;
                    return p;
                }
                // USER_HZ, the unit of /proc start times, is 100 on every Linux ABI.
                p.start_time = boot.map(|b| b + (start / 100) as i64);
            }
            if let Some(c) = comm_of(&text) {
                p.names.push(c);
            }
        }
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        p.uid = std::fs::metadata(&dir).ok().map(|m| m.uid());
    }
    p.cwd = std::fs::read_link(dir.join("cwd"))
        .ok()
        .map(|c| strip_deleted(&c.to_string_lossy()));
    if let Ok(raw) = std::fs::read(dir.join("cmdline")) {
        let args: Vec<String> = raw
            .split(|b| *b == 0)
            .filter(|a| !a.is_empty())
            .take(MAX_ARGS)
            .map(|a| String::from_utf8_lossy(a).into_owned())
            .collect();
        if !args.is_empty() {
            if let Some(b) = basename(&args[0]) {
                p.names.insert(0, b);
            }
            p.cmdline = Some(args);
        }
    }
    if let Ok(exe) = std::fs::read_link(dir.join("exe")) {
        if let Some(b) = basename(&strip_deleted(&exe.to_string_lossy())) {
            p.names.insert(0, b);
        }
    }
    p.names.dedup();
    if let Ok(rd) = std::fs::read_dir(dir.join("fd")) {
        let mut files = Vec::new();
        for e in rd.flatten().take(MAX_FDS) {
            if let Ok(t) = std::fs::read_link(e.path()) {
                let t = t.to_string_lossy();
                if t.starts_with('/') {
                    files.push(strip_deleted(&t));
                }
            }
        }
        p.open_files = Some(files);
    }
    if p.cwd.is_none() && p.cmdline.is_none() && p.ppid.is_none() && probe(pid) == Probe::Gone {
        // Nothing at all could be read and the pid no longer exists: it exited mid-read.
        p.kind = ProcKind::Gone;
    }
    p
}

/// Boot time from a `/proc/stat`-style file.
pub fn boot_time(proc_root: &Path) -> Option<i64> {
    let text = std::fs::read_to_string(proc_root.join("stat")).ok()?;
    text.lines()
        .find_map(|l| l.strip_prefix("btime "))
        .and_then(|v| v.trim().parse().ok())
}

/// Read every process from a `/proc`-style directory.
pub fn read_proc_dir(proc_root: &Path, probe: &dyn Fn(u32) -> Probe) -> ProcessSnapshot {
    let rd = match std::fs::read_dir(proc_root) {
        Ok(rd) => rd,
        Err(e) => {
            return ProcessSnapshot::unknown(format!(
                "{} could not be listed: {e}",
                proc_root.display()
            ))
        }
    };
    let boot = boot_time(proc_root);
    let mut pids: Vec<u32> = rd
        .flatten()
        .filter_map(|e| e.file_name().to_str()?.parse().ok())
        .collect();
    pids.sort_unstable();
    if pids.is_empty() {
        return ProcessSnapshot::unknown(format!("{} lists no process", proc_root.display()));
    }
    ProcessSnapshot {
        procs: pids
            .into_iter()
            .map(|pid| read_proc_entry(proc_root, pid, boot, probe))
            .collect(),
        error: None,
    }
}

/// Take a snapshot of every running process. `uid` is the scanning user (macOS: whose open files
/// `lsof` lists).
pub fn snapshot(probe: &dyn Fn(u32) -> Probe, uid: Option<u32>) -> ProcessSnapshot {
    if cfg!(target_os = "linux") {
        return read_proc_dir(Path::new("/proc"), probe);
    }
    sysinfo_snapshot(probe, uid)
}

fn sysinfo_snapshot(probe: &dyn Fn(u32) -> Probe, uid: Option<u32>) -> ProcessSnapshot {
    use sysinfo::{ProcessRefreshKind, ProcessStatus, ProcessesToUpdate, System, UpdateKind};
    if !sysinfo::IS_SUPPORTED_SYSTEM {
        return ProcessSnapshot::unknown("process information is not supported on this platform");
    }
    let mut sys = System::new();
    sys.refresh_processes_specifics(
        ProcessesToUpdate::All,
        true,
        ProcessRefreshKind::nothing()
            .with_cwd(UpdateKind::Always)
            .with_cmd(UpdateKind::Always)
            .with_exe(UpdateKind::Always)
            .with_user(UpdateKind::Always),
    );
    let open = uid.and_then(lsof_open_files);
    let mut procs: Vec<Proc> = Vec::new();
    for (pid, sp) in sys.processes() {
        let pid = pid.as_u32();
        let mut p = Proc {
            pid,
            ppid: sp.parent().map(|q| q.as_u32()),
            uid: sp.user_id().map(|u| **u),
            start_time: Some(sp.start_time() as i64),
            ..Proc::default()
        };
        if matches!(sp.status(), ProcessStatus::Zombie | ProcessStatus::Dead) {
            p.kind = ProcKind::Gone;
            procs.push(p);
            continue;
        }
        if let Some(e) = sp.exe().and_then(|e| basename(&e.to_string_lossy())) {
            p.names.push(e);
        }
        let args: Vec<String> = sp
            .cmd()
            .iter()
            .take(MAX_ARGS)
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        if !args.is_empty() {
            if let Some(b) = basename(&args[0]) {
                p.names.push(b);
            }
            p.cmdline = Some(args);
        }
        p.names.push(sp.name().to_string_lossy().into_owned());
        p.names.dedup();
        p.cwd = sp.cwd().map(|c| c.to_string_lossy().into_owned());
        p.open_files = open.as_ref().and_then(|m| m.get(&pid).cloned());
        if p.cwd.is_none() && p.cmdline.is_none() && probe(pid) == Probe::Gone {
            p.kind = ProcKind::Gone;
        }
        procs.push(p);
    }
    procs.sort_by_key(|p| p.pid);
    if procs.is_empty() {
        return ProcessSnapshot::unknown("no running process could be listed");
    }
    ProcessSnapshot { procs, error: None }
}

/// Open files of `uid`'s processes from one `lsof` run (macOS), by pid. `None` when lsof could
/// not be run or did not finish within its time limit.
fn lsof_open_files(uid: u32) -> Option<std::collections::HashMap<u32, Vec<String>>> {
    let out = run_bounded(
        std::process::Command::new("lsof").args([
            "-n",
            "-P",
            "-w",
            "-F",
            "pn",
            "-u",
            &uid.to_string(),
        ]),
        std::time::Duration::from_secs(60),
    )?;
    Some(parse_lsof(&out))
}

/// Parse `lsof -F pn` output: `p<pid>` starts a process, `n<name>` names one of its files.
pub fn parse_lsof(out: &str) -> std::collections::HashMap<u32, Vec<String>> {
    let mut map: std::collections::HashMap<u32, Vec<String>> = std::collections::HashMap::new();
    let mut cur: Option<u32> = None;
    for line in out.lines() {
        if let Some(p) = line.strip_prefix('p') {
            cur = p.parse().ok();
            if let Some(pid) = cur {
                map.entry(pid).or_default();
            }
        } else if let (Some(n), Some(pid)) = (line.strip_prefix('n'), cur) {
            if n.starts_with('/') {
                let files = map.entry(pid).or_default();
                if files.len() < MAX_FDS {
                    files.push(n.to_string());
                }
            }
        }
    }
    map
}

/// Run a read-only command with a time limit; its stdout when it exits (any status: lsof exits 1
/// when some files could not be listed). `None` on failure or timeout (the child is killed).
pub fn run_bounded(cmd: &mut std::process::Command, limit: std::time::Duration) -> Option<String> {
    use std::io::Read;
    use std::process::Stdio;
    let mut child = cmd
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let mut stdout = child.stdout.take()?;
    let reader = std::thread::spawn(move || {
        let mut s = String::new();
        let _ = stdout.read_to_string(&mut s);
        s
    });
    let deadline = std::time::Instant::now() + limit;
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) if std::time::Instant::now() < deadline => {
                std::thread::sleep(std::time::Duration::from_millis(20))
            }
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    }
    reader.join().ok()
}
