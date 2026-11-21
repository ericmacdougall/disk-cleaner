# prunedisk-xfer

Content-defined, deduplicated, verified backup of a folder tree to object storage (B2, S3,
DigitalOcean Spaces, or anything else rclone reaches), and exact restore. Design and
acceptance criteria: GoodQA #16550.

## Memory budget and the tree sizes it supports

`--mem-budget` (default `1640Mi`; the Mac profile is
`--mem-budget 640Mi --max-pack 16Mi --max-streams 4`) is split into named shares, which
`prunedisk-xfer backup` prints on stderr at start. Loading and backing up charge each share
as memory is taken and stop with a clear error before going past it, never after.

The **index share** is `min(256 MiB, budget / 8)`. A backup splits it in two:

- **the hot table**: this run's new chunks until a commit stores them (a chunk id and whether
  its pack is verified). It is allocated once at start, a quarter of the share at most and
  never more than 17 MiB (2^19 buckets of 34 bytes), and it never grows. It holds 229,376 new
  chunks at a time on every profile.
- **the committed chunk map**: the rest of the share. One entry per distinct chunk in the
  repository, kept in a vector sorted by chunk id with a small prefix table: 40 bytes an entry,
  no hash-table slack and no rehash. A backup reserves room for the whole share at start (the
  pages become memory only as entries are written), and each commit merges its chunks in
  place.

A restore has no hot table: its whole share holds the committed map.

A chunk is about 1 MiB of a large file on average (FastCDC, 256 KiB to 4 MiB); a file under
1 MiB is one chunk. So the number of chunks is roughly the data size in MiB, plus one per small
file.

Every pack the repository holds also costs the share twice, about 550 bytes in the chunk map
and about 550 in the pack listing held beside it. (The figure is measured: on macOS the many
small strings of a pack listing fragment the allocator, and a pack cost about 930 bytes in all
where 804 had been charged.) With large files a pack holds about one chunk per MiB of
`--max-pack` (16 on the Mac profile, 32 by default), so a chunk costs about 110 bytes on the
Mac profile and 75 by default. With small files a pack holds thousands of chunks, and a chunk
costs about 40 bytes.

| Profile | `--mem-budget` | Index share | Backup: large files | Backup: small files | Restore: large / small | New chunks at a time |
|---|---|---|---|---|---|---|
| Mac | 640Mi | 80 MiB | 604,855 chunks (about 590 GiB) | 1,648,229 files | 767,192 / 2,090,598 | 229,376 |
| Default | 1640Mi | 205 MiB | 2,642,552 (about 2.5 TiB) | 4,921,753 | 2,881,824 / 5,367,398 | 229,376 |
| Largest share | 2048Mi or more | 256 MiB | 3,360,369 (about 3.2 TiB) | 6,258,687 | 3,599,641 / 6,704,332 | 229,376 |

These figures are the arithmetic of `map_fit`, `backup_map_fit`, `chunk_map_fit` and
`hot_room` in `src/repo.rs`, and the unit test `supported_sizes_per_profile` pins them. The
measured cost per chunk is in the next section. MEASUREMENT PENDING: the table is confirmed
only once that section carries the measured numbers on Linux and macOS.

When a limit is reached:

- **The hot table is full.** Readers wait for the next commit to move chunks into the committed
  map, then go on. This is a pass boundary, not an error: the run completes. Pending packs are
  sealed and committed at once while readers wait.
- **The committed map is full during a backup.** The run stops reading new data. It reports
  one error naming `--mem-budget` (`exit 3`) and lists the files it did not read. Files
  committed before that keep their receipts. Back the tree up in parts (`--files-from` or
  filters), or with a larger `--mem-budget`.
- **Restore, or the load phase of a backup.** The run refuses before writing anything, with
  `exit 3`. The error estimates how much the chunk map needs and names the `--mem-budget` that
  holds it, or says that no budget does.

When the tree comes within 2x of these limits, `backup` says them on stderr at start.

There is no on-disk or partitioned index yet, so a repository beyond the last row is refused.
It is not silently backed up past the budget.

## Measured memory

`tests/mem_plan.rs` (Linux, release build, real rclone) checks the engine and rclone's rcd
process together against the Mac profile's 640 MiB. `tests/hostile_restore.rs`
(`a_chunk_map_filled_to_its_share_stays_inside_it`) fills the chunk map to its share and checks
that the process grows by at most the share, plus one fetch batch of compressed index objects
(16 MiB). It prints the measured bytes per entry.
