# PruneDisk desktop app and terminal view (read-only)

Both show where the space went and what the dry-run plan proposes. Neither removes, moves,
renames or backs up anything. The terminal view writes only a plan export (`e`, always a new
file). The desktop app writes the files listed under "Files the desktop app writes" below.

## Terminal view

```
disk-cleaner tui [PATHS]...          scan and browse
disk-cleaner tui --scan scan.json    browse a scan file from `disk-cleaner scan --out`
  --one-file-system                  do not cross mount points (Unix)
  --no-git                           skip git (repository facts are then unknown)
```

Keys: Up/Down (or `k`/`j`), PageUp/PageDown, Home/End (or `g`/`G`) move; in the size tree
Enter, Right or `l` opens a folder, Left, Backspace or `h` goes up, Space expands or
collapses, `s` and `r` change the sort; Tab (or `1`, `2`) switches between the size tree and
the developer view; `e` exports the plan JSON to a new file in the current folder (never
overwriting one); `i` shows every notice of the developer view in a scrolling box (Up/Down,
PageUp/PageDown, Home/End; Esc, `i` or `q` closes it); `?` shows help; `q` or Ctrl+C quits.

The developer view's header lists, in this order: the source, the scan's time and that the
plan is a dry run; the reclaimable total; then the safety notices (an old imported scan, that
an imported scan's process check describes this computer, the process check and its note,
incomplete coverage, entries the size walk could not read, unverified remote facts); then the
breakdown by action and the active window. It grows to fit and keeps at least the item list
and the reason box; when a small terminal cuts it, its title says how many lines are hidden
and that `i` shows them all.

Size tree columns: Allocated is the disk space used (on Windows the column is Length, the
file length, as in core); Size is the apparent size, file lengths with each folder's own
directory entry included, as `du --apparent-size` counts; % parent is the entry's share of its
parent folder's allocated space, so it orders like Allocated and is not a sort key.

Disclosed limit: quitting while the analysis runs does not stop the git commands it already
started. They are local and read-only and normally end within a second (the click-test saw
three outlive the view by about 0.13 s), but core's 30 s git timeout is enforced by the view
itself, so after it exits nothing ends a git that hangs, for example on a stuck network file
system. The desktop app runs its analysis in a contained worker and has no such gap (below).

## Desktop app

Builds come from CI only (`.github/workflows/desktop.yml`, job "bundle"): unsigned macOS
arm64 and x64 (`.app`, `.dmg`), Windows x64 (`.msi`, NSIS `.exe`) and Linux x64
(AppImage, `.deb`), each with a `SHA256SUMS` file, as run artifacts named
`prunedisk-<platform>-unsigned` (the macOS `.app` comes as `PruneDisk-<platform>.app.tar.gz`):

```
gh run download <run id> --repo ericmacdougall/disk-cleaner -n prunedisk-linux-x64-unsigned
```

Unsigned bundles: on macOS a downloaded app is blocked by Gatekeeper; clear the quarantine
flag (`xattr -dr com.apple.quarantine PruneDisk.app`) and, on Apple silicon, sign it ad hoc
(`codesign --force --deep --sign - PruneDisk.app`) before opening. Windows SmartScreen asks
before running an unsigned installer.

To run from source (needs Rust, Node 22 and the Tauri system libraries):

```
cd apps/desktop
npm ci
npx tauri icon app-icon.svg
npx tauri dev
```

### Files the desktop app writes

