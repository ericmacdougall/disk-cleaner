# Apply phase specification

Status: **draft for review, revision 3**. Nothing in this document is implemented yet. It defines
what `disk-cleaner apply`, `disk-cleaner restore` and `disk-cleaner manifest` must do before any
code is written, and it is the contract the tests are written against. Section 16 lists the
decisions that are still open; text that depends on one of them says so. Section 22 answers the
review of revision 2 finding by finding. Criterion and negative-case ids were renumbered in
revision 3.

Read with [DESIGN.md](DESIGN.md) and [SCAN-FORMAT.md](SCAN-FORMAT.md). Where this document and
SCAN-FORMAT.md disagree about the plan format, SCAN-FORMAT.md wins and this document is wrong.

Revision 3 removes surface rather than hardening it: every removed item is archived whole, so no
removal depends on git's judgement that a file is clean; pushing recovery refs is an opt-in that is
off by default and stays disabled until its hosting assumption is verified; there is no local
vault; items in sync folders are kept; and removal is supported on macOS and Linux only.

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
- Pushing recovery refs for unsaved commits, as an explicit opt-in (section 8).
- A journal that makes every step resumable and idempotent; a per-user lock so two applies never
  run at once on one machine.
- The manifest (local and in storage), `disk-cleaner manifest list|find|verify|orphans`, and
  `disk-cleaner restore`.
- Ask mode (per item, the default) and automatic mode (per class, opt-in).
- **Platforms.** macOS and Linux: dry run, backup, removal and restore. Windows: dry run,
  `--backup-only` and restore; `--remove` is refused on Windows until handle-based deletion is
  specified and tested there (D-13).

Out of scope for this phase:

- Acting on plan items whose action is `ask` or `keep`. Apply never does more than a fresh plan
  would (AC-4).
- Backing up and removing outputs, loose code and agent sessions: the plan has no removing action
  for them yet. The archive and manifest formats are class-agnostic so that a later plan action can
  use them unchanged. **Against the original requirements this phase is partial**: outputs,
  agent-session and log backup to cold storage, and loose-code handling (connect an origin, back up
  raw, remove or leave) are deferred, not met.
- Items in sync folders or holding online-only placeholder files (section 7.4): kept.
- Linked worktrees, bare repositories and repositories with submodules: the plan never gives them
  a removing action in this schema (`plain_layout`).
- A local vault, client-side encryption and key management (users who want encryption configure an
  rclone `crypt` remote, see 9.2), the recycle bin, scheduled runs, the desktop app, cold-storage
  classes that cannot be read back at once, batching many items into one archive, network file
  systems as item locations (section 17).

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
   (remote URLs, `insteadOf`, `pushurl`), hooks, `.gitattributes`, symlinks, mount points, hard
   links, special files, attributes and file names are attacker-controlled. Apply must not run any
   program it names, must not contact a host it names with the user's credentials, must not follow
   any link out of the item, must not cross into another mount, and must not write outside the item
   and the tool's own state.
5. **The remote changed** (force push, deleted branch, changed URL, unreachable).
6. **Disclosure.** A secret uploaded in plaintext, pushed to a git remote that may be public or may
   run CI on what it receives, or printed to the terminal, a log or the manifest; the user's ssh
   identities or credentials offered to a host they did not choose.
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
  `--state-dir`. It holds `machine.json`, `lock`, `journal/`, `manifest/`, `index/` and `scratch/`,
  created with owner-only permissions.
- **Storage**: an rclone remote and path, `--storage <remote>:<path>`, that the user configured with
  `rclone config`. The tool never reads or writes rclone's credentials and never passes a credential
  on a command line. Its type is read with `rclone listremotes --long`.
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
- **Live remote check**: asking the git remote itself (`git ls-remote`), not the local
  remote-tracking refs.
- **Quarantine**: renaming an item, with no-replace semantics, to `.disk-cleaner-removing-<item_id>`
  in the same parent directory before deleting it.
- **Secret-like file**: defined in 9.2.

## 5. Commands

```
disk-cleaner apply --plan PLAN --scan SCAN --storage REMOTE:PATH
                   [--backup-only | --remove]
                   [--auto CLASS[,CLASS...]] [--class CLASS[,CLASS...]] [--item PATH]...
                   [--backup-regenerable] [--upload-secrets] [--require-encrypted-remote]
                   [--push-recovery --push-remote NAME] [--check-remote]
                   [--max-plan-age HOURS] [--max-upload-bytes N] [--min-free BYTES]
                   [--part-size BYTES] [--state-dir DIR] [--machine-label TEXT]
                   [--rclone PATH] [--allow-root] [--json]
disk-cleaner apply --resume RUN_ID
disk-cleaner restore RECORD_ID [--to DIR] [--storage REMOTE:PATH] [--machine ID] [--json]
disk-cleaner manifest list   [--storage REMOTE:PATH] [--machine ID] [--json]
disk-cleaner manifest find TEXT [--files] [--storage REMOTE:PATH] [--machine ID] [--json]
disk-cleaner manifest verify --storage REMOTE:PATH [--machine ID] [--json]
disk-cleaner manifest orphans --storage REMOTE:PATH [--machine ID] [--json]
```

- No mode flag: **dry run**. `--backup-only`: everything up to and including the manifest, then
  stop; nothing is removed. `--remove`: the full sequence (refused on Windows, exit 1). The two are
  mutually exclusive.
- `--auto` names the classes whose items run without a prompt: `regenerable`, `cache` (their
  `remove-regenerable` items), `repo` (`backup-then-remove`), `repo-recover`
  (`recover-backup-then-remove`). `[apply] auto = [...]` in the config file does the same. Default:
  none, so every item is asked.
