# Scan and plan JSON format

Schema version: **1** (`schema_version` in every file). Any incompatible change bumps it; `plan`
refuses a scan with a different version. All sizes are bytes. All times are unix seconds (UTC).
Paths are strings; a path that is not valid UTF-8 is converted lossily, so it will not round-trip.

Produce a scan with `disk-cleaner scan <path>... --out scan.json` (or `--json` for stdout) and a
plan with `disk-cleaner plan --scan scan.json [--active-window-days N] [--config FILE] [--active PATH]... [--json]`. Both are
read-only. Nothing in this phase deletes, moves or modifies anything; `--out` writes only the file
you name.

## Scan file

| Field | Meaning |
|---|---|
| `schema_version` | integer, currently 1 |
| `tool_version` | version of the tool that wrote the file |
| `generated_at` | when the scan finished |
| `roots` | absolute scan roots |
| `options` | `one_file_system`, `top_files`, `min_output_bytes`, `git_facts` |
| `totals` | see below |
| `nodes` | classified directories (below) |
| `outputs` | output files listed individually (below) |
| `big_files` | the biggest `top_files` files: `path`, `bytes`, `node` |
| `issues` | up to 1000 unreadable entries: `path`, `message`; `totals.unreadable` has the full count |
| `warnings` | notes about limits that applied to this run |

### `totals`

`files`, `dirs`, `symlinks`, `allocated_bytes`, `logical_bytes`, `hardlink_duplicates_skipped`,
`unreadable`, `mount_points_skipped`, `output_bytes`, and `bytes_by_class`, a map from class name to
the sum of `bytes_self` of nodes of that class, with `other` for space outside any classified
directory. Output files are not a separate slice of `bytes_by_class` (they stay in the class of
the directory that holds them); `output_bytes` counts them separately.

### How size is counted

- **Allocated size** is what the filesystem reserved: `st_blocks * 512` on Unix. On Windows it is the
  logical length (allocated and compressed size are not read in this phase).
- A file with more than one hard link is counted once per `(device, inode)`. On Windows hard links
  are not detected and are counted per name.
- Symlinks are never followed; a symlink counts as the small link object.
- `--one-file-system` skips directories on another device than their scan root. It is ignored on
  Windows and a warning says so.
- Unreadable entries are counted and listed in `issues`; the scan continues.

### `nodes`

One entry per classified directory. A directory belongs to its innermost classified ancestor.

| Field | Meaning |
|---|---|
| `path`, `class` | `repo`, `worktree`, `bare-repo`, `regenerable`, `cache`, `loose-code` |
| `parent` | index of the enclosing classified node, if any |
| `rule` | the rule that matched (regenerable and cache) |
| `bytes_self` | bytes attributed to this node, excluding nested nodes |
| `bytes_total` | `bytes_self` plus all nested nodes |
| `files_total` | files in the subtree |
| `output_bytes` | allocated bytes of output-extension files in the subtree |
| `contains_outputs` | build directory that holds output files; never offered for removal |
| `newest_mtime` | newest modification time of files attributed to this node, excluding `.git` and nested nodes |
| `repo` | present for `repo`, `worktree` and `bare-repo` |

Classification rules (design section 3): a directory with a `.git` directory is a `repo`; with a
`.git` file a `worktree`; a directory with `HEAD`, `objects` and `refs` and no `.git` is a `bare-repo`.
A directory whose name matches a rule in `rules/artifacts.toml` is `regenerable` or `cache` when
the rule's marker file or directory exists in the directory beside it, that is, its parent (a marker
further up proves nothing; a rule without markers matches by name alone; ambiguous names such as
`build` always have markers). Regenerable and cache directories are opaque: nothing inside them is
classified, so a repo vendored inside `node_modules` is not reported as a node; instead the
directory gets an `ask_reason`. A directory outside any repo that holds a project manifest or
source files is `loose-code` (a scan root counts only if it holds a manifest).

A regenerable or cache node carries an `ask_reason`, and is never offered for removal, when: a git
repository lives inside it; it is a build directory (`scan_outputs`) in loose code, where no git
history proves it holds no hand-written source; or, inside a repo, git does not show it as wholly
ignored and untracked (a tracked or partly tracked `build/` may hold source). `unreadable` counts
entries inside a node that could not be read; such a node is incomplete and is never offered for
removal.

