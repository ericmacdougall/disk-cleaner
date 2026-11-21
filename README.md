# disk-cleaner

A disk analyzer for developer machines that understands what each folder is, so it can tell what is
safe to remove. It is planned to become a cleaner for Windows, macOS and Linux. The product name is
PruneDisk; the repository and the command are still called disk-cleaner.

> **Status: early and experimental.** There is no tagged release, installer or download page yet,
> so you build it from source. (Continuous integration also builds unsigned binaries for each
> platform and keeps them as workflow artifacts for 14 days; they are test builds, not releases.)
> `scan` and `plan` only read: the one file they write is the one you name with `--out`. `apply`
> is a dry run unless you add `--apply`; with `--apply` it backs up, pushes recovery refs and
> removes on macOS and Linux (not yet on Windows), and keeps a manifest. `restore` writes the
> files it brings back and never overwrites an existing one.

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
`recover-backup-then-remove` or `ask`, each with a reason in words. These item actions are a
proposal only; `apply` executes the plan's lanes (below).

`disk-cleaner apply --scan scan.json --plan plan.json --remote <rclone remote>` executes the lanes
of a plan made with `scan --lanes`. It is a dry run unless you add `--apply`. Before it touches a
piece it re-checks it against the scan, and keeps it if anything changed, if a process works in its
project, or if a file in it has a secret name (`.env`, keys, credentials). It also runs the planner
again on the scan and keeps any noise or git piece the scan does not support. Secret-named files stay
on this disk unless you pass `--secrets upload` yourself.

- **backup:** uploaded, verified, then removed (below).
- **noise:** removed only while its evidence holds: a dependency or build folder next to its marker
  file (restore lists it to regenerate), or a duplicate whose kept copy still has the same content
  (restore copies it back from there). Noise whose evidence no longer holds is backed up like the
  backup lane instead (a secret-like name keeps it). A file kept as the copy of a duplicate removed
  earlier is itself backed up before any later run removes it.
- **git:** a repository is removed only after its `.git` directory and every file that differs from
  the last commit are backed up; unchanged files come back from the backed-up history. Unsaved work
  (unpushed commits, plus a snapshot of changed and new files, never secret-named, ignored or very
  large ones) is first pushed to `refs/recovery/<host>/<date>/...` on its remote, without force, but
  only when apply has established that the remote is private: you list it in the config key
  `private_remotes` or with `--private-remote`, or, for github.com, `gh` reports it private. Anything
  else, a repository config that apply does not trust, or a secret in an unpushed commit, means no
  push. A push that fails on a private remote keeps the repository. A repository whose config holds a
  credential (a token in a remote URL, a `credential.*` or `http.*.extraheader` key) is kept unless
  you pass `--secrets upload`. Git runs without your repository's hooks or config and without your
  global config, except your credential helper.

Each distinct file content is uploaded once, in shards of
`--shard-mib` (default 64) through `--jobs` (default 4) rclone uploads at a time, streamed with no
local copy. A shard is verified in storage by size and hash before any file in it is removed, and a
shard that fails removes nothing. Content already in storage is reused only after its shard is
confirmed again by size and hash. A shard whose backend reports no hash is downloaded in full and
checked again, which costs up to one shard of download per reuse; if that fails, the content is
uploaded again. Every removal
is written first to a manifest (`~/.disk-cleaner/prunedisk/<host>.jsonl`), which is also copied,
with a checksum, to the remote. If a crash left a torn last line in the manifest, the next `apply
--apply` saves it beside the manifest as `<manifest>.torn-<time>` and cuts it before writing.

Each file is removed by first renaming it to a hidden name in its own folder
(`.pd-rm-<hash>`), checking that it is still the file that was backed up, then deleting it; anything
else is put back. A crash between those steps can leave such a name behind, and the next run
finishes it. One that is reported as "left at" a `.pd-rm-` path is a different file that took the
name and was not removed. On a file system that cannot rename without replacing another file,
nothing is removed.

Removing part of a folder changes that folder, so a later `apply` with the same scan file keeps
the rest of it with the reason "an earlier apply run changed this folder; scan again to continue".
Run `scan` again and make a new plan.

`apply` exits with:

| Status | Meaning |
|---|---|
| 0 | Done (or the dry run was printed). |
| 1 | Another error; read the message. |
| 2 | The scan or plan was refused before anything was done. |
| 3 | Another `apply` is running (its lock was still held after a 0.5 s wait). |
| 4 | A shard failed; the pieces that depend on it were kept. |
| 5 | Some pieces were kept because an earlier apply run changed their folder; scan again. |
| 6 | The run finished, but the manifest copy in storage could not be updated; run `apply` again to store it. |

When more than one applies, 6 is reported before 4, and 4 before 5.

### Getting files back

`disk-cleaner restore --path <removed file or folder> --remote <the same remote>` puts removed files
back byte for byte, with their mode, modification time, symlinks and hard links. It never overwrites:
a different file already at the path is reported as a conflict and left alone. If the local manifest
is lost, add `--from-remote` to rebuild the history from the copies in storage; a damaged copy refuses
the restore unless you add `--accept-partial`. A copy whose upload was interrupted also refuses it
until a later `apply --apply` run has stored its rows again (from the local manifest); restore then
sees that storage holds them, skips the interrupted copy and says so. Add `--run <id>` (printed by `apply`) to restore an
older generation of a path that was backed up more than once. Replacing or downgrading the
`disk-cleaner` binary never brings data back; `restore` does.

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

- detection and backup of coding-agent sessions and logs, and age and size policies for caches and
  outputs;
- a desktop app with outline, sunburst, treemap and biggest-files views;
- installers and releases;
- renaming the repository and the command to `prunedisk`.

## Safety model in one paragraph

Scanning and planning are read-only. Apply removes nothing unless the tool has shown that it is
backed up and verified, regenerable, or present elsewhere, and has re-checked that at the moment of
removal.
A check that fails or times out leaves its fact unknown, and an unknown fact never leads the plan to
propose removal: an idle repo with one is marked `ask`. Every removal is written to a manifest with a restore command.

## License

MIT. See [LICENSE](LICENSE).