- `--push-recovery` (section 8) needs `--push-remote NAME` (the repository's remote of that name)
  in automatic mode; in ask mode the remote is confirmed per item. It is disabled in builds until
  AC-13 has passed (D-5).
- `--class` and `--item` narrow the selection; they never widen it.
- `--resume RUN_ID` takes no other flags: the run's flags are in its journal.
- Running as root (uid 0) is refused unless `--allow-root`.
- Configuration lives in the same `config.toml` as the plan settings, under `[apply]`. Unknown keys
  are an error, as for the plan.

## 6. The sequence

### 6.1 At the start of every apply

Before selecting anything, every apply (dry runs excepted, which only report) takes the lock
(10.2), then reconciles what earlier runs left behind:

- every quarantine directory named in an unfinished journal is finished or renamed back, as in the
  resume table (10.1), and a path is never reported `gone` while a quarantine with its `item_id`
  exists;
- `removed` records written but not yet in a synced segment are synced;
- scratch repositories are deleted (they hold only refs the tool made; their alternates entry would
  otherwise make a later scan report a dependent on the user's repository).

### 6.2 For one item

Items are processed one at a time, innermost first. Every transition is written to the journal
before the step it records takes effect. Any failed check ends the item as **skipped** with a reason
and the run continues with the next item; nothing that failed a check is ever removed.

1. **Select.** The item's plan action removes something and the item is within `--class` and
   `--item`.
2. **Re-check** (section 7): take the fingerprint F0, re-scan and re-plan, refuse places that are
   never removed, check processes, check the live remote, take the fingerprint again.
3. **Ask** (ask mode only, section 11).
4. **Push recovery refs** (opt-in only, section 8).
5. **Archive and upload** (section 9): stream, split into parts, upload, read back, compare; then
   the index object the same way.
6. **Manifest** (section 12): write the item's `backed-up` record to a new segment, upload it, read
   it back, compare.
7. Stop here for `--backup-only` (outcome `backed-up-kept`, by request).
8. **Second existence check**: list each part, the index object and the manifest segment in
   storage and compare their sizes with what was verified; confirm the item path is still the
   directory of F0 (same device, inode and mount id).
9. **Process check again.**
10. **Quarantine**: rename with no-replace semantics; take the fingerprint of the renamed directory
    through the handle opened on it, and compare with F0. Any difference: rename it back and end as
    `backed-up-kept` (the archive stays, the manifest says why). If the rename back fails because
    the original path was recreated, the quarantine stays, the record says where, and the summary
    names both paths.
11. **Delete** the quarantined directory (section 13.2).
12. **Record** the `removed` record; it is synced with the next item's step 6, at the end of the
    run, or by the next apply (6.1).

`remove-regenerable` without `--backup-regenerable` (and without the answer `b`) skips steps 4 to 6
and 8; step 10 still compares with F0. Its record says it was regenerable and names the rule.

### 6.3 Nested items

Removing an item removes everything inside it:

- a selected item inside another selected item is handled first. With `--remove`, if it ends as
  anything but removed (skipped, failed, kept, answered `n`, outside `--class` or `--item`), the
  enclosing item is skipped ("the nested <path> was kept"), whatever its own checks say;
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
- at step 10 the quarantined directory must equal F0.

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

- the path exists and is a directory (not a symlink, junction or file) on the same device and
  mount as in the scan;
- the fresh re-scan finds the same set of nodes inside the item as the scan file (paths and
  classes), every one of them gets the same action from the fresh plan as in the plan file (nested
  items already removed in this run excepted, 6.3), and no node has an `ask_reason`;
- class, rule and every count of the item's node equal the scan's (`files_total`, `bytes_total`,
  `newest_mtime`, `output_bytes`, `unreadable`, `skipped_mounts`, `contains_outputs`,
  `outputs_unlisted`);
- for a git node: every fact equals the plan item's `facts`; `head_commit`, `last_commit_time`,
  the remote names and URLs, `git_admin` and `object_store` counts, `unsaved_pseudorefs`,
  `hidden_index_entries` and `local_admin_content` are equal; the git directories inside the item
  equal `preserve.git_admin.git_dirs`;
- for a regenerable or cache node: its owning project (its parent node in the scan) has the same
  activity evidence as in the scan, read with core's project activity function (C-3): newest
  modification time excluding `.git` and nested nodes, and newest commit time;
- the newest activity is still outside the active window.

Dependents found elsewhere in the scan cannot be seen by a re-scan of the item; the scan file's
`no_dependents` is used as it is, and a fresh `no` wins. This is a documented limit (section 17),
bounded by the plan age.

### 7.3 Running processes

The process check (`running_cwds`) runs at step 2 and again at step 9. A process whose working
directory is inside the item, or inside the project that owns a regenerable item, skips it. A
check that could read no process skips every item; a partial check skips every item unless the plan
recorded `trusted_partial_process_check`.

### 7.4 Places that are never removed in this release

An item is skipped, with the reason, when:

- **it is in a sync folder**: its path lies under `~/Library/CloudStorage`, `~/Library/Mobile
  Documents`, `~/Desktop` or `~/Documents` on macOS (iCloud Desktop and Documents cannot be ruled
  out cheaply), a Dropbox folder listed in Dropbox's own `info.json`, a OneDrive folder named by the
  `OneDrive*` environment variables or registered as a Windows cloud-files sync root, or any
  directory with a Syncthing `.stfolder` marker in it or an ancestor. Deleting there deletes on the
  user's other machines, which this tool cannot check;
- **it holds an online-only placeholder**: an entry with the macOS `SF_DATALESS` flag, or a Windows
  file with the recall-on-open, recall-on-data-access, offline or pinned attributes, or a
  cloud-files reparse point. Reading one triggers a download or fails;
- **it holds or crosses a mount**: any directory inside the item that is a mount root (Linux:
  `statx` `STATX_ATTR_MOUNT_ROOT`, or a mount id that differs from the item's, else
  `/proc/self/mountinfo`; macOS: `st_dev` differs or `statfs` names it as a mount point), including
  a Linux bind mount from the same file system. Where no mount information can be read, removal
  is refused;
- **it is on a network file system** (`statfs` type NFS, SMB/CIFS, AFP, FUSE and the like): change
  detection by metadata is not reliable there;
- **it holds or lies inside the tool's own state directory**, or the storage path when that is a
  local directory (after resolving rclone `alias`, `union` and `crypt` wrappers with
  `rclone config show <name>` reading only the `type` and `remote` keys);
- **an entry cannot be deleted**: an immutable or append-only flag (`chflags uchg`/`schg`,
  `chattr +i`/`+a`) or a directory the user cannot write;
- **an entry's extended attributes cannot be archived** (9.3).

A `local` storage remote on the same device as the item is warned about once per run: it is not a
backup of that disk.

### 7.5 Live remote check (git items)

Remote facts in the plan are unverified. Before acting on a git item apply confirms them against
the live remote, with the network runner of section 8.1:

- The **effective URL** of each network remote is computed by the tool: the URL written in the
  repository's config, rewritten by the repository's own `url.<base>.insteadOf` and by the user's
  global `url.<base>.insteadOf`, longest prefix first as git does. It must be `https://` or ssh
  (`ssh://` or scp-style) and its host must be on the **host allowlist** (8.1). A URL or host that
  starts with `-` is rejected.
- A repository with no remote whose effective URL passes is skipped (kept), never trusted.
- `git ls-remote --end-of-options <effective URL>` lists the remote's refs. For every commit the
  plan's facts call saved (HEAD, every local branch and other local ref, every reflog and pseudoref
  commit) some advertised tip must contain it, checked locally with
  `merge-base --is-ancestor` in the user's repository (read-only). An advertised tip that is not in
  the local object store makes the check unknown: the item is skipped ("the remote has commits this
  clone does not have; fetch and scan again"). No fetch is ever made.
- Every local tag must be advertised with the same object id (an annotated tag by its tag object).
- A remote that cannot be reached, refuses authentication, asks for a host key or times out makes
  the check unknown, and the item is skipped.

The archive holds the whole repository either way; this check verifies the plan's premise, as
SCAN-FORMAT.md requires. `--check-remote` lets a dry run do it (read-only on the remote).

## 8. Network runner and recovery refs

### 8.1 The network runner

Every git command that contacts a remote uses core's network runner (C-1), built on the hardened
runner (no hooks, no system or global config, no inherited `GIT_*` variables, no programs named by
scanned content), adding only:

- the transport allowlist: `protocol.allow=never`, `protocol.https.allow=always`,
  `protocol.ssh.allow=always`;
- a fully non-interactive environment: `GIT_TERMINAL_PROMPT=0`, `credential.interactive=never`,
  `GCM_INTERACTIVE=Never`, no `SSH_ASKPASS`, and a tool-set
  `GIT_SSH_COMMAND=ssh -o BatchMode=yes -o StrictHostKeyChecking=yes -o UpdateHostKeys=no`
  (an unknown host key fails, it never prompts);
- the **host allowlist**: credentials are attached only for a host listed in `[apply] git_hosts`
  (empty by default; the summary and the ask prompt say when a skip was caused by it). For a listed
  host, the credential helpers are the user's own: the unscoped `credential.helper` and the
  host-scoped `credential.<url>.helper` entries for that host, read once with
  `git config --global --get-regexp` in an empty tool-owned directory, and passed with `-c`. For a
  host that is not listed, apply runs no network command at all for that repository (it is
  skipped). ssh is used only for listed hosts, with the user's agent and `~/.ssh` as usual;
- an optional CA file, `[apply] git_ssl_ca_file`, passed as `http.sslCAInfo`;
- `--end-of-options` before every URL and refspec (minimum git version 2.24; an older git disables
  every network command, so git items are skipped);
- an explicit URL and explicit refspecs, never a remote name, so a scanned `pushurl`,
  `remote.*.push` or `receive.*` setting cannot redirect them;
- `HOME` set to the account's real home directory only when a credential helper needs it (the
  `store` helper reads `~/.git-credentials`); the global config is never read by the command itself
  (`GIT_CONFIG_GLOBAL` stays the null device). Whether the common helpers work this way is checked
  per OS by AC-11.

### 8.2 Recovery refs (opt-in)

Off by default (D-5). The archive holds everything in every mode; recovery refs only make unsaved
commits visible on the remote. With `--push-recovery`, for a `recover-backup-then-remove` item:

- **What**: only commits that are already commits: each local branch whose tip the live remote
  does not contain, a detached HEAD, and each pseudoref in `unsaved_pseudorefs` except `FETCH_HEAD`
  (its commits came from a remote). Never stashes, reflog-only commits, uncommitted, untracked or
  ignored files: those are in the archive only.
- **Where**: the remote named by `--push-remote` (automatic mode) or confirmed per item in ask mode
  (the prompt shows its effective push URL); never another remote. Its host must be on the host
  allowlist. If an anonymous `ls-remote` (no credential helper, no ssh identity offered:
  `-o IdentitiesOnly=yes -o IdentityFile=none`) succeeds, the remote is publicly readable and the
  push is withheld ("public remote").
- **Namespace**: `refs/disk-cleaner/recovery/<UTC yyyymmdd>/<run_id>/<name>`, outside
  `refs/heads` and `refs/tags`. No machine name or label appears in a ref name. **Unverified
  assumption**: hosting services accept such refs, list them in `ls-remote`, and fire no push
  webhook, workflow or preview deployment for them. AC-13 verifies this against a real hosting
  service; until it has passed, `--push-recovery` is disabled in the build.
- **Content check before pushing**: the objects that would be sent (`rev-list --objects <tips>
  --not <advertised tips>`) are listed; if any path matches the secret-like names of 9.2, or any
  blob under 1 MiB matches the content rules of 9.2, that ref is withheld
  (`recovery_push: withheld (secret-like content)`).
- **How**: from a scratch repository under `<state>/scratch/<item_id>/`, a bare repository whose
  `objects/info/alternates` names the item's object directory, so nothing is written to the user's
  repository (refs, objects, index, reflog, config). Pushes never force: a name that already exists
  on the remote with another id gets a numeric suffix.
- **Verify**: `ls-remote` must advertise every pushed ref with exactly the pushed id.
- **Failure** (refused, authentication, mismatch, withheld): recorded with a redacted reason. In ask
  mode the user is told and asked again whether to continue with the archive alone. In automatic
  mode the item is **kept** (`backed-up-kept`) when a push the user asked for did not happen.

The user's git directory is fingerprinted before and after; any difference fails the item.

## 9. Backup

### 9.1 What is archived

Every removed item is archived **whole**: every entry of the item directory, working tree, ignored
files and git directories alike, except nested items whose own action is `remove-regenerable` and
which were removed earlier in this run (6.3). No file is left out because git says it is clean or
regenerable. `remove-regenerable` items are archived only with `--backup-regenerable` or the answer
`b`.

The **preserve sets** are a checked subset: for every git directory in
`preserve.git_admin.git_dirs`, `git_admin_files` and `object_store_files` are called again at apply
time; the call must succeed and every file they list must appear in the index as archived, or the
item is skipped.

### 9.2 Secret-like files

A file is **secret-like** when any of these holds (best effort; anything not caught is uploaded,
always to the user's own storage):

- its name matches the scan's `[[state_file]]` list that is not `fixture_prone` (`.env`, `.env.*`,
  Terraform state and variables, SSH key names) or one of: `id_rsa`, `id_dsa`, `id_ecdsa`,
  `id_ed25519` (and their `*_sk` forms), `*.pem`, `*.key`, `*.p12`, `*.pfx`, `*.jks`, `*.keystore`,
  `*.ovpn`, `.netrc`, `_netrc`, `.npmrc`, `.pypirc`, `.git-credentials`, `.envrc`, `kubeconfig`,
  `credentials`, `credentials.json`, `service-account*.json`, `secrets.yaml`, `secrets.yml`;
- it is untracked and ignored only by the user's global excludes (`core.excludesFile` from the
  user's global config, default `$XDG_CONFIG_HOME/git/ignore`), found with a dedicated
  `ls-files --others --ignored --exclude-from=<that file>` run that reads the pattern file and runs
  no program. SCAN-FORMAT.md requires these to be treated as possibly secret;
- it is untracked or ignored, smaller than 1 MiB, and its first 64 KiB contain the header line of a
  PEM private key block, a URL that carries a password or token before the host, or a token in one
  of the documented formats of the common providers (AWS access keys, GitHub and GitLab tokens,
  Slack tokens, API secret keys of the common model providers, Google API keys); the exact
  patterns live in the code and its tests, not in this document;
- it is a file of a git directory (`config`, `FETCH_HEAD`, files under `logs/`, any other) that
  contains a URL whose userinfo holds a password or a token (a bare user name, as in a default Azure
  DevOps clone URL, is not a credential).

**Default: an item with any secret-like file is kept** (skipped before upload, reason naming up to
five of them, by relative path only). With `--upload-secrets` (or `[apply] upload_secrets = true`)
they are archived like everything else. No local vault exists (D-4).

**Encryption.** Before the first upload, apply reads the storage remote's type. When it is not
`crypt` (directly or through `alias`/`union`), the run summary and every ask prompt say that the
archive is stored unencrypted in the user's storage. `--require-encrypted-remote` (or
`[apply] require_encrypted_remote = true`) refuses to upload to such a remote (exit 1).

