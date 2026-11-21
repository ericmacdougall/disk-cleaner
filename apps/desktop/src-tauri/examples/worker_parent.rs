//! Test-only parent for tests/containment.rs: starts the analysis worker as the app does
//! (`spawn_worker`), sends "go", drains the worker's stdout and stderr as state.rs does, prints
//! the worker's pid and then waits until it is killed. Killing it is a real parent death: the
//! system closes every end of the worker's pipes this process held (and, on Windows, the job
//! object handle).
//!
//! Usage: worker_parent APP_EXE GIT ROOT

use prunedisk_desktop_lib::worker::{spawn_worker, WorkerArgs, GIT_TIMEOUT, GO};
use std::io::Write;
use std::path::PathBuf;
use std::time::Duration;

fn main() {
    let mut argv = std::env::args_os().skip(1);
    let (Some(exe), Some(git), Some(root)) = (argv.next(), argv.next(), argv.next()) else {
        eprintln!("usage: worker_parent APP_EXE GIT ROOT");
        std::process::exit(2);
    };
    let args = WorkerArgs {
        roots: vec![PathBuf::from(root)],
        one_file_system: false,
        collect_facts: true,
        top_files: 10,
        min_output_bytes: 1 << 20,
        git,
        git_timeout: GIT_TIMEOUT,
    };
    let mut w = spawn_worker(&PathBuf::from(exe), &args).expect("spawn the worker");
    let mut out = w.stdout.take().expect("worker stdout");
    let mut err = w.stderr.take().expect("worker stderr");
    std::thread::spawn(move || {
        let _ = std::io::copy(&mut out, &mut std::io::sink());
    });
    std::thread::spawn(move || {
        let _ = std::io::copy(&mut err, &mut std::io::sink());
    });
    let mut stdin = w.stdin.take().expect("worker stdin");
    stdin.write_all(&[GO]).expect("send go");
    stdin.flush().expect("send go");
    let mut me = std::io::stdout();
    writeln!(me, "{}", w.handle.pid()).expect("print the worker pid");
    me.flush().expect("print the worker pid");
    std::thread::sleep(Duration::from_secs(600));
    drop(stdin);
    drop(w);
}
