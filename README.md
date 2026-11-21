# disk-cleaner

A disk analyzer for developer machines that understands what each folder is, so it can tell what is
safe to remove. It is planned to become a cleaner for Windows, macOS and Linux. The product name is
PruneDisk; the repository and the command are still called disk-cleaner.

> **Status: early, read-only.** The `scan` and `plan` commands are on the main branch. There is no
> tagged release, installer or download page yet, so you build it from source. (Continuous
> integration also builds unsigned binaries for each platform and keeps them as workflow artifacts
> for 14 days; they are test builds, not releases.) No command removes, moves or changes anything:
> the only file it writes is the one you name with `scan --out`.

## What it does today

`disk-cleaner scan` walks one or more directories and classifies them:

- git repos, linked worktrees and bare repos;
- `node_modules`, `target`, `.venv` and other directories that can be rebuilt (the rules are data, in
  [rules/artifacts.toml](rules/artifacts.toml));
- package manager and tool caches;
- source code outside any repo;
- output files (videos, archives, models and the like), listed one by one.

For every repo and worktree it records eleven safety facts, each yes, no or unknown: whether there
is a network remote, the checked-out commit and every branch are on it, the stash is empty, the tree
is clean, there are no ignored files that exist only here, nothing depends on it, the reflogs hold
no unpushed commit, no rebase or merge is half finished, the layout is plain, and the git directory
holds nothing of its own. A check that fails or times out is unknown, and unknown never counts as
yes. It prints a summary, or JSON with `--json`, and writes the JSON to a file with `--out`.

`disk-cleaner plan` reads that file and prints a dry-run plan, as a summary or as JSON with `--json`. A project touched within the active
window (`active_window_days`, default 7, set in the config file or with `--active-window-days`), a
path you mark with `--active`, or a folder a running process works in is kept. For the rest it
proposes one action per item: `keep`, `remove-regenerable`, `backup-then-remove`,
`recover-backup-then-remove` or `ask`, each with a reason in words. The plan is a proposal only;
nothing applies it yet.

The scan and plan formats, every fact and every rule are described in
[docs/SCAN-FORMAT.md](docs/SCAN-FORMAT.md). The overall design is in [docs/DESIGN.md](docs/DESIGN.md).

## Download a release

When a version is tagged, the [releases page](https://github.com/ericmacdougall/disk-cleaner/releases)
carries one archive for each of Linux (x86-64), macOS (Apple Silicon and Intel) and Windows (x86-64),
plus a `SHA256SUMS` file. If that page lists no release yet, there is nothing to download: build from
source (next section).

The programs are **not code-signed or notarized**, so macOS Gatekeeper and Windows SmartScreen can warn
when you first run one; the release notes say what to do. Check a download before you run it:

```
sha256sum -c --ignore-missing SHA256SUMS                      # Linux; see the release notes for macOS and Windows
gh attestation verify <archive file name> --repo ericmacdougall/disk-cleaner
```

The first shows the file matches the release; the second shows which repository and workflow built it.
How a release is cut is in [docs/RELEASING.md](docs/RELEASING.md).

## Build from source

You need a stable Rust toolchain (with `cargo`) and `git` on the `PATH`; the scanner runs `git` to
read repo facts.

```
cargo build --release --locked -p disk-cleaner-cli
./target/release/disk-cleaner scan ~/projects --out scan.json
./target/release/disk-cleaner plan --scan scan.json
```

`disk-cleaner --help`, `disk-cleaner scan --help` and `disk-cleaner plan --help` list every option.
Continuous integration builds and tests it on Linux, macOS and Windows.

## Planned

None of this exists yet:

- applying a plan: recovery of unsaved work, backup to your own storage (through
  [rclone](https://rclone.org)), removal with a manifest, and restore;
- detection and backup of coding-agent sessions and logs, and age and size policies for caches and
  outputs;
- a desktop app with outline, sunburst, treemap and biggest-files views;
- installers and releases;
- renaming the repository and the command to `prunedisk`.

## Safety model in one paragraph

Scanning and planning are read-only. The planned apply step will remove nothing unless the tool has
shown that it is regenerable or exists elsewhere, and has re-checked that at the moment of removal.
A check that fails or times out leaves its fact unknown, and an unknown fact never leads the plan to
propose removal: an idle repo with one is marked `ask`. Every removal will be written to a manifest with a restore command.

## License

MIT. See [LICENSE](LICENSE).
