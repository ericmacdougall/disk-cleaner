//! The analysis worker: core's scan (git included) runs in a child process, the app's own
//! executable re-run with `--analysis-worker`, so Cancel can end it and every git command it
//! started (spec AC-3, NEG-3, NEG-16).
//!
//! Containment. Unix: the worker is the leader of a new session (`setsid` before exec); core
//! starts each git command in its own process group inside that session, and git does not call
//! `setsid`, so every descendant keeps the worker's session id. Ending the worker sends SIGKILL
//! to every process of that session, repeating every 50 ms until a pass finds none (at most 40
//! passes), before the worker is reaped (so its pid cannot be reused meanwhile). Windows: the
//! worker runs in a job object that kills every process in it when it is closed or terminated.
//! Both: the worker starts no work until the parent sends a "go" byte on stdin, after the
//! containment is in place; when the parent dies, stdin closes and the worker's watchdog ends
//! its own session (Unix) or the job closes (Windows).
//!
//! Parent death on Unix: the analysis keeps running while the watchdog sweeps, so the watchdog
//! first stops this process from starting any other (the soft RLIMIT_NPROC limit becomes 0:
//! fork and posix_spawn then fail with EAGAIN, which core reports as a git error, a fact
//! "unknown"), then sweeps the session twice, 50 ms apart, then exits. From the moment the
//! worker is ending, the analysis cannot end the process (its exits and panics wait for the
//! watchdog), so the sweep always finishes (code inspection F1).
//!
//! Residual (disclosed): the process limit does not bind root, nor on Linux a process with
//! CAP_SYS_RESOURCE or CAP_SYS_ADMIN; an app run that way can, when its parent dies, have one
//! more git start between the sweep and the worker's exit, which then runs to its end. If the
//! parent dies after killing the worker but before its own sweep has finished, a git command
//! still running finishes on its own. Core runs only local, read-only git commands.

use disk_cleaner_core::{scan, FactsConfig, Git, ScanOptions, ScanResult};
use std::ffi::{OsStr, OsString};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{ChildStderr, ChildStdin, ChildStdout, ExitStatus, Stdio};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

/// The hidden flag that makes the app run as the analysis worker.
pub const WORKER_FLAG: &str = "--analysis-worker";
/// The byte that tells the worker to start.
pub const GO: u8 = b'g';
/// Sweep passes, and the pause between them.
pub const SWEEP_PASSES: usize = 40;
pub const SWEEP_PAUSE: Duration = Duration::from_millis(50);
/// How long the worker waits for "go" before giving up.
const GO_TIMEOUT: Duration = Duration::from_secs(60);

/// What the worker is asked to do. Passed as an argument list, never through a shell.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkerArgs {
    pub roots: Vec<PathBuf>,
    pub one_file_system: bool,
    pub collect_facts: bool,
    pub top_files: usize,
    pub min_output_bytes: u64,
    /// The git program (`git`; tests use a fake one).
    pub git: OsString,
}

impl WorkerArgs {
    /// The arguments after `--analysis-worker`.
    pub fn to_args(&self) -> Vec<OsString> {
        let mut a: Vec<OsString> = Vec::new();
        if self.one_file_system {
            a.push("--one-file-system".into());
        }
        if !self.collect_facts {
            a.push("--no-git".into());
        }
        a.push("--top-files".into());
        a.push(self.top_files.to_string().into());
        a.push("--min-output-bytes".into());
        a.push(self.min_output_bytes.to_string().into());
        a.push("--git".into());
        a.push(self.git.clone());
        a.push("--".into());
        a.extend(self.roots.iter().map(|r| r.as_os_str().to_os_string()));
        a
    }

    /// Parse the arguments after `--analysis-worker`.
    pub fn parse(args: &[OsString]) -> Result<WorkerArgs, String> {
        let mut out = WorkerArgs {
            roots: Vec::new(),
            one_file_system: false,
            collect_facts: true,
            top_files: 200,
            min_output_bytes: 100 << 20,
            git: "git".into(),
        };
        let mut it = args.iter();
        let number = |v: Option<&OsString>, name: &str| -> Result<u64, String> {
            v.and_then(|s| s.to_str())
                .and_then(|s| s.parse().ok())
                .ok_or_else(|| format!("{name} needs a number"))
        };
        while let Some(a) = it.next() {
            match a.to_str() {
                Some("--one-file-system") => out.one_file_system = true,
                Some("--no-git") => out.collect_facts = false,
                Some("--top-files") => out.top_files = number(it.next(), "--top-files")? as usize,
                Some("--min-output-bytes") => {
                    out.min_output_bytes = number(it.next(), "--min-output-bytes")?
                }
                Some("--git") => out.git = it.next().cloned().ok_or("--git needs a program")?,
                Some("--") => {
                    out.roots.extend(it.by_ref().map(PathBuf::from));
                    break;
                }
                _ => return Err("unknown worker argument".to_string()),
            }
        }
        if out.roots.is_empty() {
            return Err("no roots".to_string());
        }
        Ok(out)
    }
}