Build directories (rule `scan_outputs = true`) are searched for output files (video, audio, images,
documents, archives, datasets, model weights, VM disks, database files and dumps; the extension
list is in `rules/artifacts.toml`) before they are offered for removal.
Each is listed in `outputs` whatever its size and the directory is flagged `contains_outputs`.
Dependency directories such as `node_modules` are not searched, because packages routinely ship
fixture archives. Outside build directories only outputs of at least `min_output_bytes` are listed.

### `repo`

| Field | Meaning |
|---|---|
| `facts` | the seven safety facts, each `"yes"`, `"no"` or `"unknown"` |
| `notes` | why facts are not `yes` (git errors, ignored paths found); credentials removed |
| `remotes` | `name`, `url` (credentials stripped), `network` |
| `head_commit`, `last_commit_time` | HEAD id; newest commit on any ref |
| `worktrees` | live linked worktrees reported by git (may be outside the scan) |
| `parent_repo`, `parent_node` | for a linked worktree: its repo as git reports it, and its index in `nodes` if scanned |
| `dependents` | clones in the scan that borrow objects from this repo (alternates; the file is read only if it is a regular file under 64 KiB, otherwise a warning is recorded) |
| `submodule_gitdirs`, `git_dir` | submodule git directories under `.git/modules`; this node's own git directory |
| `sizes` | `total`, `git` (the `.git` directory), `regenerable`, `output` |

Facts (the safe value is always `yes`):

| Fact | `yes` means |
|---|---|
| `has_network_remote` | at least one remote that is a network location; a local path, `file://` URL or `.bundle` is not |
| `head_on_remote` | the worktree's own HEAD, including a detached HEAD, is contained in a remote-tracking ref of a network remote |
| `all_branches_on_remote` | every local ref (branches, notes, replace refs...; not the stash or HEAD) is contained in a network remote ref, and there are no local tags: tag names are not visible in tracking refs, so a repo with tags is `unknown` until apply runs `git ls-remote` |
| `stash_empty` | the stash is empty |
| `tree_clean` | no tracked changes and no untracked files in this repo's own tree (submodules are their own nodes), and no file marked skip-worktree or assume-unchanged (those hide local edits) |
| `no_unique_ignored` | no ignored files outside regenerable and cache directories (`.env`, local databases...) |
| `no_dependents` | no linked worktree and, within the scanned roots, no alternates clone depends on this object store |

Any git failure, timeout or missing binary makes the affected fact `unknown`, never `yes`. A bare
repo has no working tree, so its `tree_clean` and `no_unique_ignored` are `unknown`. Remote-tracking
refs can be stale; a later phase confirms them against the live remote with `git ls-remote` at apply
time. `no_dependents` cannot see clones outside the scanned roots.

Remote URLs have embedded credentials removed (`https://user:token@host/x` is stored as
`https://host/x`; everything up to the last `@` counts as userinfo, and query strings and fragments are
dropped) before they are stored, and all git error text is redacted the same way.

**The repo's own config is untrusted.** A scanned folder may be an unpacked download, and its
`.git/config` can name programs that git would run (`filter.*.clean`, `gpg.program`, `core.fsmonitor`,
`core.sshCommand`, ...). Before any other git command the tool lists the config keys
(`git config --local --includes --name-only --list`, plus the worktree config when enabled). If any
key is outside a small allowlist of plain-data keys (`core.*` format and line-ending settings,
`remote.<n>.url|pushurl|fetch|push|tagopt|prune|promisor|partialclonefilter`,
`branch.<n>.remote|merge|rebase|description|pushremote`, `user.name|email|signingkey`,
`submodule.*.url|path|active|branch|ignore`, `url.*.insteadOf`, `extensions.*`, `commit.gpgsign`,
`tag.gpgsign`, and simple `init`, `gc`, `pack`, `receive`, `transfer`, `fetch`, `pull`, `push`,
`color`, `advice`, `gui`, `include` settings) then every fact is `unknown`, a note says
which key, and no further git command is run in that repo. The same check is applied to every
`<gitdir>/modules/**/config` (bounded walk; a symlink, unreadable file or too many entries also
counts as unsafe), because git runs a child status inside submodules using their own config; the
outer status uses `--ignore-submodules=all` since each checked-out submodule is its own node. A
`core.worktree` that does not resolve to the node's own directory is refused. Repos using Git LFS
(`filter.lfs.*`, `lfs.*`) or custom merge or diff drivers are therefore reported `unknown`: no `lfs`
key is allowed. Git runs with `core.fsmonitor=false`,
`log.showSignature=false`, `core.ignoreStat=false`, `GIT_NO_LAZY_FETCH=1`, in its own process group
that is killed on timeout.