### 9.3 Archive format

- A POSIX pax tar stream (`ustar` with pax extended headers for long names, large sizes, nanosecond
  times and names that are not UTF-8 on Unix). Uncompressed.
- Deterministic: entries in byte-wise order of relative path, directories before their contents,
  owner and group 0 with empty names, mode bits, file flags and modification times as on disk. The
  exact length of every part is computed from F0 before the upload starts.
- Directories, regular files and symlinks (the target is stored as written, never followed). Two
  names of one file inside the item are stored as a hard-link entry after the first name. A hard
  link to a file outside the item is stored as a regular file. Fifos, sockets (except the fsmonitor
  socket, see section 4) and devices make the item unsupported.
- **Extended attributes** (macOS and Linux, including the macOS resource fork and Finder info) are
  stored as pax `SCHILY.xattr.<name>` records, up to 64 KiB per attribute and 1 MiB per file. These
  harmless attributes are dropped and listed by count: `com.apple.quarantine`,
  `com.apple.provenance`, `com.apple.lastuseddate#PS`, and an all-zero `com.apple.FinderInfo`.
  An attribute over the limits makes the item unsupported. On Windows, an alternate data stream
  other than `Zone.Identifier` makes the item unsupported (Windows is backup-only in this release).
  ACLs are not archived (section 17).
