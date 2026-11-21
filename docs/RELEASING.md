# Releasing

How a version of the command-line program reaches the releases page. The workflow is
`.github/workflows/release.yml`; the text of the release notes is `.github/release-notes.md`.
The binaries are **not signed**; the notes say so and say how to check a download.

## Rehearse (no release is made)

Actions tab, "release", "Run workflow", on any branch. It builds the four targets, packages them,
writes `SHA256SUMS`, attests build provenance, and keeps everything as artifacts named
`release-*` and `release-files` for 14 days. It never creates a tag or a release. The attestation
records are real (public Sigstore log, tied to this repository) but name a `-rehearsal-` version.

The `crates-dry-run` job in the same run is the crates.io check (below). It does not block.

## Cut a release

1. Bump `version` in `[workspace.package]` in `Cargo.toml` (and `Cargo.lock`) on `main`; wait for CI.
2. `git tag vX.Y.Z <commit on main>` and `git push origin vX.Y.Z`. The tag must equal `v` plus the
   workspace version (a suffix such as `-rc.1` is allowed and makes a pre-release), point at a commit
   on `main`, and that commit must have a successful `ci.yml` run. Otherwise the `meta` job fails.
3. The workflow creates a **draft** release with the archives, `SHA256SUMS` and the notes. Nothing is
   public yet. Read it, download one archive, run `gh attestation verify`, then publish:
   `gh release edit vX.Y.Z --draft=false`.
4. Update the status wording in `README.md` and `site/index.html` only if it no longer reads true
   (the download text is written to be true with or without a release).

Rollback: delete the draft, or after publishing mark the release as a pre-release or delete it and
the tag. Attestations stay; they are history.

## Desktop bundles (extension point)

The desktop app has its own bundle jobs on its branch. Add them to `release.yml` next to the `build`
job; each uploads its installers as an artifact named `release-desktop-<target>`. The `package` job
collects every `release-*` artifact, so checksums and attestation need no other change. The matrix
comment marks the place. Installers need signing decisions (macOS notarization, Windows signing)
that are not made yet.

## crates.io (preparation only)

Checked 2026-11-20 against the crates.io API: `prunedisk`, `prunedisk-core`, `prunedisk-cli` and
`prunedisk-rules` are free. `disk-cleaner` is already taken by someone else; `disk-cleaner-core` and
`disk-cleaner-cli` are free. Publish under the new names (`prunedisk` for the binary package,
`prunedisk-core` for the library) after the rename, and re-check right before publishing, because a
name is first come, first served.

What the manifests need before a real publish:

- Every published crate: `description`, `license`, `repository` (present); add `readme`, `keywords`
  (at most five), `categories`, and `rust-version`. A `README.md` and the `LICENSE` file inside each
  crate directory so the package carries them.
- The CLI's dependency on the core crate has `path = "../core"` only. A published crate needs a
  version as well: `prunedisk-core = { version = "0.1.0", path = "../core" }`. Publish the core crate
  first, then the CLI.
- **Blocker found by this check:** `crates/core/src/rules.rs` embeds `../../../rules/artifacts.toml`,
  which is outside the crate directory. `cargo package` leaves that file out of the tarball, so the
  published core would not compile. The rules file must live inside `crates/core` (for example
  `crates/core/rules/artifacts.toml`, with docs and tests updated), or the rules be copied in at
  build time. This touches `crates/**` and `rules/**`, which other streams are editing, so it is left
  for the integration step.
- Use `include` or `exclude` so tests, fixtures and docs the crate does not need are not published,
  and check with `cargo package --list -p <name>`.

The `crates-dry-run` job runs `cargo package --list` per crate, reports whether each name is free and
runs `cargo publish --dry-run --workspace --locked`. It is expected to fail until the points above
are done. It uploads nothing and uses no registry token.

The real publish needs a crates.io credential. crates.io supports trusted publishing from GitHub
Actions (OpenID Connect; the `rust-lang/crates-io-auth-action` action exchanges the job's identity
token for a short-lived crates.io token, with `id-token: write`). It is configured per crate on
crates.io with the repository owner and name and the workflow file name, and a crate must already
exist before that can be set up, so the first publish of each crate needs an API token created by the
account owner (kept as a repository secret or used once from a terminal, never committed). Confidence
medium: read the current crates.io documentation when setting it up. After the repository rename,
update the trusted publisher entry to the new repository name.

## Repository rename: what hard-codes `ericmacdougall/disk-cleaner`

One `sed` of `ericmacdougall/disk-cleaner` to `ericmacdougall/prunedisk` covers these; the command and
package renames (`disk-cleaner` to `prunedisk`) are separate and listed after.

| Where | What |
|---|---|
| `Cargo.toml` | `repository` (workspace table; the crates inherit it) |
| `README.md` | releases link, `gh attestation verify --repo`, and other links |
| `site/index.html` | about eight links to the repository, its releases page, LICENSE, README and design doc; the `gh attestation verify --repo` example |
| `site/brand/BRAND.md`, `site/README.md` | the "not yet renamed" paragraphs (rewrite, not sed) |
| `site/tests/test_site.py` | `ALLOWED_LINK_PREFIX`, the repository link checks, the `/disk-cleaner` base path assertion |
| `.github/workflows/site.yml`, `site/tools/smoke.py` | the `/disk-cleaner/` Pages base path (project sites are served under the repository name) |
| `.github/workflows/release.yml` | nothing: the repository comes from `github.repository`; the binary and package names are the two `env` lines at the top |
| `.github/release-notes.md` | nothing: `@REPO@` and `@BIN@` are filled in by the workflow |
| `docs/DESIGN.md`, `docs/SCAN-FORMAT.md`, `rules/artifacts.toml`, `crates/**` | the program name in prose, help text and tests (command rename) |
| `.github/workflows/ci.yml` | package and binary names in the test and binary jobs (command rename) |

What GitHub does on a rename: web, `git` and API URLs of the old name redirect (clone URLs, issues,
releases pages, release asset download URLs), until someone creates a new repository with the old
name. What it does not do: the Pages site moves to `https://<owner>.github.io/<new name>/` with no
redirect from the old path (a project site's URL contains the repository name), so every link to the
old site breaks; a crates.io trusted publisher entry and any other name-matched setting must be
updated by hand; `uses: owner/old-name@...` references redirect today but should be updated; local
clones keep working through the redirect but should have their `origin` updated; attestations carry
the old repository name in already-signed records and `gh attestation verify --repo` should use the
name the file was built under for old releases.
