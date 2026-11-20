# Apply spec: earlier reviews

How revisions 5 to 10 of [APPLY-SPEC.md](APPLY-SPEC.md) answered the earlier reviews, moved here
from section 22 (revision 5's answers in revision 6, revision 6's in revision 7, revision 7's in
revision 8, revision 8's in revision 9, revision 9's in revision 10, revision 10's in revision
11). Criterion and negative-case ids are those of revision 5 and later; section numbers are
those of APPLY-SPEC.md. Later changes to the same areas are listed in APPLY-SPEC.md section 22.

## Reviews of revision 9 (adversarial and acceptance)

F-ids are the adversarial plan review's; B-, C- and D-ids the acceptance review's, prefixed `r9`
to avoid clashes with core changes and decisions. The coordinator's two scope choices (D-13,
D-17) removed mechanisms. Revision 11 changed two of the answers below: the deletion pass now
compares each entry's full preserved record, content included, just before removing it (row r9
C-2), and nothing reads a leftover or walks into one in any mode (row r9 C-3).

| Finding | Change in revision 10 | Criteria and negative cases |
|---|---|---|
| r9 F1, F2 Windows spool cleanup | No Windows operation (D-13): exit 4 before any state (5, 14, 19); both findings are entry conditions (8.2) | AC-30; NEG-60 |
| r9 B-1 finish deleted unseen changes; r9 C-6 an enclosing item removed a quarantine left behind | No finishing (D-17): leftovers are never deleted, renamed or read (10.1); `quarantine list` (5); restore refuses targets inside a quarantine (13.3); items holding one are kept (7.4, C-6) | AC-21, AC-39; NEG-19, NEG-53 |
| r9 B-2 later hard-link names had no digest | Every name carries the group's size and digest (4), checked by restore (13.3) | AC-13; NEG-13 |
| r9 C-2 directory fields the deletion moves | The deletion pass compared only fields unlinking cannot move (13.2); revision 11 compares the full record and names the excluded fields | AC-19; NEG-61 |
| r9 C-3 a FIFO hangs readers | Nothing reads a leftover; every open is non-blocking with a kind check (13.2); revision 11 adds core's `open_regular` (C-7) | AC-19; NEG-19, NEG-62 |
| r9 C-4 vacuous completeness test | It parses the tar stream (AC-13) | AC-13; NEG-13 |
| r9 C-5 `u` reused after a new F0 | No answer is reused; asked again (11) | AC-38; NEG-52 |
| r9 D-1 rclone subprocesses hold the spool lock (hypothesis) | Disclosed (9.6, 17); "last known rclone pid"; revision 11 tests a descendant that outlives rclone | AC-23; NEG-63 |
| r9 minor items | Missing lock file counts as held (9.6); symlink xattrs (13.3); local index digest (12.3); macOS system flags (17); setgid (13.2); `u` labelling, object ids, automatic-mode hint (11) | AC-23, AC-38; NEG-56, NEG-61 |

Rewritten in revision 10: AC-2, 13, 19, 21, 23, 30, 38, 39 and NEG-2, 13, 19, 24, 34, 52, 53,
56. Added: NEG-60, 61, 62. None was withdrawn.

## Reviews of revision 8 (adversarial and acceptance)

F-ids are the adversarial plan review's; R8-ids and S-13 the acceptance review's. Revision 10
removed two of the mechanisms below: the finishing of interrupted deletions (rows F1, R8-1 and
the last S-13 row) and Windows operation (the Windows parts of F2 and R8-2). Revision 9's text is
at commit `fbf25f3`; APPLY-SPEC.md 8.2 and 8.3 keep the entry conditions.

| Finding | Change in revision 9 | Criteria and negative cases |
|---|---|---|
| F1 resumed deletion missed extended attributes, and checked directories by path and kind only | The preserved record (section 4), computed by the archive writer's function; resume compared it for every entry, with three deletion-caused exceptions; unarchived regenerable items never resumed on their own. Revision 10 removed resumed deletion; the record remains for restore's check | AC-21; NEG-19 |
| F2 a killed parent released the spool lock while its rclone child still wrote | An operation lock held by the tool, close-on-exec, and a spool lock every rclone child inherits (`flock`); a job object on Windows; cleanup needs both locks; the hung-child case. Revision 10 kept the Unix design and removed Windows | AC-23; NEG-56 |
| R8-1 no way out of a stranded quarantine | `quarantine list` and `quarantine finish`; a final stranded state. Revision 10 removed `finish`: a leftover is never deleted by the tool | AC-39; NEG-53 |
| R8-2 lock semantics per platform | `flock`, never `fcntl`; close-on-exec except the spool lock; an erroring call counts as held; Windows `LockFileEx` and share-delete, removed in revision 10 | AC-23; NEG-54 |
| R8-3 minimum git | 2.32, checked at start (ASSUMPTION on the value) | AC-9; NEG-55 |
| R8-4 and the user's decision on D-16 | The answer `u`, with the three limits shown word for word | AC-38; NEG-51, NEG-52 |
| S-13 network tips | The "only enlarges" sentence corrected; refs another remote's refspec writes excluded; hand-set refs disclosed | AC-12; NEG-59 |
| S-13 NEG-47 end to end | Both outcomes of core's plan | NEG-47 |
| S-13 step-2 secret checks | Run only for items that will be uploaded | AC-20; NEG-58 |
| S-13 manifest segments | Written atomically; a damaged-segment record refused without `--accept-unverified` | AC-16; NEG-57 |
| S-13 lost metadata changes | 17 listed what is not preserved and the excused changes; revision 10 removed the excused changes with resumed deletion | AC-21 |