- The stream is split into **parts** of at most `--part-size` bytes (default 8 GiB), at arbitrary
  byte offsets; restore concatenates them.
- The **index** is a separate JSON Lines object, `<archive>.index.jsonl`: a header (format version,
  machine id, original path, action, HEAD for git items, the part list with sizes and SHA-256) and
  one line per entry with its fingerprint fields and, for regular files, its SHA-256. A copy stays
  in `<state>/index/`.

### 9.4 Upload and verification

- Each part is produced by one pass over the item through directory handles (section 13.2 rules:
  `openat` with no-follow, `fstatat`, nothing resolved by path twice). The tar writer writes into a
  SHA-256 hasher, a byte counter and the stdin of
  `rclone rcat --size <exact part length> <storage>/<object>`. No copy of the archive is staged by
  the tool. Whether every backend avoids spooling when `--size` is given is **unverified**; the
  free-space check of section 18 therefore includes the volume of rclone's temporary directory and
  requires room for one part there.
- An entry whose metadata differs from F0 when opened, or whose size changes while read, or that
  cannot be read, fails the item before the part completes (fail closed).
- The part is then read back with `rclone cat` into a second hasher. It is **verified** only if
  rcat exited 0, the bytes read back equal the bytes sent and the computed length, both digests are
  equal, and `rclone lsjson` reports that size.
- Object names are unique per attempt:
  `disk-cleaner/v1/<machine id>/archives/<run_id>/<item_id>.a<attempt>.p<part>.tar`. Nothing is ever
  overwritten.
- A failed attempt (nonzero exit, mismatch, no progress for 10 minutes) is never retried in place:
  the item restarts from step 2 with attempt + 1, at most 3 attempts. Objects of failed attempts
  (including a lost response, where the object arrived but the tool saw a failure) are orphans,
  listed by `manifest orphans`, never deleted automatically.
- After the last part, the index object is uploaded and verified the same way.

### 9.5 Rate limits and budgets

rclone's own retries handle rate limiting within one call. Three items in a row that fail at the
storage stop the backup stage for the rest of the run. `--max-upload-bytes` stops before an item
whose archive would exceed the budget; the estimate is the item's logical size from F0 (sparse files
count at their full length).

## 10. Journal, resume, concurrency

### 10.1 Journal and resume

- `<state>/journal/<run_id>.jsonl` is append-only. Each line is one transition of one item
  (`selected`, `rechecked`, `answered`, `pushed`, `part-uploaded`, `part-verified`,
  `index-verified`, `recorded`, `segment-synced`, `checked-again`, `quarantine-intent`,
  `quarantined`, `delete-started`, `deleted`, `removed-recorded`, `skipped`, `failed`, `kept`) with
  the attempt, object names, sizes and digests. Each line is flushed and `fsync`ed before the step it
  records takes effect; renames are followed by an `fsync` of the parent directory where the
  platform supports it.
- The header records the plan and scan SHA-256, the storage, the flags and the build.
- `apply --resume RUN_ID` continues each unfinished item from its last durable state; every apply
  also does this for the quarantines of all unfinished runs (6.1):

| Last state | On resume |
|---|---|
| before `segment-synced` | Start the item again from step 2 with the next attempt. Earlier objects are orphans. |
| `segment-synced` | Step 2 again (F0 must equal the index), then steps 8 to 12. |
| `quarantine-intent`, original path present, no quarantine directory | The rename never happened: as `segment-synced`. |
| `quarantine-intent` or `quarantined`, quarantine directory present, original absent | Fingerprint the quarantine: equal to the index, or (after `delete-started`) a subset of it, continue deleting; otherwise rename it back and record `kept`. |
| both present | Touch neither; report both paths. |
| `deleted` | Write and sync the `removed` record if no synced segment has it. |

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
  whether it is encrypted, secret-like files found, and, with `--push-recovery`, the remote, its
  effective push URL, whether it is public, and the commits that would be pushed. Answers: `y` do
  it, `n` keep, `b` back up then remove (regenerable only), `s` back up and keep, `a` yes to the rest
  of this class, `q` stop (no further item is started). Pending typed-ahead input is discarded before
  each prompt. End of input means `n`. Without a terminal on stdin, every item is `n`; nothing waits.
- The answer binds to F0; any difference at step 10 keeps the item.
- **Automatic** applies to the classes named by `--auto` or `[apply] auto`; default none. Automatic
  items are re-checked and skipped on any difference.
- A dry run never prompts.

## 12. Manifest

### 12.1 Records

One JSON object per line. Every record has `record_id`, `item_id`, `run_id`, `machine_id`, `at`,
`path`, `class`, `action`, `outcome` and `tool_version`. Outcomes: `backed-up` (written before
removal), `removed`, `backed-up-kept` (with the reason), `regenerable-removed`. Where they apply:

- `bytes`, `files`, `xattrs_dropped`; the facts used and when the live check confirmed them;
- `git`: HEAD, branches and tags with their ids, the redacted remote URLs checked and the result;
- `recovery`: off, or each pushed ref with its id and redacted URL, or `withheld`/`failed` with a
  redacted reason;
- `archive`: `format` (`pax-tar-v1`), `parts` (object, bytes, SHA-256), `bytes`, `sha256` of the
  whole stream, `index` (object, bytes, SHA-256), `encrypted_remote` (true or false), `members`
  (one item path in this release);
- `secrets_uploaded`: the count of secret-like files archived under `--upload-secrets` (no names);
- `restore`: the method (`archive` or `regenerable`) and the exact `disk-cleaner restore
  <record_id>` command.

URLs pass core's `strip_url_credentials`; error text passes `redact_text`. No record holds a
secret.

### 12.2 Where it lives

