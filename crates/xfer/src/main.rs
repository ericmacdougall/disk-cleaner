//! `prunedisk-xfer`: back up a folder to object storage through rclone, restore it, verify it.
//!
//! Exit codes: 0 everything receipted (or restored, or verified); 2 partial (some paths changed,
//! skipped or not receipted); 3 a verification or fatal error; 1 a usage error.

use clap::{Parser, Subcommand};
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;
use xfer::backup::{backup, default_state_dir, Options};
use xfer::repo::{Budget, DEFAULT_MAX_OBJECTS, MIB};
use xfer::restore::{rebuild_receipts, restore, verify, RestoreOpts};
use xfer::transport::{self, Metered, RetryPolicy, Transport};
use xfer::util::parse_size;
use xfer::walk::Filter;

#[derive(Parser)]
#[command(
    name = "prunedisk-xfer",
    version,
    about = "Fast verified backup to object storage"
)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(clap::Args, Clone)]
struct RepoArgs {
    /// Repository: an rclone remote path (e.g. b2:bucket/xfer/repo) or local:DIR for tests.
    #[arg(long)]
    repo: String,
    /// rclone binary (version 1.60 or newer). Keys stay inside rclone's own config.
    #[arg(long, default_value = "rclone")]
    rclone: String,
    /// Memory budget, split into named shares (see the spec, Revision 6).
    #[arg(long, default_value = "1640Mi")]
    mem_budget: String,
    /// Largest pack (single-part upload); at most 128Mi.
    #[arg(long, default_value = "32Mi")]
    max_pack: String,
    /// Cap on concurrent uploads.
    #[arg(long, default_value_t = 8)]
    max_streams: usize,
    /// Cap on objects per listed prefix.
    #[arg(long, default_value_t = DEFAULT_MAX_OBJECTS)]
    max_objects: usize,
    /// Write a JSON report here (also printed to stdout).
    #[arg(long)]
    json: Option<PathBuf>,
}

#[derive(Subcommand)]
enum Cmd {
    /// Back up SOURCE to the repository, streaming receipts as commits verify.
    Backup {
        #[command(flatten)]
        r: RepoArgs,
        #[arg(long)]
        source: PathBuf,
        /// Exclude a path pattern (repeatable). With a '/', matches the relative path; without,
        /// any component. '*' and '?' stay within a component, '**' crosses them.
        #[arg(long)]
        exclude: Vec<String>,
        /// Back up only these relative paths (NUL-separated file).
        #[arg(long)]
        files_from: Option<PathBuf>,
        /// Append receipts (JSONL v1) to this file.
        #[arg(long)]
        receipts: Option<PathBuf>,
        /// Cache directory for verified index objects.
        #[arg(long)]
        state: Option<PathBuf>,
        #[arg(long, default_value = "64Mi")]
        cache_budget: String,
        /// Fixed zstd level (rate matching off).
        #[arg(long)]
        level: Option<i32>,
        /// Fixed number of concurrent uploads (controller off).
        #[arg(long)]
        streams: Option<usize>,
        #[arg(long)]
        one_file_system: bool,
        /// Read every file even when its metadata is unchanged.
        #[arg(long)]
        rehash: bool,
        /// Label stored as the snapshot root (default: the source path).
        #[arg(long)]
        label: Option<String>,
        /// Commit (and receipt) at least this often.
        #[arg(long, default_value_t = 15)]
        commit_secs: u64,
    },
    /// Restore a run into an empty or absent directory.
    Restore {
        #[command(flatten)]
        r: RepoArgs,
        #[arg(long)]
        target: PathBuf,
        /// The run to restore (default: the newest complete snapshot).
        #[arg(long)]
        run: Option<String>,
        #[arg(long)]
        keep_setid: bool,
    },
    /// Rebuild a run's receipts from the bucket alone.
    Receipts {
        #[command(flatten)]
        r: RepoArgs,
        #[arg(long)]
        run: String,
        #[arg(long)]
        out: Option<PathBuf>,
    },
    /// Check a run: its packs are listed with the recorded checksums (--deep: downloaded,
    /// decoded and BLAKE3-checked per chunk and per file).
    Verify {
        #[command(flatten)]
        r: RepoArgs,
        #[arg(long)]
        run: Option<String>,
        #[arg(long)]
        deep: bool,
    },
    /// List complete snapshots.
    Snapshots {
        #[command(flatten)]
        r: RepoArgs,
    },
    /// Generate a deterministic workload (w1, w2, w3, mac) of about SIZE bytes.
    Gen {
        #[arg(long)]
        kind: String,
        #[arg(long)]
        size: String,
        #[arg(long, default_value_t = 1)]
        seed: u64,
        #[arg(long)]
        dir: PathBuf,
    },
    /// Apply the second-run change (W4) to a generated workload.
    Mutate {
        #[arg(long)]
        dir: PathBuf,
        #[arg(long, default_value_t = 1)]
        seed: u64,
    },
}