Its own files are in core's config folder, which follows `HOME` (macOS:
`~/Library/Application Support/disk-cleaner/`), `APPDATA` (Windows: `%APPDATA%\disk-cleaner\`)
or `XDG_CONFIG_HOME` (Linux: `$XDG_CONFIG_HOME/disk-cleaner/`, else `~/.config/disk-cleaner/`):

- `prunedisk-app.json`, the app's scan settings;
- on an explicit Save of the plan settings: `config.toml` (only `active_window_days` and
  `max_git_admin_bytes` change), `config.toml.bak` (the text Settings loaded),
  `config.toml.lock` (the save lock; it stays, empty, by design) and, during the save, a temp
  file that is renamed over `config.toml` or removed;
- a plan export, always a new file, in the folder chosen for it.

Webview data. The window is the system webview, which keeps data in folders named after the
bundle id `io.github.ericmacdougall.prunedisk`. These do not follow `HOME`: WebKit uses the
account's home folder. The app runs the webview in incognito mode (`"incognito": true` in
`apps/desktop/src-tauri/tauri.conf.json`); the app keeps no web storage, so nothing is lost by it.

- macOS: incognito is WebKit's non-persistent data store (wry 0.57 uses
  `WKWebsiteDataStore.nonPersistentDataStore`), so cookies, local storage, IndexedDB and the
  resource-load statistics database are kept in memory. Without it, an idle launch wrote
  `~/Library/WebKit/<bundle id>/WebsiteData/` (520 KiB in CI and 584 KiB in the click-test,
  with a `ResourceLoadStatistics` database still being written while idle) and
  `~/Library/Caches/<bundle id>/WebKit/` (8 KiB). CI (job "bundle (macos-arm64)") launches
  the app idle for 20 s, quits it, lists anything under `~/Library/WebKit/<bundle id>`,
  `~/Library/Caches/<bundle id>`, `~/Library/HTTPStorages/<bundle id>` (and its
  `.binarycookies` file) and `~/Library/Saved Application State/<bundle id>.savedState`, and
  fails if any of them exists. With incognito, CI saw an idle launch create neither of the
  first two (run 44655472337); the check of all five runs on every push. It shows that the
  app stayed running, not that the page loaded. Folders that an older build created are not
  removed; delete them by hand if wanted.
- Windows (not observed here): WebView2 keeps a user data folder,
  `%LOCALAPPDATA%\io.github.ericmacdougall.prunedisk\EBWebView\`; incognito runs it InPrivate,
  which keeps browsing data out of it, but the runtime still creates the folder.
- Linux (not observed here): incognito gives webkit2gtk an ephemeral data manager, so no
  browsing data should be written under `~/.local/share/io.github.ericmacdougall.prunedisk/`
  or `~/.cache/io.github.ericmacdougall.prunedisk/`.

What Tauri 2 offers here: `incognito` (every desktop platform; a non-persistent store),
`dataDirectory` (Windows and Linux only; it moves the data, it does not turn it off) and
`dataStoreIdentifier` (macOS 14 and later only; a separate persistent store). None turns off
the resource-load statistics alone; incognito is the only way to stop them being stored.

Disclosed limits:

- Cancel ends the size walk within about 2 s unless a metadata call itself blocks (a network
  share that does not answer).
- The analysis runs in a worker process. Cancel and quitting end it and every git command it
  started (a session sweep on macOS and Linux, a job object on Windows). If the app dies
  outright, the worker notices its input closing; on macOS and Linux it first stops itself
  from starting any further process (its soft process limit becomes 0; macOS may also lower
  the hard limit to its per-user maximum, which does not matter as the worker exits next),
  then sweeps, then exits, and on Windows the job object ends it and everything it started.
  Its output then has no reader, so nothing on the way out depends on writing it; and ten
  seconds after the worker began ending, a separate thread sweeps once more and exits it
  whatever else is happening. When the worker dies on its own (a crash, the out-of-memory
  killer), the app sweeps what it started before reaping it. Residual: the process
  limit does not bind an app run as root (or, on Linux, with CAP_SYS_RESOURCE or
  CAP_SYS_ADMIN); there, one more git can start between the sweep and the worker's exit and
  runs to its end. If the app dies after it has killed the worker but before its own sweep
  finished, a git command still running finishes on its own. Core runs only local, read-only
  git commands.
- Saving settings: another program that writes core's config file at the moment of saving
  can lose that write; `config.toml.bak` holds the text Settings loaded.
- An imported scan shows no agent session folders (they come from the size walk).

## Tests

All in CI; nothing is built on a developer machine by default.

| What | Where |
|---|---|
| view and tui crates, three OSes | job "view and tui" (`cargo test -p disk-cleaner-view -p disk-cleaner-tui`) |
| read-only guard and its planted-violation controls | job "read-only guard" (`scripts/check-readonly.sh`, `--controls`) |
| frontend unit tests (vitest, jsdom, mocked backend) | job "frontend unit tests" (`npm test` in `apps/desktop`) |
| desktop backend, worker and containment (Cancel; the parent's input closing; the parent dying with every pipe; a real parent process killed; a stuck ending cut short by the deadline; a worker that dies on its own) | jobs "desktop backend (linux)", "worker containment" (`cargo build --examples`, then `cargo test` in `apps/desktop/src-tauri`, then `scripts/repeat-containment.sh 10`) |
| one million nodes | job "performance at one million nodes" (results in the job summary and the `perf-results` artifact) |
| licences, advisories, bans and sources of the desktop lockfile | job "cargo deny (desktop workspace)" (`apps/desktop/src-tauri/deny.toml`, `--locked`; crates only from crates.io, no git sources; duplicate versions warn; the cargo-deny version is printed first; each advisory ignore gives its reason). ci.yml's "cargo deny licenses" covers the root lockfile, licences only |
