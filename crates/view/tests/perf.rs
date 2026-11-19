//! Performance measurements at one million nodes (spec AC-9). Ignored by default; CI runs them in
//! release mode with `cargo test --release -p disk-cleaner-view --test perf -- --ignored`.

#[test]
#[ignore = "performance measurement; run in release mode in CI"]
fn placeholder_until_the_tree_lands() {}
