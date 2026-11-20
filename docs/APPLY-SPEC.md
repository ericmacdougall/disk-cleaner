# Apply phase specification

Status: **draft for review, revision 11**. Nothing in this document is implemented yet. It defines
what `disk-cleaner apply`, `disk-cleaner restore` and `disk-cleaner manifest` must do before any
code is written, and it is the contract the tests are written against. Section 16 lists the
decisions that are still open; text that depends on one of them says so. Section 22 maps the latest
review to changes and lists the minor items left for later; earlier reviews are answered in
[APPLY-SPEC-REVIEWS.md](APPLY-SPEC-REVIEWS.md). Criterion and negative-case numbers are stable from
revision 5. Revision 10 removed Windows operation and the finishing of interrupted deletions (8.2,
8.3). Revision 11 makes the deletion pass compare every entry's full preserved record just before
removing it (13.2), gives spool teardown one routine (9.6), lists every quarantine state (5),
lets the user choose the secrets handling (D-4, 9.2, 11) and defines signals during deletion
(10.1); its criteria are rewritten in place under the same numbers (section 22).

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
- Secret-like files: the user chooses what happens to an item that holds them, per item in ask mode
  or by a setting for automatic runs: keep the item, or upload them to the user's own storage with
  the rest (D-4: offering the choice, and upload to the user's own storage, are the user's
  decision; keeping when nothing is chosen and deferring the local vault option are the owner's
  ASSUMPTIONs). Nothing chosen: kept.
- A journal that makes every step resumable and idempotent; a per-user lock so two applies never
  run at once on one machine.
- The manifest (local and in storage), `disk-cleaner manifest list|find|verify|orphans`, and
  `disk-cleaner restore`.
- Ask mode (per item, the default) and automatic mode (per class, opt-in).
- **Platforms.** macOS and Linux: dry run, backup, removal and restore. On Windows `apply` in
  every mode, `restore`, `quarantine list` and every rclone-running command refuse (exit 4; 5;
  D-13, ASSUMPTION).
- **Interrupted deletions are never finished by the tool.** A deletion that stops part-way leaves
  a **leftover** quarantine that no later run deletes from or reads (10.1; D-17, ASSUMPTION).

Out of scope for this release. **Against the original requirements this release is partial; each
item marked TARGET_MISMATCH is deferred, not met:**

- **TARGET_MISMATCH: pushing recovery branches.** The requirement asked for automatic tools that
  push unsaved work to recovery branches. This release pushes nothing; the whole-item archive holds
  that work instead. Section 8 lists the entry conditions for the later release.