- Locally: `<state>/manifest/<run_id>.<seq>.jsonl`, one **segment** per sync, never modified after
  it is written, ending with a trailer line holding the record count and the SHA-256 of the bytes
  before it.
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

Removal (steps 8 to 11) happens only after, in order: the re-check passed; the recovery push, when
requested, is recorded as pushed (or the user accepted its failure in ask mode); every part and the
index object are verified; the manifest segment with the `backed-up` record is verified in storage;
and the second existence check passed.

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
   at least the record's logical `bytes`, and extraction stops if the stream exceeds it.
3. Each part is streamed with `rclone cat` into a hasher and a tar reader that extracts into
   `<target parent>/.disk-cleaner-restoring-<record_id>`. Every entry is created with create-new
   semantics; "already exists" fails the restore. Extraction refuses absolute names, `..`
   components, names invalid on the target file system, writes through a symlink (symlinks are
   created after all files), hard links to anything outside the staging directory, and special
   files. Directory modes are applied after their contents are written. On any digest mismatch the
   staging directory is deleted and nothing is created at the target.
4. Names that collide on the target file system (case, or Unicode normalisation: NFC and NFD forms
   of one name) are refused before extraction, naming them.
5. Every entry is compared with the index: kind, size, SHA-256, symlink target, mode bits and
   extended attributes (where the target supports them; otherwise the restore lists the attributes
   it could not set and exits 2).
6. The staging directory is renamed to the target with no-replace semantics. A failure at any step
   leaves nothing at the target.

Restore never runs `git` and never contacts a remote. `regenerable` records have nothing to restore:
restore prints the rule and how the directory is normally rebuilt, and exits 0.

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

- **C-1 (D-1)** A network runner: a `Git` method that builds on `Git::command` and adds only the
  values of 8.1, with a test that it keeps every hardened setting.
- **C-2 (D-2)** A plan field `scan_sha256`, written by `plan` and read by apply.
- **C-3** An item re-scan and a project activity function: `rescan_item(path, &ScanOptions)` walks
  only `path`, decides its rule from the markers in its parent directory (by name, `lstat` only) and
  applies the enclosing repository's ignore check to `path` itself, returning the same nodes a full
  scan would give for that subtree; `project_activity(node_path)` returns the newest modification
  time excluding `.git` and nested nodes, and the newest commit time. A test shows that
  `rescan_item(proj/node_modules)` yields one `regenerable` node equal to the full scan's.

No scan or plan safety rule changes.

## 16. Open decisions

The text above follows the recommended option. D-4 and D-5 change what the user approved or asked
for and need the user's own decision.

- **D-1. Network access for git.** Recommended: section 8.1 (host allowlist, empty by default;
  non-interactive; user's own helpers for listed hosts only). Alternative: no network access at all,
  which makes every git item skip the live check (SCAN-FORMAT.md requires it), so no git item could
  be removed.
- **D-2. Binding plan and scan.** Recommended: C-2; until then, `generated_at` plus per-item path and
  class (7.1).
- **D-3. Non-git items and `ask` items.** Recommended: apply acts only on removing actions;
  outputs, loose code and sessions wait for a core plan action (reported as deferred against the
  requirements, section 2).
- **D-4. Secrets.** Recommended: no local vault; an item holding a secret-like file is kept unless
  `--upload-secrets`, with an unencrypted-storage warning and `--require-encrypted-remote`. Why: a
  same-disk copy is not a backup, a vault adds key or file management that can lose the only copy,
  and keeping the item loses nothing. Cost: items with `.env` files are not reclaimed until the user
  opts in. Alternative: client-side encryption of secret-like files with a key kept in the OS
  keychain and shown once, which adds a crypto dependency, key loss as a new way to lose data, and a
  restore that needs the key. The user earlier approved a local vault for a private script; this
  drops it, so the user should confirm.
- **D-5. Recovery refs.** Recommended: off by default; opt-in as in 8.2, disabled in builds until
  AC-13 passes. This departs from the requirement as first written (push recovery branches for
  unsaved work, including dirty and untracked work); the archive holds all of it.
- **D-6. rclone command line**, not the rc API that DESIGN.md names: `rcat --size`, `cat`, `lsjson`
  behind a `Store` trait; minimum rclone version pinned in the code and checked at start.
- **D-7. Full read-back is mandatory.** The dry-run summary estimates the egress.
- **D-8. Permanent deletion**, only under 13.2 (handle-relative, mount-aware, no sync folders);
  `manifest verify` notices objects lost later.
- **D-9. Plan age** 24 hours, checked per item.
- **D-10. Automatic mode per class**, with `repo-recover` separate.
- **D-11. One apply per user per machine**, lock path from the user database, root refused unless
  `--allow-root`.
- **D-12. Whole-item archive** for every removed git item (more storage than the preserve sets
  alone, no reliance on git's clean judgement).
- **D-13. Windows is backup-only and restore** in this release.

## 17. Documented limits

- A process can still write into an item through a file it opened before the quarantine rename;
  writes after the final comparison and before deletion are lost. Only working directories are
  checked, not open files.
- Dependents outside the item are as fresh as the scan (bounded by the plan age); references from
  outside the scanned roots were never seen (SCAN-FORMAT.md, known limits).
- Secret detection is best effort (9.2). Anything it misses is uploaded to the user's own storage,
  unencrypted unless the remote is `crypt`.
- ACLs, owners and groups are not archived; mode bits are restored on Unix only. Sparse files are
  restored dense. A hard link to a file outside the item is restored as an independent file.
- Sync-folder detection covers the providers listed in 7.4; another sync client is not recognised.
  macOS `~/Desktop` and `~/Documents` are always treated as synced.
- Verification proves the stored object equalled what was sent at the time; `manifest verify` notices
  later loss, it cannot prevent it.
- The lock does not cover two users, or two machines sharing a network home.
- Whether `rclone rcat --size` avoids a local spool on every backend is unverified (9.4); the
  temporary volume is checked for room for one part instead.

## 18. Resource limits

- Archives are streamed; local disk use is the journal, the manifest segments, the index copies and
  scratch repositories (refs only, no fetch).
- Before each item apply checks free space on the state directory's volume and on rclone's
  temporary directory's volume, and stops (before starting the item) below `--min-free` (default the
  larger of 2 GiB and 2% of the volume) plus, on the temporary volume, one part.
- Memory is bounded by fixed buffers (1 MiB per stream); nothing holds a whole file in memory.
- One item at a time; one rclone process at a time.
- Bytes freed are measured as the change in free space, alongside the sum of sizes.

## 19. Exit codes and output

- 0: dry run completed, or every selected item ended removed, `backed-up-kept` by request
  (`--backup-only`, answer `s`), answered `n`, or was already done.
- 1: refused before acting (usage, inputs not bound, plan too old, lock held, root without
  `--allow-root`, `--remove` on Windows, rclone or git missing or too old, storage unusable,
  `--require-encrypted-remote` with a plain remote).
- 2: completed, but at least one item was skipped, failed, or ended `backed-up-kept` for any reason
  other than a request.
- 3: interrupted; the run id is printed for `--resume`.

The summary lists every item with its outcome and reason, bytes freed (measured and summed), bytes
uploaded, whether the storage is encrypted, and where the manifest is. `--json` writes the same as
JSON. No output, journal line or record holds a secret value or a credential URL.

## 20. Acceptance criteria

Tests live in `crates/apply/tests/` and run in CI on ubuntu, macOS and Windows. Unless stated, they
use temporary directories, the in-process `LocalDirStore` or `FaultStore` (a wrapper that injects
partial writes, flipped bytes on read, success reported as failure, objects deleted after
verification, stalls), and local git repositories. The network paths are exercised through the
production runner against local servers (AC-10). A separate job runs the flows through the real
`rclone` binary against an rclone `local` remote defined in a temporary config file (no credentials
anywhere). A library step hook lets a test act between any two steps; the binary never enables it.

- **AC-1** Dry run is the default: without `--backup-only` or `--remove`, apply prints what it would
  do with the fresh re-check result and writes nothing: no state directory, no storage object, no
  ref or file anywhere. Verify by: `dry_run.rs` snapshots tree, state path, store and remote refs
  before and after; zero store writes.
- **AC-2** `--backup-only` does steps 1 to 7 and removes nothing; `--remove` does the whole sequence
  on macOS and Linux and is refused (exit 1, nothing done) on Windows. Verify by: `modes.rs` in the
  three-OS matrix.
- **AC-3** Plan and scan are bound (7.1): schema, scan identity, per-item path and class, no
  duplicate items, no future dates; the plan age is enforced at start and before each item. Verify
  by: `inputs.rs` with a clock injected through the library.
- **AC-4** Only removing actions are acted on, and only when a fresh plan of the item gives the same
  action. Verify by: `inputs.rs` with `ask` and `keep` items edited into removing actions.
- **AC-5** The item is re-scanned with core's item re-scan and skipped, naming the first difference,
  when anything in 7.2 differs; a regenerable item is classified by its parent's markers. Verify by:
  `recheck.rs`, one case per field of 7.2, and a core test that `rescan_item(proj/node_modules)`
  equals the full scan's node (C-3).
- **AC-6** The process check runs at step 2 and step 9; a process working inside the item skips it.
  Verify by: `recheck.rs` starts a child process inside the item at each point.
- **AC-7** Items in the places of 7.4 are skipped: sync folders (each listed marker), placeholders,
  mount roots including a same-file-system bind mount, network file systems, the tool's state or
  local storage, immutable entries. Verify by: `places.rs` with markers and flags created in
  temporary directories; the Linux mount job with a tmpfs and a bind mount inside an item; macOS
  `SF_DATALESS` simulated through the library's metadata interface (setting it needs a file
  provider) plus a unit test of the flag check.
