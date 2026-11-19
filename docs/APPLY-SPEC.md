# Apply phase specification

Status: **draft for review, revision 6**. Nothing in this document is implemented yet. It defines
what `disk-cleaner apply`, `disk-cleaner restore` and `disk-cleaner manifest` must do before any
code is written, and it is the contract the tests are written against. Section 16 lists the
decisions that are still open; text that depends on one of them says so. Section 22 maps the latest
review to changes; earlier reviews are answered in [APPLY-SPEC-REVIEWS.md](APPLY-SPEC-REVIEWS.md).
Criterion and negative-case numbers are stable from revision 5; revision 6 only adds.

Read with [DESIGN.md](DESIGN.md) and [SCAN-FORMAT.md](SCAN-FORMAT.md). Where this document and
SCAN-FORMAT.md disagree about the plan format, SCAN-FORMAT.md wins and this document is wrong. The
one deliberate difference, the live remote check, is stated in section 8, and core change C-4
(section 15) brings the plan's wording in line.

This release runs **no git command that can reach the network**: no `ls-remote`, no push, no
fetch, no credential helper. The only host apply contacts is the user's own configured storage,
through rclone (9.6). The invariant that makes this safe:

> **Apply removes a git item only after a verified archive of the entire item, every entry
> included, is in storage. Remote facts from the scan are informational in this release; no
> removal depends on them.**

`recover-backup-then-remove` and `backup-then-remove` therefore execute identically (whole-item
archive, read back, manifest, remove). The manifest records which of the two the plan said. Pushing
recovery refs and the live remote check are a named later release (section 8).

## 1. Outcome

A user who has a plan from `disk-cleaner plan` can run `disk-cleaner apply` and get back disk
space without losing anything: every item is re-checked at the moment of acting, the whole item is
archived to the user's own rclone storage and read back before anything is removed, every action is
written to a manifest that is itself in storage before the removal it describes, and
`disk-cleaner restore` puts any removed item back exactly as it was archived.

## 2. Scope

In scope:

- `disk-cleaner apply`: executes the removing items of a plan (`remove-regenerable`,
  `backup-then-remove`, `recover-backup-then-remove`), dry run by default.
- A whole-item archive of every removed git item (working tree, ignored files and git
  directories), streamed to an rclone remote, read back and compared.
- Secret-like files: such an item is kept unless the user opts in to uploading them.
- A journal that makes every step resumable and idempotent; a per-user lock so two applies never
  run at once on one machine.
- The manifest (local and in storage), `disk-cleaner manifest list|find|verify|orphans`, and
  `disk-cleaner restore`.
- Ask mode (per item, the default) and automatic mode (per class, opt-in).
- **Platforms.** macOS and Linux: dry run, backup, removal and restore. Windows: dry run,
  `--backup-only` and restore; `--remove` is refused on Windows until handle-based deletion is
  specified and tested there (D-13).

Out of scope for this release. **Against the original requirements this release is partial; each
item marked TARGET_MISMATCH is deferred, not met:**

- **TARGET_MISMATCH: pushing recovery branches.** The requirement asked for automatic tools that
  push unsaved work to recovery branches. This release pushes nothing; the whole-item archive holds
  that work instead. Section 8 lists the entry conditions for the later release.
- **TARGET_MISMATCH: outputs, agent sessions and logs, loose code.** The plan has no removing
  action for them yet. Backing up outputs and agent sessions to cold storage, and loose-code
  handling (connect an origin, back up raw, remove or leave), are deferred. The archive and manifest
  formats are class-agnostic so a later plan action can use them unchanged.
- Acting on plan items whose action is `ask` or `keep`. Apply never does more than a fresh plan
  would (AC-4).
- Items in sync folders or holding online-only placeholder files (section 7.4): kept.
- Linked worktrees, bare repositories and repositories with submodules: the plan never gives them
  a removing action in this schema (`plain_layout`).
- A local vault, client-side encryption and key management (users who want encryption configure an
  rclone `crypt` remote, see 9.2), an encrypted rclone configuration file (refused, 9.6), the
  recycle bin, scheduled runs, the desktop app, cold-storage classes that cannot be read back at
  once, batching many items into one archive, network file systems as item locations (section 17).

## 3. Threat model

**What is protected.** The user's unique data (commits that exist nowhere else, stashes,
uncommitted, untracked and ignored files, the git directory's own files, extended attributes) and
the history of what was removed. Also the user's credentials and identity.

**Where harm comes from**, in order of likelihood:

1. **Change between check and use.** The user, an editor, a build, a coding agent or a sync client
   writes into an item after the scan, after the re-check, during the upload, or between the last
   check and the removal, on this machine or (through a sync folder) on another.
2. **Storage that lies or fails.** A partial upload, a corrupted object, a lost response, a rate
   limit, a backend that spools to local disk, an object deleted later by a lifecycle rule.
3. **Interruption.** The process is killed, the machine sleeps or loses power, at any step.
4. **Scanned content is untrusted.** A scanned folder may be an unpacked download. Its git config
   (remote URLs, `insteadOf`, `pushurl`, programs), hooks, `.gitattributes`, symlinks, mount points,
   hard links, special files, attributes and file names are attacker-controlled. Apply must not run
   any program it names, must not contact any host it names (apply contacts only the user's
   configured storage, through rclone), must not follow any link out of the item, must not cross
   into another mount, and must not write outside the item, the tool's own state and, through
   rclone, the user's storage. rclone may rewrite its own configuration file when it refreshes a
   storage token; that is rclone's behaviour for the user's own remote, not a write the tool makes.
5. **Stale remote facts.** The remote may have been force-pushed, deleted or changed since the scan.
   In this release no removal depends on remote facts (the invariant above), so this cannot cause
   loss; it only makes the manifest's informational remote facts out of date.
6. **Disclosure.** A secret uploaded in plaintext, or printed to the terminal, a log or the
   manifest; rclone's credentials printed or a password prompt answered by the archive stream.
7. **Operator mistakes.** A stale plan, a plan from another scan, a plan edited to remove more,
   running apply twice, two applies at once, a typed-ahead answer, running as root.

**Not defended against.** Another program running as the same user that deliberately destroys data;
an administrator; a storage provider reading the archives (needs an rclone `crypt` remote); a
storage provider or user deleting or altering an archive after it was verified (`manifest verify`
and restore detect it, they cannot undo it); memory corruption.

## 4. Terms

- **Item**: one plan item that apply acts on, identified by `item_id`, a hex SHA-256 of the scan
  identity, the node index and the path (stable across resumes).
- **Run**: one invocation of `apply` (or its resumption), identified by a random `run_id`.
- **State directory**: `<data dir>/disk-cleaner/`, where `<data dir>` is derived from the account's
  home directory in the user database (not from `HOME` or `XDG_*`): `~/.local/share` on Linux,
  `~/Library/Application Support` on macOS, the known-folder `LocalAppData` on Windows; or
  `--state-dir`. It holds `machine.json`, `lock`, `journal/`, `manifest/`, `index/` and
  `rclone-tmp/` (rclone's temporary directory, 9.6), created with owner-only permissions.
- **Storage**: an rclone remote and path, `--storage <remote>:<path>`, that the user configured with
  `rclone config`. The tool never reads or prints rclone's credentials and never passes a credential
  on a command line (9.6).
- **Machine id**: a random 128-bit id created on the first run, kept in `machine.json` with an
  optional label (`--machine-label`, default empty). Remote paths use the id only.
- **Fingerprint**: for every entry below an item (the item directory included), its relative path
  (bytes), kind (directory, regular file, symlink), size, modification time in nanoseconds, change
  time (not for the item directory, which the quarantine rename changes), device, inode and mount
  id where the platform provides one, mode bits, file flags (macOS `st_flags`), link count, the
  names and sizes of its extended attributes, and, for a symlink, its target bytes. Any other kind
  of entry (fifo, socket, device) makes the item unsupported, except the socket
  `.git/fsmonitor--daemon.ipc`, which SCAN-FORMAT.md allows in a plain git directory: it is
  recorded by name, not archived, and not recreated by restore.
- **Index**: the item's fingerprint F0 plus the SHA-256 of every regular file (9.3). For a
  `remove-regenerable` item removed without a backup, the index is F0 alone, written to
  `<state>/index/` before its quarantine (never in a dry run, which writes nothing).
- **Quarantine**: renaming an item, with no-replace semantics, to `.disk-cleaner-removing-<item_id>`
  in the same parent directory before deleting it.
- **Secret-like file**: defined in 9.2.

## 5. Commands

```
disk-cleaner apply --plan PLAN --scan SCAN --storage REMOTE:PATH
                   [--backup-only | --remove]
                   [--auto CLASS[,CLASS...]] [--class CLASS[,CLASS...]] [--item PATH]...
                   [--backup-regenerable] [--upload-secrets] [--require-encrypted-remote]
                   [--max-plan-age HOURS] [--max-upload-bytes N] [--min-free BYTES]
                   [--part-size BYTES] [--state-dir DIR] [--machine-label TEXT]
                   [--rclone PATH] [--allow-root] [--json]
disk-cleaner apply --resume RUN_ID
disk-cleaner restore RECORD_ID [--to DIR] [--storage REMOTE:PATH] [--machine ID] [--json]
disk-cleaner manifest list   [--storage REMOTE:PATH] [--machine ID] [--json]
disk-cleaner manifest find TEXT [--files] [--storage REMOTE:PATH] [--machine ID] [--json]
disk-cleaner manifest verify --storage REMOTE:PATH [--deep] [--machine ID] [--json]
disk-cleaner manifest orphans --storage REMOTE:PATH [--machine ID] [--json]
```

- No mode flag: **dry run**. `--backup-only`: everything up to and including the manifest, then
  stop; nothing is removed. `--remove`: the full sequence (refused on Windows, exit 1). The two are
  mutually exclusive.
- `--auto` names the classes whose items run without a prompt: `regenerable`, `cache` (their
  `remove-regenerable` items), `repo` (`backup-then-remove`), `repo-recover`
  (`recover-backup-then-remove`). `[apply] auto = [...]` in the config file does the same. Default:
  none, so every item is asked. `repo` and `repo-recover` execute identically; they stay separate so
  the user decides about repositories with unsaved work on their own.
- `--class` and `--item` narrow the selection; they never widen it.
- `--resume RUN_ID` takes no other flags: the run's flags are in its journal.
- Running as root (uid 0) is refused unless `--allow-root`.
- Configuration lives in the same `config.toml` as the plan settings, under `[apply]` (`auto`,
  `upload_secrets`, `require_encrypted_remote`, `min_free`, `part_size`, `max_upload_bytes`,
  `max_plan_age`). Unknown keys are an error, as for the plan.

## 6. The sequence

### 6.1 At the start of every apply

Every apply first checks rclone (9.6). Then, dry runs excepted (they only report), it takes the
lock (10.2) and reconciles what earlier runs left behind, according to its mode:

- **Every mode, dry run included:** every quarantine directory named in an unfinished journal, and
  every record not yet in a synced segment, is reported with its run id. A path is never reported
  `gone` while a quarantine with its `item_id` exists; its item is reported "removal pending from
  run RUN_ID" and is not selected again.
- **`--backup-only`:** in addition, `removed` records written but not yet in a synced segment are
  synced to the storage named in **that run's journal** (not the current `--storage`, if it
  differs). No quarantine is renamed and nothing is deleted.
