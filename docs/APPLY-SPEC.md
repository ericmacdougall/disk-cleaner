# Apply phase specification

Status: **draft for review**. Nothing in this document is implemented yet. It defines what
`disk-cleaner apply`, `disk-cleaner restore` and `disk-cleaner manifest` must do before any code
is written, and it is the contract the tests are written against. Section 14 lists the decisions
that are still open; text that depends on one of them says so.

Read with [DESIGN.md](DESIGN.md) and [SCAN-FORMAT.md](SCAN-FORMAT.md). Where this document and
SCAN-FORMAT.md disagree about the plan format, SCAN-FORMAT.md wins and this document is wrong.

## 1. Outcome

A user who has a plan from `disk-cleaner plan` can run `disk-cleaner apply` and get back disk
space without losing anything unique: every item is re-checked at the moment of acting, its unique
content is saved first (recovery refs on the git remote where possible, and always a verified
archive in the user's own storage), every action is written to a manifest that is itself backed up,
and `disk-cleaner restore` puts any removed item back exactly as it was archived.

## 2. Scope

In scope:

- `disk-cleaner apply`: executes the removing items of a plan (`remove-regenerable`,
  `backup-then-remove`, `recover-backup-then-remove`), dry run by default.
- Recovery refs for unsaved commits, stashes and pseudorefs, pushed without writing to the
  scanned repository and without running any of its programs.
- Archives of both preserve sets and of the other content an action must save, streamed to an
  rclone remote, read back and compared.
- A local vault for secret-like files, which are not uploaded unless the user opts in.
- A journal that makes every step resumable and idempotent; a per-user lock so two applies never
  run at once on one machine (and so never on one root).
- The manifest (local and mirrored to storage), `disk-cleaner manifest list|find|orphans`, and
  `disk-cleaner restore`.
- Ask mode (per item, the default) and automatic mode (per action, opt-in).
- Windows, macOS and Linux.

Out of scope for this phase:

- Acting on plan items whose action is `ask` or `keep`. Apply never does more than a fresh plan
  would (see AC-4). Answering an `ask` item with a removal is a later decision (section 14, D-3).
- Backing up and removing outputs, loose code and agent sessions: the plan has no removing
  action for them yet. The archive and manifest formats here are class-agnostic so that a later
  plan action can use them unchanged.
- Batching many small items into one archive (the format allows it, see `archive.members`; v1
  writes one archive per item).
- Linked worktrees, bare repositories and repositories with submodules: the plan never gives
  them a removing action in this schema (`plain_layout`), so `git worktree remove` is not needed.
- The recycle bin (`trash`), scheduled runs, the desktop app, cold-storage classes that cannot be
  read back immediately (section 13).
- Encryption. Users who want it configure an rclone `crypt` remote; the tool does not manage keys.

## 3. Threat model

**What is protected.** The user's unique data: commits that exist nowhere else, stashes,
uncommitted and untracked work, the git directory's own files (hooks, config, notes, reflogs),
ignored-but-unique files, and the history of what was removed. Also the user's credentials.

**Where harm comes from**, in order of likelihood:

1. **Change between check and use.** The user, an editor, a build or a coding agent writes into an
   item after the scan, after the re-check, during the upload, or between the last check and the
   removal. This is the main risk and most controls in this document address it.
2. **Storage that lies or fails.** A partial upload, a corrupted object, a lost response (the
   upload succeeded but the tool saw an error), a rate limit, an object deleted later.
3. **Interruption.** The process is killed, the machine sleeps or loses power, at any step.
4. **Scanned content is untrusted.** A scanned folder may be an unpacked download: its git config,
   hooks, `.gitattributes`, symlinks, junctions, hard links, special files and file names are
   attacker-controlled. Apply must not run any program it names, follow any link out of the item,
   cross into another file system, or write outside the item and the tool's own state.
5. **The remote changed.** A branch was force-pushed or deleted, a remote URL was changed, the
   remote is unreachable or refuses the push.
6. **Disclosure.** A secret file or a credential URL uploaded to cloud storage or pushed to a git
   remote that may be public; a credential printed to the terminal, a log or the manifest.
7. **Operator mistakes.** A stale plan, a plan from another scan, a plan edited by hand to remove
   more, running apply twice, two applies at once, a typed-ahead answer in ask mode.

**Not defended against.** Another program running as the same user that deliberately deletes or
corrupts data (it could do so without this tool); an administrator; a storage provider that reads
the archives (confidentiality needs an rclone `crypt` remote); a storage provider or user that
deletes or alters an archive after it was verified (restore detects alteration, it cannot undo it);
hardware that corrupts data in memory.

## 4. Terms

- **Item**: one plan item that apply acts on, identified by `item_id`, a hex SHA-256 of the scan
  identity, the node index and the path (stable across resumes).
- **Run**: one invocation of `apply` (or its resumption), identified by a random `run_id`.
- **State directory**: `<platform data dir>/disk-cleaner/` (`$XDG_DATA_HOME` or
  `~/.local/share` on Linux, `~/Library/Application Support` on macOS, `%LOCALAPPDATA%` on
  Windows), or `--state-dir`. It holds `machine.json`, `lock`, `journal/`, `manifest/`, `index/`,
  `vault/` and `scratch/`. Created with owner-only permissions.
- **Storage**: an rclone remote and path given by `--storage <remote>:<path>` (for example a
  Backblaze B2, S3, Dropbox or local-disk remote the user configured with `rclone config`). The tool
  never reads or writes rclone's credentials and never passes a credential on a command line.
- **Machine id**: a random 128-bit id created on the first run and kept in `machine.json` with a
  label (`--machine-label`, default the computer's name). Remote paths use the id, so two machines
  with the same name never collide.
- **Fingerprint**: for every entry below an item (the item directory included), its relative path
  (bytes), kind (directory, regular file, symlink), size, modification time in nanoseconds, change
  time (Unix, not for the item directory itself, which the quarantine rename changes), device and
  inode (Unix) or volume serial and file index (Windows), mode bits, link count and, for a symlink,
  its target bytes. Any other kind of entry (fifo, socket, device) makes the item unsupported.
- **Live remote check**: asking the git remote itself (`git ls-remote`), not the local
  remote-tracking refs, which can be stale.
- **Quarantine**: renaming an item to `.disk-cleaner-removing-<item_id>` in the same parent
  directory (same file system, so the rename is atomic) before deleting it.

## 5. Commands

```
disk-cleaner apply --plan PLAN --scan SCAN --storage REMOTE:PATH
                   [--backup-only | --remove]
                   [--auto ACTION[,ACTION...]] [--class CLASS[,CLASS...]] [--item PATH]...
                   [--backup-regenerable] [--upload-secrets] [--no-recovery-push]
                   [--push-worktree-snapshot]
                   [--require-recovery-push] [--check-remote]
                   [--max-plan-age HOURS] [--max-upload-bytes N] [--min-free BYTES]
                   [--part-size BYTES] [--state-dir DIR] [--machine-label TEXT]
                   [--rclone PATH] [--json]
disk-cleaner apply --resume RUN_ID [same flags as the run]
disk-cleaner restore RECORD_ID [--to DIR] [--storage REMOTE:PATH] [--machine ID] [--json]
disk-cleaner manifest list [--storage REMOTE:PATH] [--machine ID] [--json]
disk-cleaner manifest find TEXT [--files] [--storage REMOTE:PATH] [--machine ID] [--json]
disk-cleaner manifest orphans --storage REMOTE:PATH [--json]
```

- No mode flag: **dry run**. `--backup-only`: everything up to and including the manifest, then
  stop; nothing is removed. `--remove`: the full sequence. The two are mutually exclusive.
- `--auto` names the classes whose items run without a prompt: `regenerable`, `cache` (their
  `remove-regenerable` items), `repo` (`backup-then-remove`) and `repo-recover`
  (`recover-backup-then-remove`, a separate opt-in because it pushes refs). The config file key
  `[apply] auto = [...]` does the same. Default: none, so every item is asked.
- `--push-worktree-snapshot` (D-5): also push a snapshot commit of uncommitted and untracked work
  (section 8.2). Off by default.
- `--class` and `--item` narrow the selection; they never widen it.
- Configuration lives in the same `config.toml` as the plan settings, under `[apply]`. Unknown
  keys are an error, as for the plan.

## 6. The sequence for one item

Items are processed one at a time, innermost first (a `remove-regenerable` item inside a repository
item is handled before the repository). Every transition is written to the journal (section 10)
before the next step starts. Any failed check ends the item as **skipped** with a reason and the run
continues with the next item; nothing that failed a check is ever removed.

**Nested items.** Removing an item removes everything inside it, so:

- a selected item inside another selected item is handled first; if it ends as anything but
  removed (skipped, failed, kept, answered `n`, not selected because of `--class` or `--item`),
  the enclosing item is skipped ("the nested <path> was kept"), whatever its own checks say;
- the enclosing item's re-check (7.2) compares the scan's counts **net of** the nested items this
  run removed: each such nested path must be absent and the journal must show it `deleted`, and
  the scan's counts of those nodes are subtracted before comparing.

**One fingerprint chain.** Every comparison of the item is against one fingerprint, F0, taken at
the start of step 2 before any fact is computed:

- at the end of step 2 the fingerprint is taken again and must equal F0, so the facts were computed
  on exactly F0;
- while archiving (step 6) every entry's metadata is compared with F0 as it is read, and the index
  written is F0 (plus content digests); after the last part the fingerprint is taken again and
  must equal F0;
- at step 11 the quarantined directory must equal F0.

An entry marked `reproducible` in the index (not archived, because the facts say it can be
rebuilt) is therefore covered by the same chain: any change to it after the facts were computed
fails one of these comparisons.

1. **Select.** The item's plan action removes something and the item is within `--class` and
   `--item`.
2. **Re-check** (section 7): re-scan, compare, process check, live remote check.
3. **Ask** (ask mode only, section 11): show the re-check result and what will happen; the answer
   binds to the fingerprint just taken.
4. **Recover** (`recover-backup-then-remove` only, section 8): push recovery refs; verify them on
   the live remote.
5. **Vault** (section 9.5): copy secret-like files to the local vault and verify them.
6. **Archive and upload** (section 9): stream, split into parts, upload, read back, compare; then
   the index object the same way.
7. **Manifest** (section 12): write the item's `backed-up` record to a new segment, upload it,
   read it back, compare.
8. Stop here for `--backup-only` (outcome `backed-up-kept`).
9. **Second existence check**: list each part, the index object and the manifest segment on the
   storage and compare their sizes with what was verified; confirm the item path is still a
   directory of the same identity (device and inode or file index).
10. **Process check again**, immediately before the rename.
11. **Quarantine**: rename the item; re-take the fingerprint of the renamed directory and compare it
    with F0 (which is the archive index). Any difference: rename it back and end as
    `backed-up-kept` (the archive stays, the manifest says why).
12. **Delete** the quarantined directory (section 13.2).
13. **Record** the `removed` record in the next manifest segment, synced at the next item's step
    7 or at the end of the run, whichever comes first.

`remove-regenerable` without `--backup-regenerable` (and without the answer `b` in ask mode) skips
steps 4 to 7 and 9; step 11 compares with F0 all the same. Its manifest record says it was
regenerable and names the rule.

## 7. Re-check (time of check, time of use)

### 7.1 Inputs are bound together

- The plan and scan must have the same `schema_version` as this build supports.
- The plan must have been made from this scan: today by `plan.scan_generated_at ==
  scan.generated_at` and, for every item, `scan.nodes[item.node].path == item.path` and the same
  class; once core records it, by the SHA-256 of the scan file (section 14, D-2).
- A plan older than `--max-plan-age` (default 24 hours) is refused. Facts that cannot be re-derived
  for one item (dependents outside the item, the scan's coverage, see 7.2) are only as fresh as the
  scan, so the age is bounded.
- A resumed run must use the same plan and scan (their hashes are in the journal).

Any of these failing is exit code 1 before anything is done.

### 7.2 The item is scanned again

Immediately before acting, apply scans the item's own path again with the core scanner (same
options as the scan file, git facts on, the hardened runner) and plans the result again with the
plan's options. The item proceeds only if **all** of these hold, and the skip reason names the
first that does not:

- the path exists, is a directory (not a symlink, junction or file) and is on the same device
  as in the scan;
- the item neither contains nor lies inside the tool's own state directory (the vault may hold
  the only copy of a secret) or the rclone storage path when that is a local directory;
- the fresh scan finds the same set of nodes inside the item as the scan file (same paths and
  classes), and every one of them gets the same action from the fresh plan as in the plan file
  (nested items already removed in this run excepted, see section 6);
- class, rule and every count of the node equal the scan's (`files_total`, `bytes_total`,
  `newest_mtime`, `output_bytes`, `unreadable`, `skipped_mounts`, `contains_outputs`,
  `outputs_unlisted`), and it has no `ask_reason`;