- **TARGET_MISMATCH: Windows.** The requirement asked for Windows, macOS and Linux. In this
  release a Windows machine cannot back up, remove or restore, not even an archive made on macOS
  or Linux (5, 8.2). ASSUMPTION: the coordinator's choice, not the user's (D-13).
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
  `~/Library/Application Support` on macOS; or `--state-dir`. It holds `machine.json`, `lock`, `journal/`, `manifest/`, `index/` and
  `rclone-tmp/` (rclone's spool, one directory per operation, 9.6), created with owner-only
  permissions.
- **Storage**: an rclone remote and path, `--storage <remote>:<path>`, that the user configured with
  `rclone config`. The tool never reads or prints rclone's credentials and never passes a credential
  on a command line (9.6).
- **Machine id**: a random 128-bit id created on the first run, kept in `machine.json` with an
  optional label (`--machine-label`, default empty). Remote paths use the id only.
- **Fingerprint**: for every entry below an item (the item directory included), its relative path
  (bytes), kind (directory, regular file, symlink), size, modification time in nanoseconds, change
  time (not for the item directory, which the quarantine rename changes), device, inode and mount
  id where the platform provides one, mode bits, file flags (macOS `st_flags`), link count, the
  names and sizes of its extended attributes, and, for a symlink, its target bytes. For the item
  directory alone, whose change time the rename moves, it also holds the SHA-256 of each extended
  attribute's value, so that a change to them is still seen. Every other entry's change time moves
  on any write, chmod, flag or attribute change, which is what makes the fingerprint sufficient
  before deletion begins. Any other kind of entry (fifo, socket, device) makes the item
  unsupported, except the socket `.git/fsmonitor--daemon.ipc`, which SCAN-FORMAT.md allows in a
  plain git directory: it is recorded by name, not archived, and not recreated by restore.
- **Preserved record**: for one entry, everything the archive carries for it **and restore writes
  back** (9.3, 13.3), and nothing else:
  - relative path (bytes) and kind;
  - for a regular file, its size and the SHA-256 of its content. Every name of a hard-link group
    carries the group's size and digest, also the names the archive stores as a link entry
    without content;
  - for a symlink, its target bytes;
  - mode bits, macOS user file flags (`UF_*`) and modification time (nanoseconds);
  - every archived extended attribute, by name, size and SHA-256 of its value. On macOS this
    includes the resource fork and Finder info. `security.selinux` is written to the index beside
    the record, for information only, and is not part of it;
  - hard-link grouping (which names inside the item share one file).

  The archive writer computes it, in the same function that writes the entry's tar header, pax
  records and content, from the bytes it actually wrote, so the record and the archive cannot
  disagree. The record is read by the deletion pass (13.2), by restore's check (13.3 step 6) and
  by `manifest find --files`; the checks before the quarantine compare against F0 (6.4). What the
  archive does not carry, or carries without restoring, is **not preserved**: owner and group,
  ACLs, access, change and birth times, inode and device numbers, link counts, system file flags
  (`SF_*`), sparseness, `security.selinux`, and the harmless attributes 9.3 drops. Section 17
  lists these.
- **Index**: the item's fingerprint F0 plus the preserved record of every entry (9.3); the
  deletion pass compares against it (13.2). A `remove-regenerable` item removed without a backup
  has no index: its F0 is held in memory for steps 9 and 10 only, nothing of it is written, and
  nothing of it is preserved.
- **Quarantine**: renaming an item, with no-replace semantics, to `.disk-cleaner-removing-<item_id>`
  in the same parent directory before deleting it. Only the one deletion pass of the run that
  renamed it ever deletes from it (13.2). A quarantine whose deletion began (`delete-started`,
  10.1) and did not finish is a **leftover**: no later run deletes from it, renames it or reads
  its contents.
- **Secret-like file**: defined in 9.2.

## 5. Commands

```
disk-cleaner apply --plan PLAN --scan SCAN --storage REMOTE:PATH
                   [--backup-only | --remove]
                   [--auto CLASS[,CLASS...]] [--class CLASS[,CLASS...]] [--item PATH]...
                   [--backup-regenerable] [--secrets keep|upload] [--require-encrypted-remote]
                   [--max-plan-age HOURS] [--max-upload-bytes N] [--min-free BYTES]
                   [--part-size BYTES] [--state-dir DIR] [--machine-label TEXT]
                   [--rclone PATH] [--allow-root] [--json]
disk-cleaner apply --resume RUN_ID
disk-cleaner restore RECORD_ID [--to DIR] [--storage REMOTE:PATH] [--machine ID]
                     [--min-free BYTES] [--accept-unverified] [--state-dir DIR]
                     [--rclone PATH] [--json]
disk-cleaner manifest list   [--storage REMOTE:PATH] [--machine ID] [--json]
disk-cleaner manifest find TEXT [--files] [--storage REMOTE:PATH] [--machine ID] [--json]
disk-cleaner manifest verify --storage REMOTE:PATH [--deep] [--machine ID] [--json]
disk-cleaner manifest orphans --storage REMOTE:PATH [--machine ID] [--json]
disk-cleaner quarantine list [--state-dir DIR] [--json]
```

- `quarantine list` prints **every quarantine any journal names that still exists**, in every
  state (10.1), with its path, the item and run it came from, its state, and what to do. It reads
  the journals and `lstat`s each quarantine path and original path, never anything inside them.
  No command deletes a quarantine.
  - **Not started** (`quarantine-intent` or `quarantined`): "This folder is the complete item;
    nothing was deleted." Then the commands that put it back: `disk-cleaner apply --resume RUN_ID`,
    and the manual `mv -n Q O`. When something occupies the original path, it says so and names
    it, and says that the folder is put back only once the original path is free.
  - **Leftover** (`delete-started` or `left`): the record and archive that hold the verified copy
    (none for an unarchived regenerable item, which says "never archived; rebuildable"), the exact
    `disk-cleaner restore RECORD_ID --to DIR` command with an absolute DIR outside the
    quarantine's parent, and these statements: "It is incomplete: some of its files were already
    deleted. Do not use it as a working copy; the restore command gives the complete item." and
    "disk-cleaner will not delete this folder. Removing it is your own action, after you have
    checked it."
  - **Moved back** (`moved-back`, or `delete-started` or `left` with Q absent and, by `lstat`, a
    directory at O with F0's device and inode, 10.1): the original path, the same two statements,
    and the restore command.
  - **Replaced** (something at Q's path fails the identity check of 6.1, in any state): what is
    there (kind, and a symlink's target as text), that it is not the tool's quarantine and the
    tool will not touch it, and the restore command for an archived item ("never archived;
    rebuildable" otherwise).
- No mode flag: **dry run**. `--backup-only`: everything up to and including the manifest, then
  stop; nothing is removed. `--remove`: the full sequence. The two are mutually exclusive.
- `--secrets keep|upload` (or `[apply] secrets`) is the secrets handling for items that run in
  automatic mode (11, D-4); default `keep`. Items in ask mode are asked.
- **Windows.** `apply` (every mode, `--resume` included), `restore`, `quarantine list`,
  `manifest list --storage`, `manifest find TEXT --storage`, `manifest find --files`,
  `manifest verify` and `manifest orphans` exit 4 ("not supported on this platform in this
  release"). The platform check is the first thing the binary does for these commands, before it
  reads the configuration file or validates any other argument, so it also comes before the state
  directory, a lock file, or any rclone or git process exists. `manifest list` and
  `manifest find TEXT` without `--storage` or `--files` read the local manifest at
  `<LocalAppData>\disk-cleaner\manifest\` (the account's known folder) if it exists, create
  nothing, and in practice report no records, since nothing on Windows writes there. `scan` and
  `plan` are unchanged. **WSL2** runs the Linux build and is unsupported and untested in this
  release: 7.4 refuses removal on its Windows drives (`/mnt/c`, drvfs), and backup from them is
  not tested. A manual route, also unsupported: the archive is a plain pax tar in parts, so
  `rclone cat` of the parts in order piped into `tar -x` extracts it anywhere, without the checks
  of 13.3.
- `--auto` names the classes whose items run without a prompt: `regenerable`, `cache` (their
  `remove-regenerable` items), `repo` (`backup-then-remove`), `repo-recover`
  (`recover-backup-then-remove`). `[apply] auto = [...]` in the config file does the same. Default:
  none, so every item is asked. `repo` and `repo-recover` execute identically; they stay separate so
  the user decides about repositories with unsaved work on their own.
- `--class` and `--item` narrow the selection; they never widen it.
- `--resume RUN_ID` takes no other flags: the run's flags are in its journal.
- Running as root (uid 0) is refused unless `--allow-root`.
- Configuration lives in the same `config.toml` as the plan settings, under `[apply]` (`auto`,
  `secrets`, `require_encrypted_remote`, `min_free`, `part_size`, `max_upload_bytes`,
  `max_plan_age`). Unknown keys are an error, as for the plan.

## 6. The sequence

### 6.1 At the start of every apply

Every apply first checks rclone (9.6). Then, dry runs excepted (they only report), it takes the
lock (10.2) and reconciles what earlier runs left behind, according to its mode:

- **Every mode, dry run included:** every quarantine a journal names (10.1), in any state, and
  every record not yet in a synced segment is reported with its run id, the quarantine as
  `quarantine list` prints it (5). A path is never reported `gone` while a quarantine with its
  `item_id` exists; its item is reported "in quarantine from run RUN_ID" with the quarantine's
  state, and is not selected again.
- **`--backup-only` and `--remove`:** in addition, records written but not yet in a synced
  segment are synced to the storage named in **that run's journal** (not the current `--storage`,
  if it differs).
- **`--remove` and a resume:** in addition, a quarantine whose deletion never started
  (`quarantine-intent` or `quarantined`, 10.1) is renamed back to its original path when the
  **identity check** holds, and its item is recorded `kept`: `lstat` of Q (no-follow) shows a
  directory with the device and inode the journal recorded from F0, and `lstat` of O shows that
  nothing (no directory, file, symlink or other entry) is there. The rename back is no-replace.
  Otherwise Q is left as it is, the item is `failed`, and the summary and `quarantine list` name
  Q and what is at O. That check and the rename read nothing inside Q.

No mode reads, deletes or renames anything inside a leftover.

### 6.2 For one item

Items are processed one at a time, innermost first, with one exception: a regenerable child of a
selected git item is handled with that item, not before it (6.3). Every transition is written to the journal
before the step it records takes effect. Any failed check ends the item as **skipped** with a reason
and the run continues with the next item; nothing that failed a check is ever removed.

1. **Select.** The item's plan action removes something and the item is within `--class` and
   `--item`.
2. **Re-check** (section 7): take the fingerprint F0, re-scan and re-plan, refuse places that are
   never removed, check processes, run the secret checks of 9.2 when the item will be uploaded
   (an item removed without a backup uploads nothing, so they are not run for it and cannot keep
   it), take the fingerprint again. The F0 walk is the first walk of the item and stops at a
   quarantine (7.4), so no later walk reaches one.
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
   through the handle opened on it, and compare with F0 (the item directory's attribute values
   included, section 4). Any difference: rename it back and end as `backed-up-kept` (the archive
   stays, the manifest says why). If the rename back fails because
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
  and removed with it. The enclosing item's prompt lists these children and their bytes, its record
  lists their paths in `nested_regenerable`, and each child gets a record with outcome
  `removed-with` naming the enclosing record (counted as done by a rerun, exit 0). If the enclosing item ends as anything but
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
  out cheaply), a Dropbox folder listed in Dropbox's own `info.json`, or any directory with a Syncthing `.stfolder` marker in it or an ancestor; in it or an ancestor, a
  Nextcloud or ownCloud journal (`.sync_*.db` or `._sync_*.db`), a MEGA `.debris` directory or a
  Resilio `.sync` directory; or a path under `~/Seafile` (Seafile's default; other Seafile
  libraries are not detected, section 17). Deleting there deletes on the user's other machines,
  which this tool cannot check;
- **it holds an online-only placeholder**: an entry with the macOS `SF_DATALESS` flag. Reading one
  triggers a download or fails;
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
- **an entry's extended attributes cannot be archived** (9.3);
- **it holds, or lies inside, a quarantine or a restore staging directory**: any entry whose name
  starts with `.disk-cleaner-removing-` or `.disk-cleaner-restoring-`, any path a journal names
  as a quarantine, or a directory with the device and inode that a journal records from F0 for an
  item past `delete-started` (a leftover moved back, 10.1), whether or not any run has journaled
  `moved-back` yet. Such an item is kept **in every mode, `--backup-only` and dry runs
  included**, so a leftover is never archived; the reason names the directory, and the leftover
  stays the user's to check. **Every walk stops there without descending**: the F0 walk, which
  is the first walk of step 2, meets the name and skips the item before F0 is complete, so the
  re-scan, the secret checks and the archive never start for it. Core change C-6 makes the scan
  and `rescan_item` give such a node `keep` and not descend into it either.

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
- `git status -z --porcelain`, `git ls-files -z --others --exclude-standard` and
  `git ls-files -z --others --ignored --exclude-from=FILE` in each working tree inside the item
  (9.2);
- for history held only locally (9.2), the **history commands**, each run with the runner's
  options plus three of its own before the subcommand: `-c disk-cleaner.phase=history` (a key git
  ignores, which marks the call for AC-9), `-c core.commitGraph=false` (9.2 says why) and
  `--no-replace-objects`. They are: `git rev-parse --show-object-format`; `git for-each-ref` (the
  network remotes' tracking tips, and every ref's target); `git cat-file --batch-all-objects
  --batch-check='%(objectname) %(objecttype)'` (every object in the store);
  `git rev-list --objects --missing=allow-promisor --stdin` (with the network remotes' tracking
  tips as lines on stdin, and never `--use-bitmap-index`); `git ls-files -s -z` (index entries);
  and `git cat-file --batch` (raw tree, tag and blob content, no filters).

**Minimum git: 2.32.** At start, every apply runs `git --version` and refuses an older or
unparsable version with exit 1, naming it, so an old git cannot silently keep every git item. The
history commands need `--batch-all-objects`, `--missing=allow-promisor` and
`rev-parse --show-object-format`, which are older than 2.32. 2.32 is also the first version that
honours the runner's `GIT_CONFIG_GLOBAL`. The value is an ASSUMPTION: the owner has not checked
the release each option arrived in, and NEG-55 tests the refusal, not the boundary.

That list is the **allowed set**. Restore and removal never run git. A test (AC-9) records every git
invocation through a logging shim and fails on any subcommand outside the allowed set. To find the
subcommand, the shim skips the runner's `-c NAME=VALUE` pairs and `--no-replace-objects`. It also
fails on a history command that lacks any of the three options above. When core changes its
command set, this list and the test change together, deliberately.

## 8. Deferred features

### 8.1 Live remote check and recovery push

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

### 8.2 Windows (D-13)

Revision 9 (commit `fbf25f3`) holds the Windows design (handle-based backup and restore, job
objects, locks, placeholders, alternate data streams, long paths). Entry conditions: **r9
adversarial F1**, an observable barrier proving every writer of a dead operation's spool exited
before cleanup (until then the spool is kept); **r9 adversarial F2**, a fail-closed Windows
file-system allowlist for the state directory's locks; tests of a failed
`AssignProcessToJobObject` and of breakaway under a job; handle-based removal, if wanted.

### 8.3 Finishing interrupted deletions (D-17)

Revision 9 (commit `fbf25f3`) holds a resume check and `quarantine finish`. Finishing may return
only with a negative case for each of: an equal-length overwrite with the time set back (r7
adversarial F2); attribute-only and directory changes (r8 adversarial F1); changes between showing
differences and deleting, and a replaced quarantine (r9 B-1); a later hard-link name rewritten
after the first was deleted (r9 B-2); directory fields the deletion moves (r9 C-2); a FIFO
planted in the quarantine (r9 C-3).

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

- **by name**: it is untracked, ignored, or tracked with uncommitted changes (modified or added in
  the index or the working tree, as `git status -z --porcelain` reports; a tracked `.env` pushed
  with placeholders and then filled in locally is caught here), and its name is in the
  **secret name list**: the scan's
  `[[state_file]]` names (`.env`, `.env.*`, Terraform state and variables, SSH key names) or one of
  `id_rsa`, `id_dsa`, `id_ecdsa`, `id_ed25519` (and their `*_sk` forms), `*.pem`, `*.key`, `*.p12`,
  `*.pfx`, `*.jks`, `*.keystore`, `*.ovpn`, `.netrc`, `_netrc`, `.npmrc`, `.pypirc`,
  `.git-credentials`, `.envrc`, `kubeconfig`, `credentials`, `credentials.json`,
  `service-account*.json`, `secrets.yaml`, `secrets.yml`.
  **Package exemption.** It applies only inside a directory the scan classifies `regenerable` or
  `cache` (for example a `node_modules` or `.venv` archived with its git item, 6.3), and only to a
  file there that shows the scan's evidence of belonging to an installed package: a
  `fixture_files` entry between the file and that directory's root, or a package directory
  directly below a `fixture_dirs` name (core's `has_package_evidence`, which looks no higher than
  the regenerable directory's root). Apply does not reimplement this rule. It calls core's own,
  exposed by C-5 (section 15). Such a file is exempt from the content rule whatever its
  name, because installed packages ship example keys and test certificates; if its name is one of
  the `fixture_prone` key-material names (`*.pem`, `*.key`, `*.p12`, `*.jks`, `*.keystore`) it is
  exempt from the name rule too. Any other name (`.env`, `id_rsa`) is not exempt anywhere, and
  outside such a directory nothing is exempt: an ignored `android/app/release.keystore` in a
  repository whose root holds a `package.json` is secret-like;
- **in local-only history**: for each git directory in the item, the **examined set** is every
  object in its store that is not reachable from the **network tips**. The network tips are the
  refs under `refs/remotes/NAME/` for each configured remote NAME whose URL core counts as a
  network URL (`is_network_url`). These are the same refs core's facts subtract with
  `--remotes=NAME`, with one restriction core does not make: a ref under `refs/remotes/NAME/` that
  also matches the destination of **another** remote's configured fetch refspec
  (`remote.OTHER.fetch`, read from the config core already lists) is not a network tip of NAME. A
  tracking ref that a custom fetch refspec writes outside `refs/remotes/NAME/`, or one left behind
  by a remote that is no longer configured, is not a network tip either. Leaving a ref out can only
  enlarge the examined set. The opposite is not prevented: a ref under `refs/remotes/NAME/` set by
  hand (`git update-ref`) to a local-only commit, including one at a deeper name such as
  `refs/remotes/NAME/sub/x`, is subtracted. This is the stale or hand-set tracking-ref case
  section 17 discloses. The examined set holds what unpushed commits, stashes (live or dropped),
  reflogs, the index and unreachable objects hold (a dropped stash, a file staged and then
  unstaged, a deleted branch). It is computed with the history
  commands of 7.5 only, and they run inside step 2, after F0 and before step 2 takes the
  fingerprint again. A change to the store after the check (a new loose object or pack, a moved
  ref) changes an entry of the git directory. The end-of-step-2 and end-of-archive comparisons
  with F0 then skip the item.
  **Repository-controlled inputs to the walk.** Only the walk decides what is not examined, so
  anything in the repository that can add an edge to it, or substitute an object's content, is
  refused or neutralised:
  - `info/grafts`, `objects/info/alternates` and `objects/info/http-alternates` make core's layout
    check fail (`crates/core/src/facts.rs` as of commit d0f9292 on `feat/apply`, `shape`, lines
    322 to 326; the line numbers below are from that commit too). The plan then gives `ask`
    and apply never selects the item. The runner drops inherited `GIT_*` variables
    (`GIT_GRAFT_FILE`, `GIT_ALTERNATE_OBJECT_DIRECTORIES`, `GIT_REPLACE_REF_BASE`).
  - Replace refs (`refs/replace/`), and any ref outside `refs/heads/`, `refs/tags/`,
    `refs/remotes/` and `refs/stash`, also fail the layout check, as does a ref that points at a
    blob or a tree (`layout_check`, lines 434 to 442). `--no-replace-objects` on every history
    command is a second guard that does not rely on core. Tests reach it through the library
    (NEG-45).
  - **The commit-graph** (`objects/info/commit-graph`, `objects/info/commit-graphs/`) is allowed
    by the layout check (`crates/core/src/gitshape.rs`). git takes parents and root trees from it
    without comparing them with the commit objects, so a graph that disagrees with them would make
    local-only objects count as held by the remote. Every history command therefore runs with
    `-c core.commitGraph=false`. The repository cannot turn the graph back on: `core.commitgraph`
    is not in core's config allowlist (`config_key_is_safe`), a repository that sets it is not
    inspected at all, and the runner reads no global or system config. NEG-47.
  - Pack bitmaps (`pack-*.bitmap`, `multi-pack-index-*.bitmap`) are read by `rev-list` only with
    `--use-bitmap-index` (git-rev-list(1)), which apply never passes. AC-9 asserts that.
  - `shallow` and the promisor settings (`extensions.partialClone`, `remote.NAME.promisor`,
    `*.promisor` files) can only remove parts of the walk: a shallow cut hides parents, and an
    allowed missing promisor object hides what lies below it. A smaller walk means a larger
    examined set, which is the safe direction. A missing object is not in the store, so it is
    neither examined nor archived. `GIT_NO_LAZY_FETCH=1` and `protocol.allow=never` keep it
    missing.
  - The pack indexes (`pack-*.idx`, `multi-pack-index`) map object ids to offsets, and git does
    not re-hash what it reads through them while walking. Accidental damage gives data that does
    not parse, or a `missing` line, and both fail closed. A consistent substitution would have to
    name the user's own local-only commit, which does not exist until after the repository was
    made. So it needs a deliberate edit after that commit, by a program running as the user, and
    section 3 does not defend against that. Every tree and tag object that
    apply parses itself (below) has its id recomputed from its type, size and content, and a
    mismatch fails closed. Section 17 states what remains.

  The commands:
  - `git cat-file --batch-all-objects --batch-check='%(objectname) %(objecttype)'` lists every
    object in the store, loose or packed (cruft and promisor packs included), in object-name order;
  - `git rev-list --objects --missing=allow-promisor --stdin`, given the network remotes' tracking
    tips on stdin, lists every object reachable from them; the examined set is the first list minus
    this one. With no network tip, this command is not run and the examined set is the whole store. In a repository that is not a partial clone a missing object fails this walk
    (`--missing=allow-any` would skip it silently, and the names below it with it). An object
    missing from a partial clone is not in the store, so it is neither examined nor archived;
  - **names come from entries, not from the path column of `rev-list`**: every tree in the
    examined set, read raw with `git cat-file --batch` and parsed in the repository's object
    format (`git rev-parse --show-object-format`: 20-byte ids for SHA-1, 32-byte for SHA-256), and
    every index entry (`git ls-files -s -z`) gives its file name to the object it holds, so a blob
    first reached under another name (duplicate content, a later rename) is still named `.env`. A
    tag object in the examined set, or a ref (`git for-each-ref`), that points directly at a blob
    makes that blob named without giving it a file name. An entry counts only when its object is
    in the examined set. Before a tree or tag is parsed, its id is recomputed from the `cat-file`
    header and content in the object format, and a mismatch fails closed;
  - a blob in the examined set is secret-like when an entry naming it has a name in the secret
    name list (the package exemption does not apply here), or when it is smaller than 1 MiB and its
    first 64 KiB, read with `git cat-file --batch` (raw content, no filters), match the content
    rule below. The content check reads at most 64 MiB per item in total; past that budget only
    names are checked, and the summary says so;
  - **a blob in the examined set that nothing names keeps the item**, whatever its size or
    content, because no rule can classify it. It arises from `git add` followed by
    `git rm --cached` (a `.env` swept up by `git add .` and then unstaged). It also arises from
    every version of a file that was staged and then replaced in the index before a commit:
    `git add`, an edit, then `git add` again or `git commit -a`; `git add -p`; and the hunk staging
    of editors and GUI clients. That is ordinary work, so for repositories worked on by hand this
    is likely the common case, not a rare one. Apply runs git with `gc.auto=0`, and the user's own
    automatic `gc` rarely runs in a small repository, so these objects stay for months (D-16
    states the cost). The reason reads: "an unnamed object in the git store (a file version that
    was staged and then unstaged or replaced). Inspect it with `git fsck --lost-found`. If you do
    not want those objects, `git gc --prune=now` removes them, but it also destroys dropped
    stashes you could still recover, which is why apply never does it." In ask mode on a terminal,
    when the item's only reasons are in its secrets set, the user may answer `u` for that item at
    the secrets prompt (11, D-16); automatic runs never upload unnamed objects. The
    content rule above runs on unnamed blobs too, within the same budget, so that the prompt can say
    what it saw;
  - **limits**: a store of more than 10 million objects in one git directory, or more than
    600 seconds for all the commands of this rule on one item, keeps the item ("the history check
    exceeded its limit"). When the time limit is reached, the whole process group of the running
    git command is killed. Section 18 gives the memory this needs. Both values are ASSUMPTIONs
    (D-16); tests inject smaller ones through the library;
  - a `pack-<id>.pack` without its `pack-<id>.idx` is invisible to git, so its objects cannot be
    listed: it keeps the item. Core's layout check already gives `ask` to any other object-store
    file it does not recognise, such as a `tmp_*` leftover.

  This catches a `.env` set aside with `git stash -u` or `git stash -a`, whether the stash is kept
  or dropped; a key file added by an unpushed commit, also when a later commit renamed it or it is
  reachable only from a reflog; a file staged and never committed; and a file staged and then
  unstaged. History that a network remote already holds, according to the local tracking refs, is
  not examined, and commit and tag messages are not checked (section 17).
  **A dry run performs this check too.** All its commands are in 7.5 and write nothing, so the dry
  run reports "would keep" with the same reasons a real run gives, and its summary prints the
  per-rule counts of kept items. It costs the same time as in a real run: up to the time limit of
  600 seconds per item;
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
- **by content**: it is untracked, ignored, or tracked with uncommitted changes (as for the name
  rule), smaller than 1 MiB, and its first 64 KiB contain the
  header line of a PEM private key block, a URL that carries a credential (below), or a token in
  one of the documented formats of the common providers (AWS access keys, GitHub and GitLab
  tokens, Slack tokens, API secret keys of the common model providers, Google API keys); the exact
  patterns live in the code and its tests, not in this document;
- **in a git directory**: it is a file of a git directory outside `objects/` (`config`,
  `FETCH_HEAD`, files under `logs/`, the conflict images under `rr-cache/`, any other) that
  contains a URL that carries a credential, or that is smaller than 1 MiB and whose first 64 KiB
  match the content rule (a `rr-cache` preimage holds the text of a conflicted file).

A URL **carries a credential** when its userinfo has a password, or its user name matches a token
pattern, is 40 or more hexadecimal characters, or is longer than 30 characters. A short plain user
name (as in a default Azure DevOps clone URL) is not a credential.

**Fail closed.** Any command of this section that fails (a nonzero exit, or output that does not
parse, such as a `missing` line from `cat-file --batch` for an object the store listed), exceeds
the limits above or cannot run, and any file the content rule cannot read, keeps the item (skipped
before upload, the reason names the check). A warning on stderr with exit 0, which git prints for
every partial clone, is not a failure. The one exception is the 64 MiB content budget for named
blobs in history, which is documented best effort.

**The secrets set and the user's choice (D-4).** Everything the rules above find in one item is
that item's **secrets set**: secret-like files (by name, content, global excludes or git
directory rule), secret-like names in history the network remotes do not hold, unnamed objects,
and every history check that hit a limit or could not run ("not checked"). For each item with a
non-empty secrets set the user chooses one handling:

- **keep** (the default whenever nothing was chosen): the item is skipped before upload, the
  reason naming up to five entries by relative path or, for history, by file name ("a stash, a
  local-only commit or an unreachable object holds NAME"); never contents;
- **upload**: the item is archived whole to the user's own storage, the secrets set included, like
  everything else (the **upload choice**).

In ask mode the choice is made at the secrets prompt (11), for that item only. For an item that
runs in automatic mode it is `--secrets keep|upload` or `[apply] secrets`, default `keep`; there,
`upload` covers the item's secrets set **except unnamed objects and history checks not
completed**, which only the prompt uploads (D-16, the user's per-repository choice; a check not
completed may hide unnamed objects), so an automatic item whose set holds either is kept. The
option overrides the setting; any other value of either is a usage error (exit 1) before any
item is read. A choice covers exactly the set it was made for: it binds to F0 (11), so a secret-like
entry that appears later keeps the item or asks again, and no choice uploads another item's
secrets. The third option the user was shown, a local vault for the secret-like files with the
rest archived, is deferred (D-4 says why). The run summary counts the items kept by each rule and
the items uploaded with a secrets set, so the cost of over-keeping (mainly the global-excludes
rule, the unnamed-object rule and the history limits) is visible.

**Encryption.** Before the first upload, apply reads the storage remote's type (9.6). When it is not
`crypt`, the run summary and every ask prompt say that the archive is stored unencrypted in the
user's storage, and the secrets prompt says "these files will be stored unencrypted in REMOTE".
`--require-encrypted-remote` (or `[apply] require_encrypted_remote = true`) refuses to upload to
such a remote (exit 1). The tool does no encryption of its own in this release (D-4).

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
  socket, see section 4) and devices make the item unsupported. A sparse file is stored with its
  holes read as zeros and restored dense; its layout is not carried (section 17).
- **Extended attributes** (macOS and Linux, including the macOS resource fork and Finder info) are
  stored as pax `SCHILY.xattr.<name>` records, up to 64 KiB per attribute and 1 MiB per file. These
  harmless attributes are dropped and listed by count: `com.apple.quarantine`,
  `com.apple.provenance`, `com.apple.lastuseddate#PS`, and an all-zero `com.apple.FinderInfo`.
  An attribute over the limits makes the item unsupported. Linux namespaces: `user.*` is archived;
  `security.selinux` is recorded in the index but not restored or compared (the target system
  labels the files); any other `security.*` attribute makes the item unsupported;
  `system.posix_acl_*` are ACLs, not archived (section 17); `trusted.*` is not visible to an
  ordinary user and is not read. ACLs are not archived (section 17).
- The stream is split into **parts** at arbitrary byte offsets; restore concatenates them. The part
  size is chosen per attempt (section 18), written to the journal before the first part, and never
  changes within the attempt.
- The **index** is a separate JSON Lines object, `<archive>.index.jsonl`: a header (format version,
  machine id, original path, plan action, HEAD for git items, the part list with sizes and SHA-256)
  and one line per entry with its fingerprint fields and its preserved record (section 4), the
  SHA-256 of a regular file's content and of each archived extended attribute's value included. A
  copy stays in `<state>/index/`.

### 9.4 Upload and verification

- An attempt starts with a journal line `attempt-started` holding the attempt number, the part
  size, the part count and every object name the attempt will use; it is synced before the first
  byte is sent. Attempt numbers only grow, so no name is ever used twice, also across a crash.
- Each part is produced by one pass over the item through directory handles (section 13.2 rules:
  `fstatat` first, the non-blocking no-follow open, nothing resolved by path twice). The tar writer writes into a
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
- **Temporary directory.** Every rclone command that moves archive data (`rcat`, `cat`) runs with
  `--temp-dir <state>/rclone-tmp/OP`, so the tool, not the environment (`TMPDIR`,
  `RCLONE_TEMP_DIR`, rclone's own defaults), decides where rclone may spool. Those commands run
  only in non-dry runs, so a dry run still creates no state directory.
  **One spool directory per operation.** Every command that runs a data-moving rclone command is
  an operation with a fresh random id OP: a non-dry apply or resume, a restore, and
  `manifest list --storage`, `manifest find TEXT --storage`, `manifest find --files` and
  `manifest verify --deep`.
  The spool must outlive nobody who can still write to it, and a spool whose user is gone must not
  be kept forever. One lock cannot do both, because a hung rclone that holds the lock that marks
  an operation live would block cleanup and every later operation. So there are two locks, with
  different holders and lifetimes:
  - **Operation lock**, `rclone-tmp/OP.lock`: held by the tool's own process from the operation's
    start until it exits, and opened close-on-exec, so no child ever holds it. It says "the
    operation is live". The apply locks of 10.2 and the guard below are close-on-exec too, so a
    surviving child can never block a later apply, restore or cleanup.
  - **Spool lock**, `rclone-tmp/OP.spool`: taken before the first child is started and
    deliberately **inherited by every rclone child**: its descriptor is the one descriptor passed
    to the child. It is close-on-exec at all other times, and it is cleared only in the forked
    child just before rclone is executed, so git and every other child of the tool never get it.
    A `flock` lock belongs to the open file description, which the parent and every child share,
    so it is held while the tool **or any child** is alive. rclone's own subprocesses (an ssh for
    an sftp remote, a password or token command) may inherit it too and then count as holders
    (section 17). It says "someone may still write to `OP/`".
  - **Primitive.** `flock(LOCK_EX | LOCK_NB)`, never `fcntl` locks (a process loses those when it
    closes any descriptor of the file, and they are not shared with a child).
  - **A lock call that fails** with anything other than "held by another" (`EWOULDBLOCK`), for
    example `ENOLCK` or `ENOTSUP` on a network file system, counts as **held**. Teardown then
    leaves that spool, and an operation that cannot take its own locks, or
    the apply locks, stops with exit 1.
  - **Guard.** `rclone-tmp/guard` is a fixed file that is created once and never removed. Creating
    an operation's lock files and cleaning up both happen only while the exclusive lock on the
    guard is held. That lock is held for milliseconds, never during a transfer.
  - **Start.** Holding the guard, the operation creates `OP.lock` and `OP.spool` with create-new
    semantics, takes both locks, creates the directory `OP/` and releases the guard. It is the only
    writer of `OP/`, through its children.
  - **Teardown of a spool `ID`: one routine**, used both at the end of the tool's own operation
    and, at the start of every operation, for every other `ID`. It runs holding the guard, and only
    when the state directory is on a file system of the 7.4 allowlist (elsewhere advisory locks are
    not trusted: spools are reported and never removed).
    1. Own operation only: wait for the operation's direct children, then close every descriptor
       the tool holds on `OP.lock` and `OP.spool`. Waiting for a direct child proves nothing about
       rclone's own subprocesses, and the tool's own description would make the next step succeed
       or fail for the wrong reason, so neither is the test.
    2. Open `ID.lock` and `ID.spool` afresh (new open file descriptions) and try non-blocking
       exclusive locks on both. A missing lock file next to an existing `ID/` counts as held: the
       tool creates both files before `ID/` and removes them after it, so a missing one means
       someone else removed it, perhaps while a process still holds the old one (with no `ID/`,
       the lock files that exist are locked and removed the same way).
    3. Only when **both** are granted, so no process holds either description, are `ID/`, then
       `ID.spool` and `ID.lock`, removed while both are held. Otherwise nothing of `ID` is
       touched and the spool is reported (below).
  - **A surviving holder.** When an rclone child hangs after the tool died, or a subprocess of
    rclone outlives rclone at the tool's own end, it keeps `ID.spool` locked. The spool then stays:
    the operation that found it, and every later one, reports it ("a process of operation ID still
    holds its spool; last known rclone pid N"; the pid is recorded in `ID.pids` for this message
    only, is never trusted as a lock, and may already be gone when a subprocess of rclone is the
    holder) and counts its bytes against the state volume's floor. No operation waits for it,
    because the operation lock and the apply locks are close-on-exec, and the tool never kills a
    process it did not start in the current operation. When the holder exits, the next operation
    removes the spool.

  An apply, a restore and a manifest command can therefore run at once, and none of them touches
  another's spool, whoever of them dies first. Two machines that share a network home are not
  covered (section 17). The spool volume is the state volume, which section 18 (apply) and 13.3
  (restore) check before each part.
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
  `failed`, `kept`, `vanished`, `left`, `moved-back`) with the attempt, object names, sizes and
  digests; `quarantine-intent` also holds Q's path and the item directory's device, inode and
  mount id from F0. Each line is flushed and `fsync`ed before the step it records takes effect;
  renames are followed by an `fsync` of the parent directory where the platform supports it.
- `segment-reserved` holds the next manifest segment number and is synced before that segment name
  is used, so a segment name is never reused, also across a crash.
- The header records the plan and scan SHA-256, the storage, the flags and the build.
- `apply --resume RUN_ID` continues each unfinished item from its last durable state, but only
  work before deletion (re-checks, uploads, verification, records, items not yet quarantined). A
  resumed `--backup-only` run never deletes. **A deletion that began is never continued.**

**Leftovers.** When the deletion pass of 13.2 stops before emptying Q (a kill, a machine stop, or
a difference it found), Q is a leftover: no run ever deletes from it, renames it or reads inside
it again. A difference journals `left` with the entry and field; the item is `failed`. Every run
reports a leftover (6.1), but only a `--remove` run or a resume writes for one: `left` on finding
`delete-started` with Q present (without looking inside), and the table's records. `quarantine
list` (5) shows how to restore the item and that removing the folder is up to the user (D-17).

In the table, **O** is the original path and **Q** the quarantine path, both examined with
`lstat` (no-follow), never read inside. "Present" for O means any entry is there: a directory, a
file, a symlink or anything else. "Present" for Q means Q passes the identity check of 6.1 (a
directory with the journaled device and inode); anything else at Q's path is a **replaced Q**:
touch nothing, `failed`, and `quarantine list` names it. A resume never touches an entry at O that
is not the item.

| Last state | O | Q | On resume |
|---|---|---|---|
| Archived item: before `segment-synced`. Unarchived regenerable item: any state before `quarantine-intent` | any | absent | Start the item again from step 2 with the next attempt. Earlier objects are orphans. O absent: skipped `gone`. |
| Archived item: `segment-synced` or `checked-again` | present | absent | Step 2 again (F0 must equal the index), then steps 7 to 11. |
| Archived item: `segment-synced` or `checked-again` | absent | absent | Skipped `gone`; the `backed-up` record stays valid for restore. |
| `quarantine-intent` | present | absent | The rename never happened: as `segment-synced`. |
| `quarantine-intent` or `quarantined` | absent | present | Deletion never started: rename Q back to O (no-replace) without reading it, and record `kept` ("interrupted before deletion"). Only a `--remove` run or a resume does this; other modes report it, and `quarantine list` shows it as not started. |
| `quarantine-intent` or `quarantined` | present | present | Touch neither; `failed`; report both paths and what is at O; `quarantine list` shows it as not started and blocked. |
| `quarantined` | present | absent | Touch nothing; `failed` ("quarantine missing, something is at the original path"). |
| `quarantine-intent` or `quarantined` | absent | absent | Something outside the tool removed the item. Archived item: write a `vanished` record (the archive is the copy; restore works); `failed` in the summary. Unarchived regenerable item: skipped `gone`, no record. Never `removed`, never `kept`. |
| `delete-started` or `left` | any | present | A leftover: journal `left` if it is not there, report it, touch nothing (Q is not read; a directory at O is not touched). `failed` in the summary. |
| `delete-started` or `left` | a directory with F0's device and inode | absent | **The leftover was moved back** (a rename keeps the inode): journal `moved-back`, write no `removed` record, `failed` in the summary with "PATH is incomplete: some of its files were already deleted; `disk-cleaner restore RECORD_ID` gives the complete item" (for an unarchived regenerable item: "never archived; rebuildable"). The folder is kept by 7.4 in every mode, before and after this line is written. |
| `delete-started` or `left` | absent, or anything else | absent | Deletion finished before `deleted` was written, or the user removed the leftover: write `deleted`, then the `removed` record (`regenerable-removed` for an unarchived regenerable item) with `completed_by: "unknown"`, once. An entry at O is new and is not touched; the summary names it. |
| `moved-back` | any | any | Report only, as `quarantine list` prints it. |
| `deleted` | any | absent | Write and sync the `removed` record if no synced segment has it. |
| `removed-recorded` | any | absent | Sync the segment holding the record if it is not synced. |

- Records are identified by `(item_id, run_id, outcome)`. When storage holds two records with one
  identity (a lost response on a segment upload, then a retry), readers use the latest record from a
  verified segment, and `manifest list` shows one.
- Applying a finished plan again changes nothing and reports items as already done.
- **Signals.** Outside a deletion pass, Ctrl-C (SIGINT), SIGTERM and SIGHUP all finish the
  current journal write, stop before the next step, and exit 3 with the run id. **During a
  deletion pass** (ASSUMPTION, the owner's):
  - the first Ctrl-C prints "finishing the deletion of PATH (N entries left); press Ctrl-C again to
    stop and leave a partial folder you must remove yourself; the item is in archive RECORD_ID"
    (for an unarchived regenerable item: "the item was not archived; it is rebuildable"), lets the
    pass finish, then completes that item as usual (`deleted`, the `removed` record and its
    manifest segment, 12), starts no other item, and exits 3. The pass takes about as long as
    reading the rest of the item (13.2, 18), which can be minutes for a large `node_modules`. A
    second Ctrl-C or a SIGTERM after the pass ended, or a failed segment upload, stops before the
    remaining steps; the next `--remove` run or resume writes them by the rows `delete-started`
    and `deleted` of the table above;
  - a second Ctrl-C, or a SIGTERM at any point (a shutdown or a service manager, which may follow
    with SIGKILL), stops the pass before its next entry, journals `left`, prints the leftover as
    `quarantine list` does, and exits 3;
  - SIGHUP (the terminal closed) is handled as a first Ctrl-C: the pass finishes, since no one is
    left to choose, and output that can no longer be written is dropped;
  - a SIGKILL or a power loss leaves a leftover without a `left` line; the next run journals it.

### 10.2 Concurrency

One apply per user per machine (D-11): an OS advisory lock on `<data dir>/disk-cleaner/apply.lock`
(the path from the user database, also when `--state-dir` points elsewhere) and a second one on
`<state>/lock`. Both are released by the operating system when the process ends. They use the
primitive of 9.6 (`flock`), are opened close-on-exec so no child can hold them, and a lock call that errors counts as held (exit 1). The file holds the
run id and process id for the message; its content is never trusted as the lock. A second apply
exits 1 at once naming the holder. A dry run takes no lock. Applies by different users, or by two
machines sharing a network home, are not covered (section 17); the no-replace rename and the F0
comparison make the loser skip the item.

Restore takes neither lock, so a restore stays possible while a long apply runs. The two share only
the state directory. Each owns its spool directory (9.6), restore writes only below its target's
parent (13.3), and apply never cleans up a live operation's spool. A restore's target must not
exist, so it is never an item apply is still processing. A restore into a directory that holds an
item apply then processes changes that item's parent, not the item. An item restored into a
directory apply has not yet re-checked is a new entry there, and it makes an enclosing item fail
its re-check.

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
- **The secrets prompt: one prompt for the whole secrets set** (9.2). The user decided that the
  handling options are offered and that uploading to their own storage is a choice they intend
  to use (D-4); the user chose a per-repository answer for unnamed objects (D-16). The mechanics
  below (one prompt, its contents, when it is offered, what it binds to) are the owner's
  ASSUMPTION. An item whose only reasons to be kept are in its secrets set is not skipped
  silently in ask mode on a terminal. It gets this prompt first, listing the whole set:
  - secret-like files: up to 20 by relative path with the rule that matched (name, content,
    global excludes, git directory file), then a count of the rest; never contents;
  - history: each secret-like file name and where it sits (a stash, a local-only commit, an
    unreachable object);
  - unnamed objects: their number, and for each its id, a `git cat-file -p ID` hint for inspecting
    it (shown on the terminal only, never in a record or the journal), its size and what the
    content rule saw, which is one of: "read in full, no pattern matched"; "first 64 KiB read, no
    pattern matched" (64 KiB to 1 MiB); "not read: 1 MiB or more"; "not read: the item's 64 MiB
    content budget was spent" (the figures here and in statement 1 are the limits in effect; the
    binary has no option to change them, and only the library's tests inject smaller ones);
  - checks not completed: each history check that hit a limit or could not run, "not checked";
  - the storage they would go to, and for a remote that is not `crypt`, "these files will be
    stored unencrypted in REMOTE";
  - when the set holds unnamed objects, these three statements, shown every time, word for word:
    1. "Objects of 64 KiB to 1 MiB were read only for their first 64 KiB. Objects of 1 MiB or more,
       and any object after this item's 64 MiB reading budget, were not read at all.";
    2. "'No pattern matched' is not evidence that an object holds no secret. These objects have no
       file name. One may be a real .env or key that was staged and then unstaged. Answering u
       uploads them as they are.";
    3. "Unnamed objects are uploaded only by this answer. Automatic runs never upload them, and
       without a terminal the answer is no.";
  - the answers `u`, upload this item's whole secrets set with the rest of the item, and `n`, keep
    the item.

  `u` is the upload choice for this item only, never for later items (`a` does not carry it).
  There is no answer that uploads part of the set: one answer covers everything listed, so the
  prompt cannot disagree with itself. The item is then asked about as usual (`y`, `n`, `s` and the
  rest), and `n` there still keeps it with nothing uploaded. Typed-ahead input is discarded before
  the prompt, and end of input means `n`. Without a terminal the prompt is never offered and the
  item is kept; the summary line for such an item says that ask mode on a terminal offers the
  choice. In automatic mode the setting of 9.2 decides and no prompt is shown. The answer binds to
  F0, which includes the store's files, so a new secret-like entry or object before step 9 keeps
  the item. The record carries `secrets_uploaded` (the count of secret-like files and history
  names, no names), `unnamed_uploaded` (the count and total bytes, no names or ids) and
  `secrets_choice` (`prompt` or `setting`); the summary counts such items.
- Every answer, `u` included, binds to the F0 it was given against. A failed attempt or a resume
  that restarts the item from step 2 takes a new F0, so a journaled answer is **never reused**:
  the item is asked again, and without a terminal it is kept. Any difference at step 9 keeps the
  item.
- **Automatic** applies to the classes named by `--auto` or `[apply] auto`; default none. Automatic
  items are re-checked and skipped on any difference.
- A dry run never prompts.

## 12. Manifest

### 12.1 Records

One JSON object per line. Every record has `record_id`, `item_id`, `run_id`, `machine_id`, `at`,
`path`, `class`, `action` (the plan's action), `outcome` and `tool_version`. Outcomes: `backed-up`
(written before removal), `removed`, `backed-up-kept` (with the reason), `regenerable-removed`,
`removed-with` (a regenerable child removed inside an archived git item, 6.3; restore points to the
enclosing record), `vanished` (the item disappeared after its backup and before apply removed it).
Where they apply:

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
- `secrets_uploaded`: the count of secret-like files and history names archived under the upload
  choice (9.2; no names);
- `unnamed_uploaded`: the count and total bytes of unnamed objects archived after the answer `u`
  (11; no names or ids);
- `secrets_choice`: `prompt` or `setting`, how the upload choice was made (9.2, 11);
- `completed_by`: `"unknown"` on a `removed` or `regenerable-removed` record written after the
  tool found a leftover's quarantine gone (10.1): the user, or the interrupted pass, finished it;
- `restore`: the method (`archive` or `regenerable`) and the exact `disk-cleaner restore
  <record_id>` command.

URLs pass core's `strip_url_credentials`; error text passes `redact_text`. No record holds a
secret.

### 12.2 Where it lives

- Locally: `<state>/manifest/<run_id>.<seq>.jsonl`, one **segment** per sync, never modified after
  it is written, ending with a trailer line holding the record count and the SHA-256 of the bytes
  before it. `seq` comes from a `segment-reserved` journal line (10.1). A segment is written
  atomically: to `<run_id>.<seq>.jsonl.tmp`, `fsync`ed, renamed to its name with no-replace
  semantics, and the directory `fsync`ed, so a reader never sees a segment being written. A
  leftover `.tmp` file is reported and ignored.
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
  the indexes (a local copy only when its SHA-256 equals the record's `index` digest, otherwise
  downloaded and checked the same way).
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

### 13.2 Walking and deleting

- Archiving, fingerprinting and deletion all work relative to open directory handles: the item is
  opened once with `O_DIRECTORY|O_NOFOLLOW`, every child is examined with
  `fstatat(..., AT_SYMLINK_NOFOLLOW)` first, and every removal uses `unlinkat`. No path is resolved
  a second time, so swapping a directory for a symlink during the run cannot redirect a read or a
  delete.
- **Every open of an entry inside an item or the state directory** (archiving, the secret checks'
  content reads, fingerprinting, restore's checks) is non-blocking and no-follow:
  `openat(..., O_RDONLY|O_NONBLOCK|O_NOFOLLOW|O_NOCTTY|O_CLOEXEC)` (directories add
  `O_DIRECTORY`), then `fstat` on the descriptor must show the kind `fstatat` showed (a regular
  file or a directory) and the same device and inode. Anything else (a FIFO, socket or device
  swapped in after the `fstatat`) is a difference: the item fails, and nothing blocks. Core's own
  readers use `open_regular` (`crates/core/src/regular.rs`, merged in PR 7 at `c81a341`):
  non-blocking and no-follow, then a regular-file check on the open file. It opens by path and
  does not compare the inode, so apply's opens add the handle-relative open and the inode check;
  that every reader of core's item re-scan goes through it is core change C-7.
- A symlink is removed as a link, never followed. A directory whose device or mount id differs from
  the item's, or that is a mount root, is never entered: deletion stops, leaves the rest in
  quarantine and reports the item `failed` (for an archived item the data is in the verified
  archive; an unarchived regenerable item has no other copy).
- The quarantine rename uses `renameat2(RENAME_NOREPLACE)` on Linux and `renamex_np(RENAME_EXCL)`
  on macOS; where the call is unavailable, removal is refused.
- A directory without the owner's write permission is made writable (the owner write bit) only
  inside the quarantine, so that its children can be unlinked. Files are never chmodded, because
  unlinking needs write permission on the directory alone. Removal never runs `git` and never
  touches anything outside the quarantine.
- **The deletion pass** runs once, depth first, immediately after step 9 found Q equal to F0. Each
  directory is listed once, and only names that are in F0 are unlinked; a name that is not in F0
  stops the pass. **One rule, for every entry of an archived item** (regular file, every name of
  a hard-link group, symlink, directory): immediately before the entry's `unlinkat`, its
  **preserved record** (section 4) is computed again, through a descriptor opened on it by the
  open rule above (a symlink is read with `readlinkat`), by the same function that wrote its
  archive entry, and compared with the index record for that path: kind, content SHA-256 and size
  for every name of a hard-link group, symlink target, mode bits, user flags, modification time,
  and every archived extended attribute by name, size and value digest. `fstat` on the
  descriptor before and after the read must show the same device and inode as F0 and the same
  size, modification time and change time as each other, so a write during the read is seen. No
  field stands in for another: size and modification time never replace the content digest. An
  entry with no index record, any difference, or any failed call stops the pass. The only fields
  not compared are the ones this pass has itself changed by then, each for its own reason:
  - a directory's modification time: removing its children moves it;
  - a directory's mode is compared with F0's mode plus the owner write bit when this pass set it
    (above), and on Linux also with the setgid bit cleared, which the kernel does when the pass's
    chmod runs as a user outside the directory's group; any other mode is a difference;
  - a hard-link group's names this pass has already unlinked are missing from the group; the
    remaining names must still share one file.

  Change time and link count are not part of the preserved record, so the unlinks that move them
  never cause a false stop. A directory is compared after its children are gone, through the
  descriptor the walk holds. A child added after the listing makes its `unlinkat` fail with "not
  empty", which stops the pass; entries already removed before that point matched their records.
  The socket `.git/fsmonitor--daemon.ipc`, the one entry F0 holds and the archive does not (section
  4), is unlinked only when it is still a socket with F0's device and inode.
- **An unarchived regenerable item** (`remove-regenerable` without a backup) has no index record,
  only F0. ASSUMPTION (the owner's, agreed by the coordinator): its entries are compared with F0's
  fields (kind, device, inode, size, modification time, change time, mode, flags, link count,
  extended attribute names and sizes; for a later hard-link name without the change time and with
  the link count reduced by the names already unlinked; for a directory the mode rule above and
  no times or link count), and it is not held to the content rule. Why: there is no digest to
  compare with, re-reading would prove nothing, and the user agreed to delete this rebuildable data
  without a copy, which already loses any hand edit inside it (section 17).

  Any difference, failed call or stop leaves Q as a leftover (10.1). **Cost**: every byte of an
  archived item is read once more before it is deleted, once per hard-link name, so a removal
  takes about as long as reading the item again (18).

### 13.3 Restore

`disk-cleaner restore RECORD_ID [--to DIR]`:

1. Finds the record (local manifest, or `--storage` alone on a new machine). It uses a record only
   from a segment whose trailer matches, locally or in storage. A record found only in a damaged
   segment is refused (exit 1, naming the segment) unless `--accept-unverified` is given. The
   output then says the record is unverified, and every part is still checked against that
   record's digests.
2. The target is the original path or `--to`. It must not exist, and neither it nor its staging
   directory may lie inside a `.disk-cleaner-removing-*` directory or any quarantine a journal
   names (exit 1), so a restore can never put data where a user will delete a leftover. Free
   space on its volume must be at least the record's logical `bytes` plus 1 GiB, and extraction
   stops if the stream
   exceeds it. **The state volume**, which holds restore's own spool directory (9.6), is checked
   too. Before the first part, and before each later part, at least `--min-free` (default 1 GiB, or
   `[apply] min_free`, as for apply, section 18) must be free there. A part during which it falls below that floor
   fails the restore: the staging directory is deleted and nothing is created at the target. When
   the target and the state directory are on one volume, step 2 requires `bytes` plus 1 GiB plus
   `--min-free`. The spool directory is removed when the restore ends, also after a failure.
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
   as links to the first name; extended attributes (on symlinks with `XATTR_NOFOLLOW` on macOS and
   `lsetxattr` on Linux); modification times of files and symlinks (with
   `utimensat`/`AT_SYMLINK_NOFOLLOW`, nanoseconds where the file system keeps them); user file flags
   on macOS (`UF_*`, such as hidden); then mode bits and modification times of directories,
   deepest first. **Not restored**: access and change times, inode numbers, owners and groups,
   ACLs, system file flags (`SF_*`; an item with immutable flags is never removed, 7.4), Linux
   `security.selinux` (9.3), and sparseness (section 17).
6. Every entry is compared with the index's preserved record (section 4). A difference in kind,
   size, SHA-256 (for every name of a hard-link group), symlink target or hard-link structure
   (which names share one file) fails the restore: the staging directory is
   deleted, nothing is created at the target, exit 1. Mode bits, modification times (to the
   target file system's precision), user flags and extended attributes that the target cannot
   hold are listed by name; the restore completes and exits 2.
7. The staging directory is renamed to the target with no-replace semantics. A failure at any step
   leaves nothing at the target.

Restore never runs `git` and never contacts a git remote. `regenerable` records have nothing to
restore: restore prints the rule and how the directory is normally rebuilt, and exits 0. A
`vanished` record restores like a `removed` one.

## 14. Platform differences

| | macOS | Linux |
|---|---|---|
| Dry run, `--backup-only`, `--remove`, restore | yes | yes |
| Mount detection | `st_dev`, `statfs` | `statx` mount root and mount id, else `/proc/self/mountinfo`; refuse if neither |
| Placeholders | `SF_DATALESS` | none expected (FUSE is a refused network file system) |
| Extended data | xattrs and resource forks archived | xattrs archived |
| Case | APFS may be case-insensitive: restore refuses collisions | case-sensitive |

Process checks use `sysinfo`. **Windows**: the commands section 5 lists (`apply` in every mode,
`restore`, `quarantine list`, every rclone-running command) exit 4 as the binary's first check,
before the configuration file is read (19, D-13, 8.2). WSL2 is unsupported and untested (5).

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
  - `docs/SCAN-FORMAT.md`, six places, each changed to say the first apply release does not
    contact remotes and archives the whole item instead: the `scope` row ("the apply phase
    re-verifies"); the `all_branches_on_remote` row ("until apply runs `git ls-remote`"); the
    paragraph after the fact table ("a later phase confirms them against the live remote with
    `git ls-remote` at apply"); the global `insteadOf` note ("unverified until apply re-checks them
    against the live remote"); the paragraph on stale tracking refs ("apply (a later phase) must
    re-check them with `git ls-remote`"); and the `ready_bytes` paragraph ("unverified until apply
    re-checks them").

  The quoted phrases above are the **C-4 phrase list**. Tests that compare these strings change with
  them. C-4 belongs to the core owner; until it lands, apply prints the correction in the header
  of every run that selects a git item.
- **C-5** Package evidence per file: core makes its `has_package_evidence` (`crates/core/src/scan.rs`)
  public, or `rescan_item` reports for each file below a `regenerable` or `cache` node whether that
  evidence holds. Apply's package exemption (9.2) uses this and nothing of its own. A core test
  shows, over the NEG-42 fixture, that the answer apply gets for each file equals the one the
  scan's own classification used, so the two cannot drift.
- **C-6** Quarantines and restore staging directories are kept: the scan gives `keep`, with a
  reason that names it, to a directory whose name starts with `.disk-cleaner-removing-` or
  `.disk-cleaner-restoring-` and to every node that contains one, and neither the scan nor
  `rescan_item` descends into such a directory, so no plan ever removes a leftover or the item
  around it and no core walk reads inside one. Until C-6 lands, apply's own check (7.4) keeps such
  items, and because apply's F0 walk comes first and stops there, `rescan_item` is never called on
  them. A core test plans a tree holding a leftover inside a repository and inside a
  `node_modules`, and asserts that no path inside the leftover is opened or listed.
- **C-7** Non-blocking reads in core, a **release precondition**: every file core's scan and
  `rescan_item` read goes through `open_regular` (`crates/core/src/regular.rs`, merged in PR 7 at
  `c81a341`: `O_NONBLOCK | O_NOFOLLOW`, then a regular-file check before any read). PR 7 converted
  `read_small_regular_file` (`scan.rs`), `read_bounded` (`facts.rs`) and `ref_file_ok`
  (`gitshape.rs`); the precondition is that no other reader remains, checked by a core test that
  swaps a FIFO in for each file `rescan_item` reads, between its
  `lstat` and its open, and asserts the re-scan returns an error within 5 seconds (NEG-62).

No scan or plan safety rule changes, except that C-6 keeps more.

## 16. Open decisions

The text above follows the recommended option. D-4 is the user's decision as far as their words
go, sourced in D-4; the rest of D-4 is the owner's assumption. D-1 and D-13 depart from the
user's stated requirements (section 2, TARGET_MISMATCH) and are reported to the user as such.
D-13 and D-17 are the coordinator's assumptions after the reviews of revision 9. D-15 and D-16
are the owner's assumptions, not the user's choices, except D-16's handling of unnamed objects,
which the user chose (option (i), sourced in D-16).

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
- **D-4. Secrets: a handling the user chooses** (9.2, 11). **The user's decision** (Source:
  answer:4577b940-1ee7-4a57-a14c-89e1db1f9384/149728c7-19cb-470a-8dec-786c239d4d77 "offer
  options to user, in my case back it up to my private storage anyway"): the handling options are
  offered to the user, and uploading secret-like files to the user's own private storage is a
  legitimate choice that this user intends to use. The options shown were: keep the whole folder;
  keep those files in a local vault and archive the rest; upload them with the rest.
  **Everything else is the owner's ASSUMPTION**, not the user's:
  - two options in the first release, keep and upload; one prompt per item for the whole secrets
    set, merged with D-16's answer `u` so the two cannot contradict (11); for automatic runs the
    setting `--secrets` / `[apply] secrets`, which never uploads unnamed objects and keeps an
    item whose history checks did not complete (unnamed objects stay per repository, as the user
    chose in D-16);
  - **the default when nothing was chosen is keep**, because keeping loses nothing and an upload by
    mistake cannot be taken back;
  - the unencrypted-storage statement in the prompt and summary, and `--require-encrypted-remote`
    (9.2);
  - **no encrypted-upload option of the tool's own** in the first release (client-side encryption
    with a key in the OS keychain adds a crypto dependency, key loss as a new way to lose the only
    copy, and a restore that needs the key); an rclone `crypt` remote gives encryption today;
  - **the local vault is deferred.** For a git item, the secret material sits inside pack files
    and loose objects shared with the unsaved work, so "the rest" cannot be archived without
    rewriting the object store, which breaks the whole-item archive and exact restore (9.1, 13.3).
    A vault is also a second store on the very disk being cleaned, a single copy that looks like a
    backup, and it needs its own record, permissions, restore path and secret-free handling. Its
    entry conditions: a location under the state directory with owner-only permissions, a
    manifest record for each vaulted file with its digest, a restore that merges vault and
    archive and refuses when either is missing, and a decision for history secrets.
  Cost: until the user chooses upload, items with secret-like material are not reclaimed; the
  summary counts them by rule. **Automatic upload does not reach every item**: `--secrets upload`
  still keeps every item with an unnamed object or a history check not completed, which D-16
  expects to be common in hand-worked repositories; only the prompt in ask mode reclaims those.
  So the user's stated intent, to back such folders up to their own storage anyway, is fully met
  only in ask mode. The user was shown three options and gets two in this release. The
  coordinator reports both points to the user.
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
- **D-13. No Windows operation in the first apply release** (5, 8.2). TARGET_MISMATCH and
  ASSUMPTION: the coordinator's choice, not the user's, who asked for Windows, macOS and Linux.
  Why: Windows removal was already disabled; review rounds kept finding siblings of the last fix,
  and both open revision 9 adversarial findings were in Windows spool cleanup. Cost: no backup,
  removal or restore on Windows, not even of a macOS or Linux archive.
- **D-14. Free-space floor** a fixed `--min-free` (default 1 GiB) and a part size derived from free
  space (section 18), so apply runs on a nearly full disk, which is the case it exists for.
- **D-15. Regenerable children of a git item are archived with it** (6.3), not removed first
  without a backup. ASSUMPTION: the owner's choice between the two options the coordinator offered
  (include or exclude), not the user's. Why: the archive is then truly whole, and a hand-edited file
  in `node_modules` survives. Cost: rebuildable bytes are uploaded and read back (time, egress and
  storage, shown in the dry run), and the secret checks read the children's small files once more
  (package files are exempt, 9.2). Alternative: exclude them; then they are the only data not
  archived, rebuildable by definition, and a hand edit inside them is lost.
- **D-16. The history rule examines every object the network remotes do not hold** (9.2), added
  in revision 7 for review finding R6-1. ASSUMPTION: the owner's choice, not the user's. Chosen:
  one walk from the network tips, the examined set as the store minus that walk, names from tree
  and index entries, the content rule on named blobs, and a blob nothing names keeps the item;
  limits on object count and time keep the item when exceeded; `core.commitGraph=false` and id
  checks on parsed trees and tags (9.2). Rejected: the review's two-walk correction (a nameless,
  pattern-free blob is still uploaded); keeping on any unclassified object (no cheaper, and it
  keeps every repository with a dropped stash); leaving unreachable objects out or running
  `git gc` (destroys recoverable stashes); `git fsck --unreachable` (reads the repository's
  `fsck.*` settings). Revision 9 (commit `fbf25f3`) has the full comparison. Why: keeping
  loses nothing (D-4), while an upload by mistake cannot be taken back. Cost (not measured):
  every apply walks all pushed history of each git directory without the commit-graph, linear in
  the object count; apply already reads and uploads every byte of the item twice. A measurement
  on a repository of at least 5 million objects, with apply's exact options, is a release
  precondition (section 23).

  **The over-keeping cost, stated plainly.** The unnamed-object rule fires on any repository
  where a file version was staged and then replaced before a commit: `git add`, an edit, and
  `git add` or `git commit -a`; `git add -p`; GUI hunk staging. A repository with no remote and
  one harmless file staged twice is kept (NEG-46). Apply runs with `gc.auto=0` and automatic `gc`
  rarely runs in small repositories, so for hand-worked repositories this is likely the common
  case. The only override is the answer `u` at the secrets prompt, in ask mode on a terminal
  (D-4, 11); automatic runs keep these items. The remedy (9.2): inspect with
  `git fsck --lost-found`, and prune with `git gc --prune=now` only if the objects are not
  wanted. Pruning also destroys dropped stashes, which is why apply never prunes. A corpus
  replayed from public clones understates the rate, so section 23 requires those habits in it.

  **Handling of unnamed objects: option (i), the user's choice.** The user chose the per-repository
  answer in ask mode (Source: answer:4577b940-1ee7-4a57-a14c-89e1db1f9384/81b262d5-2447-43d9-9eff-6da6c414ca08
  "Ask per repo (Recommended)"), specified in section 11 as the answer `u`. Offered: (i)
  **chosen**, a per-item answer that uploads the item's unnamed objects after showing their count,
  sizes and content results; (ii) a local vault; (iii) as it is. The question said objects of
  1 MiB or more would be uploaded unscanned. It did not carry three further limits (r8
  acceptance review): partial or no reading, "no pattern matched" is not evidence, and the answer
  exists only in ask mode on a terminal (`--auto` runs reclaim none of these items). Section 11
  states them and the prompt shows them word for word. Revision 11 merges `u` into D-4's one
  secrets prompt (11): `u` there uploads the item's whole secrets set, so the unnamed objects of
  an item that also holds a `.env` are offered too, which revision 10 did not do. The rest of
  D-16 (the examined set, the limits of 10 million objects and 600 seconds, the commit-graph
  setting, the mechanics of `u`) stays the owner's ASSUMPTION. Confidence medium.
- **D-17. An interrupted deletion is never finished by the tool** (10.1, 8.3). ASSUMPTION: the
  coordinator's choice, not the user's. Why: every round since revision 7 found a way for resumed
  deletion or its escape command to delete bytes the archive lacks; anything ambiguous is kept.
  **Cost**, all of it the user's:
  - a leftover is a hidden, incomplete `.disk-cleaner-removing-ID` folder that the user must
    check and remove by hand; its space is not reclaimed until then;
  - until it is removed, **the item around it is kept too** (7.4, C-6): a leftover `node_modules`
    keeps its whole repository from being cleaned;
  - a leftover moved back by the user is detected and kept, never recorded as removed, but it is
    still incomplete (10.1);
  - a quarantine whose deletion never started is complete and listed (5), but it is put back only
    by a `--remove` run, a resume, or the user's own `mv`;
  - Ctrl-C during a deletion no longer stops at once: the pass finishes unless Ctrl-C is pressed
    again (10.1), which can take minutes;
  - every false stop of the deletion pass (13.2), such as a link added to a file of an
    unarchived `node_modules` from a pnpm store outside it while the pass runs, leaves a leftover
    the user must clean by hand.

## 17. Documented limits

- For a removed git item, the verified archive is the only copy apply relies on. Unsaved commits,
  stashes and uncommitted work exist afterwards only in the archive; remote facts were not checked.
  The user's storage is therefore the single point of recovery, and `manifest verify` should be run
  after any change to its lifecycle rules.
- The apply-time live remote check that SCAN-FORMAT.md describes is not performed (section 8).
- **The one disclosed race.** A process can still write into an item through a file it opened
  before the quarantine rename. The deletion pass compares each entry's full preserved record
  with the archive just before that entry's own unlink (13.2), so a write is lost only when it
  lands between one entry's comparison and its own unlink. Only working directories are checked,
  not open files.
- **A hand edit inside a rebuildable directory removed without a backup is lost**: an unarchived
  `remove-regenerable` item (a `node_modules`, a build cache) has no copy, and its deletion pass
  compares only F0's fields, not content (13.2).
- **Leftovers** (D-17): after a kill, a crash or a difference during a deletion, the user must
  check and remove a partly deleted, hidden `.disk-cleaner-removing-ID` folder by hand; its space
  is not reclaimed until then, and the item around it is kept until then too. The archive holds
  the item as at F0; later writes are only in the folder. D-17 lists the whole cost.
- Dependents outside the item are as fresh as the scan (bounded by the plan age); references from
  outside the scanned roots were never seen (SCAN-FORMAT.md, known limits). Two known core gaps,
  open as core follow-ups, are not relied on to be fixed: reference entry names are matched exactly,
  so a case-variant spelling on a case-insensitive file system is not seen as a dependency. Apply
  adds no dependency check of its own.
- Secret detection is best effort (9.2): by name and a bounded content check. Anything it misses is
  uploaded to the user's own storage, unencrypted unless the remote is `crypt`. **Secrets in history
  that a network remote already holds** (reachable from its tracking refs) are archived as they are;
  they were already on that remote. That judgement rests on the local tracking refs, which can be
  stale or set by hand. In history the remote does not hold, these are still uploaded by default: a
  named blob whose names are not secret-like and whose content matches no pattern or lies past the
  64 MiB content budget; a blob of 1 MiB or more under a name that is not secret-like, or with no
  file name because only a tag or ref points at it; and secrets in commit and tag messages. Files
  of a git directory outside `objects/` (`rr-cache` among them) are checked by content only, never
  by name. A consistent substitution through a deliberately edited pack index (9.2) is not
  detected for the commits the walk reads. A `core.excludesFile` inside `includeIf` is not seen.
- Repositories where a file version was staged and then replaced before a commit are kept by the
  unnamed-object rule, likely most repositories worked on by hand (D-16), until the user prunes
  them or answers `u` for them at the secrets prompt in ask mode (11); automatic runs keep them.
  After `u`, unnamed objects are uploaded as they are: those not read, or read only in part, and
  those in which no pattern matched may hold a secret.
- A repository whose store has more than 10 million objects, or whose history check takes more
  than 600 seconds, is kept unless the upload choice is made for it (9.2, D-4); its history is then
  uploaded unchecked, as the prompt says.
- **Secrets the user chose to upload** (D-4) are stored in the user's own storage like the rest of
  the archive, unencrypted unless the remote is `crypt`. The local vault option is not in this
  release.
- **Not preserved** (section 4): what the archive does not carry, or carries without restoring, is
  lost with a removal: owners and groups; ACLs (`system.posix_acl_*`, macOS ACLs);
  access, change and birth times; inode and device numbers and link counts; system file flags
  (`SF_*`; an item with immutable flags is never removed, 7.4); `security.selinux` (recorded in
  the index, not restored); sparseness (sparse files are restored dense); the harmless attributes
  9.3 drops (`com.apple.quarantine`, `com.apple.provenance`, `com.apple.lastuseddate#PS`, an
  all-zero `com.apple.FinderInfo`); and the link from a file inside the item to one outside it (a
  hard link to outside is restored as an independent file). macOS may set `UF_TRACKED` or
  `UF_COMPRESSED` on its own; restore then lists the flag (exit 2), which is safe.
- Sync-folder detection covers the providers and markers listed in 7.4; another sync client, or a
  Seafile library outside `~/Seafile`, is not recognised. macOS `~/Desktop` and `~/Documents` are
  always treated as synced.
- Regenerable children of a git item are archived with it (6.3, D-15). A `remove-regenerable` item
  removed on its own without `--backup-regenerable` has no archive: a file edited by hand inside it
  (a patched `node_modules` package) is lost, as the plan's regenerable rule accepts.
- Verification proves the stored object equalled what was sent at the time; `manifest verify` notices
  later loss, it cannot prevent it.
- The lock does not cover two users, or two machines sharing a network home. On such a shared
  home, the advisory locks that keep spool directories apart (9.6) may not hold either, so no
  spool there is ever cleaned up automatically. An rclone child that hangs after the tool died
  keeps its spool until it exits (9.6); the tool reports it and does not kill it. Hypothesis, not
  tested: Go passes non-close-on-exec descriptors to children, so rclone's subprocesses (ssh for
  sftp, a password or token command, an ssh `ControlPersist` master) may inherit the spool lock
  and keep a spool long after rclone exits: too long, never too early.
- Whether `rclone rcat --size` avoids a local spool is tested for the `local` backend only (9.4);
  for other backends the part size is bounded by the state volume's free space instead. The
  `local` backend never spools, so the tmpfs test cannot show that a spooling backend honours
  `--temp-dir`; that is unverified until the click-test's real remote, where the tester observes
  the spool directory's peak size and the state volume's free space during each upload
  (section 24).
- Removal is refused on file systems outside the allowlist of 7.4, which includes the overlay file
  systems of most development containers.
- An encrypted rclone configuration is refused (9.6).

## 18. Resource limits

- Archives are streamed; local disk use is the journal, the manifest segments and the index copies.
- **Floor.** Before each item, and before each part, apply checks free space on the state
  directory's volume, which also holds the spool directories (9.6). Restore checks the same floor
  before each part (13.3). `--min-free` (default 1 GiB, a fixed amount, not a share of the
  volume) must remain free there.
- **Part size.** At the start of each attempt the part size is
  `min(--part-size, (free on the state volume - min-free) / 2)`, rounded down to a MiB, with
  `--part-size` defaulting to 1 GiB. Below 64 MiB the item is not started ("not enough free space
  on VOLUME"). The part size is journaled with the attempt (9.4); a later attempt may choose a
  different one. Worked examples, with the default 1 GiB floor: a 500 GiB volume 97% full (15 GiB
  free) uses 1 GiB parts; 1.6 GiB free uses 307 MiB parts; 1.1 GiB free stops before the item.
- A part during which the state volume falls below the floor fails the attempt (fail closed);
  the next attempt sizes its parts again.
- Streams use fixed buffers (1 MiB per stream); nothing holds a whole file in memory. Two
  structures grow with the item: the fingerprint F0, about 200 bytes per entry (half a million
  entries in a `node_modules` archived with its repository, D-15, is about 100 MB), and the history
  rule's object table (9.2), one object id plus one byte (type and mark) per object, kept sorted in
  the order `--batch-all-objects` prints: at the 10 million object limit about 210 MB for SHA-1
  and 330 MB for SHA-256, one git directory at a time. git's
  own memory for `rev-list --objects` on such a store is not bounded by the tool. Only the time
  limit bounds it, and section 23 measures it.
- One item at a time; one rclone process at a time.
- **Deletion reads the item again** (13.2): every byte of an archived item is read once more just
  before it is deleted, once per hard-link name, with the same fixed buffers, and the preserved
  record of each entry needs the index record in memory (about the size of F0 again). A removal
  therefore takes about as long as reading the item, and longer than the unlinks alone; the dry run
  and the prompt show the bytes to be read.
- Bytes freed are measured as the change in free space, alongside the sum of sizes.

## 19. Exit codes and output

- 0: dry run completed, or every selected item ended removed (alone or `removed-with` its
  enclosing item), `backed-up-kept` by request (`--backup-only`, answer `s`), answered `n`, or was
  already done.
- 1: refused before acting (usage, inputs not bound, plan too old, lock held, root without
  `--allow-root`, rclone or git missing or too old, an encrypted rclone
  configuration, storage unusable, `--require-encrypted-remote` with a plain remote).
- 2: completed, but at least one item was skipped, failed, vanished, left in quarantine, or ended
  `backed-up-kept` for any reason other than a request.
- 3: interrupted; the run id is printed for `--resume`.
- 4: not supported on this platform in this release (Windows, 5). Nothing was created or run.
- `quarantine list` exits 0, or 1 when the state directory cannot be read.

The summary lists every item with its outcome and reason, bytes freed (measured and summed), bytes
uploaded, whether the storage is encrypted, and where the manifest is. `--json` writes the same as
JSON. No output, journal line or record holds a secret value or a credential URL.

## 20. Acceptance criteria

Tests live in `crates/apply/tests/` and run in CI on ubuntu and macOS; the Windows runner runs the
refusal test of AC-30 and core's own tests. Unless stated, they use temporary directories, the
in-process `LocalDirStore` or `FaultStore` (a wrapper that injects
partial writes, flipped bytes on read, success reported as failure, objects deleted after
verification, stalls), and local git repositories. No test contacts a network host. A separate job
runs the flows through the real `rclone` binary against an rclone `local` remote defined in a
temporary config file (no credentials anywhere). A library step hook lets a test act between any
two steps; it exists only in test builds (AC-35).

- **AC-1** Dry run is the default: without `--backup-only` or `--remove`, apply prints what it would
  do with the fresh re-check result and writes nothing: no state directory, no storage object, no
  file anywhere, and runs no git command outside 7.5. It runs the secret checks of 9.2, the
  history check included, reports "would keep" with the reasons a real run gives, and prints the
  per-rule counts of kept items and the bytes the deletion pass would read again (18); the ask
  prompt shows the same figure per item. Verify by: `dry_run.rs` snapshots the tree, state path and store before and after (zero store writes), compares the dry run's kept items and per-rule counts with a `--backup-only` run on a copy of the same fixtures, and checks the bytes-to-read figure against the fixtures' logical size counted once per hard-link name.
- **AC-2** `--backup-only` does steps 1 to 6 and removes nothing, also when an earlier run left a
  quarantine or a leftover (reported only, 6.1), and so does a resume of a `--backup-only` run;
  `--remove` does the whole sequence. Verify by: `modes.rs` on macOS and Linux, with a leftover
  from an earlier `--remove` run present.
- **AC-3** Plan and scan are bound (7.1): schema, scan identity, per-item path and class, no
  duplicate items, no future dates; the plan age is enforced at start and before each item.
  Verify by: `inputs.rs` with a clock injected through the library.
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
  hardened runner with `protocol.allow=never`, and its subcommand is within the set of 7.5. Every
  history command of 9.2 carries `-c disk-cleaner.phase=history`, `-c core.commitGraph=false` and
  `--no-replace-objects`, and none carries `--use-bitmap-index`. A git older than 2.32 is refused
  at start (7.5).
  Verify by: `offline.rs` puts a logging git shim first on `PATH` for full apply, resume and restore runs and asserts the subcommand set (found after the runner's `-c` pairs and `--no-replace-objects`) and `protocol.allow=never` on every call; for the calls marked `disk-cleaner.phase=history`, the other two overrides and no `--use-bitmap-index`, and that each of the six history command shapes of 7.5 appears marked at least once per git directory that has a network tip (the five other than `rev-list` without one); a local test server that counts connections sees none. The behavioural cases NEG-45 (graft) and NEG-47 (commit-graph) fail an implementation that omits an override on `rev-list`.
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
  local-only history including stashes, reflogs, the index and unreachable objects, global
  excludes with includes, content, git directory files); the history rule fails closed on its
  limits and on an unnamed object; the upload choice (9.2) archives them; a non-`crypt` remote is
  warned about and `--require-encrypted-remote` refuses it. Verify by: `secrets.rs` plants fake values for each rule
  (global-excludes matches set through a fake account home; the history fixtures of NEG-41, NEG-43,
  NEG-45, NEG-46 and NEG-47, loose and packed, SHA-1 and SHA-256; and the C-5 equality test of
  section 15 for the package exemption) and searches every stored object,
  segment, journal line and output for them; the search extracts each archive, and inside every
  extracted git directory inflates every loose object and every packed object (with
  `git cat-file --batch-all-objects --batch` run on the test's own copy), so a value held only in
  the object store is found. A control run with the upload choice (`u` at the secrets prompt
  through the library's prompt interface) must find each value in the inflated objects, which
  proves the search looks there.
- **AC-13** The archive is a deterministic pax tar as in 9.3, with extended attributes recorded and
  the listed harmless ones dropped, split into parts whose length is known before upload, with a
  self-describing index whose preserved records (section 4) give every name of a hard-link group
  the group's size and digest. Verify by: `archive.rs` archives a tree twice with different part
  sizes and compares the concatenated streams byte for byte, and checks long and non-UTF-8 names,
  symlinks, internal hard links, xattrs and a resource fork (macOS), the Linux namespace rules of
  9.3, and a part size small enough to split a file; a completeness test parses the produced tar
  stream itself, not the record function, and checks that every header field and pax key it
  carries for an entry is in that entry's record or on the not-preserved list of section 4, and
  that each later hard-link name's record holds the group's size and digest.
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
  entered, no-replace rename, nothing outside the quarantine touched, every open non-blocking and
  checked for kind and inode. Immediately before each entry's `unlinkat`, the deletion pass
  recomputes that entry's full preserved record through a descriptor, with the archive writer's
  function and `fstat` stable around the read, and compares it with the index record, excluding
  only the pass-caused fields 13.2 names; an unarchived regenerable item is compared on F0's
  fields. Any difference, a missing record or a failed call stops the pass and leaves a leftover.
  Verify by: `remove.rs` (a symlink to an outside directory; a subdirectory swapped for such a
  symlink during archiving and during deletion through the step hook; a read-only tree; an
  occupied rename target; the cases of NEG-61, NEG-62 and NEG-66 through the step hook between
  unlinks); a unit test asserting that the pass and the archive writer call one record function
  and that every field the writer emits is compared or is one of 13.2's named exclusions; a
  broken variant that skips the content digest for later hard-link names, and one that skips
  directory extended attributes, each of which must fail NEG-61; and the Linux bind-mount job.
- **AC-20** A `remove-regenerable` item is removed only while a fresh plan still says so, with no
  backup unless asked. Verify by: `regenerable.rs`.
- **AC-21** Every transition is journaled before it takes effect; every row of the resume table in
  10.1 leads to its stated outcome; every apply first reports every quarantine and syncs unsynced
  records; a resume refuses changed inputs and continues only work before deletion. A deletion
  that began is never continued: after `delete-started`, no run in any mode deletes, renames,
  opens or lists anything inside Q. A rename back happens only after the identity check of 6.1
  with nothing at O; a leftover moved back to O is recorded `moved-back`, never `removed`, and
  kept. Verify by: `resume.rs` stops at each transition in turn
  (between two `unlinkat` calls and before the `deleted` line included), then resumes and also
  runs a new `--remove` apply; builds each O and Q combination of the table by hand, with a
  directory, a file and a symlink at O and a replaced Q (NEG-64, NEG-65); runs every
  later command through the library's recording file-system interface and asserts that no call
  names a path inside Q; and kills the real binary during an upload and during a deletion.
- **AC-22** Applying a finished plan again changes nothing and reports items as done. Verify by:
  `resume.rs`.
- **AC-23** One apply per user per machine (10.2), the lock path independent of `HOME` and
  `XDG_*`, no stale lock after a kill, root refused without `--allow-root`. Each operation owns
  its spool directory; one teardown routine, at an operation's own end and for every other
  operation's spool, removes a spool only when fresh exclusive locks on both lock files are
  granted after the tool's own descriptors are closed (9.6); every lock is a `flock` lock and is
  close-on-exec except the inherited spool lock, a lock call that errors counts as held, and a
  missing lock file next to an existing spool directory counts as held.
  Verify by: `lock.rs` with two processes under different environments and state directories, a killed holder, and an injected lock error; and `spool.rs` on macOS and Linux, which runs a restore whose fake rclone stalls with a file in its `--temp-dir`, starts an apply meanwhile, and then lets the restore finish; kills only the parent while its fake rclone child stays stalled; deletes a dead operation's `ID.spool` while its child still holds it; lets a fake rclone exit normally while a descendant it started keeps the spool descriptor and keeps writing (NEG-63); and checks a killed operation's leftover spool directory that the next operation removes.
- **AC-24** Free space and budgets follow section 18 and 9.5: the floor is checked on the state
  volume (which holds the spool directories) before each item and part, the part size is
  derived from free space and journaled per attempt, the upload budget uses logical size, and
  three consecutive storage failures stop the backup stage. A 500 GiB volume 97% full runs with 1 GiB parts. rclone's `local`
  backend does not spool. Restore checks the state volume's floor before each part and stops when
  it is crossed (13.3). Verify by: `limits.rs` with an injected free-space reader and `FaultStore`
  (the worked examples of section 18 as cases, and restore's floor before and during a part); and `rclone_spool.rs` in the Linux job, which runs
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
  flags and hard-link structure; what 13.3 does not restore is listed, not silently dropped.
  Verify by: `restore.rs` round trips for each action and for a `vanished` record.
- **AC-28** Restore works on a new machine from storage alone. Verify by: `restore.rs` with an empty
  state directory and `--storage`.
- **AC-29** Nested items follow 6.3, including `--backup-only` and a nested git item; regenerable
  children of a git item are archived and removed with it, not before it. Verify by: `nested.rs` (a
  repository holding a removable `node_modules`: removed with the repository and present in its
  archive; the repository skipped, then `node_modules` handled on its own; `node_modules` outside
  `--class`; backup-only; a nested repository).
- **AC-30** Platform behaviour of section 14: case and normalisation collisions refused on restore,
  read-only files restored. On Windows, each command 5 lists exits 4 as the binary's first check,
  before it reads the configuration file or validates other arguments, and before the state
  directory, a lock file, or any rclone or git process exists, while `scan`, `plan` and the local
  `manifest list` and `manifest find TEXT` run and create nothing. Verify by: `platform.rs` on
  macOS and Linux; on the Windows CI runner, `windows_refusal.rs` runs the real binary for each
  refused command and mode (NEG-60) with a fake rclone and a fake git first on `PATH` that record
  any start, and asserts exit 4, the message, no state directory under a fresh account home,
  the account's LocalAppData folder or `--state-dir`, and no recorded start.
- **AC-31** Exit codes and the summary follow section 19; no secret value or credential URL appears
  in any output, journal or record. Verify by: `output.rs` with a remote URL holding a fake token.
- **AC-32** No program named by scanned content runs during re-check, archive, removal or restore.
  Verify by: `hardening.rs` with hooks, a `.gitattributes` filter, an fsmonitor setting and config
  keys naming programs, each writing a marker file.
- **AC-33** rclone is used as 9.6 states: minimum version checked, `--ask-password=false` on every
  call and `--temp-dir` set to the operation's own spool directory, `rclone-tmp/OP` in the state directory (9.6), on every data-moving call, an encrypted configuration refused at start, wrappers resolved with
  `config redacted` only (encrypted only when every path reaches `crypt`), stderr redacted.
  Verify by: `rclone_cli.rs` with a fake rclone that logs its arguments and stdin reads, and the real rclone job with a temporary config file that carries rclone's
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
  lands, every apply run that selects a git item prints the correction in its header.
  Verify by: a core test renders the plan text and the report, reads `SCAN-FORMAT.md`, collapses
  every run of whitespace (line breaks included) to one space, and asserts that none of the C-4
  phrase list (section 15) appears and that no sentence pairs "apply" with `ls-remote`,
  "re-check" or "re-verif"; a control copy of `SCAN-FORMAT.md` with one phrase restored, wrapped
  across a line break, must fail it; meanwhile `output.rs` checks the header line.
- **AC-38** The secrets prompt follows 11. It is offered only in ask mode on a terminal, only when
  every reason to keep the item is in its secrets set, and only for that item; it lists the whole
  set (files by rule, history names, unnamed objects with each size and content result, checks
  not completed, the storage and the unencrypted statement for a non-`crypt` remote), and shows
  the three statements of 11 word for word whenever the set holds unnamed objects. `u` uploads
  the whole set of that item and nothing of any other; after `u` the item is asked about again;
  the record carries `secrets_uploaded`, `unnamed_uploaded` and `secrets_choice`. No answer is
  reused after F0 is taken again (11). Verify by: `ask.rs` through the library's prompt interface and the binary through a pseudo-terminal, comparing the prompt with the three statements of 11 and with each content result (read in full, first 64 KiB, 1 MiB or more, budget spent); and `secrets.rs`, which shows the item's secret values and unnamed values in the inflated archive after `u` and in no stored object after `n`, and no value of a second item in the same run in any stored object.
- **AC-39** Quarantines follow 5, 7.4, 10.1 and 13.3: `quarantine list` prints every quarantine a
  journal names that still exists, in each state, with the text 5 gives for that state (the
  put-back commands for one not started; the record, archive, restore command with an absolute
  target outside the quarantine's parent and both statements for a leftover or a leftover moved
  back; what is there and the restore command for a replaced Q); no command deletes a quarantine; restore refuses a target or staging directory inside a
  quarantine; an item that holds or lies inside a quarantine, a restore staging directory or a
  leftover moved back is kept in every mode, `--backup-only` and dry runs included, and no walk
  descends into it, by apply and by core's plan (C-6). Verify by: `quarantine.rs` with a not
  started quarantine (O free, and O holding a directory, a file and a symlink), archived and
  unarchived leftovers and a leftover moved back, comparing the printed text and the JSON with
  the journal and records, and the file system before and after; `restore.rs` with `--to` inside
  a leftover (relative and absolute); `nested.rs` with a leftover inside a repository and inside a
  `node_modules`, in every mode, through the recording file-system interface (no call names a
  path inside the leftover); and the core test of C-6.
- **AC-40** The secrets handling follows 9.2 and D-4: with nothing chosen, an item with a
  non-empty secrets set is kept; in ask mode on a terminal the user chooses per item at the
  secrets prompt; in automatic mode `--secrets` or `[apply] secrets` decides, default `keep`, and
  `upload` there never uploads unnamed objects and keeps an item whose history checks did not
  complete; a choice uploads only the secrets set of the item
  it was made for, as seen at its F0; the vault option does not exist in this release. Verify by:
  `secrets.rs` and `ask.rs` with two items in one run, each holding a fake `.env`, a history name
  and, for one, an unnamed object, under each combination of mode, answer and setting, searching
  every stored object for each fake value (NEG-67).
- **AC-41** Signals during a deletion pass follow 10.1: the first Ctrl-C prints the message 10.1
  gives, lets the pass finish and completes that item's record and segment, starting no other
  item; a second Ctrl-C or a SIGTERM stops it before the next entry and
  journals `left`; SIGHUP lets it finish; outside a pass each stops before the next step; every
  case exits 3 with the run id. Verify by: `signals.rs` runs the real binary on a large fixture
  through a pseudo-terminal and sends each signal through the step hook at a chosen entry
  (NEG-68).

## 21. Negative cases

- **NEG-1 (AC-1)** A dry run with unreachable storage still completes with zero store calls and no
  prompt.
- **NEG-2 (AC-2)** `--backup-only --remove` together: usage error, nothing touched.
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
  the upload choice they are archived; with `--require-encrypted-remote` and a plain remote, exit 1
  before any upload. A `.DS_Store` matched only by the global excludes file does not keep the item;
  a `node_modules` ignored by the repository's own rules and also by the global file is not flagged.
- **NEG-13 (AC-13)** An xattr over 64 KiB: item unsupported, skipped. An archive whose second
  hard-link name was given no size or digest in its record (a broken variant of the writer): the
  completeness test of AC-13 fails.
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
  record, unless it is an unarchived regenerable item the plan let go without one; never two records for one identity in `manifest list`; every quarantine renamed back or
  reported. A resume with a different plan, scan or flags: refused. Both O and Q absent before
  `delete-started`: a `vanished` record, not `removed` or `kept`. A Q at `quarantined` with O
  absent: renamed back without being read, `kept`. Stopped after `delete-started` (archived and
  unarchived regenerable items), with nothing changed, and again with a file rewritten with equal
  length and its time set back, a new file, and a FIFO with no writer planted in Q: a resume, a
  new `--remove` run, a `--backup-only` run and a dry run each report the leftover, delete
  nothing, never hang, and make no call naming a path inside Q (failing any implementation that
  compares or finishes Q); only the `--remove` run and the resume journal `left`. Q removed by
  hand: the next `--remove` run, not a `--backup-only` or dry run, writes `deleted` and the record
  with `completed_by: "unknown"` once; `quarantine list` omits Q; a new directory at O: untouched.
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
  a case-insensitive or normalising file system: refused, naming them.
- **NEG-25 (AC-31)** A remote URL with an embedded fake token: absent from output, journal and
  records.
- **NEG-26 (AC-32)** A repository with a `.gitattributes` filter, hooks and config naming programs:
  no marker file is written during re-check, archive, removal or restore.
- **NEG-27 (AC-29)** A repository whose nested `node_modules` is outside `--class` or `--item`,
  with `--remove`: the repository is skipped. The repository answered `n` in ask mode: it is kept,
  and its `node_modules` is then asked about on its own (6.3). The same with `--backup-only`: the
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
  non-local remote flow or its spool observation: not accepted.
- **NEG-32 (AC-12)** A repository with a network remote whose tracking refs hold all branches, and:
  a fake `.env` set aside with `git stash -a`; a commit adding `id_ecdsa` that was then reset away
  (reachable only from a reflog); an unpushed commit adding `.env.production`; a stash whose index
  commit holds a staged `credentials.json`. Each alone keeps the item by default, with a reason
  that names the file and not its contents; with the upload choice the item is archived; the fake
  values appear in no output, journal line or record. A `.env` in a commit the remote already holds
  does not keep the item (section 17).
- **NEG-33 (AC-12)** A global excludes file named only through `include.path = ~/...` in the
  user's global config: its matches are found. An `https` URL whose user name is 40 hexadecimal
  characters, with no password, in `.git/config`: secret-like. A `test/fixtures/cert.pem` inside an
  installed package in an ignored `node_modules`: not secret-like by name.
- **NEG-34 (AC-2)** A `--backup-only` run, and a dry run, after a `--remove` run was killed with an
  item in quarantine after `delete-started`: the leftover is untouched and reported, and the item
  is not selected again; the next `--remove` run does the same.
- **NEG-35 (AC-29)** A repository whose `node_modules` holds a hand-edited file: after `--remove`
  and `restore`, the edited file is back with its content.
- **NEG-36 (AC-7)** A file bind-mounted into the item from outside, and an item on a vfat file
  system: skipped, and no byte of the bind-mounted file appears in any stored object.
- **NEG-37 (AC-27)** An archive whose hard-link groups differ from the index: the restore fails,
  nothing at the target, exit 1. A target that cannot hold nanosecond modification times or an
  extended attribute: the restore completes, lists them, exit 2. A source with `security.selinux`
  labels restored on a system that relabels: not reported as a difference.
- **NEG-38 (AC-33)** A `union` remote with one `crypt` upstream and one plain upstream: reported
  unencrypted, and `--require-encrypted-remote` refuses it. `RCLONE_TEMP_DIR` set in the
  environment: rclone still uses the tool's `--temp-dir`.
- **NEG-39 (AC-37)** A plan whose `remote_facts_note` still promises an `ls-remote` check: the run
  header states that apply does not contact remotes and archives the whole item.
- **NEG-40 (AC-12)** A tracked `.env` pushed with placeholder values and then filled in locally
  without a commit; a `credentials.json` staged with `git add` but never committed: each keeps the
  item by default.
- **NEG-41 (AC-12)** The history check fails: a corrupt object; a tree missing from a repository
  that is not a partial clone (made with `git mktree --missing`, which `--missing=allow-any` would
  let through); the time limit reached (injected as 1 second, with a git shim that stalls); the
  object limit exceeded (injected as 10 objects); or a file cannot be read for the content check.
  Each keeps the item, the reason names the check, and nothing is uploaded. A blobless partial
  clone, with git's warning on stderr: not kept for the blobs it lacks, and no connection is
  counted (AC-9).
- **NEG-42 (AC-12)** A `node_modules` child of a git item whose installed package ships a test
  private key: the repository is not kept for it. The same key file placed directly in
  `node_modules` with no package evidence: the repository is kept. A file inside an installed
  package (below its `package.json`) whose text holds a provider's documented example access key
  and a PEM private-key header: not kept for it; the same file placed directly in
  `node_modules`: kept.
- **NEG-43 (AC-12)** A repository whose network remote's tracking ref equals HEAD, and, each
  alone, a fake value that matches no content pattern (a `DB_PASSWORD` line with a fake value):
  (1) a `.env` set aside with `git stash -u`, then `git stash drop`; (2) a `.env` added with
  `git add`, then `git rm --cached`, then deleted from the working tree; (3) an unpushed commit
  adding `.env` and a later unpushed commit renaming it to `env.bak` with `git mv`; (4) an unpushed
  commit adding `a.txt` and `config/.env` with identical bytes and a later unpushed commit removing
  `config/.env`; (5) case 1 followed by `git repack -a -d -k`, and again by
  `git repack -d --cruft`, so that the objects are packed; (6) cases 1 and 4 again in a
  repository made with `git init --object-format=sha256`. Each keeps the item by default, case 2 with the
  unnamed-object reason and the others naming `.env`; with `u` at the secrets prompt the item is archived
  and AC-12's inflate-all search finds each value.
- **NEG-44 (AC-12)** An ignored, binary `android/app/release.keystore` in a repository whose root
  holds a `package.json`: the item is kept by the name rule; the package exemption does not reach
  outside a regenerable or cache directory.
- **NEG-45 (AC-12)** Each alone keeps the item by default: a fake PEM private-key header in a
  `rr-cache` preimage; a pack file (`pack-*.pack`) without its `.idx`. A blob that only an
  annotated tag object points at, the tag's ref deleted: not kept by the unnamed-object rule, and
  checked by content. Core's layout check refuses replace refs and refs at blobs (9.2). So these
  cases call the library's history check on the git directory directly, and each also runs end to
  end, where the item is not removed: (1) a replace ref that substitutes, for a local-only tree
  naming `.env`, a tree without it: kept, naming `.env`; (2) `git replace --graft NETWORK_TIP
  LOCAL_COMMIT`, where the local-only commit adds a fake `.env`: kept, naming `.env`. A control
  walk without `--no-replace-objects` must reach that `.env` blob, so an implementation that omits
  the flag on `rev-list` fails this case; (3) a lightweight tag at a blob: not kept by the
  unnamed-object rule, and checked by content.
- **NEG-46 (AC-12)** The documented cost: a repository with no remote and one harmless `app.py`,
  staged with `git add`, edited, staged again and committed, holding no secret-like file. The item
  is kept with the unnamed-object reason, and the reason text holds the `git fsck --lost-found`
  remedy and the dropped-stash warning. The summary counts it under the unnamed-object rule. The
  same with `git commit -a` instead of the second `git add`: kept the same way.
- **NEG-47 (AC-12)** A repository whose network remote's tracking ref equals its network tip T,
  and a local-only commit L on a branch adding a fake `.env`, where L's parent is T's parent (L is
  a sibling of T, so the forged edge makes no cycle). The test writes a commit-graph that
  holds T and L, then edits T's entry so that T's parent is L, and recomputes the file's trailing
  checksum. A control walk with the graph enabled must reach the `.env` blob from T, which proves
  the graph is load-bearing. The library's history check keeps the item, naming `.env`, so this
  case does not depend on what core's scan makes of it. Core's runner does not disable the graph,
  so its scan may see L as pushed and its plan may give either a removing action or `ask`. End to
  end, the item is not removed and the value is in no stored object in both cases: apply keeps it
  under a removing action, and does not select it under `ask`. The test runs whichever case core
  produces and states which. With the upload choice and a removing action, AC-12's search does
  find the value.
- **NEG-48 (AC-23)** A restore whose fake rclone writes a file into its `--temp-dir` and then
  stalls, and an apply started during the stall: the restore's spool directory and file are
  untouched, and the restore completes when the stall is released. A spool directory left by a
  killed apply: removed by the next apply. A spool directory whose lock is held by a live process
  of the test: not removed. A cleanup started through the step hook between an operation's
  creation of its lock file and its lock: it waits for the guard, and the new operation's spool
  survives. A `manifest verify --deep` running during an apply: each has its own spool directory.
- **NEG-49 (AC-24)** A restore with less than `--min-free` free on the state volume: refused
  before the first part. The state volume falls below the floor during a part: the restore fails,
  the staging directory is deleted, nothing is created at the target, and the spool directory is
  removed. The target and the state directory on one volume with `bytes` plus 1 GiB free, but not
  `bytes` plus 1 GiB plus `--min-free`: refused.
- **NEG-50 (AC-1)** A dry run over a repository whose `.env` was set aside with `git stash -u`
  and then dropped, and one kept by the unnamed-object rule: both are reported "would keep" with
  their reasons, the per-rule counts show them, and nothing is written.
- **NEG-51 (AC-38)** The secrets prompt, each alone: an item with an unnamed object and also a
  `.env` (one prompt listing both; `u` uploads both; `n` keeps the item with neither uploaded);
  the same item as NEG-46 in automatic mode with `--secrets upload`, and in ask mode with no
  terminal (no prompt, kept); `u` typed ahead before the prompt (discarded; end of input then means `n`); `u`
  for the first of two such items followed by `a` (the second is asked again, not uploaded); `u`
  then `n` at the item prompt (kept, nothing uploaded). The prompt for objects of 10 KiB, 200 KiB
  and 2 MiB, read in that order, and a fourth of 10 KiB, with an injected budget of 74 KiB (what
  the first two use), shows "read in full", "first 64 KiB read", "not read: 1 MiB or more" and
  "not read: the item's 74 KiB content budget was spent" (the injected figure in place of
  64 MiB), and the three statements of 11; the binary through a pseudo-terminal shows them with
  64 MiB.
- **NEG-52 (AC-38)** After `u` and `y`, a new loose object written into the store before step 9:
  the item is kept, nothing removed. After `u` and `y`, the first attempt failed at the storage
  and a new unnamed object created before the retry: the retry asks again and shows the new
  object; without a terminal, and in a `--resume` without one, the item is kept and the new
  object is in no stored object.
- **NEG-53 (AC-39)** Leftovers from NEG-19: `quarantine list` prints each with its restore command
  and both statements, and for the unarchived one says "never archived; rebuildable".
  `disk-cleaner quarantine finish` and `--discard-changes` are usage errors (no such command or
  option). `restore RECORD_ID --to` a path inside the leftover, given relative from inside it and
  absolute: refused (exit 1), nothing written. A git item that contains a leftover `node_modules`
  quarantine, and a `node_modules` that contains a leftover: kept by apply with the reason naming
  the folder in a dry run, a `--backup-only` run and a `--remove` run, and given `keep` by core's
  plan (C-6); nothing archived from inside the leftover, and the recording file-system interface
  shows no call naming a path inside it in any of the three runs, `rescan_item` included.
- **NEG-54 (AC-23)** A non-blocking lock call made to fail with `ENOLCK`, through the library's
  file-system interface, on another operation's spool lock: that spool is left. The same on the
  apply lock: exit 1 before anything is done. A state directory on a file system outside the 7.4
  allowlist (injected): leftover spools are reported and not removed.
- **NEG-55 (AC-9)** A fake git reporting 2.31.0, and one printing an unparsable version: exit 1 at
  start, naming the version, nothing done.
- **NEG-56 (AC-23)** On macOS and on Linux, during a restore whose fake rclone child stalls in a
  transfer with a file in its spool, only the parent is killed (SIGKILL), then a new apply starts
  at once: the child still holds the spool lock, so the new apply leaves that spool and file
  alone and reports the surviving process; the next operation after the child exits removes the
  spool. A variant where the child hangs forever: no later apply waits for it. A variant where the
  test deletes `ID.spool` while the child still runs: the spool is kept and reported.
- **NEG-57 (AC-16)** A crash while a manifest segment is written: only a `.tmp` file is left,
  reported and ignored, and the segment is written under its name later. A restore whose record
  appears only in a damaged segment: refused (exit 1) without `--accept-unverified`; with it, the
  output says the record is unverified and a part altered to differ from the record's digest
  still fails the restore.
- **NEG-58 (AC-20)** A `remove-regenerable` `node_modules` holding, directly in it, a file whose
  text has a PEM private-key header. Removed without a backup: the 9.2 checks do not run, and the
  item is removed (nothing is uploaded). The same with `--backup-regenerable` and nothing chosen:
  kept by the content rule, nothing uploaded.
- **NEG-59 (AC-12)** Network tips: a repository with remotes `origin` (a network URL) and `mirror`
  whose `remote.mirror.fetch` writes into `refs/remotes/origin/*`, and a local-only commit holding
  a fake `.env` that only `refs/remotes/origin/feature` reaches, set there through `mirror`'s
  refspec: the ref is not a network tip, the commit is examined, and the item is kept. A control
  where `refs/remotes/origin/feature` is set by hand with `git update-ref` and no other refspec
  matches it: the commit is subtracted and the item is archived, the case section 17 discloses; the
  test asserts this so a later change to the rule is seen.
- **NEG-60 (AC-30)** On the Windows CI runner, each alone: `apply` dry run, `--backup-only`,
  `--remove`, `--resume`; `restore` with and without `--to`; `quarantine list`; `manifest list
  --storage`, `manifest find TEXT --storage`, `find --files`, `verify`, `orphans`; and `apply`
  with a malformed configuration file and with an unknown option. Each exits 4 with the message
  (not exit 1 for the malformed file or the unknown option), no state directory exists
  afterwards, the fake rclone and git record no start. `scan`, `plan`, and `manifest list` and
  `manifest find TEXT` without `--storage`: exit 0, and with no `disk-cleaner` folder in the
  account's LocalAppData folder beforehand, none exists afterwards.
- **NEG-61 (AC-19)** During the first deletion pass, through the step hook between two unlinks,
  each alone, on an archived item: a file not yet unlinked rewritten with equal length and its
  mtime set back; a later hard-link name, after the first name was unlinked, rewritten with equal
  length and its mtime set back, and separately appended to; a directory's mode changed; a
  directory extended attribute added, one removed, and one replaced by a value of equal length; a
  file's extended attribute replaced the same way. Each stops the pass before that entry is
  removed: nothing more is deleted, `left` names the entry and the field, the item is `failed`,
  and the changed entry is still in Q. A new file created in a directory the pass already listed:
  the planted file survives, that directory's `unlinkat` fails with "not empty", the pass stops
  and journals `left`, the item is `failed`, and entries listed before it may already be removed
  (they had matched); the test asserts exactly this, not that nothing was removed. Controls that
  must reach an empty Q and the `removed` record: directories whose size, link count and times
  the unlinks moved; a read-only directory whose owner write bit the pass set; a Linux setgid
  directory whose bit the chmod cleared; a two-name hard-link group left unchanged.
- **NEG-62 (AC-19)** A regular file replaced by a FIFO with no writer between its `fstatat` and its
  open, during archiving, during the secret checks' content read, during fingerprinting, during
  the deletion pass's re-read and during core's `rescan_item` (C-7): no call blocks (the test
  fails on a 5-second bound), the item fails with the entry named, and nothing more is removed
  (for the deletion pass, entries removed before that entry had matched; the FIFO stays in Q).
- **NEG-63 (AC-23)** A fake rclone that starts a descendant holding the inherited spool descriptor,
  then exits 0 while the descendant keeps writing into the spool: the operation's teardown finds
  the spool lock held, keeps the spool, and reports it as a surviving holder with the last known
  rclone pid (9.6); a second operation started meanwhile keeps it too; after the
  descendant exits, the next operation's teardown removes it. A broken variant that removes the
  spool after reaping only direct children must fail this case.
- **NEG-64 (AC-21)** After `delete-started` and some unlinks, the run is killed; the user renames Q
  back to O by hand. The next run, in each of dry run, `--backup-only`, `--remove` and `apply
  --resume`: warns that the folder at O is an incomplete leftover, keeps the item and any item
  enclosing it with that reason, archives nothing of it, writes no `removed` record, and opens
  nothing inside O; only the `--remove` run and the resume journal `moved-back`, and the dry run
  writes nothing. `quarantine list`, run before any of those and after, shows the moved-back
  folder with both statements. A different directory with a new inode at O instead: treated as
  a new folder, and Q is still a leftover.
- **NEG-65 (AC-21)** A quarantine whose deletion never started (journal ends at `quarantined`),
  each alone: O holds a regular file; O holds a symlink, to a directory and dangling; Q was
  replaced by a symlink to a directory holding the same names; Q was replaced by a different
  directory (another inode). In each case no rename happens, nothing inside Q or O is opened or
  removed, the item is `failed` with the case named, and `quarantine list` shows, for an occupied
  O, the quarantine as not started with `apply --resume RUN_ID`, the `mv -n` command and what
  occupies O, and for a replaced Q, the replaced state of 5 with the restore command. A control
  with O free and Q intact: `--resume` renames Q back and records `kept`.
- **NEG-66 (AC-19)** Through the step hook during the deletion pass: the fsmonitor socket replaced
  by a regular file of the same name, and by a socket with another inode: the pass stops before
  it and the replacement survives; a control with the original socket: removed. In an unarchived
  `node_modules`, a hard link from a pnpm store outside it to a file not yet unlinked, made by
  another process during the pass: the link count differs from F0, so the pass stops (a false
  stop; the store's name still holds the content), the item is a leftover the user removes by
  hand, and D-17's cost entry names this case. The same link in an archived item: no stop, since
  the link count is not in the preserved record; Q is emptied and the store's name keeps the
  content.
- **NEG-67 (AC-40)** Two git items in one run, A with a fake `.env` and a history name and B with
  the same plus an unnamed object, each value distinct, each alone: no choice anywhere (both kept,
  no value stored); ask mode, `u` for A and `n` for B (A's values in its archive, no value of B in
  any stored object); `--secrets upload` in automatic mode (A archived, B kept for its unnamed
  object, no value of B stored); `[apply] secrets = "upload"` overridden by `--secrets keep`
  (both kept); `u` for A, then A changed before upload so F0 is taken again (asked again; with
  no terminal, kept); a third item C with a `.env` whose history check hit the object limit,
  under `--secrets upload` (kept, "not checked"); `[apply] secrets = "vault"` and `--secrets
  vault` (exit 1, nothing read);
  a non-`crypt` remote with `u` (the unencrypted statement shown before the answer). Every stored
  object is inflated and searched for every fake value.
- **NEG-68 (AC-41)** On a large fixture through a pseudo-terminal, during the deletion pass: one
  Ctrl-C (the message of 10.1 printed once, the pass reaches an empty Q, `deleted` journaled and
  the `removed` record in a synced segment, no later item started, exit 3); one Ctrl-C, then a
  second one while that segment is uploaded (stops after `deleted`; the next `--remove` run
  writes and syncs the `removed` record once, and `manifest list` shows one record); two Ctrl-C (the pass stops before the next entry, `left` journaled, the leftover printed,
  exit 3); SIGTERM alone (same as two Ctrl-C); SIGHUP with the terminal closed (the pass
  finishes, no write error escapes, exit 3); Ctrl-C during an upload (stops before the next step,
  nothing quarantined, exit 3). Each is checked against the journal and the file system.

## 22. Reviews: findings and changes

Earlier reviews and how revisions 5 to 10 answered them are in
[APPLY-SPEC-REVIEWS.md](APPLY-SPEC-REVIEWS.md). This table answers the revision 10 adversarial
review (F-ids) and acceptance review (S-ids), prefixed `r10` to avoid clashes with core changes
and earlier findings, and the user's answer on D-4.

| Finding | Change in revision 11 | Criteria and negative cases |
|---|---|---|
| r10 F1 a later hard-link name could be rewritten after the first name's check | One rule: just before each entry's unlink, its full preserved record is recomputed through a descriptor with the archive writer's function and compared, content digest included, for every hard-link name; never mtime or size as a stand-in (13.2). Cost: every byte is read again | AC-19; NEG-61 |
| r10 F2 directory extended attributes were not compared | The same rule covers every extended attribute of every entry, directories included; the excluded fields are named and justified (13.2) | AC-19; NEG-61 |
| r10 F3 two spool cleanup paths, and a descendant of rclone keeps the descriptor | One teardown routine through the two-lock barrier, at an operation's end and for others' spools (9.6) | AC-23; NEG-63 |
| r10 F4 a negative case promised more than the pass can give | NEG-61 states the achievable outcome for a file added to a listed directory; the negative cases of AC-2, AC-19, AC-20, AC-21, AC-29 and AC-39, which the new stop conditions could falsify, were reread in full and the rest searched for absolute claims (NEG-19, 53, 62 changed) | AC-19; NEG-61 |
| r10 S-1 quarantines not started were invisible, an occupied O was unhandled, Q was not checked | `quarantine list` shows every state, a replaced Q included (5); the identity check with `lstat` before any rename; "present" means any entry (6.1, 10.1) | AC-21, AC-39; NEG-65 |
| r10 S-2 a leftover moved back looked like a new folder | A directory at O with F0's device and inode after `delete-started` is `moved-back`: warned and kept, never `removed` (10.1, 7.4) | AC-21, AC-39; NEG-64 |
| r10 S-3 walks entered a leftover before the enclosing item was kept | Every walk in every mode stops at a quarantine, staging name or moved-back folder; `--backup-only` never archives one (7.4, C-6) | AC-39; NEG-53 |
| r10 S-4 the non-blocking open was not yet in core | Core change C-7, a release precondition, cites `open_regular` at `c81a341`; this branch merged `origin/main` | AC-19; NEG-62 |
| r10 S-5 pre-unlink checks were uneven | Covered by the rule of r10 F1; the fsmonitor socket rule; the pnpm false stop stated as a cost (13.2, D-17) | AC-19; NEG-66 |
| r10 S-6 Windows gaps | `manifest find TEXT --storage` refused; the platform check before configuration parsing; local listing reads a defined folder and creates nothing; WSL2 unsupported and untested (5, 14) | AC-30; NEG-60 |
| r10 S-7 the cost statement was incomplete | D-17 lists every cost; signals during deletion are defined (10.1); two stale texts removed | AC-41; NEG-68 |
| The user's answer on D-4 | A secrets handling the user chooses: keep (default) or upload, per item in ask mode and by setting in automatic mode; the vault deferred (9.2, 11, D-4) | AC-12, AC-38, AC-40; NEG-51, NEG-67 |

Rewritten in place: AC-1, 12, 19, 21, 23, 30, 38, 39 and NEG-12, 19, 32, 43, 47, 51, 53, 58,
60, 61, 62. Added: AC-40, 41 and NEG-63 to 68. None was withdrawn.

### 22.1 Minor items left for a later revision

These items of earlier reviews are accepted and not yet changed. None of them lets a removal lose
data; each is either a disclosure, a fidelity detail or a test addition.

- **S-4** SCAN-FORMAT.md says `recover-backup-then-remove`'s "recovery step re-checks every fact";
  add it to the C-4 phrase list (core owner). The `plan.rs` doc comment about recovery refs or a
  bundle is internal.
- **S-7** Restore: write a file, then its extended attributes, then set its exact mode with
  `fchmod`, ignoring the umask (13.3 step 5 states the order only for directories).
- **S-9** Threat model item 5 and section 17: stale or hand-set tracking refs make a local secret
  count as already on the remote (section 17 now says so for the history rule; section 3 still
  does not).
- **S-10** A repository with no network remote and a committed fixture key is always kept; count
  it in the summary and name it in D-4's cost.
- **S-12** Relative `include.path` values; whether the `fixture_prone` database names (`*.db`,
  `*.sqlite`) are in the secret name list; F0's size per entry stated as a measured figure. (The
  pnpm part of S-12 is answered in revision 11 by NEG-66 and D-17.)
- **Review section 4** AC-28 has no negative case: a storage holding two machines' manifests and no
  `--machine` should be refused, not guessed. AC-15, AC-22 and AC-26 have none either.

## 23. Test and build plan

- New crate `crates/apply` (library: re-check, places, archive, store, manifest, journal, lock,
  restore) and CLI wiring in `crates/cli`. Core changes are additive only (section 15).
- Tests are written first; each behaviour gets a red CI run before its implementation lands.
- Builds and tests run in GitHub Actions on ubuntu and macOS; extra jobs: Linux mounts and
  spool (tmpfs, directory and file bind mounts, a vfat loop mount, with `sudo`), real rclone (a
  pinned release whose published SHA-256 is checked before use, `local` remotes only), and a release build that checks the hook is absent
  (AC-35). The pseudo-terminal tests of the secrets prompt (AC-38, NEG-51) and of signals
  (AC-41, NEG-68), the spool tests (NEG-56, NEG-63), the deletion-pass tests (NEG-61, NEG-62
  and NEG-66) and the quarantine-state tests (NEG-64, NEG-65) run in the ordinary test job of
  both runners; the signals test sends real signals to the real binary and has a bound of 120 seconds.
  A Windows runner builds the binary and runs NEG-60 and core's tests. Core's FIFO test for
  `rescan_item` (C-7) runs in core's own job.
- No test touches any directory outside its own temporary directory, and no test uses a network
  remote, a network host or a real credential.
- **Measurements before release** (claims to be checked, not assumed; run in CI or on the fleet,
  never on a developer's laptop):
  - the history rule of 9.2 on a public repository of at least 5 million objects cloned locally,
    run with exactly the options of 7.5 (`core.commitGraph=false`, `--no-replace-objects`, the
    hardened runner): wall time and peak memory of the whole check, git's included, against the
    limits of D-16 (10 million objects, 600 seconds), with the hardware stated. The same walk with
    the commit-graph enabled is run once for comparison only, so the cost of the safe option is
    known; it is never the configuration apply ships;
  - on a corpus of real repositories prepared for the purpose: the number of items kept by each
    rule of 9.2, in particular the unnamed-object rule (D-16) and the content rule inside
    regenerable children (S-2). Public clones with local work replayed understate the
    unnamed-object rate. So the replay must include the staging workflows that create unnamed
    objects (`git add`, edit, `git add` again; `git add` then `git commit -a`; `git add -p`;
    hunk staging as GUI clients do it), or the corpus must include repositories of users who
    consented. Never a user's own repositories without their consent. The thresholds for "too
    many kept" are set before the run and reported with the result. The user has chosen the
    answer `u` (D-16); the result is reported to the user as the share of items `u` could reclaim
    in ask mode and the share automatic runs keep.

## 24. Click-test plan

An independent tester, arranged by the coordinator (not the author, not a spec or code reviewer),
runs the real binary before the work is called ready.

- **Binary.** The release artifacts of the CI run for the PR's head commit, for macOS, Linux and
  Windows (where only the refusals are tested), with their SHA-256 recorded in the report. Nothing is built on the test machines.
- **Machines.** A macOS host, a Linux host and a Windows host or VM, each with a few GiB free.
  Nothing outside a fresh temporary directory created for the test is scanned, archived or removed.
- **Fixtures.** `scripts/make-click-fixtures DIR` (part of the PR) creates, inside DIR only: a
  repository whose remote is an https URL on a reserved `.invalid` host and whose remote-tracking
  refs are set locally to its tips, so the scan reports it saved (`backup-then-remove`; apply never
  contacts that host); one with an unpushed commit, a stash, uncommitted, untracked and ignored
  files (`recover-backup-then-remove`); a project with a `node_modules` (`remove-regenerable`); one
  repository with a fake `.env`, one with a fake `.env` set aside with `git stash -u`, and one
  where that stash was then dropped with `git stash drop` (all three kept by default); one where a
  plain text file matching no secret pattern was staged and then replaced before a commit (kept by
  the unnamed-object rule alone, so `u` is offered); a nested case with a hand-edited file in
  `node_modules`; a file with an extended attribute; a `node_modules` large enough (a few hundred
  MiB in many files) that its deletion takes several seconds, so the tester can stop the tool
  during it. The script sets every modification time and every commit date 60 days in the past,
  beyond the default 7-day active window, so the plan gives the actions named. The tester runs
  `scan` and `plan` on DIR.
- **Storage.** (a) A local-directory rclone remote in a temporary rclone config. (b) One real
  non-local remote in a disposable bucket or folder that the coordinator provides at test time. The
  tester configures it in their own rclone config; no credential appears in the repository, the
  report, the ticket or any log, and the bucket or folder is deleted after the test.
- **Flows, on macOS and Linux, against (a) and then (b):** dry run (nothing written); `--backup-only`
  then `manifest list`, `find`, `verify`; `--remove` in ask mode answering `y`, `n`, `s` and `q`;
  `--remove --auto repo,regenerable`; Ctrl-C during an upload, then `apply --resume`; `restore` to
  the original path and with `--to`; `restore` with an empty `--state-dir` and `--storage` only;
  `manifest orphans` after the interrupted run; `--require-encrypted-remote` on a plain remote
  (refused); a `restore` from (b) started while a `--backup-only` run is uploading, both
  completing, with the tester checking that `<state>/rclone-tmp/` holds one spool directory for
  each live operation and none after both end; `--remove` in ask mode on the staged-and-replaced
  repository, answering `n` at the secrets prompt (kept, nothing uploaded), then on a new run `u`
  after reading the three statements the prompt shows (11) and `y` (archived and removed, the
  record counting the unnamed objects); the secrets prompt on the `.env` repository and on the
  dropped-stash repository (each lists the file or history name; `n` keeps, `u` then `y` archives
  with `secrets_choice` recorded), with the unencrypted statement on (b) when it is not `crypt`;
  `--remove --auto repo --secrets upload` over all the secrets repositories (the `.env` and
  dropped-stash ones archived, the staged-and-replaced one kept for its unnamed objects) and the
  same without `--secrets` (all kept); Ctrl-C once during the deletion of the large `node_modules` (the message of 10.1 printed, the
  pass finishes, exit 3), and twice (a leftover, exit 3); the tool killed with `kill -9` during
  the deletion of the large `node_modules`, once removed on its own and once inside its archived
  repository, then `apply --resume` and a new `--remove` run (each reports the leftover and
  deletes nothing; the tester records the leftover's file count before and after),
  `quarantine list` (the restore command and both statements of 5), the printed restore command
  run for the archived case (the restored tree equals the fixture); one leftover renamed back to
  its original path by hand (`quarantine list` shows it as moved back; the next `--remove` run
  warns that it is incomplete, records `moved-back`, and keeps it); and finally the tester removing the other leftover by hand and running
  `apply --remove` once more (the record is written once, the leftover no longer listed).
  After each flow the tester compares the restored trees with the fixture and searches every
  output for the fake `.env` value. A quarantine whose deletion never started cannot be reached
  by hand on a release binary (the step hook is compiled out, AC-35, and the window between the
  rename and the first unlink is short), so that state is covered by NEG-65 and `quarantine.rs`,
  not by this flow.
- **Killed parent, macOS and Linux, against (b).** During an upload the tester stops the rclone
  child with `kill -STOP`, kills the tool with `kill -9`, starts a `--backup-only` run, and checks
  that the stopped child's spool directory is still there and is reported with its pid; then sends
  the child `kill -CONT` and `kill -TERM` (the tester's own processes, in the test directory only)
  and checks that the next operation removes that spool.
- **Spool observation, (b) only.** During every upload to the non-local remote, the tester samples
  once a second the size of `<state>/rclone-tmp` (all spool directories) and the free space of the
  state volume, and records the peak size, the lowest free space and the part size the journal
  shows. The flow fails if the peak exceeds the part size or the free space falls below
  `--min-free`. This is the check section 17 defers to the click-test.
- **Flows on Windows:** `scan` and `plan` on DIR; each command of NEG-60 exits 4 with the
  message and creates no state directory (checked in local application data and a given
  `--state-dir`).
- **Record.** The report (flows, platforms, artifact digests, commands, outcomes, findings) is
  attached to the ticket with `spec_flow.py click-test`. Findings are fixed and the affected flows
  retested on the new artifacts.
- This is a command-line tool: the flows run in a terminal session on each host, not through a
  browser. That is a labelled deviation from a browser-based click-test and is reported as such.