fn size(s: &str, what: &str) -> Result<u64, String> {
    parse_size(s).ok_or_else(|| format!("bad {what} {s:?}"))
}

fn budget(r: &RepoArgs) -> Result<Budget, String> {
    let total = size(&r.mem_budget, "--mem-budget")?;
    let p = size(&r.max_pack, "--max-pack")?;
    Budget::compute(total, p, r.max_streams).map_err(|e| e.to_string())
}

fn open(
    r: &RepoArgs,
    p_max: u64,
) -> Result<
    (
        std::sync::Arc<dyn Transport>,
        std::sync::Arc<xfer::transport::Counters>,
    ),
    String,
> {
    let heap = budget(r).ok().map(|b| b.rcd_heap());
    let t = transport::open_limited(&r.repo, &r.rclone, p_max, heap).map_err(|e| e.to_string())?;
    let m = Metered::new(t);
    let c = m.counters.clone();
    Ok((std::sync::Arc::new(m), c))
}

fn emit(r: &RepoArgs, v: serde_json::Value) {
    let s = serde_json::to_string_pretty(&v).unwrap_or_default();
    println!("{s}");
    if let Some(p) = &r.json {
        if let Err(e) = std::fs::write(p, &s) {
            eprintln!("prunedisk-xfer: cannot write {}: {e}", p.display());
        }
    }
}

fn read_files_from(p: &PathBuf) -> Result<Vec<Vec<u8>>, String> {
    let b = std::fs::read(p).map_err(|e| format!("{}: {e}", p.display()))?;
    Ok(b.split(|&c| c == 0)
        .filter(|s| !s.is_empty())
        .map(|s| s.to_vec())
        .collect())
}

/// A notice for the person running the command, on stderr.
fn notice(m: &str) {
    eprintln!("prunedisk-xfer: {m}");
}