- the fresh plan gives the same action as the plan file. A hand-edited plan can only remove
  items or turn them into `keep`; it cannot make apply do more (AC-4);
- for a git node: every fact is the same value as in the plan item's `facts`, `head_commit` and
  `last_commit_time` are equal, the remote names and URLs are equal, `git_admin` and
  `object_store` counts are equal, `unsaved_pseudorefs`, `hidden_index_entries` and
  `local_admin_content` are equal, and the list of git directories inside the item equals
  `preserve.git_admin.git_dirs`;
- for a regenerable or cache node: the project that owns it (its parent node in the scan, or the
  node itself when it has none) has no file or commit newer than in the scan;
- the newest activity is still outside the active window.

Dependents found elsewhere in the scan (a clone outside the item that borrows its objects) cannot
be seen by a scan of the item alone; the scan file's `no_dependents` is used as it is, and a fresh
`no` from the item's own scan wins. This is a documented limit (section 15).

### 7.3 Running processes

The process check (`running_cwds`) runs at step 2 and again at step 10. A process whose working
directory is inside the item, or inside the project that owns a regenerable item, skips it. A
check that could read no process skips every item; a partial check skips every item unless the
plan recorded `trusted_partial_process_check`.

### 7.4 Live remote check (git items)

Remote facts in the plan are unverified. Before acting on a git item apply confirms them against
the live remote. Network git commands never run in the scanned repository (section 8.1):