- **`--remove`:** in addition, each quarantine is finished or renamed back as in the resume table
  (10.1). Before a deletion is completed, step 7 (the second existence check) is repeated against
  the storage named in that run's journal; if it fails, the quarantine is left and reported.

### 6.2 For one item

Items are processed one at a time, innermost first. Every transition is written to the journal
before the step it records takes effect. Any failed check ends the item as **skipped** with a reason
and the run continues with the next item; nothing that failed a check is ever removed.

1. **Select.** The item's plan action removes something and the item is within `--class` and
   `--item`.
2. **Re-check** (section 7): take the fingerprint F0, re-scan and re-plan, refuse places that are
   never removed, check processes, take the fingerprint again.
3. **Ask** (ask mode only, section 11).
4. **Archive and upload** (section 9): fix the attempt number and part size in the journal, stream,
   split into parts, upload, read back, compare; then the index object the same way.
5. **Manifest** (section 12): reserve the next segment number in the journal, write the item's
   `backed-up` record to that new segment, upload it, read it back, compare.
6. Stop here for `--backup-only` (outcome `backed-up-kept`, by request).
7. **Second existence check**: list each part, the index object and the manifest segment in
   storage and compare their sizes with what was verified; confirm the item path is still the
   directory of F0 (same device, inode and mount id).
8. **Process check again.**
9. **Quarantine**: rename with no-replace semantics; take the fingerprint of the renamed directory
   through the handle opened on it, and compare with F0. Any difference: rename it back and end as
   `backed-up-kept` (the archive stays, the manifest says why). If the rename back fails because
   the original path was recreated, the quarantine stays, the record says where, and the summary
   names both paths.
10. **Delete** the quarantined directory (section 13.2).
11. **Record** the `removed` record; it is synced with the next item's step 5, at the end of the
    run, or by the next apply (6.1).

`remove-regenerable` without `--backup-regenerable` (and without the answer `b`) skips steps 4, 5
and 7; step 9 still compares with F0. Its record says it was regenerable and names the rule.

Git items: both `backup-then-remove` and `recover-backup-then-remove` run steps 1 to 11 exactly as
written. No step contacts a remote.

### 6.3 Nested items

Removing an item removes everything inside it:

- **regenerable children of a git item.** A `remove-regenerable` item inside a git item that is
  selected in the same run is **not** handled on its own first. It is part of the enclosing item:
  archived inside it (the archive includes regenerable children, so "every entry" has no exception)
  and removed with it. The enclosing item's prompt lists these children and their bytes, and its
  record lists their paths in `nested_regenerable`. If the enclosing item ends as anything but
  removed, each child is then handled on its own as an ordinary item (re-checked and, in ask mode,
  asked). If a child is not selected (outside `--class` or `--item`), the enclosing item is skipped
  under `--remove`, as below. Cost: the archive holds regenerable bytes that could be rebuilt;
  the dry run shows how many;
- any other selected item inside another selected item is handled first. With `--remove`, if it
  ends as anything but removed (skipped, failed, kept, answered `n`, outside `--class` or `--item`), the
  enclosing item is skipped ("the nested PATH was kept"), whatever its own checks say;
- with `--backup-only` the veto does not apply: nothing is removed, and the enclosing item's
  archive holds the nested item like any other content;
- the enclosing item's re-check compares the scan **net of** the nested items this run removed:
  each removed nested path must be absent and the journal must show it `deleted`, and those nodes'
  counts are subtracted before comparing. A nested **git** item that was removed changes the
  enclosing item's `preserve` lists and facts, so the enclosing item is then always skipped (safe;
  it is asked about again after a new scan).

### 6.4 One fingerprint chain

Every comparison of an item is against one fingerprint, F0, taken through directory handles at the
start of step 2, before any fact is computed:

- at the end of step 2 the fingerprint is taken again and must equal F0, so the facts were computed
  on exactly F0;
- while archiving, every entry's metadata is compared with F0 as it is opened, and the index
  written is F0 plus content digests; after the last part the fingerprint is taken again and must
  equal F0;
- at step 9 the quarantined directory must equal F0.

Every entry is archived (9.1), so the chain is the only thing that ties the removed directory to
the archive, and it is complete.

## 7. Re-check (time of check, time of use)

### 7.1 Inputs are bound together

- The plan and scan must have the schema version this build supports.
- The plan must have been made from this scan: today by `plan.scan_generated_at ==
  scan.generated_at` and, for every item, `scan.nodes[item.node].path == item.path` and the same
  class; once core records it, by the SHA-256 of the scan file (D-2).
- A plan with two items for one node or one path is refused.
- A plan or scan dated more than 5 minutes in the future is refused.
- The plan's age is checked at start and again before each item: an item is not started once the
  plan is older than `--max-plan-age` (default 24 hours; D-9). The remaining items are listed as not
  started.
- A resumed run must use the same plan, scan and flags (their hashes are in the journal header).

Any of these failing at start is exit code 1 before anything is done.

### 7.2 The item is scanned again

Immediately before acting, apply re-scans the item with core's item re-scan (section 15, C-3), which
walks only the item, reads the rule markers in its parent directory by name, applies the enclosing
repository's ignore check to the item itself, uses the same options as the scan file and the
hardened runner, and plans the result with the plan's options. The item proceeds only if all of
these hold; the skip reason names the first that does not:

- the path exists and is a directory (not a symlink, junction or file), it is not a mount root, and
  its device and mount id equal those of its parent directory, read again now (the scan file
  records no device, so this is how "the same file system as in the scan" is checked);
- the fresh re-scan finds the same set of nodes inside the item as the scan file (paths and
  classes), every one of them gets the same action from the fresh plan as in the plan file (nested
  items already removed in this run excepted, 6.3), and no node has an `ask_reason`;
- class, rule and every count of the item's node equal the scan's (`files_total`, `bytes_total`,
  `newest_mtime`, `output_bytes`, `unreadable`, `skipped_mounts`, `contains_outputs`,
  `outputs_unlisted`);
- for a git node: every fact equals the plan item's `facts`; `head_commit`, `last_commit_time`,
  the remote names and URLs, `git_admin` and `object_store` counts, `unsaved_pseudorefs`,
  `hidden_index_entries` and `local_admin_content` are equal; the git directories inside the item
  equal `preserve.git_admin.git_dirs`. This is **drift detection** on local state (a new commit, a
  new stash, a changed remote URL means the user touched the item since the scan); the remote facts
  compared here are read from the local repository, not from the remote;
- for a regenerable or cache node: its owning project (its parent node in the scan) has the same
  activity evidence as in the scan, read with core's project activity function (C-3): newest
  modification time excluding `.git` and nested nodes, and newest commit time;
- the newest activity is still outside the active window.

Dependents found elsewhere in the scan cannot be seen by a re-scan of the item; the scan file's
`no_dependents` is used as it is, and a fresh `no` wins. This is a documented limit (section 17),
bounded by the plan age.

### 7.3 Running processes

The process check (`running_cwds`) runs at step 2 and again at step 8. A process whose working
directory is inside the item, or inside the project that owns a regenerable item, skips it. A
check that could read no process skips every item; a partial check skips every item unless the plan
recorded `trusted_partial_process_check`.

### 7.4 Places that are never removed in this release

An item is skipped, with the reason, when:

- **it is in a sync folder**: its path lies under `~/Library/CloudStorage`, `~/Library/Mobile
  Documents`, `~/Desktop` or `~/Documents` on macOS (iCloud Desktop and Documents cannot be ruled
  out cheaply), a Dropbox folder listed in Dropbox's own `info.json`, a OneDrive folder named by the
  `OneDrive*` environment variables or registered as a Windows cloud-files sync root, or any
  directory with a Syncthing `.stfolder` marker in it or an ancestor; in it or an ancestor, a
  Nextcloud or ownCloud journal (`.sync_*.db` or `._sync_*.db`), a MEGA `.debris` directory or a
  Resilio `.sync` directory; or a path under `~/Seafile` (Seafile's default; other Seafile
  libraries are not detected, section 17). Deleting there deletes on the user's other machines,
  which this tool cannot check;
- **it holds an online-only placeholder**: an entry with the macOS `SF_DATALESS` flag, or a Windows
  file with the recall-on-open, recall-on-data-access, offline or pinned attributes, or a
  cloud-files reparse point. Reading one triggers a download or fails;
- **it holds or crosses a mount**: any entry inside the item, file or directory, that is a mount
  root (Linux: `statx` `STATX_ATTR_MOUNT_ROOT`, or a mount id that differs from the item's, else
  `/proc/self/mountinfo`; macOS: `st_dev` differs or `statfs` names it as a mount point), including
  a Linux bind mount of a directory or of a single file from the same file system. Where no mount
  information can be read, removal is refused;
- **it is not on a known local file system.** Removal is allowed only on an allowlist of types:
  Linux ext2/3/4, xfs, btrfs, f2fs, zfs and tmpfs; macOS apfs and hfs. Everything else (NFS,
  SMB/CIFS, AFP, FUSE, FAT, exFAT, NTFS through a driver, 9p, WSL drvfs, overlay) is refused for
  removal: change detection by metadata is weak or unreliable there (FAT and exFAT store coarse
  modification times and no change time). Backup from such a file system is allowed;
- **it holds or lies inside the tool's own state directory**, or the storage path when that is a
  local directory (after resolving rclone wrappers as 9.6 states);
- **an entry cannot be deleted**: an immutable or append-only flag (`chflags uchg`/`schg`,
  `chattr +i`/`+a`) or a directory the user cannot write;
- **an entry's extended attributes cannot be archived** (9.3).

A `local` storage remote on the same device as the item is warned about once per run: it is not a
backup of that disk.

### 7.5 Git commands apply runs

All of them are local and read-only, and all go through core's hardened runner (`Git::command`:
`protocol.allow=never`, no hooks, no fsmonitor, no system or global config, no inherited `GIT_*`
variables). `protocol.allow=never` also stops a lazy fetch in a partial clone. Apply reaches git
only through:

- core's `rescan_item` and `project_activity` (C-3), which run the read-only commands core's scan
  runs, unchanged. At core commit 28705e2 these are exactly: `--version`, `rev-parse`, `config`
  (`--get` and `--list` forms only), `status`, `ls-files`, `ls-tree`, `rev-list`, `for-each-ref`,
  `log`, `cat-file`, `remote -v`, `stash list` and `worktree list`. (`git_admin_files` and
  `object_store_files` classify by name and run no git.);
