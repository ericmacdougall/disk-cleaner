# Local CI (no GitHub Actions minutes)

The GitHub-hosted `ci`, `Site tests` and `Pages` workflows are `workflow_dispatch` only. They no longer run on push or pull request, because the personal account's included Actions minutes are exhausted.

## How to run the same checks

From the repository root, with a stable Rust toolchain and `python3` on `PATH`:

```
./scripts/local-ci.sh
```

That runs, in order:

1. `cargo fmt --all -- --check`
2. `cargo clippy --workspace --all-targets --locked -- -D warnings`
3. `cargo test --workspace --locked --no-fail-fast`
4. `python3 -m unittest discover -s site/tests -v`

On macOS this is the same coverage as the former macOS CI job plus the site tests. It does not run the Linux-only tmpfs mount test or the Windows matrix job.

## Manual GitHub runs (optional)

If Actions minutes are available again, the workflows can still be started by hand:

```
gh workflow run ci.yml
gh workflow run site.yml
gh workflow run pages.yml
```

## Feature-branch workflows

`desktop.yml`, `desktop-close.yml`, `mutants.yml`, `e2e-probe.yml`, `e2e-variants.yml`, `macdiag.yml`, `release.yml` and `release-checks.yml` exist on feature branches, not on `main`. A change on `main` cannot stop those from firing on those branches. Disable those workflows in the repository's Actions settings (or rebase the branch onto this change and drop the push triggers) to keep them from using minutes.
