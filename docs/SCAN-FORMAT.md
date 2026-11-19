# Scan and plan JSON format

Schema version: **3** (`schema_version` in every file). Any incompatible change bumps it; `plan`
refuses a scan with a different version. Version 2 added the facts `reflog_saved` and
`no_pending_state` to `repo.facts` (a version 1 scan has neither, and a plan built from it would
have read a repo as saved without them, so it is refused: scan again). All sizes are bytes. All times are unix seconds (UTC).
Paths are strings; a path that is not valid UTF-8 is converted lossily, so it will not round-trip.

Produce a scan with `disk-cleaner scan <path>... --out scan.json` (or `--json` for stdout) and a
plan with `disk-cleaner plan --scan scan.json [--active-window-days N] [--config FILE] [--active PATH]... [--trust-partial-process-check] [--json]`. Both are
read-only. Nothing in this phase deletes, moves or modifies anything; `--out` writes only the file
you name.

## Scan file

| Field | Meaning |
|---|---|
| `schema_version` | integer, currently 3 (3 added `enclosing_repos`; a version 2 scan lacks the checks that fill it, so it is refused) |
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
| `enclosing_repos` | scan roots that lie inside a git repository the scan did not cover: `root`, `repo`, `note`. Before the walk each root is looked at upward for a `.git` directory or file in any ancestor (the real path first, then the spelled one; symlinks are not followed). That repository is not a node and never gets a plan item, but the regenerable and cache directories below the root that no scanned repo encloses are decided by its git exactly as in a full scan: the config allowlist and work-tree checks run on it, and a directory is removable only if git lists it as wholly ignored and untracked. A refused config, a git failure or timeout, `--no-git`, or an ancestor `.git` that is not a plain directory or file makes them ask. The plan header and the human outputs say the repository was not assessed |

A `.git` entry that is neither a regular directory nor a regular file (a symlink, socket, fifo or
device) is ambiguous evidence of a repository. It is never followed and git is never run through it.
Inside a regenerable or cache directory it gives that directory an `ask_reason`; anywhere else the
directory becomes a `repo` node whose facts are all `unknown` (note: the `.git` entry was not
followed), so it and the artifacts under it ask and are not classified as plain loose code.

A git directory that another directory reaches through a `.git` symlink has dependents: removing it
would break that checkout's history. The link is read as a path only (`readlink`, then a
canonicalisation of the path itself; nothing is opened and no git runs through it). When the
target is, or lies inside, a git node of the scan, that node's `no_dependents` becomes `no` with a
note and an entry in `dependents` naming the checkout. When a `.git` entry is not a symlink, cannot
be resolved, or leads outside every git directory in the scan, which repository it depends on is
unknown: every git node that had `no_dependents = yes` gets `unknown` (a known `no` stays) and a
warning says why.

**The scan document is validated on load** (`plan` reads it through `read_scan`) before anything
plans from it: every `nodes[].parent`, `nodes[].repo.parent_node`, `big_files[].node` and
`outputs[].node` must be in range, a node cannot be its own parent, the parent links must form no
cycle (one iterative pass, whatever the order of `nodes`), and the sums of the byte and file counts
must not overflow 64 bits. Negative counts are rejected by the parser. A violation is a clean error
naming the field and index (`invalid scan file: nodes[3].parent: index 999999 is out of range`),
exit code 1, no partial plan. The planner itself has no recursion, so a deep valid tree cannot
overflow the stack; for a scan built by hand, bad parent links are ignored.

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
  Windows and a warning says so. A skipped directory's contents are never seen, so the node that
  holds it (or the mount point itself, when a rule matches it) records it in `skipped_mounts` and is
  never offered for removal; enclosing nodes then ask through the plan's enclosing rule.
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
| `unreadable` | entries inside this node that could not be read (omitted when 0) |
| `skipped_mounts` | directories inside this node on another file system that `--one-file-system` did not enter (omitted when 0) |
| `ask_reason` | why this regenerable or cache node is never offered for removal |
| `repo` | present for `repo`, `worktree` and `bare-repo` |

