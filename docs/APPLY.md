# apply (lane 1: regenerable directories)

`disk-cleaner apply` removes the `remove-regenerable` items of a plan you have reviewed, under a
journal. It is for a person at their own terminal. Nothing is uploaded and no git repository is
removed in this lane.

```
disk-cleaner apply --plan p.json                       # prints the plan id and a dry run
disk-cleaner apply --plan p.json --plan-id ID --only regenerable [--max-items N]
disk-cleaner apply --resume --plan-id ID --plan p.json # or: --resume --journal ID
```

## Run order

1. Courtesy check: standard input must be a terminal and none of `CLAUDECODE`,
   `CLAUDE_CODE_ENTRYPOINT`, `CODEX_SANDBOX`, `AGENT_HARNESS_SESSION` may be set, unless `--sandbox
   DIR` is valid (under a temporary root, outside the home, holding `.prunedisk-sandbox`, containing
   every candidate and no protected root). This is a courtesy, not a control.
2. Plan id: the first 16 hex characters of the SHA-256 of the plan file. Missing or wrong: the id,
   a dry run and any pending journal work are printed and nothing changes (exit 0).
3. Host lock `~/.prunedisk/apply.lock`; a second apply exits 3.
4. `--resume` reconciles this plan id's journal and exits; it needs no fresh plan and no guard
   state. Without it, unfinished journal items refuse new work.
5. New work: the guard state must be verified, the plan of this schema and at most
   `--max-plan-age-days` (3) old; a fresh scan and plan are made from the plan's recorded roots and
   options, and only items both plans agree on (path, class, rule, action) are acted on.

This build ships with the guard state unverified and the in-use rules not built in, so new work
refuses; `--resume` works.

## Per item

Pre-checks (a directory, not a link; no mount point at or below; not reached by a process), then
`quarantining`, a no-replace rename to `<parent>/.prunedisk-q-<plan id>-<index>`, `quarantined`
(identity and an inventory digest), checks on the copy by the scan's own classifier, a last check
(digest unchanged, nothing new reaches it), `removing`, the bottom-up deletion relative to verified
directory fds, `removed`. Any doubt restores the item in place; if the original path is taken it
stays in quarantine and is reported. Plans never target a `.prunedisk-q-*` directory.

## Journal

`~/.prunedisk/journal/<plan id>.jsonl`: append-only, fsynced per record, holding paths, sizes, rule
ids, digests and (dev, ino) pairs, never file contents. Every mutation is preceded by its fsynced
intent record: the code enforces it with a proof type only the journal can make.

## Wrappers are not allowed

Do not wrap `apply` in a Makefile target, script or alias: the agent-deny hook cannot see through
wrappers. A `.prunedisk-q-*` directory that no journal names is never touched; it is listed.