- `git config --file FILE --get core.excludesFile` and
  `git config --file FILE --get-all include.path`, once per run for each of the user's global
  config files and the files they include (9.2), run from the file-system root;
- `git ls-files -z --others --exclude-standard` and
  `git ls-files -z --others --ignored --exclude-from=FILE` in each working tree inside the item
  (9.2);
- for history held only locally (9.2): `git for-each-ref` (remote-tracking tips),
  `git rev-list --objects --all --reflog --missing=allow-any --stdin` (with the network remotes'
  tracking tips as `^OID` lines on stdin), `git stash list`, `git ls-tree -r -z` of stash parents,
  and `git cat-file --batch` (raw blob content, no filters) for the bounded content check.

That list is the **allowed set**. Restore and removal never run git. A test (AC-9) records every git
invocation through a logging shim and fails on any subcommand outside the allowed set; when core
changes its command set, this list and the test change together, deliberately.

## 8. Deferred: live remote check and recovery push

This release runs **no git command that touches the network**: no `ls-remote`, no push, no fetch,
no credential helper, no host allowlist, and no network runner in core. `--push-recovery`,
`--push-remote`, `--check-remote`, `[apply] git_hosts` and `[apply] git_ssl_ca_file` do not exist.

**Deviation from SCAN-FORMAT.md.** SCAN-FORMAT.md says the remote facts are confirmed with
`git ls-remote` at apply time (the `all_branches_on_remote` row, the paragraph on stale
remote-tracking refs, and the notes on global `insteadOf` rewrites), and DESIGN.md said the same.
This release instead removes nothing on the strength of remote facts: every removed git item is
archived whole and verified first (the invariant at the top of this document). SCAN-FORMAT.md and
the plan's own text are core's; C-4 (section 15) lists every place that promises an apply-time
check and the wording the core owner is asked to adopt.

A later release may add both features. Its entry conditions are the findings of the reviews of
revisions 2 and 4, each answered in a spec with its own criteria:

- **M1** Credentials and prompts must never reach a host named by scanned config: host allowlist,
  non-interactive runner, ssh batch mode with strict host keys, `--end-of-options`, URLs starting
  with `-` rejected.
- **M2** Never push stash parents, reflog-only commits, uncommitted work or commits that add
  secret-like files.
- **M3** Recovery refs must not trigger CI, webhooks or deployments, go only to a named or confirmed
  remote, never to a publicly readable one, and carry no machine name; the namespace must be proven
  against each hosting provider it is enabled for (review F8).
- **F1** The scanned repository's `url.*.insteadOf` and `url.*.pushInsteadOf` still rewrite a URL
  given on the command line: every network command must run in an empty tool-owned directory, never
  with the scanned repository's config loaded; the push URL must be defined exactly
  (`remote.NAME.pushurl` and `pushInsteadOf` included) and the public-readability check run on that
  same URL; `http.followRedirects=false`.
- **F2** Git before 2.32 ignores `GIT_CONFIG_GLOBAL`, so `HOME` must stay the null device or the
  minimum git must be 2.32; network configuration must pass through `GIT_CONFIG_COUNT` or an
  owner-only file, never argv, where other users can read it; helpers must be matched by host with
  userinfo dropped; `includeIf` handling stated.
- **F3** Credential helpers run `store` after success and `erase` after a 401: only `get` may be
  forwarded to the user's helpers, through a tool-owned wrapper; a dry run must not write anywhere.
- **F6** A failed live check must block only the push and removal, never `--backup-only`.
- **F10** A CA file must be scoped per URL, never replace the system bundle.
- **F11** ssh must be tested on macOS and Windows, not only Linux.
- **PR-stage check** The content check before a push must use a fresh `ls-remote` taken just before
  the push, and must list blob names from tree walks, not from the one name `rev-list --objects`
  shows.

## 9. Backup

### 9.1 What is archived

Every removed item is archived **whole**: every entry of the item directory, working tree, ignored
files, git directories and regenerable children (6.3) alike. No file is left out because git says
it is clean or regenerable, and no file is left out because a remote is believed to hold it. The
one case where an enclosing archive lacks a nested directory is a nested item removed earlier in
the run on its own, which only happens for items that are not regenerable children of a git item
(6.3) and which have their own record. A `remove-regenerable` item on its own is archived only with
`--backup-regenerable` or the answer `b`.

The **preserve sets** are a checked subset: for every git directory in
`preserve.git_admin.git_dirs`, `git_admin_files` and `object_store_files` are called again at apply
time; the call must succeed and every file they list must appear in the index as archived, or the
item is skipped.

### 9.2 Secret-like files

A file is **secret-like** when any of these holds (best effort; anything not caught is uploaded,
always to the user's own storage):

- **by name**: it is untracked or ignored (a tracked file's content is in the object store, which
  the history rule below covers) and its name is in the **secret name list**: the scan's
  `[[state_file]]` names (`.env`, `.env.*`, Terraform state and variables, SSH key names) or one of
  `id_rsa`, `id_dsa`, `id_ecdsa`, `id_ed25519` (and their `*_sk` forms), `*.pem`, `*.key`, `*.p12`,
  `*.pfx`, `*.jks`, `*.keystore`, `*.ovpn`, `.netrc`, `_netrc`, `.npmrc`, `.pypirc`,
  `.git-credentials`, `.envrc`, `kubeconfig`, `credentials`, `credentials.json`,
  `service-account*.json`, `secrets.yaml`, `secrets.yml`. For the `fixture_prone` key-material
  names (`*.pem`, `*.key`, `*.p12`, `*.jks`, `*.keystore`), a file that shows the scan's evidence
  of belonging to an installed package (SCAN-FORMAT.md, same rules file) is exempt from the name
  rule; the content rule still applies to it;
- **in local-only history**: for each git directory in the item, a path that git history holds
  but no network remote's tracking refs contain has a name in the secret name list (the
  `fixture_prone` exemption does not apply here), or is a blob smaller than 1 MiB whose first
  64 KiB match the content rule below. The paths come from local commands only (7.5):
  - `git rev-list --objects --all --reflog --missing=allow-any --stdin`, given on stdin a `^OID`
    line for every remote-tracking tip of each network remote (core's definition of a network
    remote), lists every object reachable from local refs, the stash and every reflog but not from
    those tips, with the path git first reached it by;
  - because that command shows one path per object, `git ls-tree -r -z` of each stash entry's
    index commit (`^2`) and untracked-files commit (`^3`, when present) lists every name in them;
    an entry counts when its object is in the local-only set above;
  - the content check reads local-only blobs with `git cat-file --batch` (raw content, no
    filters), at most 64 MiB in total per item; past that budget only names are checked and the
    summary says so. A blob missing from a partial clone is skipped (it is not in the archive
    either).

  This catches a `.env` set aside with `git stash -u` or `git stash -a`, a key file added by an
  unpushed commit, and one added by a commit that is now reachable only from a reflog. History
  that a network remote already holds is not examined (section 17);
- **in the user's global excludes file**: it is untracked and matched by that file. SCAN-FORMAT.md
  requires such files to be treated as possibly secret, and core's scan does not apply global
  excludes, so apply finds them itself:
  - the file is the last `core.excludesFile` value in the user's global config files,
    `<config home>/git/config` and then `~/.gitconfig` (the later one wins, as in git), each read
    with `git config --file FILE --get core.excludesFile`. `include.path` values in those files
    are read with `git config --file FILE --get-all include.path`, a leading `~/` is expanded by
    the tool, and each included file is read the same way, in git's order, at most 10 levels deep.
    `~` and `<config home>` (`XDG_CONFIG_HOME`, else `~/.config`) come from the account in the
    user database (the hardened runner's `HOME` is the null device, so git cannot expand `~`
    itself). A leading `~/` in the excludes value is expanded the same way; with no value, the
    default is `<config home>/git/ignore`;
  - `includeIf` blocks are not evaluated (they depend on the repository; section 17);
  - in each working tree inside the item, a file is secret-like when it is listed both by
    `git ls-files -z --others --exclude-standard` (untracked and not ignored by the repository's
    own rules, which is how core's scan sees it) and by
    `git ls-files -z --others --ignored --exclude-from=FILE` (matched by the global file). A file
    the repository's own rules ignore is not caught by this rule (other rules still apply);
  - operating-system metadata files are exempt from this rule only: `.DS_Store`, `Thumbs.db`,
    `desktop.ini` and the macOS `Icon` file with a trailing carriage return. Without the exemption
    a global `.DS_Store` rule, which is common on macOS, would keep almost every repository;
- **by content**: it is untracked or ignored, smaller than 1 MiB, and its first 64 KiB contain the
  header line of a PEM private key block, a URL that carries a credential (below), or a token in
  one of the documented formats of the common providers (AWS access keys, GitHub and GitLab
  tokens, Slack tokens, API secret keys of the common model providers, Google API keys); the exact
  patterns live in the code and its tests, not in this document;
- **in a git directory**: it is a file of a git directory (`config`, `FETCH_HEAD`, files under
  `logs/`, any other) that contains a URL that carries a credential.

A URL **carries a credential** when its userinfo has a password, or its user name matches a token
pattern, is 40 or more hexadecimal characters, or is longer than 30 characters. A short plain user
name (as in a default Azure DevOps clone URL) is not a credential.

**Default: an item with any secret-like file is kept** (skipped before upload, reason naming up to
five of them by relative path, or "a stash or a local-only commit holds NAME", never contents).
With `--upload-secrets` (or `[apply] upload_secrets = true`) they are archived like everything else.
No local vault exists (D-4). The run summary counts the items kept by each rule, so the cost of
over-keeping (mainly the global-excludes rule) is visible.

**Encryption.** Before the first upload, apply reads the storage remote's type (9.6). When it is not
`crypt`, the run summary and every ask prompt say that the archive is stored unencrypted in the
user's storage. `--require-encrypted-remote` (or `[apply] require_encrypted_remote = true`) refuses
to upload to such a remote (exit 1).

### 9.3 Archive format

- A POSIX pax tar stream (`ustar` with pax extended headers for long names, large sizes, nanosecond
  times and names that are not UTF-8 on Unix). Uncompressed.
- Deterministic: entries in byte-wise order of relative path, directories before their contents,
  owner and group 0 with empty names, mode bits, file flags and modification times as on disk. The
  stream depends only on F0 and the file contents, not on the part size; its exact length, and so
  every part's length, is computed from F0 before the upload starts.
- Directories, regular files and symlinks (the target is stored as written, never followed). Two
  names of one file inside the item are stored as a hard-link entry after the first name. A hard
  link to a file outside the item is stored as a regular file. Fifos, sockets (except the fsmonitor
  socket, see section 4) and devices make the item unsupported.
- **Extended attributes** (macOS and Linux, including the macOS resource fork and Finder info) are
  stored as pax `SCHILY.xattr.<name>` records, up to 64 KiB per attribute and 1 MiB per file. These
  harmless attributes are dropped and listed by count: `com.apple.quarantine`,
  `com.apple.provenance`, `com.apple.lastuseddate#PS`, and an all-zero `com.apple.FinderInfo`.
  An attribute over the limits makes the item unsupported. Linux namespaces: `user.*` is archived;
  `security.selinux` is recorded in the index but not restored or compared (the target system
  labels the files); any other `security.*` attribute makes the item unsupported;
  `system.posix_acl_*` are ACLs, not archived (section 17); `trusted.*` is not visible to an
  ordinary user and is not read. On Windows, an alternate data stream
  other than `Zone.Identifier` makes the item unsupported (Windows is backup-only in this release).
  ACLs are not archived (section 17).
- The stream is split into **parts** at arbitrary byte offsets; restore concatenates them. The part
  size is chosen per attempt (section 18), written to the journal before the first part, and never
  changes within the attempt.
- The **index** is a separate JSON Lines object, `<archive>.index.jsonl`: a header (format version,
  machine id, original path, plan action, HEAD for git items, the part list with sizes and SHA-256)
  and one line per entry with its fingerprint fields and, for regular files, its SHA-256. A copy
  stays in `<state>/index/`.

### 9.4 Upload and verification

- An attempt starts with a journal line `attempt-started` holding the attempt number, the part
  size, the part count and every object name the attempt will use; it is synced before the first
  byte is sent. Attempt numbers only grow, so no name is ever used twice, also across a crash.
- Each part is produced by one pass over the item through directory handles (section 13.2 rules:
  `openat` with no-follow, `fstatat`, nothing resolved by path twice). The tar writer writes into a
  SHA-256 hasher, a byte counter and the stdin of
  `rclone rcat --size <exact part length> <storage>/<object>`. No copy of the archive is staged by
  the tool. rclone's `local` backend is tested not to spool (AC-24); whether every other backend
  avoids spooling when `--size` is given is **unverified**, so the part size is bounded by the free
  space of rclone's temporary directory (section 18), and the click-test exercises one real
  non-local remote (section 24).
- An entry whose metadata differs from F0 when opened, or whose size changes while read, or that
  cannot be read, fails the item before the part completes (fail closed).
- The part is then read back with `rclone cat` into a second hasher. It is **verified** only if
  rcat exited 0, the bytes read back equal the bytes sent and the computed length, both digests are
  equal, and `rclone lsjson` reports that size.
- Object names are unique per attempt:
  `disk-cleaner/v1/<machine id>/archives/<run_id>/<item_id>.a<attempt>.p<part>.tar`. Nothing is ever
  overwritten; the store interface refuses to write a name that exists.
- A failed attempt (nonzero exit, mismatch, no progress for 10 minutes) is never retried in place:
  the item restarts from step 2 with attempt + 1, at most 3 attempts. Objects of failed attempts
  (including a lost response, where the object arrived but the tool saw a failure) are orphans,
  listed by `manifest orphans`, never deleted automatically.
- After the last part, the index object is uploaded and verified the same way.

### 9.5 Rate limits and budgets

rclone's own retries handle rate limiting within one call. **Three consecutive storage failures
stop the backup stage**: an item counts as a storage failure when it used all 3 attempts and each
failed at the storage (upload, read-back, listing); an item that is skipped by a check before
upload does not count, and an item that is verified resets the count to zero. The remaining items
are not started and are listed. `--max-upload-bytes` stops before an item whose archive would exceed
the budget; the estimate is the item's logical size from F0 (sparse files count at their full
length).