Remote facts come from local remote-tracking refs, which can be stale (a remote changed with
`set-url` keeps its old refs). The plan therefore labels them unverified; apply (a later phase) must
re-check them with `git ls-remote`.

A repo's submodule git data under `.git/modules` (with its stash and local branches) is deleted
with the repo: `submodule_gitdirs` lists those directories (real paths) and `git_dir` is each git
node's own git directory. The plan asks unless every listed directory is the `git_dir` of a scanned git
node inside the repo (matched by identity, not by count); the reason names the orphaned directory.

## Plan file

`schema_version`, `dry_run` (always `true`), `generated_at`, `scan_generated_at`,
`active_window_days`, `window_source`, `process_check` (`ok`, `partial` or `unknown`),
`process_check_note`, `remote_facts_verified` (always `false` in this phase) and
`remote_facts_note`, `active`, `totals` and `items`.

Each item: `node` (index into the scan's `nodes`), `path`, `class`, `action`, `state` (`active`,
`idle` or `unknown`), `state_reason` (why it was judged so), `reason` (plain language),
`reclaimable_bytes`, `bytes_total`, `days_since_activity` (if there is any evidence) and `facts`.

### The active window

One setting decides activity: `active_window_days`, default **7**. It is read from
`<platform config dir>/disk-cleaner/config.toml` (`$XDG_CONFIG_HOME` or `~/.config` on Linux,
`~/Library/Application Support` on macOS, `%APPDATA%` on Windows), or from the file named by
`--config`, and overridden per run by `plan --active-window-days N`. The file is TOML:

```toml
active_window_days = 7
```

A missing default file means the default. A file that exists but is malformed, has an unknown key
or a non-integer or negative value is an error; there is no fallback to a shorter window. A
`--config` file must exist. The config is validated even when the flag overrides it.

A project is **active** when any of these holds:

1. an `--active PATH` covers it, or an active path lies inside it (or inside the project that owns a
   regenerable directory). `--active` paths must exist and must lie inside or around something the
   scan covered, otherwise `plan` fails; both sides are compared as real (symlink-free) paths;
2. its newest activity, the later of `newest_mtime` (non-regenerable, non-`.git` files) and
   `last_commit_time`, is at most `active_window_days` old (exactly at the edge counts as active);
3. a running process has its working directory inside it.

Process information comes from `sysinfo`. `process_check` is `ok` when every process's working
directory was readable, `partial` when some (usually other users') were not, and `unknown` when none
could be read. When `unknown`, a removing action is never proposed: it becomes `ask` and says so.
With no evidence at all the state is `unknown`.

`action` is one of `keep`, `remove-regenerable`, `recover-then-archive`, `archive`,
`backup-then-remove` and `ask`. The plan has no age of its own beyond the window and the
`--active` paths, which are the user's. Rules, in order:

1. Active: `keep`.
2. Regenerable and cache: contains output files, has an `ask_reason`, is incomplete (`unreadable`),
   or state unknown: `ask`; otherwise `remove-regenerable`.
3. Repo and worktree: state unknown: `ask`; any fact `unknown`: `ask`; any fact `no`:
   `recover-then-archive`; every fact `yes`: `archive` (labelled unverified, see
   `remote_facts_verified`). A repo whose `.git/modules` hold a submodule git directory that no scanned
   git node inside it uses, or that is incomplete, asks.
4. Bare repo: `keep` when something depends on it, otherwise `ask`.
5. Loose code: `ask`.
6. Enclosing: a node that would be removed becomes `ask` when any node inside it is `keep`, `ask` or
   `recover-then-archive` (a nested video, a submodule with a stash, a recently active nested
   project), because everything inside goes with it. A removable regenerable child does not block.

Activity evidence is the newest of `newest_mtime` and `last_commit_time`; a regenerable directory
uses its owning project. `totals.reclaimable_bytes` does not count an item nested inside another
removing item twice; `totals.ready_bytes` is only `remove-regenerable` bytes: an archive rests on
remote facts that are unverified until apply re-checks them, so it is never "ready". An absurdly large
`--active-window-days` saturates (everything with evidence is active) instead of wrapping.
`backup-then-remove` is reserved for the backup phase and is not emitted yet.