- The **effective URL** of each network remote is computed by the tool: the URL written in the
  repository's config, rewritten by the repository's own `url.<base>.insteadOf` and by the user's
  global `url.<base>.insteadOf` (D-1), longest prefix first as git does. It must still be a network
  URL (`is_network_url`) with an allowed transport: `https://` or ssh (`ssh://` or scp-style). Any
  other transport (`file://`, a local path, `git://`, `ext::`, a remote helper `<x>::`) does not
  count as a remote for this check.
- `git ls-remote <effective URL>` lists the remote's refs. For every commit the plan's facts call
  saved (HEAD, every local branch, every other local ref, every reflog and pseudoref commit), some
  advertised ref must contain it: if the advertised tip is in the local object store, ancestry is
  checked locally; if not, that tip is fetched into the tool's scratch repository (never the
  user's) and checked there.
- Every local tag must be advertised with the same object id (an annotated tag compared by its tag
  object, not the commit it peels to).
- A remote that cannot be reached, refuses authentication or times out makes the check unknown,
  and the item is skipped. It never counts as saved.

`--check-remote` lets a dry run do this check too (it is read-only on the remote; a fetch into the
scratch repository is not done in a dry run, so an advertised tip missing locally reads as
"would need a fetch").

## 8. Recovery (`recover-backup-then-remove`)

### 8.1 Nothing is written to the scanned repository

All recovery work happens in a **scratch repository** that apply creates for the item under
`<state>/scratch/<item_id>/`: a bare repository whose `objects/info/alternates` names the item's
object directory, so it can read the user's commits without copying them and without writing a
ref, object, index, reflog or config entry into the user's repository. Every git command runs
through core's hardened runner (no hooks, no system or global config, no inherited `GIT_*`
variables, no programs named by scanned content); network commands add only
`protocol.https.allow=always`, `protocol.ssh.allow=always` and the credential helper decided in D-1,
and pass an explicit URL and explicit refspecs, never a remote name (so a scanned `pushurl`,
`remote.*.push` or `receive.*` setting cannot redirect them). The scratch repository is deleted
when the item ends. It holds nothing unique (refs the tool made and objects fetched from the
remote), so any scratch repository left by a crash is deleted when the next apply starts, before
anything else: its alternates entry would otherwise make a later scan report a dependent on the
user's repository.

The user's git directory is fingerprinted before and after recovery; any difference fails the
item (and is a bug).

### 8.2 What is pushed

To the effective push URL of the first network remote (the repository's `pushurl` if written,
otherwise its URL, rewritten as in 7.4), under `refs/heads/recovery/<machine label>/<yyyymmdd>/`:

- each local branch whose tip the live remote does not contain: `.../<branch>`;
- each local tag the remote does not advertise with the same id: `refs/tags/recovery/<machine
  label>/<yyyymmdd>/<tag>` (the tag object itself);
- each stash entry: `.../stash-<n>` (the stash commit, which carries its index and untracked
  parents);
- each pseudoref in `unsaved_pseudorefs`: `.../pseudo-<NAME>`;
- the newest commit of each unsaved reflog line of descent (`rev-list --reflog --not` the live
  remote's tips, reduced to commits that are not ancestors of another): `.../reflog-<n>`;
- uncommitted and untracked work is **not** pushed by default; it goes into the private archive
  (section 9.1). This departs from the requirement as first written, which listed dirty and
  untracked work among the things to push; see D-5. With `--push-worktree-snapshot` apply builds a
  snapshot commit in the scratch repository: a temporary index filled from HEAD plus the working
  tree's tracked changes and untracked files (not ignored files, not secret-like files, not files
  over 50 MiB, each left-out file named in the record), committed with HEAD as parent, and pushed as
  `.../worktree`. Nothing is written to the user's index, working tree or object store.

Names are sanitised to `[A-Za-z0-9._/-]`. Pushes never force: when a recovery ref of that name
already exists on the remote with another id, the name gets a numeric suffix. Pushes never touch a
ref outside the `recovery/` namespaces.

After pushing, `ls-remote` must advertise every pushed ref with exactly the pushed id. Only then is
the recovery recorded as `pushed`.

### 8.3 When the push cannot be done

No network remote, a refused push, an authentication failure or a mismatch after pushing is recorded
in the manifest as `recovery_push: failed` with a redacted reason. It does **not** stop the item by
default: the archive of a `recover-backup-then-remove` item holds the whole item (working tree and
git directory) and is verified before removal, so nothing depends on the push. With
`--require-recovery-push` (or `[apply] require_recovery_push = true`) a failed push skips the item.
`--no-recovery-push` skips the push and says so in the record.

### 8.4 The post-recovery check

DESIGN.md section 4 step 5 says to re-run the facts after recovery. Because recovery writes nothing
to the repository, `tree_clean` and `stash_empty` still read `no` afterwards. The check that
replaces it: every ref pushed in 8.2 is advertised by the live remote with the pushed id (or the
push failed and 8.3 applies), and the fingerprint taken at step 11 equals the archive index, which
proves the archived working tree is the one being removed.

## 9. Backup

### 9.1 What is archived

| Action | Archive contents |
|---|---|
| `backup-then-remove` | Every git directory in `preserve.git_admin.git_dirs`, each enumerated again at apply time with `git_admin_files` and `object_store_files` (both preserve sets); the enumeration must succeed and must equal the counts of the plan, or the item is skipped. The rest of the item is not archived: the facts say it is reproducible from HEAD (clean tree) and regenerable directories. |
| `recover-backup-then-remove` | The whole item: working tree, ignored files and every git directory, except nested items whose own action is `remove-regenerable` and which passed their own re-check in this run. |
| `remove-regenerable` with `--backup-regenerable` or answer `b` | The whole item. |

Secret-like files are taken out first (9.5). Every entry of the item, archived or not, is in the
**index** with its fingerprint fields and, for a regular file that is archived, its SHA-256; an
entry that is not archived is marked `reproducible` (clean tracked file or regenerable directory) so
that step 11 can compare the whole item.

### 9.2 Archive format

- A POSIX pax tar stream (`ustar` with pax extended headers for long names, large sizes,
  nanosecond times and names that are not UTF-8 on Unix). Uncompressed in v1: git packs are
  already compressed.
- Deterministic: entries in byte-wise order of relative path, directories before their contents,
  owner and group recorded as 0 with empty names, mode bits and modification times as on disk.
  The same content therefore gives the same bytes, which makes a retry comparable.
- Directories, regular files and symlinks (the target is stored as written; it is never followed).
  Two names of one file inside the item are stored as a tar hard-link entry. A hard link to a file
  outside the item is stored as a regular file. Fifos, sockets and devices make the item
  unsupported (skipped).