## Reviews of revision 7 (adversarial and acceptance)

F-ids are the adversarial plan review's, T-ids the acceptance review's.

| Finding | Change in revision 8 | Criteria and negative cases |
|---|---|---|
| F1 the history walk trusts the commit-graph, which can name a parent the commit does not have | Every history command runs with `-c core.commitGraph=false`; the repository cannot re-enable it (core's config allowlist). Audit of the other repository-controlled inputs: grafts and alternates refused by core's layout check, replace refs and refs at blobs refused by its ref rule, `--no-replace-objects` as a second guard, bitmaps unused, shallow and promisor only shrink the walk, ids of parsed trees and tags recomputed, pack-index substitution disclosed (9.2, 7.5, 17); measured in the safe configuration (23) | AC-9, AC-12; NEG-47 |
| F2 a resumed deletion trusted metadata, so an equal-length overwrite with its modification time set back was deleted unarchived | Subset check before any resumed deletion: digests with `fstat` identity checks for an archived item; the strict F0 rule with change time for an unarchived regenerable item; a check before each `unlinkat` (10.1). Revision 9 replaced the field list with the complete preserved record and stopped unarchived regenerable items from resuming on their own | AC-21; NEG-19 |
| F3 apply emptied the spool directory that a running restore used; restore checked no state-volume space | One spool directory per operation (apply, restore and the manifest commands that read data) under a held lock, created and cleaned up only under a fixed guard lock, so cleanup removes only dead operations' spools (9.6, 10.2); restore's state-volume floor (13.3, 18); 13.2 never chmods files. Revision 9 added the spool lock that rclone children inherit | AC-23, AC-24, AC-33; NEG-48, NEG-49 |
| T-1 does a dry run perform the history check | Yes, with the same reasons and per-rule counts (9.2) | AC-1; NEG-50 |
| T-2 9.2 relies on core's private package evidence | C-5 exposes it, with a core equality test (15) | AC-12 |
| T-3 AC-9 could not tell history commands apart; the replace case did not test the walk | `-c disk-cleaner.phase=history` marker; AC-9 asserts the three overrides on marked calls; graft case in NEG-45 with a control walk; the replace cases run through the library because core's layout check refuses replace refs first | AC-9; NEG-45 |
| T-4 no SHA-256 fixture | NEG-43 case 6 | AC-12; NEG-43 |
| T-5 network tips undefined for unconfigured remotes and custom refspecs | Defined as core's `--remotes=NAME` refs of configured network remotes; anything else only enlarges the examined set (9.2) | AC-12 |
| T-6 when the history check runs | Inside step 2, between F0 and the end-of-step-2 comparison (9.2) | AC-8, AC-12 |
| T-7 and question 4: the over-keeping cost was understated | Stated plainly in 9.2, D-4, D-16 and 17: likely the common case for repositories worked on by hand, the run-wide override also uploads real secrets, the reason gives the remedy, a replayed corpus understates it (23); three options left to the user (D-16) | AC-12; NEG-46 |
| T-8 no case made the cost observable | NEG-46 | AC-12; NEG-46 |
| Minor remarks of the acceptance review | 17 names nameless blobs of 1 MiB or more and content-only checks of git-directory files; the time limit kills the git process group (9.2); git's own memory is unbounded (18) | AC-12 |

## Review of revision 6 (acceptance)

| Finding | Change in revision 7 | Criteria and negative cases |
|---|---|---|
| R6-1 the history rule misses unreachable objects (dropped stash, `git add` then `git rm --cached`) and names hidden by a duplicate or renamed blob | Examined set = every object in the store minus what the network tips reach; names from tree, index, tag and ref entries; an unnamed blob keeps the item; object and time limits; `--no-replace-objects`; disclosure in 17 (9.2, 7.5, 18, D-16). The reviewer's correction was evaluated as option (a). Revision 8 added `core.commitGraph=false` (F1) | AC-9, AC-12; NEG-41, NEG-43, NEG-45 |
| S-1 scope of the package exemption | Only inside a regenerable or cache directory, with core's evidence (9.2); revision 8 added C-5 | AC-12; NEG-44 |
| S-2 the content rule over-keeps inside regenerable children | A file with package evidence there is exempt from the content rule whatever its name; the name exemption stays limited to `fixture_prone` names; kept-by-rule counts measured on a corpus before release (9.2, 23) | AC-12; NEG-42 |
| S-3 the AC-37 phrase test can pass on wrapped text | Rendered text, whitespace collapsed, a pairing check and a wrapped control (AC-37) | AC-37 |
| S-5 NEG-27 describes a case 6.3 makes impossible | NEG-27 reworded; 6.2 states the 6.3 exception | AC-29; NEG-27 |
| S-6 `--missing=allow-any` skips a missing tree silently | `--missing=allow-promisor`; reproduced with git 2.54.0: a missing tree in a repository that is not a partial clone gives exit 0 with `allow-any` and exit 128 with `allow-promisor` | AC-12; NEG-41 |
| S-8 the spool limit had no observing step | The (b) flows record the spool directory's peak size and the free space (24) | AC-36 |
| S-11 the history timeout had no value | 600 seconds per item, injected in tests (9.2) | AC-12; NEG-41 |
| Owner, found while closing R6-1 | Replace refs, conflict images in `rr-cache`, a pack without its index, blobs that only a tag names (9.2) | AC-12; NEG-45 |

## Review of revision 5 (acceptance)

| Finding | Change in revision 6 | Criteria and negative cases |
|---|---|---|
| N1 secrets only in the object store (stash parents, local-only and reflog-only commits) uploaded by default | History rule in 9.2 with offline commands (7.5), the index included; tracked files with uncommitted changes checked by name and content; fail closed; limit restored in 17. Revision 7 replaced the history rule (R6-1) | AC-12; NEG-32, NEG-40, NEG-41 |
| I-1 plan text promises an apply-time remote check | C-4 lists the exact strings; run header correction until it lands | AC-37; NEG-39 |
| I-2 reconcile deletes under `--backup-only` | Reconcile by mode; second existence check against the journal's storage (6.1, 10.1) | AC-2, AC-21; NEG-34 |
| I-3 name rule over-keeps | Name and content rules skip committed tracked files (history rule covers them); `fixture_prone` package evidence honoured, also for content inside regenerable children; kept counts by rule (9.2) | AC-12; NEG-33, NEG-42 |
| I-4 `include.path` with `~` | Includes read and expanded by the tool (9.2) | AC-12; NEG-33 |
| I-5 token as URL user name | Credential definition in 9.2 | AC-12; NEG-33 |
| I-6 more sync clients | Nextcloud, ownCloud, MEGA, Resilio, Seafile default (7.4); limit (17) | AC-7 |
| I-7 bind-mounted file | Every entry checked for a mount (7.4) | AC-7; NEG-36 |
| I-8 Linux xattr namespaces; file system types | Namespace rules (9.3); removal only on an allowlist of local file systems (7.4) | AC-7, AC-13; NEG-36 |
| I-9 nested regenerable children not archived | Included: archived and removed with the git item, `removed-with` records (6.3, 9.1, D-15) | AC-29; NEG-35 |
| I-10 restore metadata; device check in 7.2 | Restored and compared metadata listed, the rest named as not restored (13.3); 7.2 compares with the parent directory | AC-27; NEG-37 |
| I-11 rclone temporary directory; wrappers | `--temp-dir` under the state directory; recursive wrappers, encrypted only when every path is `crypt` (9.6) | AC-24, AC-33; NEG-38 |
| I-12 "no network" wording | No git command touches the network; rclone contacts only the user's storage and may refresh its own token (top, section 3) | AC-9 |

## Review of revision 4 (adversarial)

| Finding | Change in revision 5 | Criteria and negative cases |
|---|---|---|
| F1 `insteadOf`/`pushInsteadOf` redirect explicit URLs | Network git removed; listed as an entry condition (section 8) | AC-9; NEG-9 |
| F2 git below 2.32 reads the global config; helpers visible in argv | Network git removed; entry condition (8) | AC-9 |
| F3 credential helpers `store`/`erase`; dry run writes | Network git removed; entry condition (8); dry run runs only 7.5 commands | AC-1, AC-9; NEG-1 |
| F4 resume table incomplete; "subset" undefined | Full O-by-Q table per state, `vanished` outcome, subset defined, stop point before `deleted` (10.1) | AC-21; NEG-19 |
| F5 no click-test plan | Section 24 | AC-36; NEG-31 |
| F6 live check gates `--backup-only` | No live check; entry condition (8) | AC-2 |
| F7 `rclone config show`, password prompt on stdin | `config redacted` only, `--ask-password=false`, encrypted config refused, version 1.64 (9.6) | AC-33; NEG-28 |
| F8 namespace probe for one provider only | Push removed; per-provider proof is an entry condition (8, M3) | n/a |
| F9 floor refuses a 97%-full disk; spooling unverified | Fixed 1 GiB floor, part size from free space with 1 GiB default, worked examples, tmpfs spool test (18, 9.4) | AC-24; NEG-21 |
| F10 "three in a row" against "three failing items"; CA file scope | One rule, three consecutive storage failures (9.5); CA file gone with the network | AC-24; NEG-21 |
| F11 ssh untested on macOS and Windows | ssh not used; entry condition (8) | n/a |
| F12 DESIGN.md sections 7 and 9 conflict | DESIGN.md sections 3 to 7 and 9 brought in line in the same change | n/a |
| F13 D-4/D-5 need the user; deferred scope | D-4 kept for the user; D-5 moot; TARGET_MISMATCH labels in section 2 and D-1 | n/a |
| PR-stage: durable attempt and segment numbers | `attempt-started`, `segment-reserved` (9.4, 10.1) | AC-34; NEG-29 |
| PR-stage: hook compiled out | Test-only feature, release job checks (20) | AC-35; NEG-30 |
| PR-stage: fresh `ls-remote` content check | Push removed; entry condition (8) | n/a |

## Review of revision 2 (acceptance)

| Finding | Status in revision 5 | Criteria and negative cases |
|---|---|---|
| M1 credentials and prompts to hosts named by scanned config | No network git (8) | AC-9; NEG-9 |
| M2 stash parents and secret-bearing commits pushed | Nothing pushed (8) | n/a |
| M3 refs that trigger CI, wrong remote, public remote, machine name | Nothing pushed (8) | n/a |
| M4 bind mounts and symlink races | Handle-relative walk and delete, mount roots and ids, no-replace rename (13.2, 7.4) | AC-7, AC-19; NEG-7, NEG-17 |
| M5 sync folders and placeholders | Such items are kept (7.4) | AC-7; NEG-7 |
| M6 "reproducible" files trusted on git's stat check; restore fidelity | Whole-item archive; restore compared with the original snapshot (9.1, 13.3, D-12) | AC-11, AC-27; NEG-8 |
| M7 same-disk vault as the only copy | No vault; secret-like files keep the item unless uploaded (9.2, D-4) | AC-12; NEG-12 |
| M8 globally ignored files | Untracked files matched by the global excludes file are secret-like, OS metadata names exempt (9.2) | AC-12; NEG-12 |
| M9 detection by name only, unencrypted upload | More names, bounded content check, encryption warning, `--require-encrypted-remote` (9.2) | AC-12; NEG-12 |
| M10 regenerable items cannot be re-scanned alone | Core item re-scan and project activity (C-3, 7.2) | AC-5, AC-20 |
| M11 production network path untested | No network path in this release | AC-9 |
| M12 rclone spools to disk | `rcat --size`; part size from the temporary volume's free space; spool test (9.4, 18) | AC-14, AC-24; NEG-21 |
| M13 xattrs, forks, ADS dropped silently | Archived in pax records; harmless ones dropped by list; otherwise unsupported (9.3) | AC-13, AC-27; NEG-13 |
| Improvements 1-2 nested items | 6.3 | AC-29; NEG-27 |
| Improvements 3, 5 stranded quarantine and records | 6.1, 10.1 | AC-16, AC-21; NEG-19 |
| Improvement 4 rename semantics | 6.2, 13.2 | AC-19; NEG-17 |
| Improvements 6-7 self-describing archives, fsmonitor socket | 9.3, section 4 | AC-11, AC-13 |
| Improvements 8-9 restore hardening, sparse files | 13.3, 9.5 | AC-24, AC-27; NEG-23, NEG-24 |
| Improvements 10-11 lock path, root, clocks | 10.2, 5, 7.1 | AC-3, AC-23; NEG-3, NEG-20 |
| Improvements 12, 16, 17 push degrade, `FETCH_HEAD`, fetch | No push or fetch (8) | n/a |
| Improvements 13, 18, 19 storage guards, deletion pre-checks, duplicates | 7.4, 7.1 | AC-3, AC-7; NEG-3, NEG-7 |
| Improvements 14-15, 20-23 vault, exit codes, bytes freed, prompt, outside links, coverage | No vault; 19; 18; 11; 17; section 2 | AC-21, AC-25, AC-31 |

