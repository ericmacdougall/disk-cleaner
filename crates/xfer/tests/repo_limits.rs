//! The shared repository's limits: concurrent runs (NEG-14), the local index cache (NEG-19) and
//! the object caps (NEG-21, NEG-22).

mod common;
use common::*;
use std::collections::BTreeSet;
use std::path::Path;
use std::sync::atomic::Ordering;
use std::time::{Duration, SystemTime};
use xfer::Error;

/// Every key in the local store, so a test can show that a refused run wrote nothing.
fn keys(store: &Path) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    let mut stack = vec![store.to_path_buf()];
    while let Some(d) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&d) else {
            continue;
        };
        for e in rd.flatten() {
            let p = e.path();
            if p.is_dir() {
                stack.push(p);
            } else {
                let k = p.strip_prefix(store).expect("in store");
                out.insert(k.to_string_lossy().replace('\\', "/"));
            }
        }
    }
    out
}

fn index_count(store: &Path) -> usize {
    keys(store)
        .iter()
        .filter(|k| k.starts_with("v1/index/"))
        .count()
}

fn small_tree(root: &Path, seed: u64) {
    write(&root.join("a.bin"), &rand_bytes(seed, 1 << 20));
    write(&root.join("d/b.txt"), format!("tree {seed}\n").as_bytes());
}

fn restores_exactly(env: &Env, src: &Path, run: &str, max_objects: usize) {
    let target = env.p(&format!("restored-{run}"));
    let mut ro = restore_opts(&target, Some(run.to_string()));
    ro.max_objects = max_objects;
    let rr = xfer::restore::restore(env.local(), &ro).expect("restore");
    assert_eq!(rr.exit_code, 0, "{rr:#?}");
    assert_eq!(manifest(src), manifest(&target));
}

#[test]
fn two_concurrent_runs_from_an_empty_repository_both_restore() {
    let env = Env::new();
    small_tree(&env.p("one"), 1);
    small_tree(&env.p("two"), 2);
    let runs: Vec<String> = std::thread::scope(|s| {
        let hs: Vec<_> = ["one", "two"]
            .into_iter()
            .map(|n| {
                let env = &env;
                s.spawn(move || {
                    let mut o = opts(&env.p(n), &env.p(&format!("{n}.jsonl")));
                    o.root_label = n.into();
                    let rep = xfer::backup::backup(env.local(), &o).expect("backup");
                    assert_eq!(rep.exit_code, 0, "{rep:#?}");
                    rep.run
                })
            })
            .collect();
        hs.into_iter().map(|h| h.join().expect("join")).collect()
    });
    assert_ne!(runs[0], runs[1]);
    let fmt = std::fs::read(env.p("store/v1/format.json")).expect("format");
    assert_eq!(fmt, xfer::repo::FORMAT_BYTES);
    restores_exactly(
        &env,
        &env.p("one"),
        &runs[0],
        xfer::repo::DEFAULT_MAX_OBJECTS,
    );
    restores_exactly(
        &env,
        &env.p("two"),
        &runs[1],
        xfer::repo::DEFAULT_MAX_OBJECTS,
    );
}

#[test]
fn a_different_format_object_is_refused_before_any_data() {
    let env = Env::new();
    small_tree(&env.p("src"), 3);
    write(
        &env.p("store/v1/format.json"),
        b"{\"format\":\"prunedisk-xfer\",\"v\":1,\"avg\":65536}\n",
    );
    let before = keys(&env.p("store"));
    let (t, c) = env.store();
    let r = xfer::backup::backup(t, &opts(&env.p("src"), &env.p("r.jsonl")));
    assert!(matches!(r, Err(Error::Refused(_))), "{r:?}");
    assert_eq!(c.puts.load(Ordering::Relaxed), 0);
    assert_eq!(keys(&env.p("store")), before);
    assert!(receipts(&env.p("r.jsonl")).is_empty());
}