- The stream is split into **parts** of at most `--part-size` bytes (default 8 GiB), cut at
  arbitrary byte offsets; restore concatenates them. Parts keep each object below the streaming
  limits of common backends (for example, rclone's S3 backend streams an object of unknown size in
  at most 10,000 chunks of 5 MiB by default, about 48 GiB).
- The index is a separate JSON Lines object, `<archive>.index.jsonl`, with a header line (format
  version, item, part list) and one line per entry; a copy stays in `<state>/index/`.

### 9.3 Upload and verification

- Each part is produced by one pass over the files: a reader walks the item (lstat only, opening
  each regular file with no-follow semantics), the tar writer writes into a SHA-256 hasher and a
  byte counter and into the stdin of `rclone rcat <storage>/<object>`. No copy of the archive is
  staged on disk.
- A file whose size or modification time changes while it is read, or that cannot be read, fails
  the item (fail closed).
- The part is then read back with `rclone cat` into a second hasher. It is **verified** only if
  rcat exited 0, the bytes read back equal the bytes sent, and both SHA-256 digests are equal.
  A listing (`rclone lsjson`) of the object must also report that size.
- Object names are unique per attempt:
  `disk-cleaner/v1/<machine id>/archives/<run_id>/<item_id>.a<attempt>.p<part>.tar`. An object is
  never overwritten.
- A failed attempt (nonzero exit, mismatch, no progress for `stall_timeout`, default 10 minutes) is
  never retried in place; the whole item restarts with attempt + 1, at most 3 attempts per item.
  Objects of failed attempts are orphans: they are listed by `manifest orphans` and never deleted
  automatically.
- A **lost response** (rcat reports failure or is killed although the object arrived complete) is
  handled the same way: the attempt counts as failed; the complete object is an orphan.
- After the last part, the index object is uploaded and verified the same way.

### 9.4 Rate limits and budgets

rclone's own retries handle rate limiting within one call (`--retries 3 --low-level-retries 10`,
and the user's own rclone flags). Three items in a row that fail at the storage stop the backup
stage for the rest of the run (remaining items untouched, reported). `--max-upload-bytes` stops
before an item whose archive would exceed the budget (the estimate is the scan's byte count).

### 9.5 Secrets and the local vault

Secret-like files are:

- files whose names match the scan's `[[state_file]]` list that is not `fixture_prone` (`.env`,
  `.env.*`, Terraform state and variables, private SSH key names) plus `*.pem`, `*.key`, `*.p12`,
  `*.pfx`, `*.jks`, `*.keystore`, `.netrc`, `_netrc`, `.npmrc`, `.pypirc`, `credentials.json`,
  wherever they are in the item;
- files in a git directory (`config`, `FETCH_HEAD`, files under `logs/`, any other own file) whose
  bytes contain a URL with userinfo (`scheme://user:secret@` or a token as user name), found with
  the same rule core's redaction uses;
- untracked files that only a global ignore rule would have ignored cannot be recognised
  (the hardened runner does not read global config), so they are treated like other untracked files;
  this is a documented limit.

