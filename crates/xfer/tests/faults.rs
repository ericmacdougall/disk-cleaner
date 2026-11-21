//! AC-5 (b, c) and NEG-13: lost responses, throttling and a transport that dies mid-run.

mod common;
use common::*;
use std::collections::HashSet;
use std::sync::Arc;
use xfer::transport::fault::{FaultConfig, Faulty};

fn backup(env: &Env, t: Arc<Faulty>, rcp: &str) -> xfer::backup::Report {
    let src = env.p("src");
    if !src.exists() {
        sample_tree(&src);
    }
    xfer::backup::backup(t, &opts(&src, &env.p(rcp))).expect("backup returns a report")
}

#[test]
fn a_lost_response_is_not_uploaded_twice() {
    let env = Env::new();
    // puts 0..3 cover format.json and the first packs: their responses are lost after the
    // store kept the object
    let t = Arc::new(Faulty::new(
        env.local(),
        FaultConfig {
            lose_response: [0, 1, 2, 3].into_iter().collect(),
            ..Default::default()
        },
    ));
    let rep = backup(&env, t.clone(), "r.jsonl");
    assert_eq!(rep.exit_code, 0, "{rep:#?}");
    let keys = t.put_keys.lock().expect("keys").clone();
    let distinct: HashSet<_> = keys.iter().collect();
    assert_eq!(
        keys.len(),
        distinct.len(),
        "an object was uploaded twice after a lost response: {keys:?}"
    );
    // every attempt reached the store once: the retry found the object by stat instead
    assert_eq!(
        t.puts.load(std::sync::atomic::Ordering::SeqCst),
        keys.len() as u64
    );
    assert_eq!(
        receipts(&env.p("r.jsonl")).len(),
        manifest(&env.p("src")).len()
    );
}

#[test]
fn throttling_halves_the_streams_and_the_run_completes() {
    let env = Env::new();
    let src = env.p("src");
    for i in 0..48u64 {
        write(
            &src.join(format!("f{i:02}.bin")),
            &rand_bytes(300 + i, 1 << 20),
        );
    }
    let t = Arc::new(Faulty::new(
        env.local(),
        FaultConfig {
            throttle_first: 3,
            throttle_prefix: "v1/packs/".into(),
            put_delay: Some(std::time::Duration::from_millis(400)),
            slow_prefix: "v1/packs/".into(),
            ..Default::default()
        },
    ));
    let rep = backup(&env, t, "r.jsonl");
    assert_eq!(rep.exit_code, 0, "{rep:#?}");
    assert!(rep.throttles > 0, "{rep:#?}");
    let cap = rep.budget.as_ref().expect("budget").k;
    assert!(
        rep.k_trace.iter().any(|&(_, k, _)| k <= cap / 2),
        "K never dropped below {cap}: {:?}",
        rep.k_trace
    );
    let target = env.p("restored");
    let rr = xfer::restore::restore(env.local(), &restore_opts(&target, None)).expect("restore");
    assert_eq!(rr.exit_code, 0);
    assert_eq!(manifest(&env.p("src")), manifest(&target));
}

#[test]
fn a_dead_transport_fails_closed_and_a_rerun_completes() {
    let env = Env::new();
    let t = Arc::new(Faulty::new(
        env.local(),
        FaultConfig {
            die_after_puts: Some(2),
            ..Default::default()
        },
    ));
    let rep = backup(&env, t, "r1.jsonl");
    assert_ne!(rep.exit_code, 0, "{rep:#?}");
    let rs = receipts(&env.p("r1.jsonl"));
    // nothing whose objects were never verified is receipted; with the store dead before the
    // first commit could verify, that is nothing at all
    assert!(rs.is_empty(), "{} receipts from a dead transport", rs.len());

    let healthy = Arc::new(Faulty::new(env.local(), FaultConfig::default()));
    let rep2 = backup(&env, healthy, "r2.jsonl");
    assert_eq!(rep2.exit_code, 0, "{rep2:#?}");
    let target = env.p("restored");
    let rr = xfer::restore::restore(env.local(), &restore_opts(&target, None)).expect("restore");
    assert_eq!(rr.exit_code, 0);
    assert_eq!(manifest(&env.p("src")), manifest(&target));
}
