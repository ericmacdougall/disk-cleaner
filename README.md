# disk-cleaner

A disk analyzer for developer machines that understands what each folder is, so it can tell what is
safe to remove. It is planned to become a cleaner for Windows, macOS and Linux. The product name is
PruneDisk; the repository and the command are still called disk-cleaner.

> **Status: early, read-only.** The `scan` and `plan` commands are on the main branch. There is no
> tagged release, installer or download page yet, so you build it from source. (Continuous
> integration also builds unsigned binaries for each platform and keeps them as workflow artifacts
> for 14 days; they are test builds, not releases.) No command removes, moves or changes anything:
> the only file it writes is the one you name with `scan --out`. The desktop app is read-only
> too; what it writes outside the folders it scans is listed below.

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

### Desktop app (read-only)

`apps/desktop` is a Tauri window over the same scan and plan: choose drives or folders (or open a
scan file), then look at the size tree, a treemap, the top files, the developer view (repos,
worktrees, rebuildable folders and caches with their plan action and reason) and Settings. It has
no apply step: nothing in it deletes, moves or backs up anything. A sunburst view, planned in
[docs/DESIGN.md](docs/DESIGN.md), is not built yet. Outside the folders it scans, it writes only
what Settings lists under "What this app writes" and
[docs/DESKTOP.md](docs/DESKTOP.md#files-the-desktop-app-writes) repeats. That includes its config
files on Save, the plan you export, the web view's own data folder, and on Linux the GTK file
dialog's entry in your recent files. How to build it, its checks and its limits are in
[docs/DESKTOP.md](docs/DESKTOP.md).

The scan and plan formats, every fact and every rule are described in
[docs/SCAN-FORMAT.md](docs/SCAN-FORMAT.md). The overall design is in [docs/DESIGN.md](docs/DESIGN.md).

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
- a sunburst view in the desktop app, and applying a plan from it;
- installers and releases;
- renaming the repository and the command to `prunedisk`.

## Safety model in one paragraph

Scanning and planning are read-only. The planned apply step will remove nothing unless the tool has
shown that it is regenerable or exists elsewhere, and has re-checked that at the moment of removal.
A check that fails or times out leaves its fact unknown, and an unknown fact never leads the plan to
propose removal: an idle repo with one is marked `ask`. Every removal will be written to a manifest with a restore command.

## License

MIT. See [LICENSE](LICENSE).
