# Local CI (no GitHub Actions minutes)

The GitHub-hosted `ci`, `Site tests` and `Pages` workflows are `workflow_dispatch` only. They no longer run on push or pull request, because the personal account's included Actions minutes are exhausted.

**Status (2026-11-21): 10 workflows are disabled in the repository's Actions settings** (`ci`, `Site tests`, `desktop`, `desktop-close`, `mutants`, `e2e-probe`, `e2e-variants`, `macdiag`, `release`, `release checks`), so none of them run on any branch, including the feature-branch copies. Checks now run locally with `./scripts/local-ci.sh`. Only `Pages` (the deploy, `workflow_dispatch` only on `main`) is still enabled.

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

They remain in the workflow files. If Actions minutes return, they can be run by hand, but **enable first**, because a disabled workflow cannot be dispatched:

1. `gh workflow enable ci.yml` (and/or `site.yml`)
2. `gh workflow run ci.yml` (and/or `site.yml`)
3. `gh workflow disable ci.yml` (and/or `site.yml`) as soon as the run finishes. While a workflow is enabled, every push triggers it too.

The [Manual GitHub runs](#manual-github-runs-optional) section below has the full commands.

## Manual GitHub runs (optional)

If Actions minutes are available again, re-enable a workflow first (a disabled workflow cannot be dispatched), then start it by hand:

```
gh workflow enable ci.yml        # or: gh workflow enable 423127909
gh workflow enable site.yml      # or: gh workflow enable 423305490
gh workflow run ci.yml
gh workflow run site.yml
gh workflow run pages.yml
```

After those runs finish, disable them again. While enabled, every feature-branch push triggers them too:

```
gh workflow disable 423127909    # ci
gh workflow disable 423305490    # Site tests
```

## Feature-branch workflows

Feature branches (`feat/**` and others) carry their own copies of `ci.yml` (push on `main` and `feat/**`) and `site.yml` (push on any branch). Merging this change to `main` does not change those copies; in the last 100 runs before it, 60 were such feature-branch runs (48 `ci`, 12 `Site tests`). `desktop.yml`, `desktop-close.yml`, `mutants.yml`, `e2e-probe.yml`, `e2e-variants.yml`, `macdiag.yml`, `release.yml` and `release-checks.yml` exist only on feature branches.

**Current state (2026-11-21):** all 10 of these workflows are disabled in the repository's Actions settings, so none of them start on any push. Re-enable one with `gh workflow enable <id>` only for a manual run, and only after the relevant feature branches are rebased onto this change (otherwise their push triggers come back too). Disable it again afterwards.
