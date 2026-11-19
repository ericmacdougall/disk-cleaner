# Apply spec: earlier reviews

How revisions 5 and 6 of [APPLY-SPEC.md](APPLY-SPEC.md) answered the earlier reviews, moved here
from section 22 (revision 5's answers in revision 6, revision 6's in revision 7). Criterion and
negative-case ids are those of revision 5 and later; section numbers are those of APPLY-SPEC.md.
Later changes to the same areas are listed in APPLY-SPEC.md section 22.

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

