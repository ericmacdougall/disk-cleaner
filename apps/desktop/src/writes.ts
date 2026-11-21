// What the desktop app writes outside the folders it scans: the one list the app shows
// (Settings, "What this app writes") and docs/DESKTOP.md repeats ("Files the desktop app
// writes"; src/writes.test.ts fails when they differ). It never deletes, moves or backs up
// a scanned file. Each entry was observed (e2e/idle-launch.sh and the end-to-end checks on
// Linux, the bundle job on macOS) unless it says otherwise.
//
// The recent-files entry and the file dialog's settings are GTK's: turning GTK's recent-files
// setting off erases the user's existing list (measured with GTK 3.24.41), so the app leaves
// it on and says so here. The owner chose this, "Keep and disclose" (user decision cited in the
// spec and GoodQA goodqa#16452 as user:51f9a04e-0653-462e-a2aa-b86109718f90/
// b90ef1a3-95ba-4b8a-83d7-9caaf7f3767d). Removing the app's entries after each dialog and the
// desktop portal are not built.

export interface Write {
  /** Where, as the user would find it (default locations). */
  path: string;
  /** What is written and why. */
  what: string;
}

export interface WriteGroup {
  title: string;
  writes: Write[];
}

export const TAGLINE =
  "Read-only disk view: nothing here deletes, moves or backs up anything. What it writes is listed in Settings.";

export const WRITES: WriteGroup[] = [
  {
    title: "Every system, only when you ask",
    writes: [
      {
        path: "disk-cleaner/config.toml (with config.toml.bak and config.toml.lock)",
        what: "on Save of the plan settings: the two plan settings you changed, a copy of the text loaded and the save lock. In ~/.config (Linux), ~/Library/Application Support (macOS) or %APPDATA% (Windows).",
      },
      {
        path: "disk-cleaner/prunedisk-app.json",
        what: "on Save: the app's own scan settings, in the same folder.",
      },
      { path: "the plan file you export", what: "on Export: a new file in the folder you choose; never an existing file." },
    ],
  },
  {
    title: "Linux, by the system libraries the window uses",
    writes: [
      {
        path: "~/.local/share/io.github.ericmacdougall.prunedisk/",
        what: "the web view's private session still stores random salts (storage/salt, mediakeys/v1/salt, CacheStorage/salt, WebKitCache/Version 17/salt), an empty WebKitCache/Version 17/Blobs folder and its HTTPS policy store (hsts-storage.sqlite). No browsing data: the app shows only its own page.",
      },
      { path: "~/.cache/mesa_shader_cache/", what: "the graphics driver's shader cache, as for any program that draws with Mesa." },
      { path: "~/.cache/mesa_shader_cache_db/", what: "the same cache in newer Mesa's single-file form." },
      { path: "~/.cache/fontconfig/", what: "the font cache, when the system's is out of date." },
      {
        path: "$XDG_RUNTIME_DIR/dconf/user",
        what: "the settings system's change flag, in memory, not on disk (in ~/.cache/dconf/user on a system with no runtime folder).",
      },
      {
        path: "$XDG_RUNTIME_DIR/at-spi2-XXXXXX/",
        what: "only while a screen reader or other assistive technology is on: the accessibility bridge's socket for the window, in memory (in ~/.cache on a system with no runtime folder). XXXXXX is six random letters or digits.",
      },
      {
        path: "~/.local/share/recently-used.xbel",
        what: "the file dialog (GTK) adds the folders and files you pick in it to your recent files, as in every GTK app. It writes a temporary recently-used.xbel.XXXXXX (six random letters or digits) beside the list and renames it over the list.",
      },
      { path: "~/.config/gtk-3.0/", what: "an empty folder the file dialog creates for its bookmarks, when there is none." },
      {
        path: "~/.config/glib-2.0/settings/",
        what: "an empty folder for the settings system's file store, on a system without dconf.",
      },
      {
        path: "dconf: org.gtk.Settings.FileChooser",
        what: "the file dialog saves its own size and view settings when it closes. Not yet observed by the app's checks: the settings service writes it from its own process, which the trace does not follow.",
      },
      {
        path: "~/.dbus/session-bus/ and /tmp/dbus-*",
        what: "only on a system with no desktop session bus, where the system library starts one.",
      },
    ],
  },
  {
    title: "macOS",
    writes: [
      {
        path: "~/Library/WebKit/io.github.ericmacdougall.prunedisk and ~/Library/Caches/io.github.ericmacdougall.prunedisk",
        what: "none: the web view's session is kept in memory (checked after every build).",
      },
    ],
  },
  {
    title: "Windows",
    writes: [
      {
        path: "%LOCALAPPDATA%\\io.github.ericmacdougall.prunedisk\\EBWebView\\",
        what: "the web view's (WebView2) data folder, created by the runtime; the private session keeps browsing data out of it. Not yet observed file by file.",
      },
    ],
  },
];