/// The app's own executable, which the worker re-runs.
pub fn worker_exe() -> std::io::Result<PathBuf> {
    std::env::current_exe()
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|p| p.into_inner())
}

#[cfg(unix)]
type ChildProc = std::process::Child;
#[cfg(windows)]
type ChildProc = command_group::GroupChild;

/// A started worker: its pipes, and the handle that ends it.
pub struct WorkerProcess {
    pub stdin: Option<ChildStdin>,
    pub stdout: Option<ChildStdout>,
    pub stderr: Option<ChildStderr>,
    pub handle: Arc<WorkerHandle>,
}

/// Ends and reaps a worker. Shared between the job thread and Cancel.
pub struct WorkerHandle {
    pid: u32,
    /// The worker's start time (Unix seconds), so a reused pid is never killed.
    #[cfg_attr(not(unix), allow(dead_code))]
    start_time: Option<u64>,
    child: Mutex<ChildProc>,
    /// True once the worker was reaped. Ending and reaping take this lock, so a sweep never
    /// runs after the reap (when the pid could belong to another process).
    reaped: Mutex<bool>,
}

impl WorkerHandle {
    pub fn pid(&self) -> u32 {
        self.pid
    }

    /// End the worker and everything it started; does nothing once the worker was reaped.
    /// Returns how many processes the sweep found (Unix; 0 on Windows, where terminating the
    /// job object ends them).
    pub fn terminate(&self) -> usize {
        let reaped = lock(&self.reaped);
        if *reaped {
            return 0;
        }
        #[cfg(unix)]
        {
            sweep_session(self.pid, self.start_time, true)
        }
        #[cfg(windows)]
        {
            let _ = lock(&self.child).kill();
            0
        }
    }