fn run(cli: Cli) -> Result<i32, String> {
    match cli.cmd {
        Cmd::Backup {
            r,
            source,
            exclude,
            files_from,
            receipts,
            state,
            cache_budget,
            level,
            streams,
            one_file_system,
            rehash,
            label,
            commit_secs,
        } => {
            let b = budget(&r)?;
            if let Some(k) = streams {
                if k == 0 || k > b.k {
                    return Err(format!(
                        "--streams {k} is outside 1..={} for this budget",
                        b.k
                    ));
                }
            }
            eprintln!("prunedisk-xfer: {}", b.describe());
            let mut o = Options::new(&r.repo, &source, b);
            o.filter = Filter::new(&exclude).map_err(|e| e.to_string())?;
            o.files_from = files_from.as_ref().map(read_files_from).transpose()?;
            o.receipts = receipts;
            o.state = state.or_else(|| default_state_dir(&r.repo));
            o.cache_budget = size(&cache_budget, "--cache-budget")?;
            o.level = level;
            o.streams = streams;
            o.one_file_system = one_file_system;
            o.rehash = rehash;
            o.max_objects = r.max_objects;
            o.commit_every = Duration::from_secs(commit_secs.max(1));
            o.notice = Some(notice as fn(&str));
            if let Some(l) = label {
                o.root_label = l;
            }
            let (t, c) = open(&r, b.p_max)?;
            let rep = backup(t.clone(), &o).map_err(|e| e.to_string())?;
            let desc = t.describe();
            drop(t);
            let code = rep.exit_code;
            emit(
                &r,
                serde_json::json!({"command": "backup", "transport": desc, "report": rep, "requests": c.snapshot()}),
            );
            Ok(code)
        }
        Cmd::Restore {
            r,
            target,
            run,
            keep_setid,
        } => {
            let b = budget(&r)?;
            let (t, c) = open(&r, b.p_max)?;
            let rep = restore(
                t,
                &RestoreOpts {
                    target,
                    run,
                    keep_setid,
                    budget: b,
                    max_objects: r.max_objects,
                    policy: RetryPolicy::default(),
                },
            )
            .map_err(|e| e.to_string())?;
            let code = rep.exit_code;
            emit(
                &r,
                serde_json::json!({"command": "restore", "report": rep, "requests": c.snapshot()}),
            );
            Ok(code)
        }
        Cmd::Receipts { r, run, out } => {
            let b = budget(&r)?;
            let (t, _) = open(&r, b.p_max)?;
            let rs =
                rebuild_receipts(t, &r.repo, &run, &b, r.max_objects).map_err(|e| e.to_string())?;
            let mut buf = Vec::new();
            for x in &rs {
                serde_json::to_writer(&mut buf, x).map_err(|e| e.to_string())?;
                buf.push(b'\n');
            }
            match out {
                Some(p) => std::fs::write(&p, &buf).map_err(|e| format!("{}: {e}", p.display()))?,
                None => {
                    use std::io::Write;
                    std::io::stdout()
                        .write_all(&buf)
                        .map_err(|e| e.to_string())?;
                }
            }
            Ok(0)
        }
        Cmd::Verify { r, run, deep } => {
            let b = budget(&r)?;
            let (t, c) = open(&r, b.p_max)?;
            let rep =
                verify(t, run.as_deref(), deep, &b, r.max_objects).map_err(|e| e.to_string())?;
            let code = rep.exit_code;
            emit(
                &r,
                serde_json::json!({"command": "verify", "report": rep, "requests": c.snapshot()}),
            );
            Ok(code)
        }
        Cmd::Snapshots { r } => {
            let b = budget(&r)?;
            let (t, _) = open(&r, b.p_max)?;
            let repo = xfer::repo::Repo::new(t, r.max_objects, RetryPolicy::default())
                .map_err(|e| e.to_string())?;
            let s = repo.snapshots().map_err(|e| e.to_string())?;
            emit(
                &r,
                serde_json::json!({"command": "snapshots", "snapshots": s}),
            );
            Ok(0)
        }
        Cmd::Gen {
            kind,
            size: sz,
            seed,
            dir,
        } => {
            let w = xfer::gen::Workload::parse(&kind)
                .ok_or_else(|| format!("unknown workload {kind:?}"))?;
            let total = size(&sz, "--size")?;
            let n = xfer::gen::generate(&dir, w, total, seed).map_err(|e| e.to_string())?;
            println!("{{\"command\":\"gen\",\"files\":{n}}}");
            Ok(0)
        }
        Cmd::Mutate { dir, seed } => {
            xfer::gen::mutate(&dir, seed).map_err(|e| e.to_string())?;
            Ok(0)
        }
    }
}

fn main() -> ExitCode {
    xfer::util::tune_allocator();
    let cli = Cli::parse();
    let _ = MIB;
    match run(cli) {
        Ok(c) => ExitCode::from(c as u8),
        Err(e) => {
            eprintln!("prunedisk-xfer: {e}");
            ExitCode::from(3)
        }
    }
}
