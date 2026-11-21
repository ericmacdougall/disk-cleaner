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
4. `cargo deny check licenses` (the former `licenses` job; needs `cargo install --locked cargo-deny`. The script fails if cargo-deny is missing, unless `LOCAL_CI_ALLOW_NO_DENY=1` is set, which skips it with a loud warning.)
5. `python3 -m unittest discover -s site/tests -v`

## Not covered (dropped from the old workflows)

This is **less** coverage than the former `ci` and `Site tests` workflows. These checks no longer run anywhere automatically:

- `ci` / `mount-test`: the Linux-only tmpfs mount test, **and** the `mount_alias` and `foreign_owner` tests in that job.
- `ci` / `test`: the Linux (`ubuntu-latest`) and Windows (`windows-latest`) legs of the test matrix. Only the host you run the script on is tested (normally macOS arm64).
- `ci` / `binaries`: the release-mode build of the `disk-cleaner` binary for Linux x86_64, macOS arm64 and Windows x86_64.
- `Site tests` / `pages-gate` and `pages-rehearsal`: the curl smoke test and `smoke.py` against the built site.

They remain in the workflow files and can be run by hand with `gh workflow run` if Actions minutes return.

## Manual GitHub runs (optional)

If Actions minutes are available again, the workflows can still be started by hand:

```
gh workflow run ci.yml
gh workflow run site.yml
gh workflow run pages.yml
```

## Feature-branch workflows

**This change alone does not stop the feature-branch burn.** Feature branches (`feat/**` and others) still carry their own copies of `ci.yml` (push on `main` and `feat/**`) and `site.yml` (push on any branch). Those copies keep their push triggers until each branch is rebased onto this change; merging to `main` does not change them. In the last 100 runs before this change, 60 were these feature-branch copies (48 `ci`, 12 `Site tests`). To stop them, either disable the `ci` and `Site tests` workflows in the repository's Actions settings (which also blocks the manual `gh workflow run` below), or rebase the feature branches.

`desktop.yml`, `desktop-close.yml`, `mutants.yml`, `e2e-probe.yml`, `e2e-variants.yml`, `macdiag.yml`, `release.yml` and `release-checks.yml` exist on feature branches, not on `main`. A change on `main` cannot stop those from firing on those branches. Disable those workflows in the repository's Actions settings (or rebase the branch onto this change and drop the push triggers) to keep them from using minutes. Those eight workflows were disabled in the repository's Actions settings on 2026-11-21 (re-enable with `gh workflow enable <id>`); `ci` and `Site tests` were left enabled.
