# Apply spec: earlier reviews

How revisions 5 to 13 of [APPLY-SPEC.md](APPLY-SPEC.md) answered the earlier reviews, moved here
from section 22 (revision 5's answers in revision 6, revision 6's in revision 7, revision 7's in
revision 8, revision 8's in revision 9, revision 9's in revision 10, revision 10's in revision
11, revision 11's in revision 12, revision 12's in revision 13, revision 13's in revision 14). Criterion and negative-case ids are those of revision 5 and later; section numbers are
those of APPLY-SPEC.md. Later changes to the same areas are listed in APPLY-SPEC.md section 22.

## Reviews of revision 12 (adversarial and acceptance)

F-ids are the adversarial plan review's and S-ids the acceptance review's, prefixed `r12`
(S-7 unused). Revision 14 changed two of the answers below: the writer's guarantee depends on
the kind of rule (row r12 F1), and the resume row for `recorded` reconciles the remote segment
before writing (row r12 S-3).

| Finding | Change in revision 13 | Criteria and negative cases |
|---|---|---|
| r12 F1 the writer repeated the content rule only for working-tree statuses, and the credential-URL rule alone for git directory files | One rule table (9.2) and one function used by step 2 and the writer; the writer applies every rule the table gives each kind of file to the bytes it sends, whatever its status; a test enumerates rows and kinds (9.2, 9.4) | AC-43, AC-46; NEG-70, NEG-80 |
| r12 F2 the first inventory came after the history commands | The inventory chain: I0 before the history commands, I1 before the git directory's first byte and equal to I0, each file's digest as sent equal to I0; what a mismatch does at each point; the cost stated (9.4, 17, 18, 23) | AC-43; NEG-72, NEG-85 |
| r12 F3 NEG-75 required outcomes the sequence cannot give | Split by first detection: step 2 (a readability probe, before any prompt) and upload (the attempt stops, objects removed, absence asserted only for the failing file and what follows it in archive order); every negative case added in revisions 11 and 12 reread (NEG-70, 72, 74 changed) | AC-44; NEG-75, NEG-81 |
| Coordinator: cost where the chain protects nothing | The history check and the chain are not run under `upload-all`, and I1 and the emitted comparison are skipped after `u`; the record says which checks ran (`checks_run`) (9.2, 12.1, 18, D-4) | AC-42, AC-47; NEG-69, NEG-83 |
| r12 S-1 NEG-74 could pass vacuously | Each damage fixture is shown to reach the prompt and fails the test, never skips; core's layout check judges names only (`gitshape.rs`), so a lone pack and a named loose object are accepted | AC-44; NEG-74 |
| r12 S-2 an unreadable entry surfaced after the prompt and the upload | The readability probe in step 2; an unreadable entry found only by a read is not retried (9.2, 9.4) | AC-44; NEG-75, NEG-81 |
| r12 S-3 where `recorded` sits | Journaled and synced before the record is written and the segment uploaded (6.2, 9.4, 10.1) | AC-14; NEG-14 |
| r12 S-4 three levels of nesting, "decision" | The digest is over the item's own entries; a decision is the latest `answered` line of this run; an earlier run's does not count (6.3, 9.2) | AC-45; NEG-82 |
| r12 S-5 a resume cannot tighten | `--resume RUN_ID --secrets keep` narrows (5, 10.1) | AC-21; NEG-79 |
| r12 S-6 late-arriving objects | The next non-dry run lists attempts removed in the last 7 days once and removes again (9.4) | AC-14; NEG-14 |
| r12 S-8 wording | Upload race: before or after the read; deletion race: write or replacement; the clock tick of F0; the name rule applies where 9.2's does (9.4, 17) | AC-43; NEG-86 |
| r12 S-9 hygiene | AC-44's `Verify by:` on one line; the rename-over case; AC-9's citations as cases of AC-12 | AC-9, AC-44; NEG-86 |
| r12 S-10 inventory proportion | Kept where it protects the decision, not run where it protects nothing (above) | AC-47; NEG-83 |
| r12 S-11 deletion-pass platform rules | `UF_COMPRESSED` and `UF_TRACKED` excluded on macOS with the reason; the setgid exemption on macOS stated as an unverified hypothesis with an injected control (13.2) | AC-19; NEG-61 |
| r12 S-12 names that attack the terminal | One printing function for every name, escaping control and bidirectional characters, citing core's `esc` and `diag` (5, 11) | AC-38; NEG-84 |
| r12 S-13 memory | Inventories and index records named in 18 | none |
| r12 S-14 nested costs | D-4 states the re-walk and shared budget, the double storage, the earlier-run rule and the mixed-handling mismatch | none |
| r12 S-15 `history_damage` labelling | Only damage the tool can name from structured output or its listing gets a label; a permission error is "could not run" (9.2) | AC-44; NEG-74 |
| Found while writing revision 13 (not a reviewer's finding) | Under `upload-all` an enclosing item with an undecided nested repository is kept, since its history is unexamined (6.3, NEG-76); I0 is under the history check's timer and count (9.2); a resume finishes a segment from the `recorded` line (10.1, NEG-14); the damage note is absent under `upload-all` (D-4, NEG-74) | AC-45; NEG-14, NEG-74, NEG-76 |
| r12 packet note | The packet's Source for AC-45 is an inferred source naming the acceptance report, not the user's D-4 answer (a rule that refuses uploads is not supported by words that ask for them) | none |

Rewritten in place: AC-9, 21, 38, 42, 43, 44, 45 and NEG-14, 61, 69, 70, 72, 74, 75, 76, 79. Added:
AC-46, 47 and NEG-80 to 86. None was withdrawn.

## Reviews of revision 11 (adversarial and acceptance)

F-ids are the adversarial plan review's and B- and S-ids the acceptance review's, prefixed `r11`.
Revision 13 changed four of the answers below: the object inventory became a chain (row r11 F1),
`upload-all` no longer runs the history check at all (row r11 clarification), the digest of a
nested part is taken over the item's own entries (row r11 B-1), and the writer's repeat covers
every row of one rule table (row r11 F1).

| Finding | Change in revision 12 | Criteria and negative cases |
|---|---|---|
| r11 F1 the archive streamed changed bytes before the final check could see them | What is checked is what is sent: an `fstat` equal to F0 before each chunk is emitted; the content verdict recomputed on the buffer that is emitted; a held-back window for git directory files; each object inventory compared before its git directory is sent; the residue disclosed as the upload race; a staged copy rejected (6.4, 9.4, 17, 18) | AC-43; NEG-70, NEG-71, NEG-72 |
| r11 F1, its second part: failed attempts were kept in storage | A failed or superseded attempt's objects are removed, journaled first, verified by a listing, named when removal fails; versioned providers disclosed (6.1, 9.4, 10.1, 12.3, 17, 19) | AC-14, AC-33; NEG-14, NEG-68 |
| r11 F2 teardown waited for children while holding the guard | The bounded wait comes first; the guard holds only file-system changes and non-blocking probes and is taken with a bounded retry (9.6) | AC-23; NEG-73 |
| r11 clarification and r11 S-11 secrecy and preservation were mixed | Two kinds that never overlap: not examined for secrets (a limit reached, a check that could not run, a corrupt or missing object, a pack without its index), which `u` and `upload-all` may upload, and not shown to be preserved (the archive itself incomplete or unverifiable), which nothing overrides; the placement follows that definition, not the clarification's example list (the coordinator's ASSUMPTION); the prompt states the reason, and the prompt and the record say git reported damage, `history_damage` (9.2, 11, 12.1, D-4) | AC-38, AC-42, AC-44; NEG-41, NEG-74, NEG-75 |
| r11 B-1 an enclosing item uploaded a nested item's secrets | The nested-secrets rule in every mode: the enclosing item is kept unless each nested part is empty or covered, by set digest, by that nested item's own decision; the prompt marks covered entries; core's veto bounds the nested cases (6.3, 9.1, 9.2, 11, 12.1) | AC-29, AC-45; NEG-27, NEG-76 |
| r11 S-8 inode reuse and device numbers | The directory identity (file-system identity, inode, birth time) for the moved-back match and the rename back; no match after `deleted`; "cannot confirm" (4, 5, 6.1, 7.4, 10.1, D-17) | AC-21; NEG-19, NEG-77 |
| r11 S-9 limit and could not run | Only the tool's own counter or timer makes a limit reached; a foreign signal or a crash is a check that could not run (9.2) | AC-42; NEG-69, NEG-74 |
| r11 S-10 `fstat` is undefined for symlinks | Symlinks are read by name between two `fstatat` calls that must agree (9.4, 13.2) | AC-19; NEG-78 |
| r11 S-12 labelling | Section 2 and D-4 name only the user's words as the user's decision; the setting, its scope and the D-16 bypass are ASSUMPTIONs (2, D-4) | none |
| r11 S-13 costs | D-4, D-16, D-17 and 18 add the nested-secrets keep, the false moved-back keep, the unexamined uploads under `upload-all`, the writer's retries and the inventory reads | none |
| r11 S-14 a resume could pick up an edited configuration | The header journals every effective setting with its source; a resume uses them (5, 10.1) | AC-21; NEG-79 |
| r11 S-15 the `mv -n` hint with O occupied | Printed only when O is free, as `mv -nT` on Linux (5) | AC-39; NEG-65 |
| r11 notes on two negative cases | NEG-66 places the archived link between unlinks; NEG-19's new folder at O is defined by the identity | AC-19, AC-21; NEG-19, NEG-66 |

Rewritten in revision 12: AC-14, 19, 21, 23, 29, 33, 38, 40, 42 and NEG-14, 19, 27, 41, 65, 66,
67, 68, 69. Added: AC-43, 44, 45 and NEG-70 to 79. None was withdrawn.

## Reviews of revision 10 (adversarial and acceptance)

F-ids are the adversarial plan review's and S-ids the acceptance review's, prefixed `r10` to
avoid clashes with core changes and earlier findings; the last two rows answer the user's answer
on D-4. Revision 12 changed three of the answers below: `upload-all` now also uploads an item
whose history check could not run (last row; 9.2), the moved-back match uses the directory
identity of section 4 (row r10 S-2), and the teardown wait moved out of the guard (row r10 F3).

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
| The same answer, for automatic runs ("in my case back it up to my private storage anyway") | `--secrets upload-all`: a separate, never-default value that uploads the whole secrets set of automatic items, unnamed objects and history past its limits included; a check that could not run still kept; a one-time notice; the record names the overridden rules (9.2, 12.1, D-4) | AC-42; NEG-69 |

Rewritten in revision 11: AC-1, 12, 19, 21, 23, 30, 38, 39 and NEG-12, 19, 32, 43, 47, 51, 53,
58, 60, 61, 62. Added: AC-40, 41, 42 and NEG-63 to 69. None was withdrawn.

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

