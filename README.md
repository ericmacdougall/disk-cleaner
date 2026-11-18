# disk-cleaner

A disk analyzer and cleaner for Windows, macOS and Linux that understands developer machines.

> **Status: early.** The design is written and the read-only core is being built. It does not
> delete anything yet. See [docs/DESIGN.md](docs/DESIGN.md).

Most disk tools show you a big folder and leave the judgement to you. disk-cleaner knows what the
folder is:

- a git repo whose every commit is on the remote, or one with unpushed work and a stash;
- one of forty linked worktrees an agent left behind;
- `node_modules`, `target`, `.venv` and other directories that can be rebuilt;
- package manager and tool caches;
- coding-agent transcripts and logs you want kept, but not on this disk;
- videos, models and other outputs that must survive and do not belong in git;
- source code sitting outside any repo.

For each it works out what is safe: remove it, back it up and then remove it, push a recovery
branch first, or ask you.

## Goals

- Outline, sunburst, treemap and biggest-files views, as in TreeSize and Disk Space Analyzer.
- Repo and worktree awareness, with recovery branches or bundles for anything ambiguous.
- Backup connectors for Dropbox and cold storage through [rclone](https://rclone.org).
- Automatic, verified backup of agent sessions and chat logs to your own storage.
- Age and size policies, with a manifest that makes everything removed easy to find and restore.
- A CLI with JSON output for servers and remote machines.

## Safety model in one paragraph

Scanning and planning are read-only. Nothing is removed unless the tool has shown that it is
regenerable or exists elsewhere, and has re-checked that at the moment of removal. A check that
fails or times out counts as "keep". Every removal is written to a manifest with a restore command.

## Building

Not yet. The first buildable milestone is the CLI.

## Licence

MIT. See [LICENSE](LICENSE).