By default each secret-like file is copied to `<state>/vault/<item_id>/<relative path>` (directories
0700, files 0600 on Unix; on Windows the state directory's per-user location), its SHA-256 compared
with the original, and it is left out of the archive. The manifest record lists its relative path
and size only: no content and no hash leaves the machine (a hash of a short secret can be guessed).
The vault keeps its own `vault/<item_id>/index.json` with the hashes.

A vault copy satisfies the preserve obligation for a git directory file (D-4): the file is backed
up and verified, on this machine. The archive then holds every other file of both preserve sets.

`--upload-secrets` (or `[apply] upload_secrets = true`) puts secret-like files in the archive
instead (still never in a git push). Limits: 64 MiB per item and 1 GiB for the whole vault
(`[apply] vault_item_bytes`, `vault_total_bytes`); an item over a limit is skipped.

Secrets inside committed history are not filtered: the object store is archived as it is (and its
commits are on the user's own remote already, or in the recovery refs).

## 10. Journal, interruption, resume

- `<state>/journal/<run_id>.jsonl` is append-only. Each line is one transition of one item
  (`selected`, `rechecked`, `answered`, `recovered`, `vaulted`, `part-uploaded`, `part-verified`,
  `index-verified`, `recorded`, `segment-synced`, `checked-again`, `quarantined`,
  `delete-started`, `deleted`, `skipped`, `failed`, `kept`) with the attempt, object names, sizes
  and digests. Each line is flushed and `fsync`ed before the step it records takes effect;
  renames are followed by an `fsync` of the parent directory where the platform supports it.
- The journal header records the plan and scan SHA-256, the storage, the flags and the build.
- `apply --resume RUN_ID` (or `apply` with the same plan, which finds the unfinished run and asks
  whether to resume it) continues each unfinished item from its last durable state:

| Last state | On resume |
|---|---|
| before `segment-synced` | Start the item again from the re-check with the next attempt number. Objects of the earlier attempt are orphans. |
| `segment-synced`, not `quarantined` | Re-check (7.2), then the second existence check and the fingerprint comparison, then continue. |
| `quarantined` (the line is written before the rename), the original path exists and no quarantine directory does | The rename never happened: continue as for `segment-synced`. |
| `quarantined`, not `deleted` | If the quarantine directory exists and the original path does not: fingerprint it; equal to the index, or a subset of it when deletion had begun (`delete-started`), continue deleting; otherwise rename it back and record `kept`. If both exist, touch neither and report. |
| `deleted` | Write the `removed` record if it is not in a synced segment. |

- Re-running a finished plan does nothing to finished items and reports them as already done; no
  manifest record is written twice (records carry `item_id` and `run_id`, and the journal is checked
  first).
- Ctrl-C (SIGINT, Windows console control) finishes the current journal write, stops before the
  next step, and exits 3 with the run id. A second Ctrl-C exits at once; the journal stays valid.

### 10.1 Concurrency

Two applies must never act on one root at the same time. Comparing roots between runs is
unreliable (one root can lie inside another, be spelled through a symlink, or differ only by case),
so the rule is stronger: **one apply per user per machine**.

- An OS advisory lock on `<platform data dir>/disk-cleaner/apply.lock` (always this path, also
  when `--state-dir` points elsewhere), and a second one on `<state>/lock`. Both are taken before
  anything else (a dry run takes neither and writes nothing) and released by the operating system
  when the process ends, so a crash never leaves a stale lock. The file holds the run id and
  process id for the message; its content is never trusted as the lock itself.
- A second apply exits 1 at once naming the run that holds the lock. `restore` and `manifest`
  only read and take no lock, except that `restore` refuses a target inside an item that the
  journal of a running apply has not finished.
- Different users on one machine are not covered by the per-user lock; the quarantine rename
  (atomic, so only one process can move an item) and the fingerprint comparison make the loser
  skip the item.

## 11. Ask mode and automatic mode

- **Ask** is the default for every action. On a terminal, each selected item is shown after its
  re-check: path, action, size, why the plan says it is safe, what will be pushed, archived and
  vaulted (counts and bytes), and anything the re-check found. Answers: `y` do it, `n` keep, `b`
  back up then remove (regenerable only), `s` back up and keep, `a` yes to the rest of this action,
  `q` stop (no further item is started). Pending typed-ahead input is discarded before each prompt
  so a `y` typed early cannot answer the next item. End of input means `n`.
- Without a terminal on stdin, ask mode answers `n` for every item and says so; it never waits.
- The answer binds to the fingerprint taken at the re-check. If anything differs at step 11 the item
  is not removed, whatever the answer.
- **Automatic** applies to the actions named by `--auto` or `[apply] auto`; there is no default.
  Automatic items are still re-checked, and skipped on any difference.
- A dry run never prompts.

## 12. Manifest

### 12.1 Records

One JSON object per line. Every record has `record_id` (random), `item_id`, `run_id`,
`machine_id`, `at`, `path`, `class`, `action`, `outcome` and `tool_version`. Outcomes:
`backed-up` (written before removal), `removed`, `backed-up-kept` (backed up, then not removed: the
reason says why), `regenerable-removed`. Other fields where they apply:

- `bytes`, `files`; the facts used and when the live check confirmed them;
- `git`: HEAD, branches and tags with their ids, the redacted remote URLs checked, the live check
  result;
- `recovery`: each pushed ref with its id and the redacted URL, or `failed` with a redacted reason;
- `archive`: `format` (`pax-tar-v1`), `parts` (object name, bytes, SHA-256 each), `bytes`, `sha256`
  of the whole stream, `index` (object name, bytes, SHA-256), `members` (the item paths it holds:
  one in v1);
- `vault`: relative paths and sizes (no hashes);
- `restore`: the method (`archive`, `archive-then-checkout`, `regenerable`) and the exact
  `disk-cleaner restore <record_id>` command; for git items, also the git-native alternative in words
  (clone the recorded URL, fetch the recovery refs).

Nothing in a record is a credential: URLs pass core's `strip_url_credentials`, error text passes
`redact_text`.

### 12.2 Where it lives and how it is backed up

- Locally: `<state>/manifest/<run_id>.<seq>.jsonl`, one **segment** per sync. Segments are never
  modified after they are written. Each ends with a trailer line holding the record count and the
  SHA-256 of the bytes before it, so truncation and corruption are detectable on their own.
- In storage: the same segment at `disk-cleaner/v1/<machine id>/manifest/<run_id>.<seq>.jsonl`,
  uploaded with rcat, read back and compared like an archive part. A name is never reused, so a
  lost state directory or another machine can never overwrite it. `machine.json` is uploaded once
  to `disk-cleaner/v1/<machine id>/machine.json`; if one exists there with another id, apply stops.
- A segment is synced before any removal that its records describe (step 7): **no item is removed
  until a verified copy of its `backed-up` record exists in storage**. `removed` records are synced
  in the next segment.

### 12.3 Finding

- `manifest list` prints every record (newest first) from the local segments; with `--storage`
  (and `--machine` when the storage holds several machines) it reads only the storage, so it works
  on a new machine. A segment whose trailer does not match is reported as damaged and its records
  are shown as unverified.
- `manifest find TEXT` matches paths, record ids, item ids and archive names; `--files` also searches
  the archive indexes (local copies, or downloaded from storage when absent) for file paths.
- `manifest orphans` lists objects under the machine's storage path that no record references
  (failed attempts, lost responses). It deletes nothing.

## 13. Removal and restore

### 13.1 Before removal

Removal (steps 9 to 12) happens only after, in order: the re-check passed; recovery is recorded
(pushed, failed and allowed, or not requested); the vault copies are verified; every part and the
index object are verified; the manifest segment with the `backed-up` record is verified in storage;
and the second existence check passed.

### 13.2 Deleting

- The quarantine rename is the point after which the original path no longer exists. On Windows a
  rename that fails because a file is open is a skip (`in use`).
- Deletion walks the quarantined directory itself: it never follows a symlink or junction (it
  removes the link), never descends into a directory on another device (Unix `st_dev`; Windows: any
  reparse point is removed as a link and never entered), and clears the read-only attribute only on
  entries inside the quarantine (git marks object files read-only on Windows). If it meets another
  device it stops, leaves the rest in quarantine and reports the item as `failed` (the data is in the
  verified archive).
- Removal never runs `git` in the item, and never touches anything outside the quarantine.

### 13.3 Restore

`disk-cleaner restore RECORD_ID [--to DIR]`:

1. Finds the record (local manifest, or `--storage` alone on a new machine).
2. The target is the original path or `--to`. It must not exist (no merge, no overwrite).
3. Each part is streamed with `rclone cat` into a hasher and a tar reader that extracts into
   `<target parent>/.disk-cleaner-restoring-<record_id>`. Extraction refuses absolute names, `..`
   components, names that are not valid for the target file system, writes through a symlink
   (symlinks are created after all files), hard links to anything outside the staging directory,
   and special files. The digests of each part and of the whole stream must equal the record's;
   on any mismatch the staging directory is deleted and nothing is created at the target.
4. Every entry is compared with the index (kind, size, SHA-256 of content, symlink target, mode
   bits on Unix).
5. Vault files are copied back from the local vault and compared with its hashes. On a machine
   without that vault, restore completes and lists the missing secret-like files by name (exit 2).
6. `archive-then-checkout` (a `backup-then-remove` repository): the repository config is checked
   against core's allowlist, then the working tree is written with the hardened runner
   (`git read-tree` and `git checkout-index -a`, no hooks), and restore verifies that `git status`
   is clean, HEAD equals the record, and every recorded branch and tag has its recorded id.
7. The staging directory is renamed to the target. A failure at any step leaves nothing at the
   target and reports the staging path if it was kept for inspection.

On a case-insensitive file system, an archive with two names that differ only by case is refused
before extraction, naming them. Restore never contacts a git remote.

`regenerable` records have nothing to restore: restore prints the rule and how the directory is
normally rebuilt, and exits 0.

## 14. Open decisions

Each needs main's decision (and the user's where it changes what leaves the machine). The text
above follows the recommended option.

- **D-1. Network access for git.** Core's hardened runner sets `protocol.allow=never` and reads no
  global config, so it cannot run `ls-remote` or `push` as is, and https remotes would have no
  credential helper. Recommended: an additive core API (`Git::network_command`, or apply passing
  extra `-c` values to `Git::command`) that allows only https and ssh, runs only in the tool's
  scratch repository with an explicit URL, and takes the credential helper from the user's own
  global `credential.helper` (read once with `git config --global --get-all credential.helper` in
  an empty tool-owned directory) and the user's global `url.*.insteadOf`. ssh uses the user's ssh
  agent and `~/.ssh` as usual. Without a helper, an https remote that needs authentication fails the
  live check (skip). Alternative: a tool-config key naming the helper, default none (safer, more
  setup). **Unverified:** that the common helpers (`osxkeychain`, Git Credential Manager,
  `libsecret`) work while the hardened runner points `HOME` and `XDG_CONFIG_HOME` at the null
  device. The check that settles it: a CI probe per OS that runs `git credential fill` through the
  network runner against a helper holding a dummy value; if a helper needs `HOME`, the network
  runner restores only `HOME` (never the global config) for that command.
- **D-2. Binding plan and scan.** The plan has no hash of its scan. Recommended: an additive core
  plan field `scan_sha256`; until then, apply binds by `generated_at` plus per-item path and class
  (7.1).
- **D-3. Non-git items and `ask` items.** No plan action backs up a non-git item. Recommended for
  this phase: apply acts only on removing actions; `remove-regenerable` items may be backed up first
  on request; `ask` items are never acted on. Outputs, loose code and sessions wait for a core plan
  action that reuses this archive and manifest format. Alternative: let ask mode answer certain ask
  reasons with a backup-then-remove (never dependents, hidden index entries, pseudorefs that
  could not be checked, incomplete coverage or unusual layouts).
- **D-4. Secrets versus the preserve obligation.** Recommended: a verified local vault copy
  satisfies the obligation for a git directory file holding a credential URL or with a secret-like
  name; the vault is on the same disk, which the manifest and the summary say.
- **D-5. Pushing uncommitted work.** Recommended: never push a snapshot of uncommitted or untracked
  files by default (the remote may be public, and untracked files may hold anything); they go to the
  private archive. Opt-in `--push-worktree-snapshot` builds a snapshot commit in the scratch
  repository (temporary index, secret-like files and files over 50 MiB left out) and pushes it as
  `.../worktree`. Existing commits (branches, stashes, pseudorefs, reflog tips) are pushed by
  default, as the user asked for recovery branches. This **departs** from the requirement as first
  written (dirty and untracked work were listed among the things to push); it needs an explicit
  decision. Alternative: push the snapshot by default with the same exclusions, accepting that a
  public remote receives untracked files.
- **D-10. Automatic mode per class.** The requirement says automatic mode per class. Recommended:
  `--auto` takes node classes (`regenerable`, `cache`, `repo`) plus `repo-recover`, so that pushing
  recovery refs without a prompt is its own opt-in.
- **D-11. Lock scope.** The requirement says two applies cannot run on one root. Recommended: one
  apply per user per machine (10.1), which is stronger and avoids comparing root spellings.
- **D-6. rclone command line, not the rc API.** DESIGN.md names rclone's remote-control API.
  Recommended for v1: the `rclone` command (`rcat`, `cat`, `lsjson`) behind a `Store` trait, since
  `rcat` streams from stdin without a daemon; the rc API can replace it later behind the same trait.
- **D-7. Read-back is mandatory.** Recommended: always read every part back in full (egress costs
  on some providers; cold-storage classes that cannot be read at once are unsupported in v1). No
  provider-hash shortcut, since backends differ in which hashes they keep.
- **D-8. Permanent deletion.** Recommended: delete after verified backup; the recycle bin later.
- **D-9. Plan age.** Recommended default 24 hours.

## 15. Documented limits

- A process can still write into an item through a file it opened before the quarantine rename;
  writes after the final fingerprint comparison and before deletion are lost. Only working
  directories are checked, not open files.
- Dependents outside the item (clones that borrow its objects, links into its git directory) are
  as fresh as the scan; references from outside the scanned roots were never seen (SCAN-FORMAT.md,
  known limits). The plan age bounds this.
- The vault is on the same disk as the data it protects.
- Secrets in committed history are archived as they are.
- Extended attributes, macOS resource forks and Finder tags, Windows alternate data streams, ACLs,
  owners and groups are not archived or restored. File permissions are restored as mode bits on Unix
  only. Sparse files are restored dense.
- Restoring symlinks on Windows needs Developer Mode or the privilege to create them; otherwise the
  restore fails and says so.
- Line endings: the checkout of a restored repository uses the repository's own config only, so a
  system-wide `core.autocrlf` (Git for Windows) is not applied.
- Verification proves the stored object equalled what was sent at the time; it cannot stop the
  provider or the user deleting it later (restore detects a changed object).
- Untracked files that only a global ignore rule ignored are not recognised as ignored.
- The lock is per user (section 10.1). Applies run by two different users on the same files are
  not prevented by it; the quarantine rename then lets only one of them remove an item.
- Windows change time and file identity are read where the platform provides them; where not, the
  fingerprint is weaker (size, modification time, attributes).

## 16. Resource limits

- Archives are streamed; the only local disk use is the journal, the manifest segments, the index
  copies, the vault and the scratch repository (refs plus any objects a live check fetches).
- Before each item, apply checks free space on the state directory's volume and stops (before
  starting the item) below `--min-free` (default the larger of 2 GiB and 2% of the volume).