#[test]
fn objects_under_v2_never_change_a_v1_restore() {
    let env = Env::new();
    small_tree(&env.p("src"), 4);
    let rep =
        xfer::backup::backup(env.local(), &opts(&env.p("src"), &env.p("r.jsonl"))).expect("backup");
    assert_eq!(rep.exit_code, 0);
    write(&env.p("store/v2/format.json"), b"{\"v\":2}\n");
    write(&env.p("store/v2/index/x/000000.idx"), b"not a v1 index");
    write(&env.p("store/v2/snapshots/r999.json"), b"{}");
    restores_exactly(
        &env,
        &env.p("src"),
        &rep.run,
        xfer::repo::DEFAULT_MAX_OBJECTS,
    );
    let v = xfer::restore::verify(
        env.local(),
        Some(&rep.run),
        true,
        &budget(),
        xfer::repo::DEFAULT_MAX_OBJECTS,
    )
    .expect("verify");
    assert_eq!(v.exit_code, 0, "{v:#?}");
}

fn cached_idx(state: &Path) -> Vec<std::path::PathBuf> {
    let Ok(rd) = std::fs::read_dir(state) else {
        return Vec::new();
    };
    rd.flatten()
        .map(|e| e.path())
        .filter(|p| p.to_string_lossy().ends_with(".idx"))
        .collect()
}

/// The cache holds index objects a run fetched, so the first run caches nothing and the second
/// caches the first run's index. Returns the last run.
fn runs_with_state(env: &Env, o: &mut xfer::backup::Options, n: usize) -> String {
    let mut run = String::new();
    for i in 0..n {
        write(&env.p(&format!("src/step{i}.txt")), b"one more step\n");
        o.receipts = Some(env.p(&format!("s{i}.jsonl")));
        let rep = xfer::backup::backup(env.local(), o).expect("backup");
        assert_eq!(rep.exit_code, 0, "{rep:#?}");
        run = rep.run;
    }
    run
}

/// The store object a cache file copies (`v1_index_<run>_<seq>.idx` names `v1/index/<run>/<seq>.idx`).
fn store_copy(env: &Env, cached: &Path) -> Vec<u8> {
    let name = cached.file_name().expect("name").to_string_lossy();
    std::fs::read(env.p("store").join(name.replace('_', "/"))).expect("store object")
}

#[test]
fn a_cache_budget_below_the_index_size_runs_uncached() {
    // the control: with the default budget the second run caches the first run's index
    let env = Env::new();
    small_tree(&env.p("src"), 5);
    let mut o = opts(&env.p("src"), &env.p("r.jsonl"));
    o.state = Some(env.p("state"));
    runs_with_state(&env, &mut o, 2);
    assert!(
        !cached_idx(&env.p("state")).is_empty(),
        "the control caches nothing"
    );

    let env = Env::new();
    small_tree(&env.p("src"), 5);
    let mut o = opts(&env.p("src"), &env.p("r.jsonl"));
    o.state = Some(env.p("state"));
    o.cache_budget = 1;
    let run = runs_with_state(&env, &mut o, 3);
    assert_eq!(
        cached_idx(&env.p("state")),
        Vec::<std::path::PathBuf>::new()
    );
    restores_exactly(&env, &env.p("src"), &run, xfer::repo::DEFAULT_MAX_OBJECTS);
}