    /// Wait for the worker to exit and reap it.
    pub fn wait(&self) -> Option<ExitStatus> {
        loop {
            {
                let mut reaped = lock(&self.reaped);
                match lock(&self.child).try_wait() {
                    Ok(Some(s)) => {
                        *reaped = true;
                        return Some(s);
                    }
                    Ok(None) => {}
                    Err(_) => return None,
                }
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}

/// Start the worker: `exe --analysis-worker ARGS`, contained as the module docs describe. The
/// only process spawn in the app (scripts/readonly-allow.txt). The caller sends `GO` on stdin
/// once it is ready, and keeps stdin open until the worker has exited.
#[allow(clippy::disallowed_methods)] // listed in scripts/readonly-allow.txt
#[allow(unsafe_code)] // setsid in pre_exec (Unix)
pub fn spawn_worker(exe: &Path, args: &WorkerArgs) -> std::io::Result<WorkerProcess> {
    let mut cmd = std::process::Command::new(exe);
    cmd.arg(WORKER_FLAG)
        .args(args.to_args())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[cfg(unix)]
    let mut child: ChildProc = {
        use std::os::unix::process::CommandExt;
        // SAFETY: the closure runs in the child between fork and exec and only calls setsid,
        // which is async-signal-safe and touches no memory shared with the parent.
        unsafe {
            cmd.pre_exec(|| {
                if libc::setsid() == -1 {
                    Err(std::io::Error::last_os_error())
                } else {
                    Ok(())
                }
            });
        }
        cmd.spawn()?
    };
    #[cfg(windows)]
    let mut child: ChildProc = {
        use command_group::CommandGroup;
        // Started suspended in a new job object with kill-on-close (command-group's
        // `group_spawn` leaves JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE off unless kill_on_drop is
        // set, so closing the handle would end nothing: CI run 44625966165). An error here
        // means the worker could not be contained, and no worker is left running.
        cmd.group().kill_on_drop(true).spawn().map_err(|e| {
            std::io::Error::other(format!("cannot contain the analysis process: {e}"))
        })?
    };
    #[cfg(unix)]
    let (pid, inner) = (child.id(), &mut child);
    #[cfg(windows)]
    let (pid, inner) = (child.id(), child.inner());
    let stdin = inner.stdin.take();
    let stdout = inner.stdout.take();
    let stderr = inner.stderr.take();
    let start_time = process_start_time(pid);
    Ok(WorkerProcess {
        stdin,
        stdout,
        stderr,
        handle: Arc::new(WorkerHandle {
            pid,
            start_time,
            child: Mutex::new(child),
            reaped: Mutex::new(false),
        }),
    })
}

fn process_start_time(pid: u32) -> Option<u64> {
    use sysinfo::{Pid, ProcessesToUpdate, System};
    let mut sys = System::new();
    let p = Pid::from_u32(pid);
    sys.refresh_processes(ProcessesToUpdate::Some(&[p]), true);
    sys.process(p).map(|p| p.start_time())
}

/// SIGKILL every process whose session id is `sid`, the leader last (only when
/// `include_leader` and its start time matches), repeating every 50 ms until a pass finds
/// none, at most 40 passes. Zombies are skipped. Returns how many processes were found in all.
#[cfg(unix)]
pub fn sweep_session(sid: u32, leader_start: Option<u64>, include_leader: bool) -> usize {
    use sysinfo::{Pid, ProcessStatus, ProcessesToUpdate, Signal, System};
    let mut sys = System::new();
    let session = Pid::from_u32(sid);
    let mut seen = 0;
    for pass in 0..SWEEP_PASSES {
        sys.refresh_processes(ProcessesToUpdate::All, true);
        let mut found = 0;
        let mut leader = None;
        for (pid, p) in sys.processes() {
            // On Linux sysinfo also lists every thread as a "process". A thread's id is not a
            // process: kill(tid, SIGKILL) ends the whole process the thread belongs to, so
            // the worker's own watchdog sweep killed the worker through one of its threads
            // before it reached the git (CI run 44637137532: 11 of 11 Linux runs left the
            // fake git running). Threads are skipped; their process is matched by its pid.
            if p.thread_kind().is_some()
                || p.session_id() != Some(session)
                || p.status() == ProcessStatus::Zombie
            {
                continue;
            }
            if *pid == session {
                if include_leader && leader_start.is_none_or(|t| t == p.start_time()) {
                    leader = Some(p);
                }
                continue;
            }
            found += 1;
            p.kill_with(Signal::Kill);
        }
        if let Some(p) = leader {
            found += 1;
            p.kill_with(Signal::Kill);
        }
        seen += found;
        if found == 0 {
            break;
        }
        if pass + 1 < SWEEP_PASSES {
            std::thread::sleep(SWEEP_PAUSE);
        }
    }
    seen
}

/// Run core's scan for the worker.
pub fn analyse(args: &WorkerArgs) -> Result<ScanResult, String> {
    let mut opts = ScanOptions::new(args.roots.clone());
    opts.one_file_system = args.one_file_system;
    opts.top_files = args.top_files;
    opts.min_output_bytes = args.min_output_bytes;
    opts.collect_facts = args.collect_facts;
    let git = Git::new(args.git.clone(), Duration::from_secs(30));
    opts.facts = FactsConfig::default().with_git(git);
    scan(&opts).map_err(|e| e.to_string())
}

/// Read stdin until "go" (returns true) or end of input (false); after "go", keep reading and
/// end the worker when stdin closes (the parent cancelled, quit or died).
fn watchdog(go: std::sync::mpsc::Sender<()>) {
    let mut stdin = std::io::stdin().lock();
    let mut buf = [0u8; 64];
    let mut started = false;
    loop {
        match stdin.read(&mut buf) {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                if !started && buf[..n].contains(&GO) {
                    started = true;
                    let _ = go.send(());
                }
            }
        }
    }
    end_own_session();
    test_pause_before_exit();
    std::process::exit(5);
}

/// Test builds only: stay alive for `PRUNEDISK_TEST_END_PAUSE_MS` (at most 10 s) after the
/// parent-death sweep, which holds open the window in which the analysis could start one more
/// git before the worker exits (tests/containment.rs). Release builds never read it.
fn test_pause_before_exit() {
    #[cfg(debug_assertions)]
    {
        if let Some(ms) = std::env::var("PRUNEDISK_TEST_END_PAUSE_MS")
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
        {
            std::thread::sleep(Duration::from_millis(ms.min(10_000)));
        }
    }
}

/// True once the worker is ending because its parent is gone. Held while the watchdog sweeps;
/// the analysis checks it before it may exit (see [`finish`]).
static ENDING: Mutex<bool> = Mutex::new(false);

/// The parent is gone: stop starting processes, then end everything this worker started.
fn end_own_session() {
    let mut ending = lock(&ENDING);
    *ending = true;
    #[cfg(unix)]
    {
        match stop_new_processes() {
            Ok(()) => eprintln!("analysis worker: ending at {}", unix_ns()),
            Err(e) => eprintln!("analysis worker: cannot stop new processes ({e})"),
        }
        // The worker is its session's leader, so its pid is the session id; it exits next.
        // A second sweep a moment later catches a process whose fork was already under way
        // when the limit was set.
        let sid = std::process::id();
        let _ = sweep_session(sid, None, false);
        std::thread::sleep(SWEEP_PAUSE);
        let _ = sweep_session(sid, None, false);
    }
    drop(ending);
}

/// From now on this process cannot start another (spec AC-3, code inspection F1): its soft
/// limit on processes becomes 0, which fork and posix_spawn check (Linux counts every task of
/// the real user id against it, threads too; macOS counts the user's processes; either way the
/// count is at least 1). The hard limit is kept. Root, and on Linux CAP_SYS_RESOURCE or
/// CAP_SYS_ADMIN, are not bound by it (disclosed in the module docs).
#[cfg(unix)]
fn stop_new_processes() -> std::io::Result<()> {
    let (_, hard) = rlimit::getrlimit(rlimit::Resource::NPROC)?;
    rlimit::setrlimit(rlimit::Resource::NPROC, 0, hard)
}

#[cfg(unix)]
fn unix_ns() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0)
}

