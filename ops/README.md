# prunedisk-ops

The apply-side companion to `disk-cleaner scan/plan`. It understands a home folder, saves everything that exists
only on this disk, and only then removes idle work copies. **Every step is a dry run unless `--apply` is given.**

```
python3 ops/prunedisk_ops.py --config host.json inventory            # read-only: units, git facts, live/service refs, logs, media
python3 ops/prunedisk_ops.py --config host.json plan                 # keep / secure-then-wipe, with reasons (plan.txt)
python3 ops/prunedisk_ops.py --config host.json recover [--apply]    # local-only git work -> recovery/<host>-<date>-<desc> branches
python3 ops/prunedisk_ops.py --config host.json backup  [--apply]    # verified archives: logs, media, residue of each wipe unit
python3 ops/prunedisk_ops.py --config host.json backup-dir DIR [--apply]
python3 ops/prunedisk_ops.py --config host.json manifests [--apply]  # backup-manifests branch in each private repo + full index
python3 ops/prunedisk_ops.py --config host.json wipe                 # dry run: wipe-dry.tsv and the --approve hash
python3 ops/prunedisk_ops.py --config host.json wipe --apply --approve <sha256 of plan.json>
```

State lives in `~/.prunedisk-ops/<date>/` (inventory.json, inventory-summary.md, plan.json, ledger.jsonl,
deletions.log, indexes/, vault/ for secret-named files, which are never uploaded).

## What it guarantees
- **Units** are children of the configured containers plus non-dot top-level folders that are not tool, SDK or
  synced folders. Dot folders are never units. A `.keep` file, the keep list and `protect_names` keep a unit.
- **Kept**: anything touched within `active_hours` (mtime *and* ctime), anything a process uses (cwd, root, exe,
  open files, memory maps), anything a systemd unit (system or user), cron entry, docker/podman container, launchd
  plist, nginx/caddy/apache/supervisor/pm2 config or a home symlink references (`~`, `$HOME`, `%h` expanded,
  realpath compared), anything with a mount inside, and anything another kept repo needs (worktree object stores,
  alternates; computed until stable). If any liveness probe fails, the step aborts and changes nothing.
- **Recovery** pushes only to repos `gh` reports as PRIVATE, only new `recovery/...` branches (never force, never an
  existing branch, no history rewrite), at most `max_branches_per_repo`, at most `max_push_mb`, and only after a
  secret scan of every new object (paths and content; blobs over 8 MiB, archives and office files are refused and go
  to the residue). Work with no origin goes to `salvage_repo`.
- **Backups** are written locally, `zstd -t` tested, uploaded with `rclone copyto` (never overwriting), and accepted
  only when the storage SHA-1 and size match. Each archive has a full file index next to it (`<key>.index.jsonl`).
  A unit's residue holds everything not provably recoverable: unsecured repos whole, `.git` metadata always,
  untracked/modified/ignored files, minus rebuildable folders that git does not track.
- **Wipe** needs the `--approve` hash of the reviewed plan, a residue younger than `max_backup_age_hours`, no change
  since the residue started, valid targets (canonical, direct child of home or a container), a fresh liveness
  check, and a per-unit re-check after renaming into `~/.prunedisk-ops/quarantine/<date>/`. Deletion never crosses
  filesystems. Every deletion is logged to `deletions.log` and the ledger.

## Config (JSON)
`host`, `home`, `remote` (rclone destination), `containers`, `keep`, `keep_globs`, `protect_names`, `active_hours`,
`salvage_repo`, `index_repo`, `max_residue_gb`, `max_push_mb`, `max_branches_per_repo`, `max_backup_age_hours`,
`credential_helper`, `visibility_default` (PUBLIC unless set), `tool_dirs`, `regenerable`, `log_dirs`. See
`example.json`; keep real host configs (bucket names, protected names) out of this public repo. Never put keys in the config; rclone and gh use their own stored credentials.

## Tests
`cd ops && python3 -m unittest tests.test_prunedisk_ops`