#[test]
fn a_corrupt_truncated_or_stale_cache_is_never_trusted() {
    let env = Env::new();
    small_tree(&env.p("src"), 6);
    let mut o = opts(&env.p("src"), &env.p("r.jsonl"));
    o.state = Some(env.p("state"));
    runs_with_state(&env, &mut o, 3);
    let cached = cached_idx(&env.p("state"));
    assert!(cached.len() >= 2, "{cached:?}");
    for p in &cached {
        assert_eq!(std::fs::read(p).expect("cached"), store_copy(&env, p));
    }

    // corrupt one cached index (same size), truncate another if there is one
    let mut b = std::fs::read(&cached[0]).expect("read");
    b[0] ^= 0xff;
    std::fs::write(&cached[0], &b).expect("corrupt");
    let t = std::fs::read(&cached[1]).expect("read");
    std::fs::write(&cached[1], &t[..t.len() / 2]).expect("truncate");
    // a stale temporary from a crashed run (two hours old) and a fresh one from a live run
    let stale = env.p("state/v1_index_x_000000.idx.tmp-rdead");
    let live = env.p("state/v1_index_y_000000.idx.tmp-rlive");
    write(&stale, b"partial");
    write(&live, b"partial");
    let old = SystemTime::now() - Duration::from_secs(2 * 3600);
    std::fs::File::options()
        .write(true)
        .open(&stale)
        .expect("open")
        .set_modified(old)
        .expect("age the stale tmp");

    // the next run refuses both bad copies and fetches them from the store again
    let run = runs_with_state(&env, &mut o, 1);
    for p in &cached[..2] {
        let now = std::fs::read(p).expect("recached");
        assert_eq!(now, store_copy(&env, p), "{} was trusted", p.display());
    }
    assert_ne!(std::fs::read(&cached[0]).expect("recached"), b);
    assert!(!stale.exists(), "a stale tmp file survived");
    assert!(live.exists(), "a live run's tmp file was removed");
    restores_exactly(&env, &env.p("src"), &run, xfer::repo::DEFAULT_MAX_OBJECTS);
}

/// Three runs, so the repository holds at least three index objects.
fn three_runs(env: &Env) -> Vec<String> {
    small_tree(&env.p("src"), 7);
    let mut runs = Vec::new();
    for i in 0..3 {
        write(&env.p(&format!("src/run{i}.txt")), b"one more\n");
        let rep = xfer::backup::backup(
            env.local(),
            &opts(&env.p("src"), &env.p(&format!("r{i}.jsonl"))),
        )
        .expect("backup");
        assert_eq!(rep.exit_code, 0);
        runs.push(rep.run);
    }
    runs
}

#[test]
fn backup_refuses_when_the_index_is_over_90_percent_of_the_cap() {
    let env = Env::new();
    three_runs(&env);
    let n = index_count(&env.p("store"));
    assert!(n >= 3);
    let before = keys(&env.p("store"));
    let (t, c) = env.store();
    let mut o = opts(&env.p("src"), &env.p("refused.jsonl"));
    o.max_objects = n; // n > 0.9 n
    let r = xfer::backup::backup(t, &o);
    match r {
        Err(Error::Refused(m)) => {
            assert!(
                m.contains(&n.to_string()) && m.contains("compaction"),
                "{m}"
            );
        }
        other => panic!("not refused: {other:?}"),
    }
    assert_eq!(c.puts.load(Ordering::Relaxed), 0);
    assert_eq!(keys(&env.p("store")), before);
    // the control: a cap with room (n at 80%) runs
    let mut o = opts(&env.p("src"), &env.p("ok.jsonl"));
    o.max_objects = n * 5 / 4 + 1;
    let rep = xfer::backup::backup(env.local(), &o).expect("backup under the cap");
    assert_eq!(rep.exit_code, 0);
}

#[test]
fn restore_and_verify_over_the_cap_refuse_and_succeed_with_max_objects() {
    let env = Env::new();
    let runs = three_runs(&env);
    let n = index_count(&env.p("store"));
    let cap = n - 1;
    let target = env.p("restored");
    let mut ro = restore_opts(&target, Some(runs[0].clone()));
    ro.max_objects = cap;
    match xfer::restore::restore(env.local(), &ro) {
        Err(Error::Refused(m)) => assert!(m.contains(&n.to_string()), "{m}"),
        other => panic!("restore not refused: {other:?}"),
    }
    assert!(!target.exists(), "the refused restore created its target");
    let v = xfer::restore::verify(env.local(), Some(&runs[0]), false, &budget(), cap);
    match v {
        Err(Error::Refused(m)) => assert!(m.contains(&n.to_string()), "{m}"),
        other => panic!("verify not refused: {other:?}"),
    }
    // --max-objects raised: both succeed (restore of the newest run, which has every file)
    let v = xfer::restore::verify(env.local(), Some(&runs[2]), true, &budget(), n)
        .expect("verify with the raised cap");
    assert_eq!(v.exit_code, 0, "{v:#?}");
    restores_exactly(&env, &env.p("src"), &runs[2], n);
}