- Memory is bounded by fixed buffers (1 MiB per stream) and the index writer; nothing holds a
  whole file or archive in memory.
- One item at a time; one rclone process at a time.

## 17. Exit codes and output

- 0: dry run completed, or every selected item ended as removed, backed-up-kept by request, or was
  already done.
- 1: refused before acting (usage, inputs not bound, plan too old, lock held, rclone missing or too
  old, storage path unusable).
- 2: completed, but at least one item was skipped or failed (each listed with its reason).
- 3: interrupted; the run id is printed for `--resume`.

The summary lists every item with its outcome and reason, bytes freed, bytes uploaded, and where the
manifest is. `--json` writes the same as JSON. No output, journal line or manifest record holds a
secret value or a credential URL.

## 18. Acceptance criteria

Tests live in `crates/apply/tests/` and run in CI on ubuntu, macOS and Windows. Unless stated, they
use temporary directories, a local bare repository as the git remote (reached through a test-only
transport policy injected through the library API; the binary always uses the production policy),
and the in-process `LocalDirStore` or `FaultStore` (a wrapper that injects partial writes, flipped
bytes on read, success reported as failure, objects deleted after verification, stalls). A separate
CI job runs the same flows through the real `rclone` binary against an rclone `local` remote defined
in a temporary config file (no credentials anywhere).

- **AC-1** Dry run is the default: without `--backup-only` or `--remove`, apply prints what it would
  do for each item with the fresh re-check result and writes nothing: no state directory, no
  storage object, no ref or file in any scanned or remote repository. Verify by: `dry_run.rs`
  snapshots the tree, the state directory path, the store and the remote's refs before and after
  and asserts equality and zero store writes.
- **AC-2** `--backup-only` does steps 1 to 8 and removes nothing; `--remove` does the whole
  sequence; no other flag removes anything. Verify by: `modes.rs` runs each mode on the same
  fixture and checks outcomes, store contents and the tree.
- **AC-3** Plan and scan are bound (schema, scan identity, per-item path and class) and a plan
  older than `--max-plan-age` is refused, all before anything is done (exit 1). Verify by:
  `inputs.rs`.
- **AC-4** Only `remove-regenerable`, `backup-then-remove` and `recover-backup-then-remove` items
  are acted on, and only when a fresh plan of the item gives the same action. Verify by:
  `inputs.rs` with plans whose `ask` and `keep` items were edited into removing actions.