- **AC-8** The fingerprint chain of 6.4 holds: any change between step 2 and step 10 skips the item
  or, at quarantine, renames it back and records it. Verify by: `toctou.rs` writes, deletes, touches,
  renames and swaps entries through the step hook between every pair of steps.
- **AC-9** The live remote check of 7.5 runs before any push, upload or removal; a missing tip, a
  moved branch, a changed tag, an unlisted host or a failure skips the item, and no fetch is made.
  Verify by: `remote.rs` through the production runner against the local servers of AC-10.
- **AC-10** The production network runner (8.1) works end to end and no wider: against a test
  repository served over https on localhost (with a CA file in `git_ssl_ca_file`) and over ssh from
  a local sshd, a listed host succeeds; a dummy credential helper logs every host it is asked about
  and is never asked about an unlisted host; an unknown ssh host key fails without a prompt; a URL
  or host starting with `-` is rejected. Verify by: the `network` CI job on ubuntu (sshd in a
  container) and macOS (https), running `network.rs`.
- **AC-11** Credential helpers are found and work under the network runner: on each OS a helper
  configured globally, unscoped and host-scoped (`credential.https://HOST.helper`), is passed only
  for its host, and `git credential fill` through the runner returns its dummy value (Linux:
  `store`, which needs `HOME`; macOS: `osxkeychain` with a dummy item in a temporary keychain;
  Windows: Git Credential Manager in non-interactive mode or, where the runner image lacks it, a
  scripted helper, recorded as such). Verify by: `helpers.rs` in the three-OS matrix.
- **AC-12** With `--push-recovery`, recovery refs follow 8.2: only branch, detached-HEAD and
  pseudoref commits (never `FETCH_HEAD`, stashes, reflog-only commits or uncommitted work); only to
  the named or confirmed remote; under `refs/disk-cleaner/recovery/`; withheld for a public remote or
  secret-like content; never forced; verified with `ls-remote`; no hook runs; the user's git
  directory is unchanged; in automatic mode a push that did not happen keeps the item. Without the
  flag nothing is pushed. Verify by: `recovery.rs` through the local servers of AC-10.
- **AC-13** The recovery namespace is accepted by a real hosting service, listed by `ls-remote`, and
  triggers no workflow, webhook delivery or deployment. Verify by: `scripts/probe-ref-namespace`
  run by a person against a disposable GitHub repository that has an `on: push` workflow for all
  refs and a webhook to a request bin: push a ref under the namespace with a scoped token, wait 10
  minutes, record zero workflow runs and zero deliveries. Until a passing record exists in the
  repository, the build keeps `--push-recovery` disabled.
- **AC-14** Every removed item is archived whole (9.1), and both preserve sets, enumerated at apply
  time with `git_admin_files` and `object_store_files`, are inside the archive. Verify by:
  `archive.rs` extracts the archive and compares it entry by entry with a snapshot of the fixture,
  and checks the two core lists against the index.
- **AC-15** Secret-like files (9.2) keep their item by default; `--upload-secrets` archives them; a
  non-`crypt` remote is warned about and `--require-encrypted-remote` refuses it. Verify by:
  `secrets.rs` plants fake values (names, global-ignore matches, content patterns, a password URL in
  `.git/config`) and searches every stored object, segment, journal line and output for them.
- **AC-16** The archive is a deterministic pax tar as in 9.3, with extended attributes recorded and
  the listed harmless ones dropped, split into parts whose length is known before upload, with a
  self-describing index. Verify by: `archive.rs` archives a tree twice (identical bytes), checks long
  and non-UTF-8 names, symlinks, internal hard links, xattrs and a resource fork (macOS), and a part
  size small enough to split a file.
- **AC-17** A part counts as verified only after `rcat --size` succeeded, the read-back length and
  SHA-256 equal what was sent, and the listing shows that size; names are never reused; at most 3
  attempts. Verify by: `upload.rs` with `FaultStore`, and `rclone_local.rs` with the real rclone
  binary, which also checks the `--size` argument.
- **AC-18** The index object is uploaded and verified like a part. Verify by: `upload.rs`.
- **AC-19** Records carry the fields of 12.1, segments are immutable with a valid trailer and are
  mirrored with read-back verification, and duplicate records collapse by identity. Verify by:
  `manifest.rs`.
