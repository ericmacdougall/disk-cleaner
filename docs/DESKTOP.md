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

### Linux AppImage

The AppImage carries GTK, gdk-pixbuf and its loaders, and its AppRun hook (written by
linuxdeploy's GTK plugin, which Tauri embeds) points `XDG_DATA_DIRS`,
`GDK_PIXBUF_MODULE_FILE`, `GSETTINGS_SCHEMA_DIR`, `GIO_MODULE_DIR` and `GTK_PATH` inside
the image. The plugin bundles no shared-mime-info database and no icon theme, so the
image adds them (`bundle.linux.appimage.files` in `src-tauri/tauri.conf.json`, staged
from the build host by `src-tauri/appimage/stage-data.mjs`, Tauri's
`beforeBundleCommand`):

- the compiled shared-mime-info database (`usr/share/mime`). Ubuntu's gdk-pixbuf chooses an
  image loader by content-type sniffing, which reads this database. Without it, even GTK's
  built-in fallback icon is an "Unrecognized image file format".
- Adwaita without its sizes above 48 px and without `icon-theme.cache`, plus hicolor's
  `index.theme`. Without an icon theme, the folder dialog falls back to that fallback icon.
- each package's copyright file (`usr/share/doc`).

Without them, "Add folders..." aborted the whole app ("Bail out! Gtk:ERROR ...
image-missing.png: Unrecognized image file format") on any host without
`/usr/share/mime` (CT-F1, GoodQA #16452). The bundle job fails if the AppImage lacks any of
these files.

The job "AppImage without host GTK" runs the shipped AppImage in bare `fedora:41` and
`debian:bookworm-slim` containers (`e2e/appimage-dialog.sh`). The containers have no GTK,
gdk-pixbuf, mime database or icon theme; the script refuses a host that has any of them. The
test opens the dialog through the accessibility tree, picks a folder by path and requires the
folder in the app with the app alive. It fails on a GTK error, critical or missing icon. It
runs under `e2e/idle-launch.sh`, so the write observer sees the dialog's writes. A control
leg deletes the added data from the same AppImage and requires the check to fail.

The `.deb` needs none of this: it depends on `libgtk-3-0`, which brings shared-mime-info,
Adwaita and hicolor even without recommended packages. On Debian 12 with
`--no-install-recommends`, the 682a0ff `.deb` opened the dialog and returned a folder.

### Screen readers on Linux (known limit)

WebKitGTK 2.50.0 to 2.50.4 send screen readers the wrong role for every element from
"paragraph" on in AT-SPI's list: headings are read as "document frame", paragraphs as
"footer", links as "form" and the size tree's rows as "input method window". WebKit's own
copy of AT-SPI's role list lost an entry (fixed upstream in WebKitGTK 2.50.5, WebKit bug
300090). The app cannot correct it.
- The `.deb` uses the system's WebKitGTK, so it has this fault on any host that still has one
  of those versions. Ubuntu 22.04 ships 2.50.4.
- The AppImage built on Ubuntu 22.04 carries 2.50.4 itself, so it has the fault on every
  host: 7 of 27 roles were wrong on Debian 12, Fedora 41 and Ubuntu 24.04. The same binary
  on a system WebKitGTK 2.52.6 had none wrong.
- Moving the AppImage build to a maintained WebKitGTK also moves the oldest Linux it starts
  on. That decision is open (GoodQA #16452).

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

The app never deletes, moves or backs up a scanned file. The window's tagline says: "Read-only
disk view: nothing here deletes, moves or backs up anything. What it writes is listed in
Settings." Settings, under "What this app writes", shows the list below, from
`apps/desktop/src/writes.ts`; `src/writes.test.ts` fails when this section and that file
differ. Outside the folders it scans, the app writes only this:

**Every system, only when you ask**

- `disk-cleaner/config.toml (with config.toml.bak and config.toml.lock)`: on Save of the plan settings: the two plan settings you changed, a copy of the text loaded and the save lock. In ~/.config (Linux), ~/Library/Application Support (macOS) or %APPDATA% (Windows).
- `disk-cleaner/prunedisk-app.json`: on Save: the app's own scan settings, in the same folder.
- `the plan file you export`: on Export: a new file in the folder you choose; never an existing file.

**Linux, by the system libraries the window uses**

- `~/.local/share/io.github.ericmacdougall.prunedisk/`: the web view's private session still stores random salts (storage/salt, mediakeys/v1/salt, CacheStorage/salt, WebKitCache/Version 17/salt), an empty WebKitCache/Version 17/Blobs folder and its HTTPS policy store (hsts-storage.sqlite). No browsing data: the app shows only its own page.
- `~/.cache/mesa_shader_cache/`: the graphics driver's shader cache, as for any program that draws with Mesa.
- `~/.cache/mesa_shader_cache_db/`: the same cache in newer Mesa's single-file form.
- `~/.cache/fontconfig/`: the font cache, when the system's is out of date.
- `$XDG_RUNTIME_DIR/dconf/user`: the settings system's change flag, in memory, not on disk (in ~/.cache/dconf/user on a system with no runtime folder).
- `$XDG_RUNTIME_DIR/at-spi2-XXXXXX/`: only while a screen reader or other assistive technology is on: the accessibility bridge's socket for the window, in memory (in ~/.cache on a system with no runtime folder). XXXXXX is six random letters or digits.
- `~/.local/share/recently-used.xbel`: the file dialog (GTK) adds the folders and files you pick in it to your recent files, as in every GTK app. It writes a temporary recently-used.xbel.XXXXXX (six random letters or digits) beside the list and renames it over the list.
- `~/.config/gtk-3.0/`: an empty folder the file dialog creates for its bookmarks, when there is none.
- `~/.config/glib-2.0/settings/`: an empty folder for the settings system's file store, on a system without dconf.
- `dconf: org.gtk.Settings.FileChooser`: the file dialog saves its own size and view settings when it closes. Not yet observed by the app's checks: the settings service writes it from its own process, which the trace does not follow.
- `~/.dbus/session-bus/ and /tmp/dbus-*`: only on a system with no desktop session bus, where the system library starts one.

**macOS**

- `~/Library/WebKit/io.github.ericmacdougall.prunedisk and ~/Library/Caches/io.github.ericmacdougall.prunedisk`: none: the web view's session is kept in memory (checked after every build).

**Windows**

- `%LOCALAPPDATA%\io.github.ericmacdougall.prunedisk\EBWebView\`: the web view's (WebView2) data folder, created by the runtime; the private session keeps browsing data out of it. Not yet observed file by file.

Details. Core's config folder follows `HOME` (macOS: `~/Library/Application Support/disk-cleaner/`),
`APPDATA` (Windows) or `XDG_CONFIG_HOME` (Linux, else `~/.config/disk-cleaner/`). On Save only
`active_window_days` and `max_git_admin_bytes` change in `config.toml`; `config.toml.bak` holds
the text Settings loaded; `config.toml.lock` stays, empty, by design; during the save a temp
file is renamed over `config.toml` or removed. A plan export is always a new file, in the folder
chosen for it; the app then says "Plan exported to <the file>. Nothing else was changed." Every
file and folder dialog starts in your Documents folder, or your home folder when there is none;
the app does not remember the last folder used.

Recent files (Linux). GTK has a setting that turns its recent-files list off, but with it off
the next dialog accept rewrites the account's `recently-used.xbel` empty: the user's existing
list is erased (measured with GTK 3.24.41: a real file dialog accepted through its Open
button, with one entry already in the list). So the app leaves the setting alone and lists the
entry above. The owner chose this ("Keep and disclose", GoodQA goodqa#16452); removing the
app's own entries after each dialog and the desktop portal are not built.

How the Linux list is checked. `e2e/idle-launch.sh` runs the shipped build (the release binary
in the end-to-end job) idle for 20 s in a sandbox, with a session bus, under strace, and
`e2e/trace_report.py` fails on any write outside the declared salts, Blobs folder, HTTPS policy
store, Mesa and fontconfig caches and dconf flag. In the owner's run of the 682a0ff AppImage
(Docker on Ubuntu 24.04, not a click-test) that was every write; without a session bus the same
launch also wrote `~/.dbus/session-bus/` and `/tmp/dbus-*`. The end-to-end suite picks folders
through a test hook, so the file dialog's writes are checked by the `appimage-hosts` job:
`e2e/appimage-dialog.sh` runs the AppImage through the same idle launch on bare Fedora 41 and
Debian 12, opens the real dialog through the accessibility bus and accepts a folder, and the
trace may then add only the dialog's entries above: the recent-files list and its temporary
file, the empty `gtk-3.0` and `glib-2.0/settings` folders and the accessibility socket folder.
A randomly named file is declared as a template (`--declared-temp`), which allows exactly six
letters or digits in place of `XXXXXX` and nothing else; a neighbouring name still fails. The
dialog's own settings are written by the settings service, a separate process the trace does
not follow, so that entry stays "not yet observed".

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
- Linux: incognito gives webkit2gtk an ephemeral data manager, so no browsing data is
  written, but it still creates the salts, Blobs folder and HTTPS policy store listed above
  under `~/.local/share/io.github.ericmacdougall.prunedisk/` (observed; this section said
  "not observed" until PR gate F10). With Mesa graphics drivers, the webview's GPU process
  writes the driver's shader cache, as any program drawing with Mesa does.

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
7. Export the plan (the message names the file and says nothing else changed), then New
   scan: the chooser shows no status from the results.
8. Open a valid scan file (no status left from the read), try a second export, which is
   cancelled and writes and says nothing, then New scan and Add folders (no status from the
   import), a malformed file (focus returns to Open) and a cancelled one.
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
- On Linux the app and its child processes also run under strace (`-f -y`, the calls
  `e2e/trace_report.py --strace-calls` names, via seccomp-bpf; one log per launch), and
  `e2e/trace_report.py` judges every path written anywhere, including outside the sandbox. It
  is a gate (PR gate F10): a write outside the declared paths (OTHER), a relative path or
  descriptor it cannot place (UNRESOLVED), io_uring (UNOBSERVABLE) or a traced call it cannot
  read (UNPARSED) fails the check, and so does a log with no traced call. Declared outside the
  sandbox: only dconf's flag file. Ignored: only `/dev/null`. Not covered: writes through a
  memory map of a file opened read-write (the open itself is judged), and the macOS window.
- The same job runs `e2e/idle-launch.sh` on the release binary: the shipped config, idle,
  under the same gate, with the list in "Files the desktop app writes".

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
AC-9 pass. It is a pass gate in `e2e/run.mjs`. Since main's ruling after the no-strace controls
(below), on Linux it is timed in an untraced launch; its threshold, n and no-retry rule are
unchanged.

Every check here runs on the one-million-row synthetic tree above (`PRUNEDISK_E2E_SYNTH_NODES`,
1,000,001 rows), never the 5,000-row fixture of the first session's scroll check.

The sessions, at the end of the WebDriver suite, each with its own tauri-driver because the app
takes its environment from the driver:

- Linux, the timed launch (ports 4446 and 4447): untraced (below). It must show it is untraced
  (`trace.synth_1m.timed_launch_untraced`: the app process's `TracerPid` is 0). It records (1)'s
  pages as information (`info.synth_1m.ipc_untraced`), then runs (2), then the floor and
  profile probes.
- Linux, the traced launch (ports 4448 and 4449): under the strace wrapper like every other
  launch. It runs (1), as registered, then (2)'s steps again, not timed
  (`steps.synth_1m.traced_scroll`): they must complete within the DOM limit. `e2e/check.sh`
  requires its strace log, and the write observer and the snapshot cover it.
- Windows: one session (ports 4446 and 4447) running (2). Windows has no tracer.

The untraced switch. `e2e/run.mjs` sets `PRUNEDISK_E2E_UNTRACED=1` for the timed launch only.
`e2e/check.sh`'s app wrapper (CI only; it never ships) then starts that launch without strace,
but only when `PRUNEDISK_E2E_SYNTH_NODES` is set too and the e2e binary holds the switch's name,
which only a `--features e2e` build does (`src-tauri/src/e2e.rs`). The app checks the claim:
with the switch set and a tracer attached, it shows no synthetic tree. Each untraced launch
leaves a marker in `$OUT/untraced/`, each traced one-million-row launch one in
`$OUT/traced-synth/`, and the trace step fails when a traced one-million-row launch left no
strace log or none ran. A release binary holds no `PRUNEDISK_E2E_` name (check.sh's string
check); `e2e/check-shipped.sh` names `PRUNEDISK_E2E_UNTRACED` on its own, check.sh's control
requires it to report that name on the e2e binary, and the variant `untraced-switch-shipped`
(below) plants the switch in a release build, which check-shipped.sh must fail.

1. IPC (Linux; `timing.synth_1m.ipc_tree_page_p95` in `e2e/run.mjs`, in the traced launch). In
   the page, `__TAURI_INTERNALS__.invoke`
   `tree_page` with offset k * 200 and limit 200: 10 warm-up pages, then k = 0..99 timed with
   `performance.now()`. Each page must hold 200 rows at its offset. Pass: p95 < 50 ms over
   100 samples.
2. Running-app scroll (Linux and Windows; `timing.synth_1m.scroll_p95`, in the timed launch). Size
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

First results with these probes (commit 0588a6e): workflow "desktop" run 44881694359 and the
seven jobs of "e2e-variants" run 44881704164. At that commit the floor ran just before the
gated steps, and the gated Linux scroll moved on both runner classes against earlier runs: p50
about 33 rose to 39 to 41 ms, and p50 about 46 rose to 85 to 88 ms. Windows and the 5,000-row
check did not move. These gated samples may be affected by the probe. Commit 030f494 restores
the registered order: IPC, the gated steps, then the floor, then the profile (results below).
Every runner had 4 logical CPUs. Milliseconds,
p50/p95. Floor is the empty frame. Load runs from the scroll event to the DOM holding the
target row (IPC included). Stop runs from that point to the stop (the next frame and the
post-paint task). The profile runs after the gated steps, further down the tree.

| Job | CPU | Floor | Gated scroll | Profile load | Profile stop | Verdict |
|---|---|---|---|---|---|---|
| desktop, Linux | AMD EPYC 7763 | 22/57 | 85/128 | 8/15 | 121/183 | fail |
| planted-write, Linux | AMD EPYC 7763 | 23/60 | 88/120 | 11/16 | 116/176 | fail |
| focus-broken, Linux | AMD EPYC 7763 | 23/59 | 87/126 | 10/16 | 118/175 | fail |
| planted-outside, Linux | AMD EPYC 9V74 | 22/48 | 66/100 | 9/14 | 84/133 | fail |
| capability-broadened, Linux | Intel Xeon Platinum 8370C | 18/29 | 51/86 | 7/15 | 77/122 | fail |
| offered-check-removed, Linux | AMD EPYC 9V45 | 16/25 | 39/59 | 5/11 | 44/69 | fail |
| modal-trap-removed, Linux | AMD EPYC 9V45 | 17/30 | 41/62 | 5/10 | 47/73 | fail |
| desktop, Windows | AMD EPYC 7763 | 15.2/16.7 | 31.2/32.7 | 4.2/4.9 | 14.1/15.3 | pass |
| Playwright, Chromium | (ubuntu-latest) | 16.7/16.9 | 16.6/17.7 | 2.0/2.5 | 3.8/4.9 | pass |

Other measurements:

- Linux: IPC within load was 1 to 2 ms p50, and every step made 2 layout reads and 1 IPC call.
  The runners' CPU steal and iowait were 0%, and idle was 41 to 52%.
- Linux: `xdpyinfo` and `xrandr` are not installed on the runner. The page reports a 1280 x 1024
  screen at depth 24, and the rAF interval p50 was 16 to 20 ms.
- Windows: Hyper-V video at 1024 x 768, refresh 1 (as reported).
- The first session's 5,000-row scroll in the same jobs stayed at p95 35 to 43 ms.

What the numbers show:

- On Linux the step's time is almost all in the frame after the DOM change. That frame takes
  44 to 121 ms (p50), while an unchanged frame takes 16 to 23 ms and the page's own work, IPC
  included, takes 5 to 11 ms.
- On Windows, on the same CPU model (EPYC 7763), that frame takes one frame period, 14 ms.
- The Linux time follows the CPU model: EPYC 9V45 was fastest and EPYC 7763 slowest.
- Within a session the cost grows. On the desktop Linux job the profile's stop slice rose from
  107 to 149 ms (median per 40 steps) over its 200 steps.
- The profile pass is not a decomposition of the gated samples. On Linux it took about 1.5
  times the gated p50 (137 against 85 ms, 58 against 39 ms); on Windows the two were equal.
  The slices describe the profile pass.
- Unchanged frames are already irregular on the EPYC 7763 runners (floor p95 57 to 60 ms).
- The profile does not separate the grid's DOM work from software rendering or strace. A run of
  the one-million-row session without the strace wrapper does (cc3e711, below).
- At this commit the cause of the slow Linux frame was not established. One candidate is WebKitGTK compositing
  and painting through Mesa's software renderer under Xvfb (the shader cache shows Mesa). That
  would fit the CPU dependence. Another is the strace wrapper, which also traces the
  `ftruncate` of WebKit's shared-memory buffers (`/memfd:WebKitSharedMemory`).

Results in the registered order with the probes (commit 030f494): workflow "desktop" run
44883021601 and the seven jobs of "e2e-variants" run 44883021841. Here the floor runs after
the gated steps, so it measures frames late in the session, not a fresh frame. The row-cap
variant fails on its first render and runs no steps, so its floor is a fresh one. Milliseconds;
floor and gated scroll are p50/p95/max, with the floor's rAF interval p50; profile slices are
p50/p95.

| Job | CPU | Floor after the steps | rAF | Gated scroll | Profile load | Profile stop | Verdict |
|---|---|---|---|---|---|---|---|
| desktop, Linux | AMD EPYC 9V74 | 64/100/122 | 65 | 38/60/68 | 7/11 | 89/130 | fail |
| focus-broken, Linux | AMD EPYC 9V74 | 62/104/121 | 65 | 40/56/63 | 6/13 | 88/133 | fail |
| capability-broadened, Linux | Intel Xeon Platinum 8573C | 68/161/206 | 65 | 38/53/67 | 6/11 | 79/126 | fail |
| modal-trap-removed, Linux | AMD EPYC 7763 | 78/135/157 | 81 | 46/68/84 | 8/14 | 118/176 | fail |
| offered-check-removed, Linux | AMD EPYC 7763 | 79/124/154 | 81 | 47/66/81 | 7/13 | 118/179 | fail |
| planted-outside, Linux | AMD EPYC 7763 | 84/130/146 | 81 | 47/65/82 | 7/13 | 121/173 | fail |
| planted-write, Linux | AMD EPYC 7763 | 82/130/146 | 81 | 45/68/77 | 8/14 | 116/172 | fail |
| row-cap-removed, Linux (no steps) | AMD EPYC 9V74 | 20/46/56 | 17 | not run | | | fails by design |
| desktop, Windows | AMD EPYC 9V74 | 15.5/16.7/18.1 | 15.6 | 31.3/32.6/34.9 | 3.5/5.0 | 14.4/15.5 | pass |
| Playwright, Chromium | (ubuntu-latest) | 16.7/16.8/19.3 | 16.7 | 16.7/17.0/17.7 | 1.3/1.6 | 2.5/3.2 | pass |

What these add:

- No Linux job passed. On the EPYC 7763 the gated scroll was 45 to 47 ms p50 and 65 to 68 ms
  p95, against 85 to 88 and 120 to 128 ms at 0588a6e on the same CPU model. The floor ahead of
  the gated steps made them slower; in the registered order they are faster but still over
  50 ms p95 on these runners.
- After the gated steps the Linux frame rate has fallen: rAF interval p50 65 to 81 ms, against
  16 to 20 ms in a fresh session (the 0588a6e floors, and 17 ms for the row-cap variant here).
  At 0588a6e the 200 unchanged frames also slowed the steps after them. The cost so grows with
  the frames the session has shown, or with its time; the two are not separated.
- The gated step medians per 50 steps rise through the run: 41, 46, 49, 59 ms on one EPYC 7763
  job and 34, 36, 39, 47 ms on the desktop job.
- Windows does not slow down: medians per 50 steps stay at 31.3 ms and the floor after the
  steps is one frame period.
- The page's own work, IPC included, stays at 6 to 8 ms p50 in the profile pass.
- The cause was not established at this commit. The run without the strace wrapper, later at
  cc3e711, separates strace from rendering (below).

The investigation sessions (registered in the WAL before any sample, informational, never a
gate). They are opt-in: the e2e-variants workflow's `ac9_investigate` input sets
`AC9_INVESTIGATE=1`, and `e2e/run.mjs` then runs them on Linux after the gated sessions, which
they do not change. Each one is a fresh one-million-row launch, once with the strace wrapper and
once without:

- steps: floor windows of 60 frames at the start, the middle and the end, around the 200 steps;
- app idle: the tree shown, no steps, only the floor loop, for as long as the steps session took;
- blank page: the same launch navigated to `about:blank`, so no app code runs in the page, then
  the floor loop for the same time.

They use the untraced switch described above (at cc3e711 it was named `PRUNEDISK_AC9_UNTRACED`
and honoured for any binary). The write observer (PR gate F10) does not see the untraced
launches; the sandbox snapshot still covers them. Each session
reads the app process's `TracerPid` from `/proc` and counts as invalid when it does not match
its arm. Page functions: `e2e/ac9x.mjs`, which reuses `e2e/ac9.mjs`'s step and floor code.

Running-app scroll tally, Linux under the strace wrapper. Through 030f494 the count was 4 of 16
passed in the registered order. By procedure and head, through cc3e711:

| Procedure | Head | Linux jobs passed | Note |
|---|---|---|---|
| registered order | 86fafdc, 0291587, cb39cc1 | 4 of 9 | three desktop runs and six tree-neutral variant jobs; CPU not recorded |
| registered order | 030f494 | 0 of 7 | desktop run 44883021601, e2e-variants run 44883021841 |
| registered order | cc3e711 | 1 of 7 | six tree-neutral jobs of e2e-variants run 44884912971 (the gated session each opens with) and desktop push run 44884907082 |
| registered order, total | | 5 of 23 | the earlier count of 5 of 22 left out desktop run 44884907082 |
| floor ahead of the gated steps | 0588a6e | 0 of 7 | kept apart: the procedure differed |

- Windows: 5 of 5 passed (86fafdc, 0291587, cb39cc1, 0588a6e, 030f494). Windows has no tracer.
- The row-cap variant is left out of every count, as it fails by design.
- At cc3e711 the gated session measured, p50/p95/max: EPYC 7763 46/64/86 and 46/69/79, Xeon
  Platinum 8573C 35/51/67 and 34/49/60 (the pass), EPYC 9V74 41/55/68 and 39/57/66, and in the
  desktop push run EPYC 7763 46/66/84.

The blank-page and app-idle controls (e2e-variants run 44884912971 at cc3e711, WAL entries
w07983c42191 and wf5f38e99704 registered them before any sample). Each arm is a fresh launch of
the one-million-row session, with the strace wrapper and without it, in the order steps, app
idle, blank page. The app-idle and blank-page arms run the floor loop for as long as the steps
arm of the same condition took (17 to 23 s traced, 10 s untraced). The blank page is
`about:blank` (3 elements, no grid, no app code). All 36 sessions of the six tree-neutral jobs
were valid: the app process was traced (`TracerPid` above 0) in every traced arm and untraced
in every untraced arm. Floor p95 in ms at the start, middle and end of each arm:

| CPU (jobs) | Arm | With strace | Without strace |
|---|---|---|---|
| AMD EPYC 7763 (2) | steps | 22-23 / 71 / 125-126 | 17 / 17 / 17 |
| AMD EPYC 7763 (2) | app idle | 21-22 / 77-82 / 141-145 | 17 / 18-19 / 18 |
| AMD EPYC 7763 (2) | blank page | 20-22 / 80-81 / 136-144 | 17 / 17 / 18 |
| Intel Xeon Platinum 8573C (2) | steps | 26-29 / 85-97 / 121-135 | 17 / 17 / 17 |
| Intel Xeon Platinum 8573C (2) | app idle | 27 / 40-41 / 153-157 | 17 / 17-19 / 17 |
| Intel Xeon Platinum 8573C (2) | blank page | 24-29 / 80-83 / 136-137 | 17 / 17 / 17-18 |
| AMD EPYC 9V74 (2) | steps | 20 / 57-61 / 103-106 | 17 / 17 / 17 |
| AMD EPYC 9V74 (2) | app idle | 19-20 / 63-66 / 106-111 | 17 / 18 / 17-19 |
| AMD EPYC 9V74 (2) | blank page | 20 / 59-66 / 109-118 | 17 / 17-18 / 17-18 |

- With strace the rAF interval p50 goes from 16 ms at the start to 65 to 113 ms at the end, in
  every arm. Without strace it stays at 16 ms.
- The blank page, with no app code, slows down exactly as the app does. What slows the frames
  is in the engine or the harness, not in what the app holds.
- Time is matched too. At their middle window (about 9 to 11 s in) the traced arms were already
  at floor p50 27 to 57 ms; the untraced arms were still at 16 ms at their end (10 s). The
  row-cap variant's app-idle and blank-page arms ran 20 s in both conditions: traced went from
  floor p50 16 to 62-64 ms, untraced stayed at 16 ms.

The untraced numbers. Without strace, on every CPU class seen (EPYC 7763, Xeon Platinum 8573C,
EPYC 9V74; two jobs each):

- the scroll steps measured p50/p95/max 33/35-37/36-55 ms (n=200, the same steps and clock as
  the gate, with floor windows between the halves), and the medians per 50 steps were 33, 33,
  33, 33;
- the floor stayed at p50 16 ms and p95 17 to 19 ms in every window, the rAF interval at 16 ms.

With strace, the same steps arm measured 59-60/100-104/118 ms on the EPYC 7763, 39-41/65-69/83-91
on the Xeon Platinum 8573C and 49/80-82/91-98 on the EPYC 9V74, with medians per 50 steps rising
from 33-45 to 54-90 ms.

The measurement condition was changed after these results were seen. The running-app scroll
gate was registered to be timed under the strace wrapper, and the results above were known
when the change was decided. The reason is the control data: the blank page slows down under
strace like the app, and nothing slows down without it, so under strace the gate times the
harness, not the app. Main's ruling (relayed by the stream owner; its WAL entry is in main's
WAL, inferred) keeps the threshold (50 ms nearest-rank p95), n (200 after 10 warm-up steps) and
the no-retry rule, and times the scroll in an untraced launch. Floor-relative thresholds and a
measure-only gate were rejected. The gate as changed is described at the top of this section
(the sessions and the untraced switch) and was registered
before its first run (WAL w3bd2a24a6c6, amended in wad2689e80fc before any run).

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
| `focus-broken.patch` | the shorten question gives first focus to Shorten | `settings.save_confirm_keep_escape` |
| `modal-trap-removed.patch` | the question opens non-modally: no inert background, no Tab trap | `settings.confirm_focus_contained` |
| `planted-outside.patch` | writes `/tmp/prunedisk-planted-outside`, outside every snapshot root | trace: `OTHER openat: /tmp/prunedisk-planted-outside` (and the manifest must not see it) |
| `row-cap-removed.patch` | the size tree's row cap is raised to 1000 and the grid renders up to the cap (a 1000-row buffer). Raising or deleting `MAX_DOM_ROWS` alone changes nothing at the app's window height, where the visible height (about 28 rows) is the binding limit, so the patch also renders the cap | `timing.synth_1m.scroll_p95` |
| `untraced-switch-shipped.patch` | reads `PRUNEDISK_E2E_UNTRACED` outside the e2e feature, so a release build holds the untraced switch (its own job: a release build, no suite) | `e2e/check-shipped.sh`: `holds "PRUNEDISK_E2E_UNTRACED"` |

macOS: no WebDriver exists for an app's WKWebView. Apple's safaridriver drives Safari only,
and tauri-driver supports Linux and Windows only. On macOS the app is covered by:

- the bundle job's idle launch and its webview-data check;
- the backend, state-flow and containment tests on macos-latest;
- the frontend unit tests, which are platform independent (jsdom);
- the shipped-program check above, on both macOS bundles.

No flow in the window itself is exercised on macOS. Accepting that coverage is an assumption
awaiting the owner's confirmation.
