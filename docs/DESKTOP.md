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

### What the window may ask for

- Scan roots: Start scans only folders offered to this window, exactly as they were offered:
  a drive from the drive list or a folder picked with Add folders. Any other path, a subfolder
  of an offered folder included, is refused with "... was not offered by this window; add it
  with Add folders or pick a drive", and no scan starts. At most 64 roots per scan. (This
  narrows spec AC-12, which only required absolute folders; PR gate F1. It is an assumption
  awaiting the owner's confirmation.)
- Scan files: one at a time. While a file is read, Open a scan file and Scan are disabled, and
  the backend refuses a second file ("a scan file is being opened; wait for it to finish") and
  a scan; a scan file cannot be opened while a scan runs. Focus returns to Open a scan file
  when nothing was opened.
- Settings: a save that wrote the plan settings but not the app's scan settings says "Plan
  settings saved." with the reason the rest was not saved. A save that changes the plan
  settings recomputes the shown plan. Opening Settings after another window or program changed
  core's config file adopts the file's values and recomputes the plan ("The config file
  changed; the plan was recomputed."); a plan export then uses the adopted values.
- Large results: a new analysis is installed without holding the session lock while its tags
  are prepared, so pages and Cancel are answered during the install (CI, 20,000 sibling
  folders: Linux install 397 ms with the longest page wait 0.3 ms; Windows 726 ms and 0.6 ms;
  macOS 2,265 ms and 80 ms against limits of 5 s and 250 ms).
- The size tree is one treegrid: the column headers are row 1 and the data rows follow
  (`aria-rowcount` is the row count plus one).

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
  or `~/.cache/io.github.ericmacdougall.prunedisk/`. With Mesa graphics drivers, the webview's
  GPU process writes the driver's shader cache to `~/.cache/mesa_shader_cache/`, as any
  program drawing with Mesa does (seen in the end-to-end job, below).

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
| size tree scroll at one million rows, mocked backend, headless Chromium (spec AC-9) | job "size tree scroll at one million rows" (`npm run perf:tree` in `apps/desktop`; results in the job summary and the `tree-scroll-1m` artifact) |
| a wide install: 20,000 sibling folders with pages and Cancel asked meanwhile | `tests/state_flow.rs` in the desktop backend and containment jobs (the `PERF install_wide` and `PERF cancel_wide` lines) |
| the packaged app through WebDriver, Linux and Windows (below) | job "end-to-end" (`apps/desktop/e2e/check.sh`; results in the `e2e-<os>` artifact) |
| each end-to-end check caught by a broken variant | workflow "e2e-variants" (`apps/desktop/e2e/variants/*.patch`, `e2e/expect_variant.py`) |
| licences, advisories, bans and sources of the desktop lockfile | job "cargo deny (desktop workspace)" (`apps/desktop/src-tauri/deny.toml`, `--locked`; crates only from crates.io, no git sources; duplicate versions warn; the cargo-deny version is printed first; each advisory ignore gives its reason). ci.yml's "cargo deny licenses" covers the root lockfile, licences only |

### End-to-end checks (PR gate F1)

`apps/desktop/e2e/check.sh` drives the packaged app through tauri-driver 2.1.0:
WebKitWebDriver on Linux (ubuntu-22.04, under Xvfb) and the Edge driver on Windows
(windows-latest). The Edge driver's major version is matched to the newest WebView2 runtime
folder installed on the runner. It builds two binaries:

- the release binary, with the production config and without the `e2e` feature. Its string
  table must hold no `PRUNEDISK_E2E_` name. It is opened once through WebDriver (`e2e/probe.mjs`)
  to record whether the shipped config can be driven at all;
- the e2e binary, built with `--features e2e`. In it, each native dialog answers from an
  environment variable (`src-tauri/src/e2e.rs`), because WebDriver cannot click a native
  dialog. The command then carries on exactly as after a real pick.

The e2e binary also gets an automation config, which differs from the shipped one in one
window setting per OS, plus its window title, "PruneDisk (end-to-end build)" (a marker for the
check below). These are assumptions awaiting the owner's confirmation:

- Linux: incognito off (`e2e/automation.conf.json`). wry 0.57.3 gives an incognito webview an
  ephemeral webkit2gtk context, and that context is created with automation disabled
  (`WebContextImpl::new_ephemeral`). WebKitWebDriver therefore cannot open a browsing context
  in the shipped app: "The remote session failed to create a new browsing context". CI shows
  this on every run, from the release-binary probe. So the Linux WebDriver checks run on a
  webview that keeps its data in `$XDG_DATA_HOME/<bundle id>/`. The no-write proof declares
  that folder for this build only.
- Windows: browser arguments that open a DevTools port (`e2e/windows-automation.conf.json`,
  `additionalBrowserArgs`). The Edge driver hands the webview its switches, among them
  `--remote-debugging-port=0`, in `WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS`. wry always sets
  the webview's browser arguments itself, and on the runner the variable's switches never
  reached the browser process. The driver then waits 60 s and fails with "DevToolsActivePort
  file doesn't exist". The config passes wry's own defaults plus the driver's switches.
  Incognito stays on, as shipped. CI confirmed it: with this config the suite passed on
  windows-latest, and without it no session opened.

The shipped bundles are not built from either automation config:

- Which config each job uses: the bundle job runs `tauri build --target <t> --bundles <b>`
  with `src-tauri/tauri.conf.json` alone (no `--config`, no `--features`). Only the e2e job
  and e2e-variants pass `--features e2e --config e2e/<...>.conf.json`.
- The shipped artifacts are checked on every bundle job (macOS arm64 and x64, Windows,
  Linux). `e2e/check-shipped.sh` unpacks the program from each bundle (`.app`; `.deb` with
  `dpkg-deb -x`; `.AppImage` with `--appimage-extract`; `.msi` with an administrative
  install, which only unpacks; the NSIS installer with 7-Zip). It fails if any program holds
  `PRUNEDISK_E2E_`, `remote-debugging-port` or "PruneDisk (end-to-end build)", or if a bundle
  holds no program.
- The check is not vacuous. `check.sh` runs it on the e2e binary and requires it to fail and
  name those strings: the title and the hooks on both OSes, and the DevTools switch on
  Windows. The release binary built in the e2e job must pass it.

One session (`e2e/run.mjs`) covers these flows, in order:

1. Choose folders (a seeded fixture tree from `e2e/fixture.py`, seed 16452, and a large
   system folder).
2. Scan, Cancel mid-walk, then rescan the fixture.
3. Open each view, then check the tablist's arrow keys.
4. Open the wide folder in the size tree (5,000 rows).
5. Page the developer view (Next and Previous), keeping focus on the pager, then switch class.
6. In Settings: save a longer window, then try a shorter one three times, answering with
   Escape, Keep and Shorten. Each answer checks focus and the saved value.
7. Export the plan, then try a second export, which is cancelled and writes nothing.
8. Open a valid scan file, a malformed one (focus returns to Open) and a cancelled one.
9. Check that the CSP blocks inline script.

A second session then opens on a one-million-row size tree for the AC-9 timings (below).

axe-core 4.14 runs on the chooser, each view and the imported developer view; any violation
fails.

Refused IPC, each with the job state checked unchanged:

- commands outside the capability list (`plugin:dialog|open`, `plugin:path|resolve_directory`,
  `plugin:window|close`) and an unregistered command;
- a root the window did not offer, and a subfolder of an offered root;
- 65 roots, and a 1 MiB root path.

No-write proof:

- The app runs with its home, config, data, cache, state and temp folders in a sandbox, or
  `APPDATA` on Windows.
- `e2e/manifest.py` records kind, size, mode, mtime and SHA-256 of every entry under the
  fixture and the sandbox before and after the session. The fixture includes git
  repositories, one with a stat-dirty index, and git facts are on. Any change outside a
  declared path fails.
- The declared paths are core's config folder, the export the test asked for, the webview's
  data folders and, on Linux, the graphics driver's shader cache (below).
- On Windows the real `%LOCALAPPDATA%\<bundle id>` and `%APPDATA%\disk-cleaner` folders are
  watched too.
- On Linux the app and its child processes also run under strace (writes only, via
  seccomp-bpf), and `e2e/trace_report.py` lists every path written anywhere, including outside
  the sandbox. That list is a report, not a gate.

Graphics driver cache (Linux): the webview's GPU process writes Mesa's shader cache to
`$XDG_CACHE_HOME/mesa_shader_cache/` (else `~/.cache/mesa_shader_cache/`). Any program drawing
with Mesa does the same, the shipped app included. The app does not ask for it, and it is not
the app's data. Declaring it in the no-write proof is an assumption awaiting the owner's
confirmation.

Timings, with thresholds registered before the first measurement (50 ms p95 for both;
nearest-rank percentile, 10 warm-up calls, then 200 samples; release build with the e2e
feature):

- IPC: in-page `performance.now()` around `tree_page` calls over the wide folder.
- Scroll: from setting `scrollTop` 40 rows further to the first animation frame that shows the
  new top row.

A p95 over the threshold fails the check; the threshold is not moved to fit a result. Measured
in the probe runs before the PR head (each run's numbers are in its `e2e.json`, under
`measurements`):

| | IPC p95 | Scroll p95 | Slowest scroll sample |
|---|---|---|---|
| Linux (under strace, Xvfb and software rendering) | 2 to 6 ms | 36 to 44 ms | up to 50 ms |
| Windows (windows-latest) | 2.4 ms | 35.5 ms | 36.2 ms |

The scroll check has little headroom, most of all on Linux, where strace and software
rendering add to every frame. A slow runner can turn it red. The response is to read that
run's samples (the `e2e-<os>` artifact) for a real regression against a slow runner. The
limit stays at 50 ms.

#### Timings at one million rows (spec AC-9, PR gate F9)

Built to a quoted excerpt of spec v6 AC-9; the spec text was not re-read. Registered before any
sample (WAL w8d2cd55d39e). The steps, boundary, samples, thresholds and verdict are in one
file, `e2e/ac9.mjs`, shared by checks (2) and (3).

Workload: `synth::wide_tree(1_000_000, 16452)` (crates/view), one root folder holding
1,000,000 files, root expanded, so the size tree has 1,000,001 rows. The e2e build loads it at
start when `PRUNEDISK_E2E_SYNTH_NODES=1000000` is set (`src-tauri/src/e2e.rs`,
`open_synthetic`). It goes through the scan path: a job starts as Start starts one,
`show_walk` installs the tree, and a one-folder `synth::synthetic_scan` finishes the job with
`show_scan`, so the window opens on the results (Done). No command is added, and the
release-binary string check covers the name. The workload is
`wide_tree(1_000_000, 16452)`. The pre-registration's `synthetic_scan` wording was the owner's
error, corrected before any result was judged.

Every percentile is nearest rank; pass means p95 strictly under the threshold. A miss is
reported as measured, with no retry to pass.

What counts as AC-9. The user's AC-9 target, as quoted, is check (3); the owner's and main's
ruling (no user source for it was shown) is that AC-9 is met by checks (1) and (3) and the DOM
cap. Check (2) is an additional measurement: its 50 ms threshold is the owner's own
pre-registration, inferred, not the user's words. Its results are neither an AC-9 miss nor an
AC-9 pass. It is still a pass gate in `e2e/run.mjs`, as registered, until main rules on its
form.

1. IPC (Linux; `timing.synth_1m.ipc_tree_page_p95` in `e2e/run.mjs`). A second app session at
   the end of the WebDriver suite, with its own tauri-driver (ports 4446 and 4447), because the
   app takes its environment from the driver. In the page, `__TAURI_INTERNALS__.invoke`
   `tree_page` with offset k * 200 and limit 200: 10 warm-up pages, then k = 0..99 timed with
   `performance.now()`. Each page must hold 200 rows at its offset. Pass: p95 < 50 ms over
   100 samples.
2. Running-app scroll (Linux and Windows; `timing.synth_1m.scroll_p95`, same session). Size
   tree, 10 warm-up steps, then 200 steps of 10 rows (280 px). The boundary:
   - the clock starts just before `.tg-scroll`'s `scrollTop` is set;
   - it stops after the first animation frame in which the step's target row
     (`#tg-row-<index>`) is in the DOM, plus one post-paint task (`requestAnimationFrame`, then
     a `MessageChannel` message);
   - the target is the last row fully in view after the step. The grid fetches 5 rows above
     the top and 10 past the bottom, so the new top row is already in the DOM before a 10-row
     step and would time one frame whatever the backend does. Before each measured step the
     target must be absent, or the check fails.
   - Hard limit on every step, warm-up included, and on the first render:
     `document.querySelectorAll('.tg-row').length <= 200` (the header row is one). The first
     step over it fails the check and is named. The highest and lowest counts are recorded.

   Pass: p95 < 50 ms over 200 samples.
3. Mocked backend (`perf/tree.spec.ts`, job "size tree scroll at one million rows"). Playwright
   @playwright/test 1.64.1, headless Chromium, window 1200 x 800 (the app's). The real
   `TreeGrid` (`src/views/tree.ts`) is mounted over `mockBackend` with
   `syntheticPage(view, 1_000_000, offset, limit)` (`src/testing.ts`) in a test page that vite
   builds into `dist-perf/` (`perf/vite.config.ts`), never into the app's `dist/`. Same steps,
   boundary, samples, threshold and DOM limit as (2). Results are in the `tree-scroll-1m`
   artifact and the job summary.

Controls (none lowers a threshold):

- every timing check also judges its own samples against 0 ms, and that verdict must fail;
- (3) runs a second time over a backend that answers each page 100 ms late, and that run must
  miss the 50 ms threshold (p50 at least 100 ms). A timing that stopped on a row already shown,
  or before the page answered, would pass there;
- variant `row-cap-removed.patch` (below) must fail (2).

The 0 ms judgement cannot pass for any samples, so it only shows that the verdict is computed,
not fixed. No run has shown check (1) failing on its threshold. Check (2)'s failure path has
been seen on real misses (results below). The owner accepted the last row fully in view as the target. The
pre-registration says "the new rows", and the brief says "the target row".

Demonstrations, not committed (t2-dev, docker node:22-bookworm, Playwright 1.64.1, before the
first commit): a copy of (3) with the threshold set to 0.001 ms failed (p95 17.7 ms); with
`row-cap-removed.patch` applied, (3) failed on the first render ("1001 .tg-row elements in the
DOM, more than 200").

Results, as measured. Every run is listed, and none was retried to pass. Milliseconds; Rows is
the highest `.tg-row` count on any step, the header row included.

Workflow "desktop" run 44877232202 at commit 86fafdc (the controls and the variant behaved as
required):

| Check | Where | n | p50 | p95 | max | Rows | Verdict |
|---|---|---|---|---|---|---|---|
| (1) IPC `tree_page`, limit 200 | Linux, ubuntu-22.04, under strace and Xvfb | 100 | 2 | 3 | 10 | | pass |
| (2) running-app scroll | Linux, ubuntu-22.04, under strace and Xvfb | 200 | 33 | 41 | 46 | 29 | pass |
| (2) running-app scroll | Windows, windows-latest | 200 | 31.2 | 32.2 | 32.9 | 28 | pass |
| (3) mocked scroll | headless Chromium 156.0.8078.4, ubuntu-latest | 200 | 16.7 | 17.2 | 19.4 | 29 | pass |
| (3) control: backend 100 ms late | same | 200 | 133.3 | 133.5 | 136.6 | 29 | misses 50 ms, as required |

Workflow "desktop" run 44879146053 at commit 0291587. The checks are the same as at 86fafdc;
since then only the session's script timeout was raised and the docs were changed. Linux's
running-app scroll missed:

| Check | Where | n | p50 | p95 | max | Rows | Verdict |
|---|---|---|---|---|---|---|---|
| (1) IPC `tree_page`, limit 200 | Linux, ubuntu-22.04, under strace and Xvfb | 100 | 5 | 9 | 11 | | pass |
| (2) running-app scroll | Linux, ubuntu-22.04, under strace and Xvfb | 200 | 46 | 69 | 80 | 29 | **fail** (p95 69 ms, not under 50 ms) |
| (2) running-app scroll | Windows, windows-latest | 200 | 31.3 | 32.2 | 44.4 | 28 | pass |
| (3) mocked scroll | headless Chromium 156.0.8078.4, ubuntu-latest | 200 | 16.7 | 17.0 | 17.8 | 29 | pass |
| (3) control: backend 100 ms late | same | 200 | 133.3 | 133.6 | 134.4 | 29 | misses 50 ms, as required |

In the same run, the first session's 5,000-row scroll check measured p50 33, p95 36 ms.

The e2e-variants run 44877270665 at 86fafdc ran the same one-million-row session on six more
ubuntu-22.04 runners. Six of its variants leave the size tree untouched: they change backend
writes, the root check, the capability list or the Settings question. Their Linux scroll
results (p95, then p50, in ms):

| Variant job | IPC p50 | Scroll p50 | Scroll p95 | Verdict |
|---|---|---|---|---|
| planted-write | 4 | 39 | 51 | fail |
| planted-outside | 5 | 47 | 69 | fail |
| offered-check-removed | 5 | 46 | 70 | fail |
| capability-broadened | 5 | 47 | 66 | fail |
| focus-broken | 3 | 34 | 41 | pass |
| modal-trap-removed | 3 | 34 | 42 | pass |

Running-app scroll (check 2), all runs through desktop run 44879992370 at cb39cc1:

- Windows passed 3 of 3, with p95 of 32.2 to 32.3 ms.
- Linux, under Xvfb and strace, passed 4 of 9. The p50 is about 33 ms or about 46 ms.
  - With scroll p50 about 33 ms: scroll p95 is 41 to 43 ms and IPC p50 is 2 to 3 ms.
  - With scroll p50 about 46 ms: scroll p95 is 51 to 70 ms and IPC p50 is 4 to 5 ms.

The cause of the split is not established. Run 44879992370 at cb39cc1 measured p50 34, p95 43
and max 55 ms (pass). This is an additional measurement, not an AC-9 verdict (above). The
threshold stays at 50 ms.

The suite now also records, in the same one-million-row session and never as a pass gate:

- `info.synth_1m.runner`: CPU model, logical CPUs, `nproc`, memory, load (Linux load average;
  Windows processor LoadPercentage), and the X screen (`xdpyinfo`, `xrandr`, when installed);
- `info.synth_1m.frame_floor`: 200 empty frames (requestAnimationFrame, then a MessageChannel
  message, nothing changed), on the same clock as a step, with the rAF interval;
- `info.synth_1m.scroll_profile`: the 200 steps again, each split into slices: to the scroll
  event, load() up to its IPC request, the IPC to its headers, the body, the callback and DOM
  build, and to the stop. It also counts layout reads (getBoundingClientRect, clientHeight,
  offsetHeight, scrollTop). It runs after the gated samples, so the instrumentation never
  reaches them;
- `info.synth_1m.runner_end`: load at the end, and on Linux the CPU time shares (steal,
  iowait) over the session.

The Playwright job records the floor and the profile too.

Both builds showed 1,000,001 rows before measuring in both runs. Linux's WebDriver clock ticks in whole
milliseconds. The variant run is workflow "e2e-variants" run 44877270665 at 86fafdc. In it,
`row-cap-removed` failed `timing.synth_1m.scroll_p95` on the first render ("1001 .tg-row
elements in the DOM, more than 200"), and the other six variants were still caught.

The DOM limit counts every `.tg-row`, selected or not. A grid that keeps the selected row in
the DOM while it is scrolled away can hold one row more than `MAX_DOM_ROWS` at the cap. At the
app's window height the grid holds about 28 rows, far under the limit.

Broken variants (workflow "e2e-variants", Linux): each patch in `e2e/variants/` breaks one
guarantee, and `e2e/expect_variant.py` passes only when the check written for it fails:

| Patch | What it breaks | Check that must fail |
|---|---|---|
| `planted-write.patch` | writes a file into the first scan root | manifest: an UNDECLARED addition |
| `offered-check-removed.patch` | Start no longer checks that a root was offered | `ipc.refused.root_not_offered`, `ipc.refused.subfolder_of_offered_root` |
| `capability-broadened.patch` | grants `core:path:default` | `ipc.refused.not_in_capabilities.path` |
| `focus-broken.patch` | the shorten question no longer takes focus | `settings.save_confirm_keep_escape` |
| `row-cap-removed.patch` | the size tree's row cap is raised to 1000 and the grid renders up to the cap (a 1000-row buffer). Raising or deleting `MAX_DOM_ROWS` alone changes nothing at the app's window height, where the visible height (about 28 rows) is the binding limit, so the patch also renders the cap | `timing.synth_1m.scroll_p95` |

macOS: no WebDriver exists for an app's WKWebView. Apple's safaridriver drives Safari only,
and tauri-driver supports Linux and Windows only. On macOS the app is covered by:

- the bundle job's idle launch and its webview-data check;
- the backend, state-flow and containment tests on macos-latest;
- the frontend unit tests, which are platform independent (jsdom);
- the shipped-program check above, on both macOS bundles.

No flow in the window itself is exercised on macOS. Accepting that coverage is an assumption
awaiting the owner's confirmation.