- **AC-20** Nothing is removed before every condition of 13.1 holds, in that order. Verify by:
  `ordering.rs` records store, journal and file-system operations and fails on any removal that
  precedes a required step.
- **AC-21** `manifest list`, `find` (with `--files`), `verify` (with `--deep`) and `orphans` work
  from local state and from storage alone. Verify by: `manifest.rs` with an empty state directory.
- **AC-22** Walking and deletion follow 13.2: handle-relative, no symlink followed, no mount
  entered, no-replace rename, nothing outside the quarantine touched. Verify by: `remove.rs` (a
  symlink to an outside directory; a subdirectory swapped for such a symlink during archiving and
  during deletion through the step hook; a read-only tree; an occupied rename target) and the Linux
  bind-mount job.
- **AC-23** A `remove-regenerable` item is removed only while a fresh plan still says so, with no
  backup unless asked. Verify by: `regenerable.rs`.
- **AC-24** Every transition is journaled before it takes effect; a run stopped at any transition
  resumes to the same end state; every apply first reconciles unfinished quarantines, unsynced
  records and scratch repositories; a resume refuses changed inputs. Verify by: `resume.rs` stops at
  each transition in turn and resumes in a new instance, starts a new apply instead of resuming, and
  kills the real binary during an rclone upload.
- **AC-25** Applying a finished plan again changes nothing and reports items as done. Verify by:
  `resume.rs`.
- **AC-26** One apply per user per machine (10.2), the lock path independent of `HOME` and
  `XDG_*`, no stale lock after a kill, root refused without `--allow-root`. Verify by: `lock.rs`
  with two processes under different environments and state directories, and a killed holder.
- **AC-27** Apply stops before an item when free space on the state or temporary volume is below
  section 18's floor, when the upload budget (logical size) would be exceeded, or after three
  failing items. Verify by: `limits.rs` with an injected free-space reader and `FaultStore`.
- **AC-28** Ask mode follows 11: the prompt's contents, typed-ahead input discarded, no terminal or
  end of input means keep, `q` stops before the next item. Verify by: `ask.rs` through the library's
  prompt interface and the binary through a pipe.
- **AC-29** Automatic mode runs only the classes named by `--auto` or `[apply] auto`; none by
  default. Verify by: `ask.rs`.
- **AC-30** Restore follows 13.3, and the restored item equals the original fixture snapshot taken
  before removal, entry for entry, attributes included. Verify by: `restore.rs` round trips for each
  action.
- **AC-31** Restore works on a new machine from storage alone. Verify by: `restore.rs` with an empty
  state directory and `--storage`.
- **AC-32** Nested items follow 6.3, including `--backup-only` and a nested git item. Verify by:
  `nested.rs` (a repository holding a removable `node_modules`: inner removed; inner answered `n`;
  backup-only; a nested repository).
- **AC-33** Platform behaviour of section 14: Windows backup and restore with paths over 260
  characters, read-only files, case and normalisation collisions refused on restore. Verify by:
  `platform.rs` in the three-OS matrix.
- **AC-34** Exit codes and the summary follow section 19; no secret value or credential URL appears
  in any output, journal or record. Verify by: `output.rs` with a remote URL holding a fake token.
- **AC-35** Every git command goes through core's hardened runner or the network runner; no program
  named by scanned content runs during re-check, live check or push. Verify by: `hardening.rs` with
  hooks, a `.gitattributes` filter, and config keys naming programs, each writing a marker file.

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
  10: the item is not removed.
- **NEG-9 (AC-9)** The remote branch reset so HEAD is not contained; a remote tip this clone does not
  have; a tag moved on the remote; an unreachable remote; an `insteadOf` rule to a local path; an
  `ext::` or `file://` URL: skipped, no fetch made.
- **NEG-10 (AC-10)** A scanned remote on an unlisted host with a dummy helper configured: the helper
  is never called and no connection is made. A URL `-oProxyCommand=...` or host `-x`: rejected. An
  ssh server with an unknown host key: the check fails at once, nothing prompts.
- **NEG-11 (AC-11)** A host-scoped helper for host A and a repository on host B (listed): the helper
  for A is not passed.
- **NEG-12 (AC-12)** A `git stash -a` holding `.env` and a local commit adding `id_ed25519`, with
  `--push-recovery`: the stash is not pushed and the commit's ref is withheld; neither secret is in
  any object the remote received. A remote readable anonymously: push withheld. An automatic-mode
  push that fails: item kept. An existing recovery ref of the same name: a suffix, never forced.
- **NEG-13 (AC-13)** Without a passing probe record, `--push-recovery` exits 1 saying it is not yet
  verified.
- **NEG-14 (AC-14)** A git directory file that cannot be read, a name core cannot enumerate, a fifo
  in the item, a preserve-set file missing from the index: skipped before any upload counts.
- **NEG-15 (AC-15)** `.env`, `id_ecdsa`, a globally ignored `secrets.local`, an `unusual.txt` holding
  a private key block, and an https URL that carries a user name and token before the host in
  `.git/config`, `FETCH_HEAD` and `logs/HEAD`: the item is kept by default and none of the values appears in any stored object,
  segment, journal line or output; with `--upload-secrets` they are archived; with
  `--require-encrypted-remote` and a plain remote, exit 1 before any upload.
- **NEG-16 (AC-16)** An xattr over 64 KiB, or a Windows ADS other than `Zone.Identifier`: item
  unsupported, skipped.
- **NEG-17 (AC-17)** The store fails mid-part; one byte differs on read-back; the read-back is short;
  the upload succeeds but the tool sees a failure (lost response); the transfer stalls: never
  verified, new attempt with new names, at most 3, orphans listed, nothing removed.
- **NEG-18 (AC-20)** The segment upload fails: backed up and kept. A part deleted from storage
  between verification and the second existence check: skipped, nothing removed.
- **NEG-19 (AC-21)** A truncated or altered segment in storage: reported damaged, its records
  unverified; `manifest verify` reports an object deleted after verification.
- **NEG-20 (AC-22)** A subdirectory swapped for a symlink to an outside directory during archiving:
  no outside file is read (none appears in any stored object). The same swap during deletion: the
  outside directory is unchanged. An occupied quarantine name: the rename fails, nothing replaced.
- **NEG-21 (AC-23)** A regenerable directory that gained a `.git`, an output file or a `.env`:
  skipped.
- **NEG-22 (AC-24)** A run stopped at each transition and resumed, or followed by a new apply without
  `--resume`: never an item missing from disk without a verified archive and a synced `backed-up`
  record; never two records for one identity in `manifest list`; every quarantine completed or
  renamed back; leftover scratch repositories deleted. A resume with a different plan, scan or flags:
  refused. `quarantine-intent` written but the rename not done: the item is checked again from
  `segment-synced`.
- **NEG-23 (AC-26)** Two applies at once with different `HOME`, `XDG_DATA_HOME` and `--state-dir`:
  the second exits 1. A killed holder: the next apply starts. Run as root without `--allow-root`:
  exit 1.