- **AC-5** Each item is scanned again immediately before acting and skipped, naming the first
  difference, when anything in 7.2 differs. Verify by: `recheck.rs`, one case per field in 7.2.
- **AC-6** The process check runs at the re-check and again immediately before the quarantine; a
  process working inside the item skips it. Verify by: `recheck.rs` starts a child process with its
  working directory inside the item at each point (through a library step hook).
- **AC-7** Git items pass the live remote check of 7.4 before any recovery, upload or removal.
  Verify by: `remote.rs` with a local bare repository behind the test transport policy, plus a unit
  test that the production policy refuses `file://`, local paths, `git://`, `ext::` and helpers.
- **AC-8** One fingerprint chain (section 6): F0 before the facts, equal after them, equal to every
  entry as archived and to the index, equal after archiving, equal at quarantine; any break skips
  the item before upload completes or, at quarantine, renames it back, keeps and records it.
  Verify by: `toctou.rs` writes, deletes, touches and renames entries (archived ones and
  `reproducible` ones) through the step hook between every pair of steps from 2 to 11.
- **AC-9** Recovery pushes the refs of 8.2 to the effective push URL under `recovery/`, without
  force and without running hooks, verifies each with `ls-remote`, and leaves the user's git
  directory byte-identical. Verify by: `recovery.rs` with every kind of unsaved state; fingerprint of
  the git directory before and after; a hook that would write a marker file.
- **AC-10** A failed or impossible push is recorded and does not block the item unless
  `--require-recovery-push`; the archive then holds the whole item. Verify by: `recovery.rs`.
- **AC-11** The archive holds exactly the content of 9.1 for each action, enumerated at apply time
  with `git_admin_files` and `object_store_files` for git directories. Verify by: `archive.rs`
  compares the extracted archive with the fixture and the two core lists.
- **AC-12** The archive is a deterministic pax tar split into parts as in 9.2, with a separate
  index. Verify by: `archive.rs` archives the same tree twice (identical bytes), checks long names,
  non-UTF-8 names (Unix), symlinks, internal hard links and a part size small enough to split a file.
- **AC-13** A part counts as verified only after rcat succeeded and the read-back byte count and
  SHA-256 equal what was sent, and the listing shows that size; object names are never reused.
  Verify by: `upload.rs` with `FaultStore`, and `rclone_local.rs` with the real rclone binary.
- **AC-14** The index object is uploaded and verified like a part. Verify by: `upload.rs`.
- **AC-15** Secret-like files (9.5) are copied to the vault, verified, and left out of the archive;
  the manifest lists names and sizes only. Verify by: `secrets.rs` plants fake secret values and
  searches every stored object, segment, journal line and output for them.
- **AC-16** `--upload-secrets` puts them in the archive instead; they are never pushed to a git
  remote. Verify by: `secrets.rs`.
- **AC-17** An item over the vault limits is skipped. Verify by: `secrets.rs`.
- **AC-18** Manifest records carry the fields of 12.1; segments are immutable, end with a valid
  trailer, and are mirrored to storage with read-back verification. Verify by: `manifest.rs`.
- **AC-19** Nothing is removed before every condition of 13.1 holds, in that order, including the
  second existence check of step 9. Verify by: `ordering.rs` records the order of store, journal and
  file-system operations and fails on any removal that precedes a required step.
- **AC-20** `manifest list`, `find` (with `--files`) and `orphans` work from the local state and from
  storage alone. Verify by: `manifest.rs` with an empty state directory and `--storage`.
- **AC-21** Deletion follows 13.2: no symlink or junction followed, no device crossed, read-only
  entries removed, nothing outside the quarantine touched. Verify by: `remove.rs` (symlinks to an
  outside directory on all platforms, a junction on Windows, a read-only tree), and the Linux mount
  job with a tmpfs inside an item.
- **AC-22** A `remove-regenerable` item is removed only while it is still regenerable by a fresh
  plan, with no backup unless asked. Verify by: `regenerable.rs`.
- **AC-23** Every transition is journaled before it takes effect, and a run stopped at any
  transition resumes to the same end state, with no item removed without its verified archive and
  synced record and no record written twice. Verify by: `resume.rs` stops the run at each
  transition in turn (step hook returning a stop) and resumes in a new instance; plus one test that
  kills the real binary during an rclone upload and resumes.
- **AC-24** Applying a finished plan again changes nothing and reports items as already done.
  Verify by: `resume.rs`.
- **AC-25** Only one apply runs per user per machine, whatever the roots and state directories
  (section 10.1); a second exits 1 naming the run that holds the lock, and a killed holder leaves no
  stale lock. Verify by: `lock.rs` with two processes (same and different `--state-dir`), and a
  killed holder.
- **AC-26** Archives are streamed without staging; apply stops before an item when free space is
  below `--min-free`, when the upload budget would be exceeded, or after three failing items at the
  storage. Verify by: `limits.rs` with an injected free-space reader and `FaultStore`.
- **AC-27** Ask mode behaves as in 11: per-item prompt, typed-ahead input discarded, no terminal
  means keep, end of input means keep, `q` stops before the next item. Verify by: `ask.rs` drives
  the prompt through the library's prompt interface and the binary through a pipe.
- **AC-28** Automatic mode runs only the items of the classes named by `--auto` or `[apply] auto`
  (`repo-recover` separately from `repo`); the default is none. Verify by: `ask.rs`.
- **AC-29** Restore follows 13.3 and the restored item equals the archived one, entry for entry
  (and for a repository: clean status, HEAD and refs as recorded). Verify by: `restore.rs` round
  trips for each action, including a repository restored with checkout.
- **AC-30** Restore works on a new machine from storage alone and lists vault files it could not
  restore. Verify by: `restore.rs` with an empty state directory.
- **AC-31** Platform behaviour of section 15 and 13: paths longer than 260 characters on Windows,
  read-only files, case collisions refused on case-insensitive targets, names invalid on Windows
  refused on restore. Verify by: `platform.rs` in the three-OS CI matrix.
- **AC-32** Exit codes and the summary follow section 17; no secret value or credential URL appears
  in any output, journal or record. Verify by: `output.rs` with a remote URL holding a fake token.
- **AC-33** Every git command goes through core's hardened runner, and network commands add only
  the values of 8.1; no program named by scanned content runs during re-check, recovery or restore.
  Verify by: `hardening.rs` with hooks, a `.gitattributes` filter, and config keys that name
  programs, each of which would write a marker file.
- **AC-34** With `--push-worktree-snapshot`, the snapshot commit of 8.2 is built in the scratch
  repository, leaves out ignored, secret-like and oversized files (named in the record), is pushed
  as `.../worktree` and verified; without the flag no snapshot is built or pushed. Verify by:
  `recovery.rs` compares the pushed tree with the working tree minus the exclusions, and checks the
  user's index and git directory are unchanged.