Classification rules (design section 3): a directory with a `.git` directory is a `repo`; with a
`.git` file a `worktree`; a directory with `HEAD`, `objects` and `refs` and no `.git` is a `bare-repo`.
A directory whose name matches a rule in `rules/artifacts.toml` is `regenerable` or `cache` when
the rule's marker file or directory exists in the directory beside it, that is, its parent (a marker
further up proves nothing; a rule without markers matches by name alone; ambiguous names such as
`build` always have markers). Regenerable and cache directories are opaque: nothing inside them is
classified, so a repo vendored inside `node_modules` is not reported as a node; instead the
directory gets an `ask_reason`. Opaque does not mean unchecked: while the scan is inside one, the
rules that carry `ask` or `ask_if_contains` are still evaluated for the directories nested in it,
and file names are compared with the `[[state_file]]` list (below); any hit gives the enclosing
artifact an `ask_reason`, and the plan's enclosing rule then makes every repo and directory around
it ask. A directory outside any repo that holds a project manifest or
source files is `loose-code` (a scan root counts only if it holds a manifest).

A regenerable or cache node carries an `ask_reason`, and is never offered for removal, when: a git
repository lives inside it; it is a build directory (`scan_outputs`) in loose code, where no git
history proves it holds no hand-written source; or, inside a repo, git does not show it as wholly
ignored and untracked (a tracked or partly tracked `build/` may hold source); or its rule says
it can hold content that exists nowhere else (`ask` in the rules file: the Maven local repository,
which also holds artifacts installed with `mvn install`, and `.terraform`, which records backend
configuration), or the directory holds an entry its rule says a generated directory never has
(`ask_if_contains`: a `.gradle` beside a build file that holds `gradle.properties` or `init.d` is the
Gradle user home, not a project cache), or a directory nested anywhere inside it matches such a rule
(`build/.terraform`, a `.m2/repository` inside `node_modules`), or a file inside it has a
unique-state name. `unreadable` counts entries inside a node that could not
be read and `skipped_mounts` the mount points inside it that were not entered; such a node is
incomplete and is never offered for removal.

Build directories (rule `scan_outputs = true`) are searched for output files (video, audio, images,
documents, archives, datasets, model weights, VM disks, database files and dumps; the extension
list is in `rules/artifacts.toml`) before they are offered for removal.
Each is listed in `outputs` whatever its size and the directory is flagged `contains_outputs`.
Dependency directories such as `node_modules` are not searched, because packages routinely ship
fixture archives. Outside build directories only outputs of at least `min_output_bytes` are listed.

**Unique-state file names** (`[[state_file]]` in `rules/artifacts.toml`, matched on the file name
only, ignoring case; no file is opened). Found anywhere under a regenerable or cache directory they
make it ask, whatever the rule: Terraform state (`*.tfstate`, `terraform.tfstate.*`) and variable
values (`*.tfvars`), `.env` and `.env.*` (not `.env.example`, `.sample`, `.template`, `.dist`),
`id_rsa`, `id_ed25519`, local databases (`*.sqlite`, `*.sqlite3`, `*.db`) and key material
(`*.pem`, `*.key`, `*.keystore`, `*.jks`, `*.p12`). The reason names the kind and the path inside
the directory, never contents. Trade-off: the databases and key material are `fixture_prone`:
installed packages ship them as test fixtures (a `test/fixtures/cert.pem`), and every JavaScript
project would otherwise ask. They are exempt only for a file that shows **evidence of belonging to
an installed package**, defined per rule in the rules file:

- `node_modules` (`fixture_files = ["package.json"]`): some directory between the file and the
  `node_modules` root, root excluded, holds a `package.json`. Scoped packages
  (`node_modules/@scope/pkg/package.json`) and nested `node_modules` qualify; a file in
  `node_modules/local/` with no `package.json` above it, or straight in the root, does not.
- Python environments (`.venv`, `venv`, `.tox`, `.cache/pre-commit`:
  `fixture_dirs = ["site-packages", "dist-packages"]`): the file sits in a package directory
  directly below a `site-packages` or `dist-packages` directory. A file in the environment root, or
  straight in `site-packages`, does not qualify.
- Cargo registry and git checkouts (a `Cargo.toml`), NuGet packages (a `*.nuspec`): same idea.
- Every other rule (`Pods`, `deps`, `.npm`, `.pnpm-store`, `.cache/pip`, `.cache/uv`,
  `.gradle/caches`, all build output directories) defines no evidence, so those names always ask.

What the evidence rule still cannot prove: a user file dropped inside an installed package's own
directory (`node_modules/pkg/notes.db`, `site-packages/pkg/local.key`) looks exactly like a
fixture and is not noticed by name. Terraform state, `.env` and the SSH key names ask everywhere,
with or without evidence.

### `repo`

