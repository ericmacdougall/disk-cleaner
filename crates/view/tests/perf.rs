//! Performance at one million nodes (spec AC-9, NEG-12). Thresholds were registered in the spec
//! before measuring. Ignored by default; CI runs them on ubuntu-latest in release mode:
//! `cargo test --release -p disk-cleaner-view --test perf -- --ignored --nocapture --test-threads=1`
//! Each measurement runs twice in the same process; the better run is compared and both are
//! printed. Memory is measured separately by `examples/perfmem.rs`.

// The real-walk measurement writes its fixture.
#![allow(clippy::disallowed_methods)]

use disk_cleaner_view::synth::{synthetic_tree, wide_tree, SplitMix64, SEED};
use disk_cleaner_view::table::{Sort, SortKey, TreeView};
use disk_cleaner_view::walk::{walk, WalkOptions};
use std::sync::atomic::AtomicBool;
use std::time::{Duration, Instant};

const N: usize = 1_000_000;

fn ms(d: Duration) -> f64 {
    d.as_secs_f64() * 1000.0
}

fn report(name: &str, runs: &[f64], limit: f64) -> f64 {
    let best = runs.iter().cloned().fold(f64::MAX, f64::min);
    println!(
        "PERF {name} runs_ms={:?} best_ms={best:.3} limit_ms={limit} seed={SEED}",
        runs.iter().map(|r| format!("{r:.3}")).collect::<Vec<_>>()
    );
    best
}

fn percentile(mut v: Vec<f64>, p: f64) -> f64 {
    v.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let i = ((v.len() as f64 * p).ceil() as usize).clamp(1, v.len()) - 1;
    v[i]
}

/// Build the tree and the view, with the root open.
fn build_index() -> (TreeView, f64) {
    let start = Instant::now();
    let tree = synthetic_tree(N, SEED);
    let mut view = TreeView::new(tree);
    let root = view.tree().roots()[0];
    view.expand(root);
    (view, ms(start.elapsed()))
}

#[test]
#[ignore = "performance measurement"]
fn index_build_under_3_s() {
    let runs: Vec<f64> = (0..2).map(|_| build_index().1).collect();
    assert!(report("index_build_1m", &runs, 3000.0) < 3000.0);
}

fn page_p99(view: &TreeView, seed: u64) -> f64 {
    let mut rng = SplitMix64::new(seed);
    let rows = view.row_count() as u64;
    let times: Vec<f64> = (0..1000)
        .map(|_| {
            let off = rng.below(rows.saturating_sub(200).max(1)) as usize;
            let start = Instant::now();
            let page = view.page(off, 200);
            let t = ms(start.elapsed());
            assert!(page.len() <= 200);
            t
        })
        .collect();
    percentile(times, 0.99)
}

#[test]
#[ignore = "performance measurement"]
fn page_of_200_rows_p99_under_5_ms_and_resort_under_1_s() {
    let (mut view, _) = build_index();
    let start = Instant::now();
    view.expand_all();
    println!("PERF expand_all_1m_ms={:.1}", ms(start.elapsed()));
    assert!(view.row_count() >= N - 1, "{} rows", view.row_count());
    let runs: Vec<f64> = (0..2).map(|r| page_p99(&view, SEED + r)).collect();
    let page = report("page_200_p99_1m_expanded", &runs, 5.0);
    let mut resort = Vec::new();
    for (i, key) in [SortKey::Name, SortKey::Modified].into_iter().enumerate() {
        let start = Instant::now();
        view.set_sort(Sort {
            key,
            descending: i == 1,
        });
        resort.push(ms(start.elapsed()));
    }
    let sort = report("resort_expanded_1m", &resort, 1000.0);
    assert!(page < 5.0, "page p99 {page} ms");
    assert!(sort < 1000.0, "re-sort {sort} ms");
}

#[test]
#[ignore = "performance measurement"]
fn expanding_a_folder_of_100k_children_under_300_ms() {
    let runs: Vec<f64> = (0..2)
        .map(|_| {
            let mut view = TreeView::new(wide_tree(100_000, SEED));
            let root = view.tree().roots()[0];
            let start = Instant::now();
            view.expand(root);
            ms(start.elapsed())
        })
        .collect();
    assert!(report("expand_100k_children", &runs, 300.0) < 300.0);
}

#[test]
#[ignore = "performance measurement"]
fn a_folder_of_a_million_children_expands_under_3_s_and_pages_fast() {
    let mut expand = Vec::new();
    let mut pages = Vec::new();
    for r in 0..2 {
        let mut view = TreeView::new(wide_tree(N, SEED));
        let root = view.tree().roots()[0];
        let start = Instant::now();
        view.expand(root);
        expand.push(ms(start.elapsed()));
        pages.push(page_p99(&view, SEED + r));
    }
    let e = report("expand_1m_children", &expand, 3000.0);
    let p = report("page_200_p99_1m_children", &pages, 5.0);
    assert!(e < 3000.0 && p < 5.0);
}

#[test]
#[ignore = "performance measurement (I/O bound; reported, no threshold)"]
fn real_walk_over_a_million_empty_files() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("walk");
    let start = Instant::now();
    for d in 0..1000 {
        let sub = root.join(format!("d{d:04}"));
        std::fs::create_dir_all(&sub).unwrap();
        for f in 0..1000 {
            std::fs::File::create(sub.join(format!("f{f:04}"))).unwrap();
        }
    }
    println!(
        "PERF real_walk_fixture_create_ms={:.0}",
        ms(start.elapsed())
    );
    let mut runs = Vec::new();
    for _ in 0..2 {
        let cancel = AtomicBool::new(false);
        let mut at: Vec<Instant> = Vec::new();
        let start = Instant::now();
        let tree = walk(&WalkOptions::new(vec![root.clone()]), &cancel, &mut |_| {
            at.push(Instant::now())
        })
        .unwrap();
        let t = ms(start.elapsed());
        let events = at.len() as u64;
        let closest = at
            .windows(2)
            .map(|w| ms(w[1].duration_since(w[0])))
            .fold(f64::INFINITY, f64::min);
        assert_eq!(tree.stats.files, 1_000_000);
        let per_s = events as f64 / (t / 1000.0).max(0.001);
        println!(
            "PERF real_walk_1m_files ms={t:.0} progress_events={events} per_second={per_s:.2} closest_gap_ms={closest:.1} threads={}",
            std::thread::available_parallelism().map(|n| n.get()).unwrap_or(0)
        );
        // Spec: at most 10 progress events per second, the last one included.
        assert!(
            per_s <= 10.0 && closest >= 100.0,
            "{events} progress events in {t:.0} ms ({per_s:.2} a second), closest two {closest:.1} ms apart"
        );
        runs.push(t);
    }
    report("real_walk_1m_files", &runs, f64::INFINITY);
}