- **AC-35** Nested items follow section 6: an inner item is handled first, an inner item that was not
  removed vetoes the enclosing item, and the enclosing item's re-check compares counts net of the
  inner items removed in this run. Verify by: `nested.rs` with a repository holding a removable
  `node_modules`, run once with the inner item removed and once with it answered `n`.

## 19. Negative cases

- **NEG-1 (AC-1)** A dry run with storage that is unreachable still completes and makes zero store
  calls; a dry run never prompts.
- **NEG-2 (AC-2)** `--backup-only --remove` together is a usage error (exit 1).
- **NEG-3 (AC-3)** A plan made from another scan, a plan older than the limit, an item whose path or
  class differs from the scan node: refused, exit 1, nothing done.
- **NEG-4 (AC-4)** A plan edited from `ask` or `keep` to `backup-then-remove`: the item is skipped
  ("the plan's action differs from a fresh plan").
- **NEG-5 (AC-5)** After the scan: a new file in the item; a new commit (HEAD changed); a new branch
  or tag; a new stash; a new hook in the git directory; `git remote set-url`; the item replaced by a
  symlink to another directory; the item replaced by a file; the item deleted; a mount placed at the
  item. Each is skipped with its reason; a deleted item is reported `gone` and is not an error.
- **NEG-6 (AC-6)** A process that starts inside the item after the re-check and before the
  quarantine skips it; a process check that can read nothing skips every item.
- **NEG-7 (AC-7)** The remote branch is reset so HEAD is no longer contained; a tag on the remote
  points at another object; the remote is unreachable or refuses authentication; an `insteadOf`
  rule rewrites the URL to a local path; the URL uses `ext::`, `file://` or a helper. Each skips the
  item and no recovery, upload or removal happens.
- **NEG-8 (AC-8)** A file is written into the item after the archive was verified and before the
  quarantine: the item is renamed back, kept, and recorded `backed-up-kept`.
- **NEG-29 (AC-8)** A clean tracked file of a `backup-then-remove` repository (not archived,
  `reproducible`) is edited after the facts were computed and before archiving, or during it: the
  chain breaks, nothing is removed.
- **NEG-9 (AC-9)** A repository whose hooks (`pre-push`, `post-checkout`, `reference-transaction`)
  would write a marker: no marker appears; the user's git directory is unchanged; a recovery ref
  name that already exists on the remote with another id gets a suffix and nothing is forced.
- **NEG-10 (AC-10)** No network remote: the push is recorded as not possible and the archive path
  is used; with `--require-recovery-push` the item is skipped.
- **NEG-11 (AC-11)** A git directory file that cannot be read, a name core cannot enumerate, a fifo
  in the item: skipped before any upload counts, nothing removed.
- **NEG-12 (AC-13)** The store fails after part of a part was written: the attempt fails, a new
  attempt uses new names, and after three attempts the item is skipped; nothing removed.
- **NEG-13 (AC-13)** One byte differs on read-back, or the read-back is shorter: not verified,
  nothing removed.
- **NEG-14 (AC-13)** The upload succeeds but the tool sees a failure (lost response) or a stall:
  treated as failed; the stored object appears in `manifest orphans`; no record references it.
- **NEG-15 (AC-15)** `.env`, `id_ed25519`, `prod.tfstate` in the item and a token URL in
  `.git/config`, `.git/FETCH_HEAD` and `.git/logs/HEAD`: none of the fake values appears in any
  stored object, segment, journal line or output; the vault holds them with equal hashes.
- **NEG-16 (AC-17)** Secret-like files over the vault limit: the item is skipped.
- **NEG-17 (AC-19)** The manifest segment upload fails: the item is backed up and kept, never
  removed. An archive part deleted from storage between verification and the second existence
  check: skipped, nothing removed.
- **NEG-18 (AC-20)** A truncated or altered segment in storage is reported as damaged and its
  records are marked unverified; other segments still list.
- **NEG-19 (AC-21)** A symlink in the item to a directory outside it: the outside directory is
  unchanged after removal. A mount point inside a quarantined item: deletion stops there and reports
  `failed`. A Windows junction to an outside directory: the junction is removed, its target is
  unchanged.
- **NEG-20 (AC-22)** A regenerable directory that gained a `.git`, an output file or a `.env` after
  the scan: skipped.
- **NEG-21 (AC-23)** A run stopped at each transition and resumed: never an item missing from disk
  without a verified archive and a synced `backed-up` record; never two records for one item and
  outcome; a quarantine left behind is completed or renamed back. A resume with a different plan or
  scan is refused.
- **NEG-22 (AC-25)** Two applies at once, with the same or with different state directories: the
  second exits 1 at once. A holder killed with SIGKILL: the next apply starts.
- **NEG-23 (AC-26)** Free space below the floor, the upload budget reached, three failing items:
  each stops before the next item, and the remaining items are untouched and listed.
- **NEG-24 (AC-27)** No terminal: every item kept, nothing waits. `y` typed before the prompt
  appears: discarded. The item changes between the answer and step 11: not removed.
- **NEG-25 (AC-29)** Restore with a corrupted part: nothing at the target, staging removed, exit 1.
  An archive holding `../x`, an absolute name, a symlink followed by a file written through it, or a
  hard link to outside: refused, nothing written outside the staging directory. An existing target:
  refused. A restored repository whose HEAD or refs differ from the record: nothing at the target.
- **NEG-26 (AC-31)** Two names differing only by case restored onto a case-insensitive file system,
  or a name invalid on Windows restored there: refused, naming them.
- **NEG-27 (AC-32)** A remote URL with an embedded fake token appears nowhere in output, journal or
  records.
- **NEG-28 (AC-33)** A repository with a `.gitattributes` filter, hooks, and config naming programs:
  no marker file is written during re-check, recovery, push, fetch or restore checkout.
- **NEG-30 (AC-34)** Without the flag, a dirty repository: no `.../worktree` ref on the remote. With
  it, an untracked `.env` and a 60 MiB untracked file: neither is in the pushed tree, both are named
  in the record, and the `.env` is in the vault.
- **NEG-31 (AC-35)** A repository whose nested `node_modules` item is answered `n`, skipped or
  outside `--class`: the repository is skipped and nothing in it is removed.
- **NEG-32 (AC-5)** An item that contains the tool's state directory, or a local storage path, or
  lies inside one: skipped.
- **NEG-33 (AC-23)** The journal says `quarantined` but the process died before the rename: on
  resume the item is checked again from `segment-synced` and nothing is lost or removed twice.
  A scratch repository left by the crash is deleted before the resumed run does anything else.

## 20. Test and build plan

- New crate `crates/apply` (library: re-check, recovery, archive, store, manifest, journal, lock,
  restore) and CLI wiring in `crates/cli`. Core changes are additive only (D-1, D-2).
- Tests are written first; each behaviour gets a red CI run before its implementation lands.
- Builds and tests run in GitHub Actions on ubuntu, macOS and Windows. The rclone job installs a
  pinned rclone release and checks its published SHA-256 before use.
- No test touches any directory outside its own temporary directory, and no test uses a network
  remote or a credential.