| Field | Meaning |
|---|---|
| `facts` | the nine safety facts, each `"yes"`, `"no"` or `"unknown"` |
| `notes` | why facts are not `yes` (git errors, ignored paths found); every note passes one redaction function (see below) |
| `remotes` | `name`, `url` (credentials stripped), `network` |
| `head_commit`, `last_commit_time` | HEAD id; newest commit on any ref |
| `worktrees` | live linked worktrees reported by git (may be outside the scan) |
| `parent_repo`, `parent_node` | for a linked worktree: its repo as git reports it, and its index in `nodes` if scanned |
| `dependents` | clones in the scan that borrow objects from this repo (alternates; the file is read only if it is a regular file under 64 KiB, otherwise a warning is recorded and `no_dependents` of the other git nodes becomes `unknown`, see below) |
| `submodule_gitdirs`, `git_dir` | submodule git directories under `.git/modules`; this node's own git directory |
| `sizes` | `total`, `git` (the `.git` directory), `regenerable`, `output` |

Facts (the safe value is always `yes`):

| Fact | `yes` means |
|---|---|
| `has_network_remote` | at least one remote that is a network location; a local path, `file://` URL or `.bundle` is not |
| `head_on_remote` | the worktree's own HEAD, including a detached HEAD, is contained in a remote-tracking ref of a network remote |
| `all_branches_on_remote` | every local ref (branches, notes, replace refs...; not the stash or HEAD) is contained in a network remote ref, and there are no local tags: tag names are not visible in tracking refs, so a repo with tags is `unknown` until apply runs `git ls-remote` |
| `stash_empty` | the stash is empty |
| `tree_clean` | no tracked changes and no untracked files in this repo's own tree (submodules are their own nodes), no file marked skip-worktree or assume-unchanged (those hide local edits), and every submodule gitlink agrees: the index matches HEAD, and each checked-out submodule's HEAD equals the commit the index records (`status --ignore-submodules=all` hides both). A recorded submodule with a missing or empty directory is fine; a directory with files and no `.git` at a gitlink path, an unmerged gitlink or a differing commit is `no`; a submodule whose config fails the allowlist, whose HEAD cannot be read, or more than 200 submodules is `unknown`. The commits are compared with `ls-files -s`, `ls-tree -r HEAD` and `rev-parse --verify HEAD` run in the submodule through the hardened runner; no status runs inside it |
| `no_unique_ignored` | no ignored files outside regenerable and cache directories (`.env`, local databases...) |
| `no_dependents` | no linked worktree and, within the scanned roots, no alternates clone depends on this object store |
| `reflog_saved` | every commit reachable from any reflog entry (HEAD of this worktree and every ref's reflog) is contained in a network remote's tracking refs. An unpushed commit after `git reset --hard`, an amended or rebased-away commit and a stash's commits are `no`. No network remote is `no`; an unborn HEAD, any git failure or a timeout is `unknown` |
| `no_pending_state` | no unfinished rebase, merge, cherry-pick, revert or bisect in this worktree's git directory (`rebase-merge`, `rebase-apply`, `sequencer`, `MERGE_HEAD`, `CHERRY_PICK_HEAD`, `REVERT_HEAD`, `BISECT_*`) and no local-only content under `.git/lfs/objects` or `.git/annex/objects`. Found by name only. Never `no`: a finding is `unknown` (state that no ref records cannot be recovered into a ref, so a person decides) |

`reflog_saved` is computed with `git rev-list -n 1 --reflog --not --remotes=<network remote>...`, which
only reads refs, reflogs and objects (see git-rev-list(1); it takes no lock and writes nothing) and
stops at the first commit no tracking ref contains; the command timeout bounds it. In a linked
worktree it sees that worktree's HEAD reflog and the shared repo's other reflogs; the main repo may
not see a linked worktree's HEAD reflog, but a repo with linked worktrees is already
`no_dependents = no` and never archived. A bare repo keeps no reflog unless `core.logAllRefUpdates` is set. A zero-commit
repo is never `yes`. Reflogs expire (90 days, 30 for unreachable entries), so the fact errs toward
`no` for recent history: a branch squash-merged remotely keeps its original commits in its reflog and
reads `no` until they expire. Other state git keeps outside refs: `refs/original` and
`refs/rewritten` are refs and are covered by `all_branches_on_remote`; a non-empty `.git/rr-cache`
only records conflict resolutions that can be derived again and is not work.

Any git failure, timeout or missing binary makes the affected fact `unknown`, never `yes`. A bare
repo has no working tree, so its `tree_clean` and `no_unique_ignored` are `unknown`. Remote-tracking
refs can be stale; a later phase confirms them against the live remote with `git ls-remote` at apply
time. `no_dependents` cannot see clones outside the scanned roots. An alternates file that was not
read (a symlink, larger than 64 KiB, unreadable) leaves its provider unidentified, so every other
git node in the scan that had `no_dependents = yes` gets `unknown` instead (a known `no` stays).

**Object sharing.** Every user of each canonical object directory is kept, so repos that share
one (a symlinked `.git/objects`, or a `commondir` file pointing into another repo) all get
`no_dependents = no`, each naming the others in `dependents` and `notes`: an unpushed commit in one
lives in the other's directory. `objects/info` and `objects/pack` must be plain directories; a
symlink there is read as a path (never followed to run git) and is a dependency if it lands inside
a scanned repo's object directory. A symlinked `objects`, `objects/info` or `objects/pack` or a
`commondir` that leaves the scan, or a non-plain entry, is unresolved like an unread alternates
file (every other git node's `yes` becomes `unknown`). A linked worktree makes the repo git
reports as its parent a dependency (`no`), so a forged `commondir` cannot hide it. No allowlisted
config key can relocate objects: only `objects/info/alternates`, a symlink, or `commondir` can, and
the environment variables that could are cleared.

**Alternates syntax.** The file is parsed as git does: blank lines and lines starting with `#` are
skipped; an entry starting with `"` is C-style quoted (`\\`, `\"`, `\a \b \f \n \r \t \v`, `\ooo`
octal with a first digit of 0 to 3; anything else, a missing closing quote or text after it is
malformed); other entries are paths as written; a relative entry is resolved against the borrower's
`objects` directory. Each entry is canonicalised and must equal the object directory of a scanned
git node; an entry that is malformed, cannot be canonicalised, or names a place outside the scan
is never dropped: it is unresolved (see above) and a warning says why. Chains are followed
through scanned repos only: if A borrows from B and B from C, C lists both A and B as dependents.

Remote URLs have embedded credentials removed (`https://user:token@host/x` is stored as
`https://host/x`; everything up to the last `@` counts as userinfo, and query strings and fragments are
dropped) before they are stored, and all git error text is redacted the same way.

Everything stored from text a repository controls goes through one function (`redact::diag`):
notes, remote names, ask reasons built from file names, git error output, warnings. It strips URL
credentials and turns control characters into spaces. A rejected config key is never printed in
full: a key's subsection is free text and can be a credential URL
(`[credential "https://user:TOKEN@host"]`), so only the section and variable name are shown
(`credential.<redacted>.helper`). Paths of scanned directories are not redacted; they are the
data being reported.

**The repo's own config is untrusted.** A scanned folder may be an unpacked download, and its
`.git/config` can name programs that git would run (`filter.*.clean`, `gpg.program`, `core.fsmonitor`,
`core.sshCommand`, ...). Before any other git command the tool lists the config keys
(`git config --local --includes --name-only --list`, plus the worktree config when enabled). If any
key is outside a small allowlist of plain-data keys (`core.*` format and line-ending settings,
`remote.<n>.url|pushurl|fetch|push|tagopt|prune|promisor|partialclonefilter`,
`branch.<n>.remote|merge|rebase|description|pushremote`, `user.name|email|signingkey`,
`submodule.*.url|path|active|branch|ignore`, `url.*.insteadOf`, `extensions.*`, `commit.gpgsign`,
`tag.gpgsign`, and simple `init`, `gc`, `pack`, `receive`, `transfer`, `fetch`, `pull`, `push`,
`color`, `advice`, `gui`, `include` settings) then every fact is `unknown`, a note names the
section and variable of the key (not its subsection), and no further git command is run in that repo. The same check is applied to every
`<gitdir>/modules/**/config` (bounded walk; a symlink, unreadable file or too many entries also
counts as unsafe), because git runs a child status inside submodules using their own config; the
outer status uses `--ignore-submodules=all` since each checked-out submodule is its own node. A
`core.worktree` whose effective value (the shared config, then `config.worktree` when
`extensions.worktreeConfig` is set, the later winning) does not resolve to the node's own directory
is refused, and independently `git rev-parse --show-toplevel` must equal the node's real path
(bare repos excepted); any mismatch or failure makes every fact `unknown`. Repos using Git LFS
(`filter.lfs.*`, `lfs.*`) or custom merge or diff drivers are therefore reported `unknown`: no `lfs`
key is allowed. Git runs with `core.fsmonitor=false`,
`log.showSignature=false`, `core.ignoreStat=false`, `core.hooksPath` set to the null device,
`protocol.allow=never`, `GIT_NO_LAZY_FETCH=1`, in its own process group that is killed on timeout.

**Configuration from outside the repo is not read either.** Git also reads the system config, the
user's global config (`~/.gitconfig`, `$XDG_CONFIG_HOME/git/config`) and configuration passed in the
environment (`GIT_CONFIG_COUNT`/`GIT_CONFIG_KEY_n`/`GIT_CONFIG_VALUE_n`, `GIT_CONFIG_PARAMETERS`).
None of it is checked, and a globally configured filter driver (Git LFS installs one) selected by
a scanned repo's `.gitattributes` would run during `git status`. So every git command runs with
`GIT_CONFIG_NOSYSTEM=1`, `GIT_CONFIG_SYSTEM`, `GIT_CONFIG_GLOBAL`, `HOME` and `XDG_CONFIG_HOME` set
to the null device (older git that ignores `GIT_CONFIG_GLOBAL` still finds no file under `HOME`),
and with every `GIT_*` variable of the caller removed (`GIT_DIR`, `GIT_WORK_TREE`,
`GIT_INDEX_FILE`, `GIT_CONFIG_*`, `GIT_EXTERNAL_DIFF`, `GIT_PAGER`, `GIT_ASKPASS`, `GIT_SSH*`,
`GIT_EXEC_PATH`, `GIT_TRACE*`, ...), plus `PAGER` and `SSH_ASKPASS`. What that changes:

- A `.gitattributes` `filter`, `diff` or `merge` attribute whose driver is defined only
  globally has no driver, so git compares the raw content. An LFS-tracked file that was touched
  therefore shows as modified rather than clean (`tree_clean = no`, not `unknown`).
- Global ignore rules (`core.excludesFile`, the default `$XDG_CONFIG_HOME/git/ignore`) no longer
  apply. A file only they ignored is now untracked instead of ignored: `tree_clean` becomes `no`
  while `no_unique_ignored` may become `yes`, so the repo still fails a fact and gets
  `recover-then-archive`, never `archive`. Recovery (a later phase) must treat such a file as
  possibly secret: it was ignored on this machine even though the repo does not ignore it.
- A global or system `core.autocrlf` no longer applies, which can only make files look modified
  (`tree_clean` toward `no`). It has a cost on Windows, where Git for Windows sets
  `core.autocrlf=true` system-wide:
  in a CRLF checkout, a file whose stat data is stale (touched, copied) reads as modified, so an
  otherwise saved repo gets `recover-then-archive` (whose recovery step re-checks every fact)
  instead of `archive`.
- `safe.directory` is honoured only from global and system config, so a repository owned by
  another user is refused by git and every fact of it is `unknown`, never clean.
- A global `url.<base>.insteadOf` rewrite is not applied, so a remote is classified by the URL
  written in the repo's own config. This can move remote facts toward `yes` with nothing else
  failing: a remote whose written URL is a network address but which a global rewrite sends to a
  local mirror now counts as a network remote. Remote facts are labelled unverified until apply
  re-checks them against the live remote, which must resolve the effective URL.

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
`process_check_note`, `trusted_partial_process_check`, `remote_facts_verified` (always `false` in this phase) and
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
# trust_partial_process_check = true   # optional, see below; default false
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
When `partial`, the same holds: an unreadable process may be working inside the candidate, so every
removing action becomes `ask` (reason: some running processes could not be inspected) and
`totals.ready_bytes` is 0. The user can accept the risk with the setting
`trust_partial_process_check = true` in the config file or `plan --trust-partial-process-check`;
the plan then records `trusted_partial_process_check: true` and the human header says the user
accepted it. A readable process inside a candidate still keeps it. With no evidence at all the
state is `unknown`.

`action` is one of `keep`, `remove-regenerable`, `recover-then-archive`, `archive`,
`backup-then-remove` and `ask`. The plan has no age of its own beyond the window and the
`--active` paths, which are the user's. Rules, in order:

1. Active: `keep`.
2. Regenerable and cache: contains output files, has an `ask_reason`, is incomplete (`unreadable`
   or `skipped_mounts`), or state unknown: `ask`; otherwise `remove-regenerable`.
3. Repo and worktree: state unknown: `ask`; any fact `unknown`: `ask`; any fact `no`:
   `recover-then-archive`; every fact `yes`: `archive` (labelled unverified, see
   `remote_facts_verified`). A repo whose `.git/modules` hold a submodule git directory that no scanned
   git node inside it uses, or that is incomplete (`unreadable` or `skipped_mounts`), asks.
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
