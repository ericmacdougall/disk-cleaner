//! Memory and load-time measurements for CI (spec AC-9, AC-11). Linux only for the memory
//! numbers (it reads /proc/self/status). Each mode runs in a fresh process so its peak is its
//! own.
//!
//!   perfmem index                         peak memory of the view index over 1,000,000 nodes
//!   perfmem gen-scan NODES FILE           write a synthetic scan (seed 16452)
//!   perfmem gen-adversarial KIND MIB FILE tiny | objects | strings | nested
//!   perfmem load FILE [--plan]            import FILE (no limits), report time, the estimate
//!                                         and the measured peak; exit 3 when the peak exceeds
//!                                         the estimate

// This example writes its own fixture files.
#![allow(clippy::disallowed_methods)]

use disk_cleaner_core::{Config, ProcessCheck};
use disk_cleaner_view::dev::{make_plan, PlanInputs};
use disk_cleaner_view::import::{estimate, parse_bytes, Limits};
use disk_cleaner_view::synth::{synthetic_scan, synthetic_tree, EPOCH, SEED};
use disk_cleaner_view::table::TreeView;
use std::io::Write;
use std::time::Instant;

fn status_kib(field: &str) -> Option<u64> {
    let text = std::fs::read_to_string("/proc/self/status").ok()?;
    let line = text.lines().find(|l| l.starts_with(field))?;
    line.split_whitespace().nth(1)?.parse().ok()
}

fn rss() -> u64 {
    status_kib("VmRSS:").unwrap_or(0) * 1024
}

fn hwm() -> u64 {
    status_kib("VmHWM:").unwrap_or(0) * 1024
}

fn mib(b: u64) -> f64 {
    b as f64 / (1024.0 * 1024.0)
}

fn usage() -> ! {
    eprintln!("usage: perfmem index | gen-scan NODES FILE | gen-adversarial KIND MIB FILE | load FILE [--plan]");
    std::process::exit(2)
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("index") => index(),
        Some("gen-scan") if args.len() == 3 => {
            let n: usize = args[1].parse().unwrap_or_else(|_| usage());
            let scan = synthetic_scan(n, SEED);
            let f = std::fs::File::create(&args[2]).expect("create");
            serde_json::to_writer(std::io::BufWriter::new(f), &scan).expect("write");
        }
        Some("gen-adversarial") if args.len() == 4 => {
            let mb: usize = args[2].parse().unwrap_or_else(|_| usage());
            adversarial(&args[1], mb << 20, &args[3]);
        }
        Some("load") if args.len() >= 2 => load(&args[1], args.iter().any(|a| a == "--plan")),
        _ => usage(),
    }
}

fn index() {
    let base = rss();
    let start = Instant::now();
    let tree = synthetic_tree(1_000_000, SEED);
    let mut view = TreeView::new(tree);
    let root = view.tree().roots()[0];
    view.expand(root);
    let t = start.elapsed().as_secs_f64() * 1000.0;
    let peak = hwm().saturating_sub(base);
    println!(
        "PERF index_1m build_ms={t:.0} peak_rss_mib={:.1} tree_heap_mib={:.1} limit_mib=600",
        mib(peak),
        mib(view.tree().heap_bytes() as u64)
    );
    if peak > 600 << 20 {
        eprintln!("index peak {:.1} MiB >= 600 MiB", mib(peak));
        std::process::exit(1);
    }
}

fn adversarial(kind: &str, bytes: usize, path: &str) {
    let f = std::fs::File::create(path).expect("create");
    let mut w = std::io::BufWriter::new(f);
    let mut written = 0usize;
    w.write_all(b"[").unwrap();
    let unit: &[u8] = match kind {
        "tiny" => b"0,",
        "objects" => b"{\"a\":0},",
        "strings" => b"\"xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx\",",
        "nested" => b"[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[0]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]],",
        _ => usage(),
    };
    while written + unit.len() + 2 < bytes {
        w.write_all(unit).unwrap();
        written += unit.len();
    }
    w.write_all(b"0]").unwrap();
    w.flush().unwrap();
}

fn load(path: &str, plan: bool) {
    let base = rss();
    let start = Instant::now();
    let bytes = std::fs::read(path).expect("read");
    let size = bytes.len() as u64;
    let est = estimate(&bytes);
    let limits = Limits {
        max_bytes: u64::MAX,
        budget: u64::MAX,
    };
    let parsed = parse_bytes(bytes, limits, path);
    let parse_ms = start.elapsed().as_secs_f64() * 1000.0;
    let parse_peak = hwm().saturating_sub(base);
    let ok = parsed.is_ok();
    println!(
        "PERF load file={path} file_mib={:.1} values={} objects={} parse_ok={ok} parse_ms={parse_ms:.0} \
         estimate_mib={:.1} measured_peak_mib={:.1}",
        mib(size),
        est.values,
        est.objects,
        mib(est.peak_bytes),
        mib(parse_peak)
    );
    if let (true, Ok(imported)) = (plan, &parsed) {
        let inputs = PlanInputs {
            config: Config::default(),
            window_source: "default".into(),
        };
        let p = make_plan(&imported.scan, &inputs, EPOCH, ProcessCheck::none_running());
        let total_ms = start.elapsed().as_secs_f64() * 1000.0;
        let total_peak = hwm().saturating_sub(base);
        println!(
            "PERF load_and_plan items={} total_ms={total_ms:.0} total_peak_mib={:.1} limits: 60000 ms, 4096 MiB",
            p.items.len(),
            mib(total_peak)
        );
        if total_ms > 60_000.0 || total_peak > 4 << 30 {
            eprintln!("load and plan over the limit");
            std::process::exit(1);
        }
    }
    if parse_peak > est.peak_bytes {
        eprintln!(
            "measured peak {:.1} MiB exceeds the estimate {:.1} MiB",
            mib(parse_peak),
            mib(est.peak_bytes)
        );
        std::process::exit(3);
    }
}