### 9.6 rclone

- The rclone binary is `--rclone` or the one on `PATH`; its version must be at least 1.64 (for
  `config redacted`), checked at start (exit 1).
- Every rclone command runs with `--ask-password=false`, so rclone fails instead of prompting; no
  rclone process ever reads a password from stdin, which for `rcat` is the archive.
- **Temporary directory.** Every rclone command runs with `--temp-dir <state>/rclone-tmp`, so the
  tool, not the environment (`TMPDIR`, rclone's own defaults), decides where rclone may spool. The
  directory is emptied at the start of every non-dry apply. Its volume is the state volume, which
  section 18 checks before each item and part.
- At start, apply finds the configuration file with `rclone config file` and reads its first lines.
  An encrypted rclone configuration is refused (exit 1, "encrypted rclone configuration is not
  supported in this release"); the tool never asks for, reads or passes its password.
- The storage remote's type comes from `rclone listremotes --long`; wrappers (`alias`, `union`,
  `combine`, `chunker`, `compress`, `hasher`, `crypt`) are resolved recursively with
  `rclone config redacted NAME`, reading only `type`, `remote` and `upstreams`. The remote counts
  as encrypted only when every path through the wrappers reaches a `crypt` layer (for `union` and
  `combine`, every upstream). The tool never runs `rclone config show` or `rclone config dump` (they
  print credentials).
- rclone's stderr is passed through `redact_text` before it reaches any output, journal line or
  record.

## 10. Journal, resume, concurrency

### 10.1 Journal and resume

- `<state>/journal/<run_id>.jsonl` is append-only. Each line is one transition of one item
  (`selected`, `rechecked`, `answered`, `attempt-started`, `part-uploaded`, `part-verified`,
  `index-verified`, `segment-reserved`, `recorded`, `segment-synced`, `checked-again`,
  `quarantine-intent`, `quarantined`, `delete-started`, `deleted`, `removed-recorded`, `skipped`,
  `failed`, `kept`, `vanished`) with the attempt, object names, sizes and digests. Each line is
  flushed and `fsync`ed before the step it records takes effect; renames are followed by an `fsync`
  of the parent directory where the platform supports it.
- `segment-reserved` holds the next manifest segment number and is synced before that segment name
  is used, so a segment name is never reused, also across a crash.
- The header records the plan and scan SHA-256, the storage, the flags and the build.
- `apply --resume RUN_ID` continues each unfinished item from its last durable state; a resume of a
  `--backup-only` run never deletes. Every `--remove` apply also does this for the quarantines of
  all unfinished runs; other modes only report them (6.1).

In the table, **O** is the original path and **Q** the quarantine path. "Present" for O means a
directory is there; a resume never touches a directory at O that is not the item (same device, inode
and mount id as F0). **Equal** means the fingerprint of Q equals the index exactly. **Subset** means:
every entry in Q has a relative path that is in the index, with the same kind; a regular file or
symlink also has the same size, modification time, inode and (symlink) target; a directory is
compared by path and kind only, because deleting its children changes its times and link count and
13.2 may change its mode; and Q's device and mount id equal F0's.

| Last state | O | Q | On resume |
|---|---|---|---|
| Archived item: before `segment-synced`. Unarchived regenerable item: any state before `quarantine-intent` | any | absent | Start the item again from step 2 with the next attempt. Earlier objects are orphans. O absent: skipped `gone`. |
| Archived item: `segment-synced` or `checked-again` | present | absent | Step 2 again (F0 must equal the index), then steps 7 to 11. |
| Archived item: `segment-synced` or `checked-again` | absent | absent | Skipped `gone`; the `backed-up` record stays valid for restore. |
| `quarantine-intent` | present | absent | The rename never happened: as `segment-synced`. |
| `quarantine-intent` or `quarantined` | absent | present | Fingerprint Q: equal, continue at step 10; otherwise rename it back to O and record `kept` with the difference. |
| `quarantine-intent` or `quarantined` | present | present | Touch neither; `failed`; report both paths. |
| `quarantined` | present | absent | Touch nothing; `failed` ("quarantine missing, a directory is at the original path"). |
| `quarantine-intent` or `quarantined` | absent | absent | Something outside the tool removed the item. Archived item: write a `vanished` record (the archive is the copy; restore works); `failed` in the summary. Unarchived regenerable item: skipped `gone`, no record. Never `removed`, never `kept`. |
| `delete-started` | any | present | Fingerprint Q: subset, continue deleting Q; otherwise stop, leave Q as it is, `failed`, and report Q (entries not in the index are not in the archive). A directory at O is never touched. |
| `delete-started` | any | absent | Deletion finished before `deleted` was written: write `deleted`, then the `removed` record. A directory at O is new and is not touched; the summary names it. |
| `deleted` | any | absent | Write and sync the `removed` record if no synced segment has it. |
| `removed-recorded` | any | absent | Sync the segment holding the record if it is not synced. |

- Records are identified by `(item_id, run_id, outcome)`. When storage holds two records with one
  identity (a lost response on a segment upload, then a retry), readers use the latest record from a
  verified segment, and `manifest list` shows one.
- Applying a finished plan again changes nothing and reports items as already done.
- Ctrl-C finishes the current journal write, stops before the next step, and exits 3 with the run
  id. A second Ctrl-C exits at once; the journal stays valid.

### 10.2 Concurrency

One apply per user per machine (D-11): an OS advisory lock on `<data dir>/disk-cleaner/apply.lock`
(the path from the user database, also when `--state-dir` points elsewhere) and a second one on
`<state>/lock`. Both are released by the operating system when the process ends. The file holds the
run id and process id for the message; its content is never trusted as the lock. A second apply
exits 1 at once naming the holder. A dry run takes no lock. Applies by different users, or by two
machines sharing a network home, are not covered (section 17); the no-replace rename and the F0
comparison make the loser skip the item.

## 11. Ask mode and automatic mode

- **Ask** is the default. On a terminal, each selected item is shown after its re-check: path,
  action, size, why the plan says it is safe, what will be archived (files, bytes), the storage and
  whether it is encrypted, and secret-like files found. For a git item the prompt also says that the
  remote facts come from the scan and were not checked against the remote, and that **the archive
  will be the only copy apply has verified**; for `recover-backup-then-remove` it lists the unsaved
  work the plan found (unpushed commits, stashes, uncommitted, untracked and ignored files, by count)
  that exists only in the item and the archive; and it lists the regenerable children that go with
  it (6.3) and their bytes. Answers: `y` do it, `n` keep, `b` back up then
  remove (regenerable only), `s` back up and keep, `a` yes to the rest of this class, `q` stop (no
  further item is started). Pending typed-ahead input is discarded before each prompt. End of input
  means `n`. Without a terminal on stdin, every item is `n`; nothing waits.
- The answer binds to F0; any difference at step 9 keeps the item.
- **Automatic** applies to the classes named by `--auto` or `[apply] auto`; default none. Automatic
  items are re-checked and skipped on any difference.
- A dry run never prompts.

## 12. Manifest

### 12.1 Records

One JSON object per line. Every record has `record_id`, `item_id`, `run_id`, `machine_id`, `at`,
`path`, `class`, `action` (the plan's action), `outcome` and `tool_version`. Outcomes: `backed-up`
(written before removal), `removed`, `backed-up-kept` (with the reason), `regenerable-removed`,
`vanished` (the item disappeared after its backup and before apply removed it). Where they apply:

- `bytes`, `files`, `xattrs_dropped`; the scan facts used, labelled `unverified` where the scan
  labels them so;
- `executed_as`: `whole-archive` for both git actions in this release, so the plan's distinction
  (`action`) and what was done are both visible;
- `git`: HEAD, branches and tags with their ids, the remote names and redacted URLs from the local
  configuration (informational; not checked against the remote);
- `archive`: `format` (`pax-tar-v1`), `parts` (object, bytes, SHA-256), `part_size`, `bytes`,
  `sha256` of the whole stream, `index` (object, bytes, SHA-256), `encrypted_remote` (true or
  false), `members` (one item path in this release);
- `nested_regenerable`: the paths of regenerable children archived and removed with a git item
  (6.3);
- `secrets_uploaded`: the count of secret-like files archived under `--upload-secrets` (no names);
- `restore`: the method (`archive` or `regenerable`) and the exact `disk-cleaner restore
  <record_id>` command.

URLs pass core's `strip_url_credentials`; error text passes `redact_text`. No record holds a
secret.

### 12.2 Where it lives

- Locally: `<state>/manifest/<run_id>.<seq>.jsonl`, one **segment** per sync, never modified after
  it is written, ending with a trailer line holding the record count and the SHA-256 of the bytes
  before it. `seq` comes from a `segment-reserved` journal line (10.1).
- In storage: the same segment at `disk-cleaner/v1/<machine id>/manifest/<run_id>.<seq>.jsonl`,
  uploaded with rcat, read back and compared like a part. A name is never reused. `machine.json` is
  uploaded once; if one exists there with another id, apply stops.
- **No item is removed until a verified copy of its `backed-up` record is in storage.**

### 12.3 Commands

- `manifest list` prints records newest first from the local segments; with `--storage` (and
  `--machine` when the storage holds several machines) from storage only, so it works on a new
  machine. A segment whose trailer does not match is reported as damaged and its records shown as
  unverified.
- `manifest find TEXT` matches paths, record ids, item ids and archive names; `--files` also searches
  the indexes (local copies, or downloaded when absent).
- `manifest verify` lists every object the records reference and checks that it exists with the
  recorded size (`lsjson`), so a lifecycle rule or a manual deletion is noticed; `--deep` also reads
  each object back and compares its SHA-256.
- `manifest orphans` lists objects under the machine's storage path that no record references. It
  deletes nothing.

## 13. Removal and restore

### 13.1 Before removal

Removal (steps 7 to 10) happens only after, in order: the re-check passed; every part and the index
object are verified; the manifest segment with the `backed-up` record is verified in storage; and
the second existence check passed. No remote fact is a condition.

### 13.2 Walking and deleting (macOS and Linux)

- Archiving, fingerprinting and deletion all work relative to open directory handles: the item is
  opened once with `O_DIRECTORY|O_NOFOLLOW`, every child with `openat(..., O_NOFOLLOW)` and
  `fstatat(..., AT_SYMLINK_NOFOLLOW)`, every removal with `unlinkat`. No path is resolved a second
  time, so swapping a directory for a symlink during the run cannot redirect a read or a delete.
- A symlink is removed as a link, never followed. A directory whose device or mount id differs from
  the item's, or that is a mount root, is never entered: deletion stops, leaves the rest in
  quarantine and reports the item `failed` (the data is in the verified archive).
- The quarantine rename uses `renameat2(RENAME_NOREPLACE)` on Linux and `renamex_np(RENAME_EXCL)`
  on macOS; where the call is unavailable, removal is refused.
- Read-only entries are made writable only inside the quarantine. Removal never runs `git` and never
  touches anything outside the quarantine.
- **Windows**: `--remove` is refused (D-13). Archiving on Windows opens every entry by handle with
  `FILE_FLAG_OPEN_REPARSE_POINT` and never follows a reparse point.

### 13.3 Restore

`disk-cleaner restore RECORD_ID [--to DIR]`:

1. Finds the record (local manifest, or `--storage` alone on a new machine).
2. The target is the original path or `--to`. It must not exist. Free space on its volume must be
   at least the record's logical `bytes` plus 1 GiB, and extraction stops if the stream
   exceeds it.
3. Each part is streamed with `rclone cat` into a hasher and a tar reader that extracts into
   `<target parent>/.disk-cleaner-restoring-<record_id>`. Every entry is created with create-new
   semantics; "already exists" fails the restore. Extraction refuses absolute names, `..`
   components, names invalid on the target file system, writes through a symlink (symlinks are
   created after all files), hard links to anything outside the staging directory, and special
   files. On any digest mismatch the staging directory is deleted and nothing is created at the
   target.
4. Names that collide on the target file system (case, or Unicode normalisation: NFC and NFD forms
   of one name) are refused before extraction, naming them.
5. **Metadata restored**, in this order once all contents are written: hard links inside the item
   as links to the first name; extended attributes; modification times of files and symlinks (with
   `utimensat`/`AT_SYMLINK_NOFOLLOW`, nanoseconds where the file system keeps them); user file flags
   on macOS (`UF_*`, such as hidden); then mode bits and modification times of directories,
   deepest first. **Not restored**: access and change times, inode numbers, owners and groups,
   ACLs, system file flags (`SF_*`; an item with immutable flags is never removed, 7.4), Linux
   `security.selinux` (9.3), and sparseness (section 17).
6. Every entry is compared with the index: kind, size, SHA-256, symlink target, mode bits,
   modification time (to the target file system's precision), hard-link structure (which names
   share one file) and extended attributes (where the target supports them; otherwise the restore
   lists the attributes it could not set and exits 2).
7. The staging directory is renamed to the target with no-replace semantics. A failure at any step
   leaves nothing at the target.

Restore never runs `git` and never contacts a git remote. `regenerable` records have nothing to
restore: restore prints the rule and how the directory is normally rebuilt, and exits 0. A
`vanished` record restores like a `removed` one.

## 14. Platform differences

| | macOS | Linux | Windows |
|---|---|---|---|
| Dry run, `--backup-only`, restore | yes | yes | yes |
| `--remove` | yes | yes | refused (D-13) |
| Mount detection | `st_dev`, `statfs` | `statx` mount root and mount id, else `/proc/self/mountinfo`; refuse if neither | volume serial and reparse points (backup only) |
| Placeholders | `SF_DATALESS` | none expected (FUSE is a refused network file system) | recall, offline, pinned attributes; cloud-files reparse points |
| Extended data | xattrs and resource forks archived | xattrs archived | ADS other than `Zone.Identifier`: item unsupported |
| Long paths | n/a | n/a | verbatim `\\?\` paths, tested above 260 characters |
| Case | APFS may be case-insensitive: restore refuses collisions | case-sensitive | case-insensitive: restore refuses collisions |

Process checks use `sysinfo` on all three. On Windows, symlink restore needs Developer Mode or the
privilege; otherwise restore fails and says so.

## 15. Core changes required (additive only)

- **C-1** withdrawn in revision 5: no network runner is needed (section 8).
- **C-2 (D-2)** A plan field `scan_sha256`, written by `plan` and read by apply.
- **C-3** An item re-scan and a project activity function: `rescan_item(path, &ScanOptions)` walks
  only `path`, decides its rule from the markers in its parent directory (by name, `lstat` only) and
  applies the enclosing repository's ignore check to `path` itself, returning the same nodes a full
  scan would give for that subtree; `project_activity(node_path)` returns the newest modification
  time excluding `.git` and nested nodes, and the newest commit time. A test shows that
  `rescan_item(proj/node_modules)` yields one `regenerable` node equal to the full scan's.
- **C-4 (wording only)** The plan text the user approves must not promise a check this release does
  not make. No field, action, rule or schema version changes; only these strings (core at the main
  merge, 537a426):
  - `crates/core/src/plan.rs`, `remote_facts_note`: "remote facts come from local remote-tracking
    refs, which can be stale; they are unverified until apply re-checks them with git ls-remote"
    becomes "...which can be stale; they are unverified, and apply does not check them: it archives
    the whole repository to storage and verifies the archive before removing anything";
  - `plan.rs`, the `backup-then-remove` reason: "(unverified until apply re-checks the remote with
    git ls-remote)" becomes "(unverified; apply archives the whole repository before removing it)";
  - `plan.rs`, the tags reason: "they will be verified when a removal is applied" becomes "they are
    not checked; a removal archives the whole repository first";
  - `plan.rs`, the doc comment on `remote_facts_verified` ("Apply must re-check them with
    `git ls-remote` before removing anything"): the same correction;
  - `crates/cli/src/report.rs`, the subtree scope line "and apply re-verifies" becomes "and apply
    does not look for them either";
  - `docs/SCAN-FORMAT.md`: the `scope` row ("the apply phase re-verifies"), the `ready_bytes`
    paragraph ("unverified until apply re-checks them"), the paragraph on stale remote-tracking
    refs, the global `insteadOf` note, and the `all_branches_on_remote` row, each changed to say
    the first apply release does not contact remotes and archives the whole item instead.

  Tests that compare these strings change with them. C-4 belongs to the core owner; until it lands,
  apply prints the correction in the header of every run that selects a git item.

No scan or plan safety rule changes.

## 16. Open decisions

The text above follows the recommended option. D-4 changes what the user approved earlier and needs
the user's own decision. D-1 departs from the user's stated requirement (section 2,
TARGET_MISMATCH) and is reported to the user as such.

- **D-1. Network access for git: none in this release.** ASSUMPTION: the coordinator's choice after
  the review of revision 4, not the user's. Removal rests on the verified whole-item archive alone;
  pushing recovery refs and the live remote check wait for the entry conditions of section 8.
  Cost: unsaved commits are recoverable from the archive only, not from the git remote; the
  requirement to push recovery branches is not met.
- **D-2. Binding plan and scan.** Recommended: C-2; until then, `generated_at` plus per-item path and
  class (7.1).
- **D-3. Non-git items and `ask` items.** Recommended: apply acts only on removing actions;
  outputs, loose code and sessions wait for a core plan action (reported as deferred against the
  requirements, section 2).
- **D-4. Secrets.** Recommended: no local vault; an item holding a secret-like file is kept unless
  `--upload-secrets`, with an unencrypted-storage warning and `--require-encrypted-remote`. Why: a
  same-disk copy is not a backup, a vault adds key or file management that can lose the only copy,
  and keeping the item loses nothing. Cost: items with `.env` files, with a secret-like name in a
  stash or local-only commit, or with an untracked file the user's global excludes match, are not
  reclaimed until the user opts in; the summary counts them by rule. Alternative: client-side
  encryption of secret-like files with a key kept in the OS keychain and shown once, which adds a crypto dependency, key loss as a new way to lose data, and a
  restore that needs the key. The user earlier approved a local vault for a private script; this
  drops it, so the user should confirm.
- **D-5. Recovery refs.** Moot in this release: nothing is pushed (D-1, section 8).
- **D-6. rclone command line**, not the rc API that DESIGN.md named: `rcat --size`, `cat`, `lsjson`
  behind a `Store` trait; minimum rclone version 1.64, checked at start.
- **D-7. Full read-back is mandatory.** The dry-run summary estimates the egress.
- **D-8. Permanent deletion**, only under 13.2 (handle-relative, mount-aware, no sync folders);
  `manifest verify` notices objects lost later.
- **D-9. Plan age** 24 hours, checked per item.
- **D-10. Automatic mode per class**, with `repo-recover` separate from `repo` although both execute
  identically.
- **D-11. One apply per user per machine**, lock path from the user database, root refused unless
  `--allow-root`.
- **D-12. Whole-item archive** for every removed git item (more storage than the preserve sets
  alone, no reliance on git's clean judgement or on remote facts).
- **D-13. Windows is backup-only and restore** in this release.
- **D-14. Free-space floor** a fixed `--min-free` (default 1 GiB) and a part size derived from free
  space (section 18), so apply runs on a nearly full disk, which is the case it exists for.
- **D-15. Regenerable children of a git item are archived with it** (6.3), not removed first
  without a backup: the archive is then truly whole, at the cost of uploading rebuildable bytes.

## 17. Documented limits

- For a removed git item, the verified archive is the only copy apply relies on. Unsaved commits,
  stashes and uncommitted work exist afterwards only in the archive; remote facts were not checked.
  The user's storage is therefore the single point of recovery, and `manifest verify` should be run
  after any change to its lifecycle rules.
- The apply-time live remote check that SCAN-FORMAT.md describes is not performed (section 8).
- A process can still write into an item through a file it opened before the quarantine rename;
  writes after the final comparison and before deletion are lost. Only working directories are
  checked, not open files.
- Dependents outside the item are as fresh as the scan (bounded by the plan age); references from
  outside the scanned roots were never seen (SCAN-FORMAT.md, known limits). Two known core gaps,
  open as core follow-ups, are not relied on to be fixed: reference entry names are matched exactly,
  so a case-variant spelling on a case-insensitive file system is not seen as a dependency; and
  core's dependency tests do not run on Windows. Apply adds no dependency check of its own; on
  Windows this matters less because Windows is backup-only (D-13).
- Secret detection is best effort (9.2): by name and a bounded content check. Anything it misses is
  uploaded to the user's own storage, unencrypted unless the remote is `crypt`. **Secrets in history
  that a network remote already holds** (reachable from its tracking refs) are archived as they are;
  they were already on that remote. A secret in local-only history whose blob is past the 64 MiB
  content budget, or whose only name is not a secret-like name, is caught only by name, or not at
  all. A `core.excludesFile` set inside an `includeIf` block is not seen.
- ACLs, owners and groups are not archived; mode bits are restored on Unix only; access and change
  times, system file flags and SELinux labels are not restored (13.3). Sparse files are restored
  dense. A hard link to a file outside the item is restored as an independent file.
- Sync-folder detection covers the providers and markers listed in 7.4; another sync client, or a
  Seafile library outside `~/Seafile`, is not recognised. macOS `~/Desktop` and `~/Documents` are
  always treated as synced.
- Regenerable children of a git item are archived with it (6.3, D-15). A `remove-regenerable` item
  removed on its own without `--backup-regenerable` has no archive: a file edited by hand inside it
  (a patched `node_modules` package) is lost, as the plan's regenerable rule accepts.
- Verification proves the stored object equalled what was sent at the time; `manifest verify` notices
  later loss, it cannot prevent it.
- The lock does not cover two users, or two machines sharing a network home.
- Whether `rclone rcat --size` avoids a local spool is tested for the `local` backend only (9.4);
  for other backends the part size is bounded by the state volume's free space instead.
- An encrypted rclone configuration is refused (9.6).

## 18. Resource limits

- Archives are streamed; local disk use is the journal, the manifest segments and the index copies.
- **Floor.** Before each item, and before each part, apply checks free space on the state
  directory's volume, which also holds rclone's temporary directory (9.6). `--min-free` (default
  1 GiB, a fixed amount, not a share of the volume) must remain free there.
- **Part size.** At the start of each attempt the part size is
  `min(--part-size, (free on the state volume - min-free) / 2)`, rounded down to a MiB, with
  `--part-size` defaulting to 1 GiB. Below 64 MiB the item is not started ("not enough free space
  on VOLUME"). The part size is journaled with the attempt (9.4); a later attempt may choose a
  different one. Worked examples, with the default 1 GiB floor: a 500 GiB volume 97% full (15 GiB
  free) uses 1 GiB parts; 1.6 GiB free uses 307 MiB parts; 1.1 GiB free stops before the item.
- A part during which the state volume falls below the floor fails the attempt (fail closed);
  the next attempt sizes its parts again.
- Memory is bounded by fixed buffers (1 MiB per stream); nothing holds a whole file in memory.
- One item at a time; one rclone process at a time.
- Bytes freed are measured as the change in free space, alongside the sum of sizes.

## 19. Exit codes and output

- 0: dry run completed, or every selected item ended removed, `backed-up-kept` by request
  (`--backup-only`, answer `s`), answered `n`, or was already done.
- 1: refused before acting (usage, inputs not bound, plan too old, lock held, root without
  `--allow-root`, `--remove` on Windows, rclone or git missing or too old, an encrypted rclone
  configuration, storage unusable, `--require-encrypted-remote` with a plain remote).
- 2: completed, but at least one item was skipped, failed, vanished, or ended `backed-up-kept` for
  any reason other than a request.
- 3: interrupted; the run id is printed for `--resume`.

The summary lists every item with its outcome and reason, bytes freed (measured and summed), bytes
uploaded, whether the storage is encrypted, and where the manifest is. `--json` writes the same as
JSON. No output, journal line or record holds a secret value or a credential URL.

## 20. Acceptance criteria

Tests live in `crates/apply/tests/` and run in CI on ubuntu, macOS and Windows. Unless stated, they
use temporary directories, the in-process `LocalDirStore` or `FaultStore` (a wrapper that injects
partial writes, flipped bytes on read, success reported as failure, objects deleted after
verification, stalls), and local git repositories. No test contacts a network host. A separate job
runs the flows through the real `rclone` binary against an rclone `local` remote defined in a
temporary config file (no credentials anywhere). A library step hook lets a test act between any
two steps; it exists only in test builds (AC-35).

- **AC-1** Dry run is the default: without `--backup-only` or `--remove`, apply prints what it would
  do with the fresh re-check result and writes nothing: no state directory, no storage object, no
  file anywhere, and runs no git command outside 7.5. Verify by: `dry_run.rs` snapshots the tree,
  state path and store before and after; zero store writes.
- **AC-2** `--backup-only` does steps 1 to 6 and removes nothing, also when an earlier run left a
  quarantine or a pending deletion (reported only, 6.1), and so does a resume of a `--backup-only`
  run; `--remove` does the whole sequence on macOS and Linux and is refused (exit 1, nothing done)
  on Windows. Verify by: `modes.rs` in the three-OS matrix, with a stranded quarantine from an
  earlier `--remove` run present.
- **AC-3** Plan and scan are bound (7.1): schema, scan identity, per-item path and class, no
  duplicate items, no future dates; the plan age is enforced at start and before each item. Verify
  by: `inputs.rs` with a clock injected through the library.
- **AC-4** Only removing actions are acted on, and only when a fresh plan of the item gives the same
  action. Verify by: `inputs.rs` with `ask` and `keep` items edited into removing actions.
- **AC-5** The item is re-scanned with core's item re-scan and skipped, naming the first difference,
  when anything in 7.2 differs; a regenerable item is classified by its parent's markers. Verify by:
  `recheck.rs`, one case per field of 7.2, and a core test that `rescan_item(proj/node_modules)`
  equals the full scan's node (C-3).
- **AC-6** The process check runs at step 2 and step 8; a process working inside the item skips it.
  Verify by: `recheck.rs` starts a child process inside the item at each point.
- **AC-7** Items in the places of 7.4 are skipped: sync folders (each listed marker), placeholders,
  mount roots including a same-file-system bind mount of a directory or a single file, file systems
  outside the allowlist, the tool's state or local storage, immutable entries. Verify by:
  `places.rs` with markers and flags created in temporary directories and file system types
  injected through the library's metadata interface; the Linux mount job with a tmpfs, a directory
  bind mount, a file bind mount and a vfat loop mount; macOS
  `SF_DATALESS` simulated through the library's metadata interface (setting it needs a file
  provider) plus a unit test of the flag check.
- **AC-8** The fingerprint chain of 6.4 holds: any change between step 2 and step 9 skips the item
  or, at quarantine, renames it back and records it. Verify by: `toctou.rs` writes, deletes, touches,
  renames and swaps entries through the step hook between every pair of steps.
- **AC-9** No git command apply runs can reach the network: every invocation goes through core's
  hardened runner with `protocol.allow=never`, and its subcommand is within the set of 7.5. Verify
  by: `offline.rs` puts a logging git shim first on `PATH` for full apply, resume and restore runs
  and asserts the subcommand set and the `protocol.allow=never` argument on every call; a local
  test server that counts connections sees none.
- **AC-10** Removal of a git item depends only on the verified whole-item archive: both git actions
  run the same steps, the record shows the plan's `action` and `executed_as: whole-archive`, and
  remote facts are recorded as informational. Verify by: `invariant.rs` runs `backup-then-remove`
  and `recover-backup-then-remove` fixtures and compares their store, journal and file-system
  operation logs (equal apart from names), and checks the records.
- **AC-11** Every removed item is archived whole (9.1), and both preserve sets, enumerated at apply
  time with `git_admin_files` and `object_store_files`, are inside the archive. Verify by:
  `archive.rs` extracts the archive and compares it entry by entry with a snapshot of the fixture,
  and checks the two core lists against the index.
- **AC-12** Secret-like files (9.2) keep their item by default, by each of the five rules (name,
  local-only history including stashes and reflogs, global excludes with includes, content, git
  directory URLs); `--upload-secrets` archives them; a non-`crypt` remote is warned about and
  `--require-encrypted-remote` refuses it. Verify by: `secrets.rs` plants fake values for each rule
  (global-excludes matches set through a fake account home) and searches every stored object,
  segment, journal line and output for them; the search extracts each archive, and inside every
  extracted git directory inflates every loose object and every packed object (with
  `git cat-file --batch-all-objects --batch` run on the test's own copy), so a value held only in
  the object store is found. A control run with `--upload-secrets` must find each value in the
  inflated objects, which proves the search looks there.
- **AC-13** The archive is a deterministic pax tar as in 9.3, with extended attributes recorded and
  the listed harmless ones dropped, split into parts whose length is known before upload, with a
  self-describing index. Verify by: `archive.rs` archives a tree twice with different part sizes and
  compares the concatenated streams byte for byte, and checks long and non-UTF-8 names, symlinks,
  internal hard links, xattrs and a resource fork (macOS), the Linux namespace rules of 9.3, and a
  part size small enough to split a file.
- **AC-14** A part counts as verified only after `rcat --size` succeeded, the read-back length and
  SHA-256 equal what was sent, and the listing shows that size; names are never reused; at most 3
  attempts. Verify by: `upload.rs` with `FaultStore`, and `rclone_local.rs` with the real rclone
  binary, which also checks the `--size` argument.
- **AC-15** The index object is uploaded and verified like a part. Verify by: `upload.rs`.
- **AC-16** Records carry the fields of 12.1, segments are immutable with a valid trailer and are
  mirrored with read-back verification, and duplicate records collapse by identity. Verify by:
  `manifest.rs`.
- **AC-17** Nothing is removed before every condition of 13.1 holds, in that order. Verify by:
  `ordering.rs` records store, journal and file-system operations and fails on any removal that
  precedes a required step.
- **AC-18** `manifest list`, `find` (with `--files`), `verify` (with `--deep`) and `orphans` work
  from local state and from storage alone. Verify by: `manifest.rs` with an empty state directory.
- **AC-19** Walking and deletion follow 13.2: handle-relative, no symlink followed, no mount
  entered, no-replace rename, nothing outside the quarantine touched. Verify by: `remove.rs` (a
  symlink to an outside directory; a subdirectory swapped for such a symlink during archiving and
  during deletion through the step hook; a read-only tree; an occupied rename target) and the Linux
  bind-mount job.
- **AC-20** A `remove-regenerable` item is removed only while a fresh plan still says so, with no
  backup unless asked. Verify by: `regenerable.rs`.
- **AC-21** Every transition is journaled before it takes effect; every row of the resume table in
  10.1 leads to its stated outcome; every apply first reconciles unfinished quarantines and unsynced
  records; a resume refuses changed inputs. Verify by: `resume.rs` stops at each transition in turn,
  including between the last `unlinkat` and the `deleted` line, and resumes in a new instance;
  builds each O and Q combination of the table by hand; starts a new apply instead of resuming; and
  kills the real binary during an rclone upload.
- **AC-22** Applying a finished plan again changes nothing and reports items as done. Verify by:
  `resume.rs`.
- **AC-23** One apply per user per machine (10.2), the lock path independent of `HOME` and
  `XDG_*`, no stale lock after a kill, root refused without `--allow-root`. Verify by: `lock.rs`
  with two processes under different environments and state directories, and a killed holder.
- **AC-24** Free space and budgets follow section 18 and 9.5: the floor is checked on the state
  volume (which holds rclone's temporary directory) before each item and part, the part size is
  derived from free space and journaled per attempt, the upload budget uses logical size, and
  three consecutive storage failures stop the backup stage. A 500 GiB volume 97% full runs with 1 GiB parts. rclone's `local`
  backend does not spool. Verify by: `limits.rs` with an injected free-space reader and `FaultStore`
  (the worked examples of section 18 as cases); and `rclone_spool.rs` in the Linux job, which runs
  the real rclone `rcat --size` of a 256 MiB part to a `local` remote with the state directory, and
  so `--temp-dir`, on a 64 MiB tmpfs, and checks the tmpfs never fills.
- **AC-25** Ask mode follows 11: the prompt's contents (including the only-verified-copy line for
  git items), typed-ahead input discarded, no terminal or end of input means keep, `q` stops before
  the next item. Verify by: `ask.rs` through the library's prompt interface and the binary through a
  pipe.
- **AC-26** Automatic mode runs only the classes named by `--auto` or `[apply] auto`; none by
  default. Verify by: `ask.rs`.
- **AC-27** Restore follows 13.3, and the restored item equals the original fixture snapshot taken
  before removal, entry for entry: contents, attributes, modification times, mode bits, macOS user
  flags and hard-link structure; what 13.3 does not restore is listed, not silently dropped. Verify
  by: `restore.rs` round trips for each action and for a `vanished` record.
- **AC-28** Restore works on a new machine from storage alone. Verify by: `restore.rs` with an empty
  state directory and `--storage`.
- **AC-29** Nested items follow 6.3, including `--backup-only` and a nested git item; regenerable
  children of a git item are archived and removed with it, not before it. Verify by: `nested.rs` (a
  repository holding a removable `node_modules`: removed with the repository and present in its
  archive; the repository skipped, then `node_modules` handled on its own; `node_modules` outside
  `--class`; backup-only; a nested repository).
- **AC-30** Platform behaviour of section 14: Windows backup and restore with paths over 260
  characters, read-only files, case and normalisation collisions refused on restore. Verify by:
  `platform.rs` in the three-OS matrix.
- **AC-31** Exit codes and the summary follow section 19; no secret value or credential URL appears
  in any output, journal or record. Verify by: `output.rs` with a remote URL holding a fake token.
- **AC-32** No program named by scanned content runs during re-check, archive, removal or restore.
  Verify by: `hardening.rs` with hooks, a `.gitattributes` filter, an fsmonitor setting and config
  keys naming programs, each writing a marker file.
- **AC-33** rclone is used as 9.6 states: minimum version checked, `--ask-password=false` and
  `--temp-dir` on every call, an encrypted configuration refused at start, wrappers resolved with
  `config redacted` only (encrypted only when every path reaches `crypt`), stderr redacted. Verify
  by: `rclone_cli.rs` with a fake rclone that logs its arguments and stdin reads, and the real rclone job with a temporary config file that carries rclone's
  encrypted-configuration marker over random bytes (no password exists for it).
- **AC-34** The attempt number, part size and object names (`attempt-started`) and the manifest
  segment number (`segment-reserved`) are durable in the journal before any name is used, so no
  object or segment name is ever written twice. Verify by: `resume.rs` stops between each of those
  lines and the first store write, resumes, and `LocalDirStore` (which fails any write to an
  existing name) records zero refused writes; a lost response then a crash uses the next attempt.
- **AC-35** The step hook is compiled out of release builds. Verify by: the hook exists only under
  a test-only cargo feature; the release CI job builds without it, asserts that `cargo tree -e
  features` for the release binary does not include it, and checks that the hook's marker string is
  absent from the release binary.
- **AC-36** The click-test of section 24 passes on the PR's release artifacts, run by a tester who
  is not the author or a reviewer. Verify by: the tester's report attached with `spec_flow.py
  click-test` to the ticket, covering every flow of section 24.
- **AC-37** The plan text the user approves promises no apply-time remote check (C-4); until C-4
  lands, every apply run that selects a git item prints the correction in its header. Verify by: a
  core test that plan output and `SCAN-FORMAT.md` contain none of the strings C-4 lists; meanwhile
  `output.rs` checks the header line.

## 21. Negative cases

- **NEG-1 (AC-1)** A dry run with unreachable storage still completes with zero store calls and no
  prompt.
- **NEG-2 (AC-2)** `--backup-only --remove` together: usage error. `--remove` on Windows: exit 1,
  nothing touched.
- **NEG-3 (AC-3)** A plan from another scan; a plan older than the limit; a plan that ages past the
  limit during a run (the remaining items are not started); a duplicate item; a plan dated in the
  future: refused as stated, nothing done to the refused items.
- **NEG-4 (AC-4)** A plan edited from `ask` or `keep` to `backup-then-remove`: skipped.
- **NEG-5 (AC-5)** After the scan: a new file; a new commit; a new branch or tag; a new stash; a new
  hook; `git remote set-url`; the item replaced by a symlink or a file; the item deleted (reported
  `gone`, not an error, and never when a quarantine with its id exists). Each is skipped with its
  reason.
- **NEG-6 (AC-6)** A process that starts inside the item before the quarantine: skipped. A process
  check that can read nothing: every item skipped.
- **NEG-7 (AC-7)** An item under each sync-folder marker; a dataless or recall-on-access file; a
  tmpfs and a bind mount inside the item; an item on NFS or FUSE; an item containing the state
  directory or a local storage path (also through an `alias` remote); an immutable file: each
  skipped, nothing uploaded counts and nothing removed.
- **NEG-8 (AC-8)** A file written, or a clean tracked file edited, between any two steps from 2 to
  9: the item is not removed.
- **NEG-9 (AC-9)** A repository whose config has `url.*.insteadOf` pointing at the local counting
  server, a remote URL on that server, and a partial clone whose promisor remote is missing objects:
  no connection is counted, no object is fetched, and no subcommand outside 7.5 is logged.
- **NEG-10 (AC-10)** A `backup-then-remove` repository whose scan facts say every commit is on the
  remote, with the upload made to fail: the item is kept; nothing is removed on the strength of
  remote facts.
- **NEG-11 (AC-11)** A git directory file that cannot be read, a name core cannot enumerate, a fifo
  in the item, a preserve-set file missing from the index: skipped before any upload counts.
- **NEG-12 (AC-12)** `.env`, `id_ecdsa`, a file matched only by the global excludes file, an
  `unusual.txt` holding a private key block, and an https URL that carries a user name and token
  before the host in `.git/config`, `FETCH_HEAD` and `logs/HEAD`: the item is kept by default and
  none of the values appears in any stored object, segment, journal line or output; with
  `--upload-secrets` they are archived; with `--require-encrypted-remote` and a plain remote, exit 1
  before any upload. A `.DS_Store` matched only by the global excludes file does not keep the item;
  a `node_modules` ignored by the repository's own rules and also by the global file is not flagged.
- **NEG-13 (AC-13)** An xattr over 64 KiB, or a Windows ADS other than `Zone.Identifier`: item
  unsupported, skipped.
- **NEG-14 (AC-14)** The store fails mid-part; one byte differs on read-back; the read-back is short;
  the upload succeeds but the tool sees a failure (lost response); the transfer stalls: never
  verified, new attempt with new names, at most 3, orphans listed, nothing removed.
- **NEG-15 (AC-17)** The segment upload fails: backed up and kept. A part deleted from storage
  between verification and the second existence check: skipped, nothing removed.
- **NEG-16 (AC-18)** A truncated or altered segment in storage: reported damaged, its records
  unverified; `manifest verify` reports an object deleted after verification.
- **NEG-17 (AC-19)** A subdirectory swapped for a symlink to an outside directory during archiving:
  no outside file is read (none appears in any stored object). The same swap during deletion: the
  outside directory is unchanged. An occupied quarantine name: the rename fails, nothing replaced.
- **NEG-18 (AC-20)** A regenerable directory that gained a `.git`, an output file or a `.env`:
  skipped.
- **NEG-19 (AC-21)** A run stopped at each transition and resumed, or followed by a new apply without
  `--resume`: never an item missing from disk without a verified archive and a synced `backed-up`
  record; never two records for one identity in `manifest list`; every quarantine completed, renamed
  back or reported. A resume with a different plan, scan or flags: refused. Both O and Q absent
  before `delete-started`: a `vanished` record, not `removed` or `kept`. Q gained a new file after
  `delete-started`: Q is left as it is and reported, nothing at O touched. A new directory at O
  after `delete-started`: untouched.
- **NEG-20 (AC-23)** Two applies at once with different `HOME`, `XDG_DATA_HOME` and `--state-dir`:
  the second exits 1. A killed holder: the next apply starts. Run as root without `--allow-root`:
  exit 1.
- **NEG-21 (AC-24)** 1.1 GiB free on the state volume: the item is not started, remaining
  items listed. The volume falls below the floor during a part: the attempt fails, nothing removed.
  The budget reached: stops before the next item. Two storage failures, a verified item, then two
  more: the backup stage continues; three in a row: it stops.
- **NEG-22 (AC-25)** No terminal: every item kept, nothing waits. `y` typed before the prompt:
  discarded. The item changes after the answer: not removed.
- **NEG-23 (AC-27)** A corrupted part: nothing at the target, staging removed. An archive with
  `../x`, an absolute name, a symlink then a file through it, or a hard link to outside: refused,
  nothing written outside staging. An existing target, or an entry that already exists in staging:
  refused. Too little free space: refused before extraction.
- **NEG-24 (AC-30)** Two names differing only by case, or NFC and NFD forms of one name, restored to
  a case-insensitive or normalising file system; a name invalid on Windows restored there: refused,
  naming them.
- **NEG-25 (AC-31)** A remote URL with an embedded fake token: absent from output, journal and
  records.
- **NEG-26 (AC-32)** A repository with a `.gitattributes` filter, hooks and config naming programs:
  no marker file is written during re-check, archive, removal or restore.
- **NEG-27 (AC-29)** A repository whose nested `node_modules` is answered `n` or is outside
  `--class`, with `--remove`: the repository is skipped. The same with `--backup-only`: the
  repository is archived with the `node_modules` inside.
- **NEG-28 (AC-33)** An encrypted rclone configuration, with or without a password in the
  environment: exit 1 before any upload, no prompt, and no rclone process reads stdin outside
  `rcat`. rclone older than 1.64:
  exit 1. rclone stderr carrying a fake token: absent from every output.
- **NEG-29 (AC-34)** A crash after `attempt-started` and before the first part, then a resume: the
  next attempt number is used and no name is written twice. A crash after `segment-reserved`: the
  next segment number is used.
- **NEG-30 (AC-35)** A release build with the hook's environment variable or configuration set:
  nothing changes; the marker string is absent.
- **NEG-31 (AC-36)** A click-test report by the author or a spec reviewer, or one without the real
  non-local remote flow: not accepted.
- **NEG-32 (AC-12)** A repository with a network remote whose tracking refs hold all branches, and:
  a fake `.env` set aside with `git stash -a`; a commit adding `id_ecdsa` that was then reset away
  (reachable only from a reflog); an unpushed commit adding `.env.production`; a stash whose index
  commit holds a staged `credentials.json`. Each alone keeps the item by default, with a reason
  that names the file and not its contents; with `--upload-secrets` the item is archived; the fake
  values appear in no output, journal line or record. A `.env` in a commit the remote already holds
  does not keep the item (section 17).
- **NEG-33 (AC-12)** A global excludes file named only through `include.path = ~/...` in the
  user's global config: its matches are found. An `https` URL whose user name is 40 hexadecimal
  characters, with no password, in `.git/config`: secret-like. A `test/fixtures/cert.pem` inside an
  installed package in an ignored `node_modules`: not secret-like by name.
- **NEG-34 (AC-2)** A `--backup-only` run, and a dry run, after a `--remove` run was killed with an
  item in quarantine after `delete-started`: the quarantine is untouched and reported; the next
  `--remove` run repeats the second existence check against that run's storage, then finishes it.
- **NEG-35 (AC-29)** A repository whose `node_modules` holds a hand-edited file: after `--remove`
  and `restore`, the edited file is back with its content.
- **NEG-36 (AC-7)** A file bind-mounted into the item from outside, and an item on a vfat file
  system: skipped, and no byte of the bind-mounted file appears in any stored object.
- **NEG-37 (AC-27)** A restored tree whose files' modification times or hard-link groups differ
  from the index: the restore exits 2 naming them. A source with `security.selinux` labels restored
  on a system that relabels: not reported as a difference.
- **NEG-38 (AC-33)** A `union` remote with one `crypt` upstream and one plain upstream: reported
  unencrypted, and `--require-encrypted-remote` refuses it. `RCLONE_TEMP_DIR` set in the
  environment: rclone still uses the tool's `--temp-dir`.
- **NEG-39 (AC-37)** A plan whose `remote_facts_note` still promises an `ls-remote` check: the run
  header states that apply does not contact remotes and archives the whole item.

## 22. Reviews: findings and changes

Earlier reviews (revision 2 acceptance, revision 4 adversarial) and how revision 5 answered them
are in [APPLY-SPEC-REVIEWS.md](APPLY-SPEC-REVIEWS.md). This table answers the acceptance review
of revision 5.

| Finding | Change in revision 6 | Criteria and negative cases |
|---|---|---|
| N1 secrets only in the object store (stash parents, local-only and reflog-only commits) uploaded by default | History rule in 9.2 with offline commands (7.5); limit restored in 17 | AC-12; NEG-32 |
| I-1 plan text promises an apply-time remote check | C-4 lists the exact strings; run header correction until it lands | AC-37; NEG-39 |
| I-2 reconcile deletes under `--backup-only` | Reconcile by mode; second existence check against the journal's storage (6.1, 10.1) | AC-2, AC-21; NEG-34 |
| I-3 name rule over-keeps | Name rule for untracked and ignored files only; `fixture_prone` package evidence honoured; kept counts by rule (9.2) | AC-12; NEG-33 |
| I-4 `include.path` with `~` | Includes read and expanded by the tool (9.2) | AC-12; NEG-33 |
| I-5 token as URL user name | Credential definition in 9.2 | AC-12; NEG-33 |
| I-6 more sync clients | Nextcloud, ownCloud, MEGA, Resilio, Seafile default (7.4); limit (17) | AC-7 |
| I-7 bind-mounted file | Every entry checked for a mount (7.4) | AC-7; NEG-36 |
| I-8 Linux xattr namespaces; file system types | Namespace rules (9.3); removal only on an allowlist of local file systems (7.4) | AC-7, AC-13; NEG-36 |
| I-9 nested regenerable children not archived | Archived and removed with the git item (6.3, 9.1, D-15) | AC-29; NEG-35 |
| I-10 restore metadata; device check in 7.2 | Restored and compared metadata listed, the rest named as not restored (13.3); 7.2 compares with the parent directory | AC-27; NEG-37 |
| I-11 rclone temporary directory; wrappers | `--temp-dir` under the state directory; recursive wrappers, encrypted only when every path is `crypt` (9.6) | AC-24, AC-33; NEG-38 |
| I-12 "no network" wording | No git command touches the network; rclone contacts only the user's storage and may refresh its own token (top, section 3) | AC-9 |

## 23. Test and build plan

- New crate `crates/apply` (library: re-check, places, archive, store, manifest, journal, lock,
  restore) and CLI wiring in `crates/cli`. Core changes are additive only (section 15).
- Tests are written first; each behaviour gets a red CI run before its implementation lands.
- Builds and tests run in GitHub Actions on ubuntu, macOS and Windows; extra jobs: Linux mounts and
  spool (tmpfs, directory and file bind mounts, a vfat loop mount, with `sudo`), real rclone (a
  pinned release whose published SHA-256 is checked before use, `local` remotes only), and a release build that checks the hook is absent
  (AC-35).
- No test touches any directory outside its own temporary directory, and no test uses a network
  remote, a network host or a real credential.

## 24. Click-test plan

An independent tester, arranged by the coordinator (not the author, not a spec or code reviewer),
runs the real binary before the work is called ready.

- **Binary.** The release artifacts of the CI run for the PR's head commit, for macOS, Linux and
  Windows, with their SHA-256 recorded in the report. Nothing is built on the test machines.
- **Machines.** A macOS host, a Linux host and a Windows host or VM, each with a few GiB free.
  Nothing outside a fresh temporary directory created for the test is scanned, archived or removed.
- **Fixtures.** `scripts/make-click-fixtures DIR` (part of the PR) creates, inside DIR only: a
  repository whose remote is an https URL on a reserved `.invalid` host and whose remote-tracking
  refs are set locally to its tips, so the scan reports it saved (`backup-then-remove`; apply never
  contacts that host); one with an
  unpushed commit, a stash, uncommitted, untracked and ignored files
  (`recover-backup-then-remove`); a project with a `node_modules` (`remove-regenerable`); one
  repository with a fake `.env`, and one with a fake `.env` set aside with `git stash -u` (both
  kept by default); a nested case with a hand-edited file in `node_modules`; a file with an extended
  attribute; a long path on Windows. The script sets every modification time and every commit
  date 60 days in the past, beyond the default 7-day active window, so the plan gives the actions
  named. The tester runs `scan` and `plan` on DIR.
- **Storage.** (a) A local-directory rclone remote in a temporary rclone config. (b) One real
  non-local remote in a disposable bucket or folder that the coordinator provides at test time. The
  tester configures it in their own rclone config; no credential appears in the repository, the
  report, the ticket or any log, and the bucket or folder is deleted after the test.
- **Flows, on macOS and Linux, against (a) and then (b):** dry run (nothing written); `--backup-only`
  then `manifest list`, `find`, `verify`; `--remove` in ask mode answering `y`, `n`, `s` and `q`;
  `--remove --auto repo,regenerable`; Ctrl-C during an upload, then `apply --resume`; `restore` to
  the original path and with `--to`; `restore` with an empty `--state-dir` and `--storage` only;
  `manifest orphans` after the interrupted run; `--require-encrypted-remote` on a plain remote
  (refused). After each flow the tester compares the restored trees with the fixture and searches
  every output for the fake `.env` value.
- **Flows on Windows:** dry run, `--backup-only` against (a) and (b), restore with `--to` (long
  path included), and `--remove` refused with exit 1.
- **Record.** The report (flows, platforms, artifact digests, commands, outcomes, findings) is
  attached to the ticket with `spec_flow.py click-test`. Findings are fixed and the affected flows
  retested on the new artifacts.
- This is a command-line tool: the flows run in a terminal session on each host, not through a
  browser. That is a labelled deviation from a browser-based click-test and is reported as such.
