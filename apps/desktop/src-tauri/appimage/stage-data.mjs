#!/usr/bin/env node
// Stages the desktop data the GTK file dialog needs into src-tauri/gen/appimage-data, from
// which tauri.conf.json's bundle.linux.appimage.files copies it into the AppImage
// (docs/DESKTOP.md, "Linux AppImage"). Run by Tauri as build.beforeBundleCommand; does nothing
// on macOS and Windows.
//
// Why: linuxdeploy's GTK plugin bundles GTK, gdk-pixbuf and its loaders and points the AppRun
// hook at them (XDG_DATA_DIRS, GDK_PIXBUF_MODULE_FILE, GSETTINGS_SCHEMA_DIR, GIO_MODULE_DIR,
// GTK_PATH all inside the mount), but it bundles no shared-mime-info database and no icon
// theme. Ubuntu's gdk-pixbuf picks an image loader by GIO content-type sniffing, which reads
// the mime database; on a host without /usr/share/mime even GTK's own fallback icon is
// "Unrecognized image file format", and with no icon theme GTK needs that fallback for the
// file dialog's first icon, so it aborts (CT-F1, GoodQA #16452). XDG_DATA_DIRS already puts
// the AppImage's usr/share first, so these files are found there on any host.
//
// What: the compiled shared-mime-info database (the top-level files of usr/share/mime, not
// the per-type XML sources), the Adwaita icon theme without its sizes above 48 px and without
// icon-theme.cache (a cache that lists files left out would make GTK trust missing files),
// hicolor's index.theme (Adwaita inherits it; the app's own icons are already under hicolor),
// and each package's copyright file. Source root: PRUNEDISK_APPIMAGE_DATA_ROOT or "/".
import fs from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";

if (process.platform !== "linux") process.exit(0);

const here = path.dirname(fileURLToPath(import.meta.url));
const out = path.join(here, "..", "gen", "appimage-data");
const root = process.env.PRUNEDISK_APPIMAGE_DATA_ROOT || "/";
const share = path.join(root, "usr", "share");
const ADWAITA_DIRS = ["scalable", "scalable-up-to-32", "8x8", "16x16", "22x22", "24x24", "32x32", "48x48"];
const DOCS = ["shared-mime-info", "adwaita-icon-theme", "hicolor-icon-theme"];

function fail(msg) {
  console.error(`stage-data: ${msg}`);
  console.error("stage-data: install shared-mime-info, adwaita-icon-theme and hicolor-icon-theme on the build host");
  process.exit(1);
}

function need(p) {
  if (!fs.existsSync(p)) fail(`${p} is missing`);
  return p;
}

fs.rmSync(out, { recursive: true, force: true });

// Compiled mime database: regular files at the top level only.
const mimeSrc = need(path.join(share, "mime"));
need(path.join(mimeSrc, "mime.cache"));
const mimeOut = path.join(out, "mime");
fs.mkdirSync(mimeOut, { recursive: true });
let mimeFiles = 0;
for (const e of fs.readdirSync(mimeSrc, { withFileTypes: true })) {
  if (e.isFile()) {
    fs.copyFileSync(path.join(mimeSrc, e.name), path.join(mimeOut, e.name));
    mimeFiles++;
  }
}

// Adwaita, the sizes a dialog uses, and hicolor's index.theme.
const adw = need(path.join(share, "icons", "Adwaita"));
const adwOut = path.join(out, "icons", "Adwaita");
fs.mkdirSync(adwOut, { recursive: true });
fs.copyFileSync(need(path.join(adw, "index.theme")), path.join(adwOut, "index.theme"));
for (const d of ADWAITA_DIRS) {
  const src = path.join(adw, d);
  if (fs.existsSync(src)) fs.cpSync(src, path.join(adwOut, d), { recursive: true, verbatimSymlinks: true });
}
need(path.join(adwOut, "scalable"));
fs.mkdirSync(path.join(out, "icons", "hicolor"), { recursive: true });
fs.copyFileSync(need(path.join(share, "icons", "hicolor", "index.theme")), path.join(out, "icons", "hicolor", "index.theme"));

// Licences of what is copied.
for (const pkg of DOCS) {
  fs.mkdirSync(path.join(out, "doc", pkg), { recursive: true });
  fs.copyFileSync(need(path.join(share, "doc", pkg, "copyright")), path.join(out, "doc", pkg, "copyright"));
}

let icons = 0;
const count = (d) => {
  for (const e of fs.readdirSync(d, { withFileTypes: true })) {
    if (e.isDirectory()) count(path.join(d, e.name));
    else icons++;
  }
};
count(adwOut);
console.log(`stage-data: ${mimeFiles} mime database files and ${icons} Adwaita files from ${share} into ${out}`);
