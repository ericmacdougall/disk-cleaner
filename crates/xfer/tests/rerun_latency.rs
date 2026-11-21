//! W4 in miniature: a rerun's cost is store round trips, not bytes. Every request to B2 is a
//! round trip of 0.1 to 1 s, so a rerun with nothing to upload must not wait on one request
//! after another: its load phase issues the listings and the format check at once, takes the
//! commit objects it already holds from the state dir's cache, and its commit puts the `.idx`
//! and the `.files` together.
//!
//! The checks are structural, never wall-clock ratios: a transport that sleeps a fixed time per
//! request records when each request started and ended, and the test counts the longest chain
//! of requests that waited on one another.
//!
//! Failing first: the tree before this change (cache of `.idx` only, serial load, serial
//! commit) and `--features broken-serial-load` both fail; `--features broken-commit-pair`
//! fails the commit-pair test.

mod common;
use common::*;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use xfer::hash::HashKind;
use xfer::transport::fault::{FaultConfig, Faulty};
use xfer::transport::{RemoteObject, Transport};

struct Op {
    kind: &'static str,
    key: String,
    start: Instant,
    end: Instant,
}

/// Sleeps `delay` inside every request and records it.
struct Latent {
    inner: Arc<dyn Transport>,
    delay: Duration,
    /// extra time for puts of keys under a prefix
    put_extra: Vec<(&'static str, Duration)>,
    ops: Mutex<Vec<Op>>,
}

impl Latent {
    fn new(inner: Arc<dyn Transport>, delay: Duration) -> Arc<Latent> {
        Latent::with_puts(inner, delay, Vec::new())
    }
    fn with_puts(
        inner: Arc<dyn Transport>,
        delay: Duration,
        put_extra: Vec<(&'static str, Duration)>,
    ) -> Arc<Latent> {
        Arc::new(Latent {
            inner,
            delay,
            put_extra,
            ops: Mutex::new(Vec::new()),
        })
    }
    fn timed<T>(&self, kind: &'static str, key: &str, f: impl FnOnce() -> T) -> T {
        let start = Instant::now();
        let extra = self
            .put_extra
            .iter()
            .filter(|(p, _)| kind == "put" && key.starts_with(p))
            .map(|(_, d)| *d)
            .sum::<Duration>();
        std::thread::sleep(self.delay + extra);
        let r = f();
        self.ops.lock().expect("ops").push(Op {
            kind,
            key: key.to_string(),
            start,
            end: Instant::now(),
        });
        r
    }
    fn count(&self, kind: &str, prefix: &str) -> usize {
        self.ops
            .lock()
            .expect("ops")
            .iter()
            .filter(|o| o.kind == kind && o.key.starts_with(prefix))
            .count()
    }
    /// The longest chain of requests each of which started after the previous one ended,
    /// among the requests that started before `until` (all of them when None).
    fn chain(&self, until: Option<Instant>) -> usize {
        let ops = self.ops.lock().expect("ops");
        let mut v: Vec<&Op> = ops
            .iter()
            .filter(|o| until.is_none_or(|u| o.start < u))
            .collect();
        v.sort_by_key(|o| o.start);
        let mut best = vec![1usize; v.len()];
        for i in 0..v.len() {
            for j in 0..i {
                if v[i].start >= v[j].end {
                    best[i] = best[i].max(best[j] + 1);
                }
            }
        }
        best.into_iter().max().unwrap_or(0)
    }
    fn first_put(&self) -> Option<Instant> {
        let ops = self.ops.lock().expect("ops");
        ops.iter()
            .filter(|o| o.kind == "put")
            .map(|o| o.start)
            .min()
    }
    /// Key and end of every op of `kind` on keys under `prefix`.
    fn ends(&self, kind: &str, prefix: &str) -> Vec<(String, Instant)> {
        let ops = self.ops.lock().expect("ops");
        ops.iter()
            .filter(|o| o.kind == kind && o.key.starts_with(prefix))
            .map(|o| (o.key.clone(), o.end))
            .collect()
    }
    /// (start, end) of the first op of `kind` on `key`.
    fn span(&self, kind: &str, key: &str) -> Option<(Instant, Instant)> {
        let ops = self.ops.lock().expect("ops");
        ops.iter()
            .filter(|o| o.kind == kind && o.key == key)
            .map(|o| (o.start, o.end))
            .min()
    }
    fn describe_ops(&self) -> String {
        let ops = self.ops.lock().expect("ops");
        let t0 = ops.iter().map(|o| o.start).min();
        let mut v: Vec<&Op> = ops.iter().collect();
        v.sort_by_key(|o| o.start);
        v.iter()
            .map(|o| {
                let at = t0.map_or(0, |t| o.start.duration_since(t).as_millis());
                format!("{at:>6} ms {} {}", o.kind, o.key)
            })
            .collect::<Vec<_>>()
            .join("\n")
    }
}

impl Transport for Latent {
    fn hash_kind(&self) -> Option<HashKind> {
        self.inner.hash_kind()
    }
    fn put(&self, key: &str, data: &[u8]) -> xfer::Result<()> {
        self.timed("put", key, || self.inner.put(key, data))
    }
    fn stat(&self, key: &str) -> xfer::Result<Option<RemoteObject>> {
        self.timed("stat", key, || self.inner.stat(key))
    }
    fn list(&self, prefix: &str) -> xfer::Result<Vec<RemoteObject>> {
        self.timed("list", prefix, || self.inner.list(prefix))
    }
    fn get(&self, key: &str, max: u64) -> xfer::Result<Vec<u8>> {
        self.timed("get", key, || self.inner.get(key, max))
    }
    fn describe(&self) -> String {
        "latent".into()
    }
}

fn first_run(env: &Env) -> xfer::backup::Options {
    sample_tree(&env.p("src"));
    let mut o = opts(&env.p("src"), &env.p("r1.jsonl"));
    o.state = Some(env.p("state"));
    let rep = xfer::backup::backup(env.local(), &o).expect("first backup");
    assert_eq!(rep.exit_code, 0, "{rep:#?}");
    o.receipts = Some(env.p("r2.jsonl"));
    o
}

#[test]
fn a_warm_rerun_downloads_no_commit_objects() {
    let env = Env::new();
    let o = first_run(&env);
    let t = Latent::new(env.local(), Duration::ZERO);
    let rep = xfer::backup::backup(t.clone(), &o).expect("rerun");
    assert_eq!(rep.exit_code, 0, "{rep:#?}");
    assert_eq!(rep.packs, 0, "nothing changed: {rep:#?}");
    for prefix in ["v1/index/", "v1/files/", "v1/snapshots/", "v1/format.json"] {
        assert_eq!(
            t.count("get", prefix),
            0,
            "the rerun downloaded {prefix} objects the first run wrote with this state dir:\n{}",
            t.describe_ops()
        );
    }
    // a third run still restores and reports the second run's table as its previous one
    let rep3 = xfer::backup::backup(env.local(), &o).expect("third run");
    assert_eq!(rep3.exit_code, 0, "{rep3:#?}");
    assert!(rep3.reused_files > 0, "{rep3:#?}");
}

#[test]
fn a_rerun_waits_on_few_round_trips() {
    let env = Env::new();
    let o = first_run(&env);
    let t = Latent::new(env.local(), Duration::from_millis(150));
    let rep = xfer::backup::backup(t.clone(), &o).expect("rerun");
    assert_eq!(rep.exit_code, 0, "{rep:#?}");
    assert_eq!(rep.packs, 0, "nothing changed: {rep:#?}");
    // Load: the index, pack and snapshot listings and the format stat at once, nothing after.
    let load = t.chain(t.first_put());
    assert_eq!(
        load,
        1,
        "the load phase waited on {load} requests in a row:\n{}",
        t.describe_ops()
    );
    // Whole run: load (1), the .idx and .files put together then stat together (2), the
    // snapshot put and stat (2).
    let all = t.chain(None);
    assert!(
        all <= 5,
        "the rerun waited on {all} requests in a row:\n{}",
        t.describe_ops()
    );
}

/// A commit puts the `.idx` and the `.files` at once, so a crash or a failed put can leave a
/// `.files` without its `.idx`. Nothing was receipted from it, and restore must not use it,
/// even when every chunk it names is stored and indexed by an earlier run.
#[test]
fn a_files_object_without_its_index_is_not_restored() {
    let env = Env::new();
    sample_tree(&env.p("src"));
    let first = xfer::backup::backup(env.local(), &opts(&env.p("src"), &env.p("r0.jsonl")))
        .expect("first backup");
    assert_eq!(first.exit_code, 0, "{first:#?}");
    let t = Arc::new(Faulty::new(
        env.local(),
        FaultConfig {
            fail_put_prefix: "v1/index/".into(),
            ..Default::default()
        },
    ));
    let o = opts(&env.p("src"), &env.p("r.jsonl"));
    let rep = xfer::backup::backup(t.clone(), &o).expect("backup");
    assert_eq!(rep.exit_code, 3, "{rep:#?}");
    assert!(receipts(&env.p("r.jsonl")).is_empty());
    let files = env
        .local()
        .list(&format!("v1/files/{}", rep.run))
        .expect("list");
    assert!(
        !files.is_empty(),
        "the .files put did not land beside the failed .idx put; the test needs it"
    );
    let target = env.p("target");
    let r = xfer::restore::restore(env.local(), &restore_opts(&target, Some(rep.run.clone())));
    let restored = target.exists() && std::fs::read_dir(&target).map_or(0, |d| d.count()) > 0;
    assert!(
        r.is_err() || !restored,
        "restore used a .files object whose .idx was never stored: {r:?}"
    );
    let rebuilt =
        xfer::restore::rebuild_receipts(env.local(), "local:test", &rep.run, &budget(), 100_001)
            .expect("rebuild");
    assert!(rebuilt.is_empty(), "{} receipts rebuilt", rebuilt.len());
}

/// W1's t50 in miniature: commits are slow (each object put takes 1.2 s, as small puts can on
/// a busy B2 uplink) while packs keep arriving. A commit's `.idx` and `.files` go up at once,
/// and the next commit starts as soon as the previous one ends (the one-second spacing counts
/// from a commit's start), so receipts keep pace with the uploads.
#[test]
fn slow_commits_follow_each_other_and_put_both_objects_at_once() {
    let env = Env::new();
    let src = env.p("src");
    for i in 0..96u64 {
        write(
            &src.join(format!("f{i:02}.bin")),
            &rand_bytes(300 + i, 1 << 20),
        );
    }
    // Commits slower than the one-second spacing and slower than a pack takes to make (0.9 s
    // put here, about 2.5 s on a macOS CI runner in a debug build), so packs are verified while
    // a commit is in flight and keep coming for several commits.
    let slow = Duration::from_millis(3000);
    let t = Latent::with_puts(
        env.local(),
        Duration::ZERO,
        vec![
            ("v1/packs/", Duration::from_millis(900)),
            ("v1/index/", slow),
            ("v1/files/", slow),
        ],
    );
    let mut o = opts(&src, &env.p("receipts.jsonl"));
    o.streams = Some(1);
    o.commit_every = Duration::from_secs(600);
    o.commit_bytes = 1 << 40;
    let rep = xfer::backup::backup(t.clone(), &o).expect("backup");
    assert_eq!(rep.exit_code, 0, "{rep:#?}");
    assert!(rep.commits >= 2, "{rep:#?}");
    let run = rep.run.clone();
    let store = env.local();
    let mut spans = Vec::new();
    let stored = store.list("v1/index").expect("list idx");
    let mut listed: Vec<std::collections::HashSet<String>> = Vec::new();
    for seq in 0..rep.commits {
        let ik = xfer::repo::idx_key(&run, seq);
        let o = stored.iter().find(|o| o.key == ik).expect("idx stored");
        let obj: xfer::repo::IdxObj =
            xfer::repo::decode(&store.get(&o.key, o.size).expect("get idx")).expect("idx");
        listed.push(obj.packs.into_iter().map(|p| p.key).collect());
    }
    for seq in 0..rep.commits {
        let ik = xfer::repo::idx_key(&run, seq);
        let fk = xfer::repo::files_key(&run, seq);
        let i = t.span("put", &ik).expect("idx put");
        let f = t.span("put", &fk).expect("files put");
        assert!(
            i.0 < f.1 && f.0 < i.1,
            "commit {seq}: the .idx and .files puts did not overlap:\n{}",
            t.describe_ops()
        );
        let stats = [t.span("stat", &ik), t.span("stat", &fk)];
        let end = stats
            .iter()
            .flatten()
            .map(|s| s.1)
            .chain([i.1, f.1])
            .max()
            .expect("end");
        spans.push((i.0.min(f.0), end));
    }
    // Work was waiting when commit k ended if a pack verified (its stat returned) before then
    // and no commit up to k lists it; that is read from the recorded requests and the stored
    // .idx objects, not assumed from timing. Each such commit must be followed at once (the
    // one-second spacing runs from the start of the commit, which is over 3 s ago), not a
    // second later. The last commit is left out as a successor: once the input is done the
    // final commit starts without the spacing check, so it proves nothing.
    let verified = t.ends("stat", "v1/packs/");
    let mut waiting_boundaries = 0;
    for k in 0..spans.len().saturating_sub(2) {
        let end = spans[k].1;
        let waiting = verified
            .iter()
            .any(|(key, at)| *at < end && !listed[..=k].iter().any(|l| l.contains(key)));
        if !waiting {
            continue;
        }
        waiting_boundaries += 1;
        let gap = spans[k + 1].0.saturating_duration_since(end);
        assert!(
            gap < Duration::from_millis(500),
            "{} ms between the end of commit {k}, with a verified pack waiting, and the next \
             start:\n{}",
            gap.as_millis(),
            t.describe_ops()
        );
    }
    assert!(
        waiting_boundaries >= 1,
        "no commit ended with a verified pack waiting, so nothing was checked:\n{}",
        t.describe_ops()
    );
}