/// End the worker with `code`, unless it is ending because its parent is gone: then wait for
/// the watchdog, which exits once its sweep is done.
fn finish(code: i32) -> ! {
    let ending = lock(&ENDING);
    if *ending {
        drop(ending);
        wait_for_the_watchdog();
    }
    std::process::exit(code)
}

fn wait_for_the_watchdog() -> ! {
    loop {
        std::thread::park();
    }
}

/// While the worker is ending, a panic (core cannot start a thread once new processes are
/// stopped on Linux) waits for the watchdog instead of unwinding out of the analysis and
/// ending the process mid-sweep. Otherwise the default report and unwinding apply.
fn install_panic_hook() {
    let default = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        if *lock(&ENDING) {
            wait_for_the_watchdog();
        }
        default(info);
    }));
}

/// The worker's main. Never returns.
pub fn worker_main(args: &[OsString]) -> ! {
    let parsed = match WorkerArgs::parse(args) {
        Ok(a) => a,
        Err(e) => {
            eprintln!("analysis worker: {e}");
            std::process::exit(2);
        }
    };
    install_panic_hook();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || watchdog(tx));
    if rx.recv_timeout(GO_TIMEOUT).is_err() {
        eprintln!("analysis worker: not started");
        finish(4);
    }
    match analyse(&parsed) {
        Ok(scan) => {
            let mut out = std::io::BufWriter::new(std::io::stdout().lock());
            if serde_json::to_writer(&mut out, &scan).is_err() || out.flush().is_err() {
                finish(1);
            }
            finish(0);
        }
        Err(e) => {
            eprintln!("{e}");
            finish(1);
        }
    }
}

/// Is this process the worker? (`exe --analysis-worker ...`)
pub fn worker_args_from(mut args: impl Iterator<Item = OsString>) -> Option<Vec<OsString>> {
    let _exe = args.next();
    let first = args.next()?;
    if first.as_os_str() == OsStr::new(WORKER_FLAG) {
        Some(args.collect())
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn worker_arguments_round_trip() {
        let a = WorkerArgs {
            roots: vec![PathBuf::from("/syn/a"), PathBuf::from("/syn/-- b")],
            one_file_system: true,
            collect_facts: false,
            top_files: 50,
            min_output_bytes: 7,
            git: "fake-git".into(),
        };
        assert_eq!(WorkerArgs::parse(&a.to_args()).unwrap(), a);
    }

    #[test]
    fn bad_worker_arguments_are_refused() {
        let v = |s: &[&str]| s.iter().map(OsString::from).collect::<Vec<_>>();
        assert!(WorkerArgs::parse(&v(&["--"])).is_err());
        assert!(WorkerArgs::parse(&v(&["--top-files", "x", "--", "/a"])).is_err());
        assert!(WorkerArgs::parse(&v(&["--rm", "--", "/a"])).is_err());
    }

    #[test]
    fn only_the_hidden_flag_selects_the_worker() {
        let v = |s: &[&str]| s.iter().map(OsString::from).collect::<Vec<_>>();
        assert!(worker_args_from(v(&["app"]).into_iter()).is_none());
        assert!(worker_args_from(v(&["app", "--other"]).into_iter()).is_none());
        assert_eq!(
            worker_args_from(v(&["app", WORKER_FLAG, "--", "/a"]).into_iter()),
            Some(v(&["--", "/a"]))
        );
    }
}