- **NEG-24 (AC-27)** Free space below the floor on the temporary volume only; the budget reached;
  three failing items: each stops before the next item, remaining items untouched and listed.
- **NEG-25 (AC-28)** No terminal: every item kept, nothing waits. `y` typed before the prompt:
  discarded. The item changes after the answer: not removed.
- **NEG-26 (AC-30)** A corrupted part: nothing at the target, staging removed. An archive with
  `../x`, an absolute name, a symlink then a file through it, or a hard link to outside: refused,
  nothing written outside staging. An existing target, or an entry that already exists in staging:
  refused. Too little free space: refused before extraction.
- **NEG-27 (AC-33)** Two names differing only by case, or NFC and NFD forms of one name, restored to
  a case-insensitive or normalising file system; a name invalid on Windows restored there: refused,
  naming them.
- **NEG-28 (AC-34)** A remote URL with an embedded fake token: absent from output, journal and
  records.
- **NEG-29 (AC-35)** A repository with a `.gitattributes` filter, hooks and config naming programs:
  no marker file is written during re-check, live check or push.
- **NEG-30 (AC-32)** A repository whose nested `node_modules` is answered `n` or is outside
  `--class`, with `--remove`: the repository is skipped. The same with `--backup-only`: the
  repository is archived with the `node_modules` inside.

## 22. Review of revision 2: findings and changes

| Finding | Change in revision 3 | Criteria and negative cases |
|---|---|---|
| M1 credentials and prompts to hosts named by scanned config | Host allowlist (empty by default), helpers only for listed hosts, non-interactive runner, ssh batch mode and strict host keys, `--end-of-options`, `-` rejected, unlisted host means skip (8.1, 7.5) | AC-9, AC-10, AC-11; NEG-9, NEG-10, NEG-11 |
| M2 stash parents and secret-bearing commits pushed | Push off by default; never stashes, reflog-only commits or uncommitted work; content check withholds a ref (8.2) | AC-12; NEG-12 |
| M3 refs that trigger CI, wrong remote, public remote, machine name | Namespace outside heads and tags, gated by a real-host probe; remote named or confirmed; public remote withheld; no machine name (8.2) | AC-12, AC-13; NEG-12, NEG-13 |
| M4 bind mounts and symlink races | Handle-relative walk and delete, mount roots and ids, no-replace rename, refuse without mount information (13.2, 7.4) | AC-7, AC-22; NEG-7, NEG-20 |
| M5 sync folders and placeholders | Such items are kept (7.4) | AC-7; NEG-7 |
| M6 "reproducible" files trusted on git's stat check; restore fidelity | Whole-item archive; restore compared with the original snapshot; no checkout in restore (9.1, 13.3, D-12) | AC-14, AC-30; NEG-8 |
| M7 same-disk vault as the only copy | No vault; secret-like files keep the item unless uploaded (9.2, D-4) | AC-15; NEG-15 |
| M8 globally ignored files | Treated as secret-like (9.2) | AC-15; NEG-15 |
| M9 detection by name only, unencrypted upload | More names, bounded content check, encryption warning, `--require-encrypted-remote` (9.2) | AC-15; NEG-15 |
| M10 regenerable items cannot be re-scanned alone | Core item re-scan and project activity (C-3, 7.2) | AC-5, AC-23 |
| M11 production network path untested | Local https and ssh servers through the production runner; helper probe as a criterion | AC-10, AC-11 |
| M12 rclone spools to disk | `rcat --size` with the exact length; temporary volume in the free-space check (9.4, 18) | AC-17, AC-27; NEG-24 |
| M13 xattrs, forks, ADS dropped silently | Archived in pax records; harmless ones dropped by list; otherwise unsupported (9.3) | AC-16, AC-30; NEG-16 |
| Improvement 1 backup-only and the nested veto | Veto applies only with `--remove` (6.3) | AC-32; NEG-30 |
| Improvement 2 nested git items | Enclosing item skipped, stated (6.3) | AC-32 |
| Improvement 3 stranded quarantine | Every apply reconciles first (6.1) | AC-24; NEG-22 |
| Improvement 4 rename semantics | No-replace rename; failed rename back keeps the quarantine (6.2, 13.2) | AC-22; NEG-20 |
| Improvement 5 duplicate and stranded records | Identity and dedupe; next apply syncs (10.1, 6.1) | AC-19, AC-24 |
| Improvement 6 self-describing archives | Index header carries path, machine, action, HEAD and parts (9.3) | AC-16 |
| Improvement 7 fsmonitor socket | Recorded by name, not archived (section 4) | AC-14 |
| Improvement 8 restore hardening | Create-new, normalisation collisions, free space, directory modes last (13.3) | AC-30; NEG-26, NEG-27 |
| Improvement 9 sparse files | Budget by logical size (9.5) | AC-27 |
| Improvement 10 lock path and root | Lock path from the user database; root refused (10.2, 5) | AC-26; NEG-23 |
| Improvement 11 clocks | Future dates refused; age per item; UTC dates (7.1, 8.2) | AC-3; NEG-3 |
| Improvement 12 silent push degrade | Automatic mode keeps the item; ask mode asks (8.2) | AC-12; NEG-12 |
| Improvement 13 storage guards | Same-device warning; wrappers resolved (7.4) | AC-7; NEG-7 |
| Improvement 14 vault housekeeping | No vault | n/a |
| Improvement 15 exit codes and resume flags | Section 19; resume takes no flags | AC-24, AC-34 |
| Improvement 16 FETCH_HEAD | Never pushed (8.2) | AC-12 |
| Improvement 17 unbounded fetch | No fetch at all (7.5) | AC-9; NEG-9 |
| Improvement 18 deletion pre-checks | Immutable flags refused before quarantine (7.4) | AC-7; NEG-7 |
| Improvement 19 duplicate items | Refused (7.1) | AC-3; NEG-3 |
| Improvement 20 bytes freed | Measured as free-space change (18) | AC-34 |
| Improvement 21 prompt contents | Section 11 | AC-28 |
| Improvement 22 outside hard links | Section 17 | n/a |
| Improvement 23 requirements coverage | Section 2 reports this phase as partial | n/a |
| Contradiction: plan header "saved with the manifest" | Every git file is in the archive the manifest references, so it holds | AC-14, AC-20 |
| Contradiction: DESIGN.md sections 4 to 6 and 10 | Updated in the same change to point here | n/a |

## 23. Test and build plan

- New crate `crates/apply` (library: re-check, places, network runner use, recovery, archive, store,
  manifest, journal, lock, restore) and CLI wiring in `crates/cli`. Core changes are additive only
  (section 15).
- Tests are written first; each behaviour gets a red CI run before its implementation lands.
- Builds and tests run in GitHub Actions on ubuntu, macOS and Windows; extra jobs: Linux mounts
  (tmpfs and bind mount, with `sudo`), `network` (local https and sshd), real rclone (a pinned
  release whose published SHA-256 is checked before use). AC-13 is a recorded manual probe.
- No test touches any directory outside its own temporary directory, and no test uses a network
  remote or a real credential.
