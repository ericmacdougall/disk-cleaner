// End-to-end checks on the packaged app through tauri-driver (PR gate F1). One app session
// drives the real window: choose folders, scan, cancel, rescan, every view, developer-view
// paging and class switch, settings (save, confirm, Keep, Escape), plan export, opening a valid
// and a malformed scan file, IPC the capability list or the backend refuses, axe on each view,
// focus order, and in-page IPC and scroll timings against thresholds registered beforehand.
//
// The no-write proof is outside this script: the workflow snapshots the fixture and the
// sandboxed home before and after (e2e/manifest.py) and, on Linux, traces file writes.
//
// usage: node e2e/run.mjs --app <binary built with --features e2e> --fixture <root>
//          --big <a large folder to cancel during> --valid <scan.json> --malformed <file>
//          --export <an empty folder the export folder dialog picks> --out <result.json>

import { existsSync, mkdirSync, readFileSync, readdirSync, writeFileSync } from "node:fs";
import path from "node:path";
import { App, Run, args, startDriver, stopDriver } from "./driver.mjs";
import { KEY } from "./webdriver.mjs";
import { AC9, judge, measureFloor, measureScroll, profileScroll, scrollOptions, scrollVerdict, summarizeProfile } from "./ac9.mjs";
import { cpuShares, machine, procStates, procsSince, psTree, snapshot, tracerState } from "./runner_info.mjs";

// Registered before the first measurement (WAL w0ce4fd7355e): p95 by nearest rank.
const THRESHOLDS = { ipcP95Ms: 50, scrollP95Ms: 50, samples: 200, warmup: 10 };
const ROW_HEIGHT = 28;

const a = args(process.argv.slice(2));
for (const k of ["app", "fixture", "big", "valid", "malformed", "export"]) {
  if (!a[k]) throw new Error(`--${k} is required`);
}
const run = new Run("e2e");
const linux = process.platform === "linux";
// The untraced switch (an e2e build only, honoured by e2e/check.sh's wrapper; never shipped:
// e2e/check-shipped.sh). Every timing assertion is taken in a launch with it set, after the
// session shows from /proc that its app is not traced (main's ruling, relayed by the stream
// owner; inferred). Variant untraced-ignored renames it here, so every launch is traced and each
// timing check must fail on that precondition, never on its numbers.
const UNTRACED_VAR = "PRUNEDISK_E2E_UNTRACED";
const NOT_UNTRACED = "not shown to be untraced; not measured";
const env = {
  ...process.env,
  PRUNEDISK_E2E_FOLDERS: [a.fixture, a.big].join(path.delimiter),
  // Valid, then malformed, then the list is used up: the third Open is a cancel.
  PRUNEDISK_E2E_OPEN: [a.valid, a.malformed].join(path.delimiter),
  // One export folder; the second export is a cancel. The app names the file in it (N2).
  PRUNEDISK_E2E_EXPORT: a.export,
};
// The main session is traced (Linux), whatever the caller's environment holds.
delete env[UNTRACED_VAR];
const EXPORT_LABEL = "Export plan to a folder...";

/** `YYYYMMDD-HHMMSS` (UTC), as the app stamps an exported plan. */
function stampOf(ms) {
  return new Date(ms).toISOString().replace(/[-:]/g, "").replace("T", "-").slice(0, 15);
}

/** Nearest-rank percentile: the smallest sample with at least `q` of the samples at or below it. */
function rank(xs, q) {
  const s = [...xs].sort((x, y) => x - y);
  return s[Math.ceil(q * s.length) - 1];
}

let app;
const h2 = () => app.s.exec("return document.querySelector('main h2, #app h2, h2')?.textContent ?? null");
const live = () => app.s.exec("return document.querySelector('p.live')?.textContent ?? ''");
const active = () =>
  app.s.exec(
    `const e = document.activeElement;
     return e ? { tag: e.tagName.toLowerCase(), id: e.id || null, text: (e.textContent || "").trim().slice(0, 60), label: e.getAttribute("aria-label") } : null;`,
  );
const waitH2 = (text, ms = 60_000) =>
  app.s.until("return [...document.querySelectorAll('h2')].some((e) => e.textContent === arguments[0])", ms, text);
const state = async () => {
  const r = await app.invoke("current_state");
  if (!r.ok) throw new Error(`current_state: ${r.error}`);
  return r.value;
};
const refused = async (cmd, payload, expect) => {
  const before = await state();
  const r = await app.invoke(cmd, payload);
  if (r.ok) throw new Error(`allowed: ${JSON.stringify(r.value).slice(0, 200)}`);
  if (expect && !String(r.error).includes(expect)) throw new Error(`refused, but not with "${expect}": ${r.error}`);
  const after = await state();
  if (after.state !== before.state || after.view !== before.view) throw new Error(`state changed: ${JSON.stringify(before)} -> ${JSON.stringify(after)}`);
  return String(r.error).slice(0, 200);
};
const axe = async (id, selector) =>
  run.check(id, async () => {
    const r = await app.axe(selector);
    if (!r.ok) throw new Error(r.error);
    if (r.violations.length) throw new Error(`axe ${r.version}: ${JSON.stringify(r.violations)}`);
    return { axe: r.version, violations: 0 };
  });
const tab = async (label) => {
  const el = await app.s.findText("[role=tab]", label);
  await app.s.click(el);
  await app.s.until("return document.querySelector('[role=tab][aria-selected=true]')?.textContent === arguments[0]", 10_000, label);
};

// The default fixture's timings (registered in WAL w0ce4fd7355e), as functions of a session, so
// the same steps run in the traced main session and in the untraced timing session (Linux).
/** p50/p95/max of samples, nearest rank, against a threshold (information when gate is false). */
function stats(id, ms, thresholdMs) {
  return { id, n: ms.length, p50: rank(ms, 0.5), p95: rank(ms, 0.95), max: Math.max(...ms), thresholdMs };
}
/** The timings mean something only over the wide folder (probe run 44686507550 measured a short
 *  tree when the drill failed). */
function wideShown(total) {
  if (total < 1000) throw new Error(`the wide folder is not shown (${total} rows); not measured`);
}
/** Size tree, into the fixture's wide folder (5,000 rows): the row count shown. */
async function drillIntoWide(x) {
  const el = await x.s.findText("[role=tab]", "Size tree");
  await x.s.click(el);
  await x.s.until("return document.querySelector('[role=tab][aria-selected=true]')?.textContent === arguments[0]", 10_000, "Size tree");
  // Rows are re-rendered as pages arrive, so a row found a moment ago can be gone when the
  // pointer reaches it (run 44688993733: "stale element reference"). The driver refuses such an
  // action before it clicks, so trying again cannot click twice.
  const again = async (f) => {
    const end = Date.now() + 30_000;
    for (;;) {
      try {
        return await f();
      } catch (e) {
        if (Date.now() > end || !/stale element|no such element/.test(String(e?.message))) throw e;
        await new Promise((r) => setTimeout(r, 200));
      }
    }
  };
  await again(async () => x.s.dblclick(await x.s.find(".tg-scroll [role=row][aria-rowindex='2']")));
  await x.s.until("return document.querySelectorAll(\"nav[aria-label='Folder path'] button\").length >= 2", 15_000);
  await again(async () => x.s.dblclick(await x.s.findText(".tg-scroll [role=row] .name", "wide")));
  return x.s.until(
    "const n = Number(document.querySelector('.tg-scroll').getAttribute('aria-rowcount')) - 1; return n >= 1000 ? n : 0;",
    30_000,
  );
}
/** IPC: tree_page of 60 rows at offsets (i * 37) mod total, 10 warm-up calls, 200 timed. */
async function ipcDefaultSamples(x, total) {
  const r = await x.invoke("current_state");
  if (!r.ok) throw new Error(`current_state: ${r.error}`);
  const ms = await x.s.execAsync(
    `const [view, total, warm, n, done] = arguments;
     (async () => {
       const inv = window.__TAURI_INTERNALS__.invoke;
       const out = [];
       for (let i = 0; i < warm + n; i++) {
         const offset = (i * 37) % Math.max(1, total);
         const t = performance.now();
         await inv("tree_page", { view, offset, limit: 60 });
         if (i >= warm) out.push(performance.now() - t);
       }
       return out;
     })().then(done, (e) => done({ error: String(e) }));`,
    120_000,
    r.value.view,
    total,
    THRESHOLDS.warmup,
    THRESHOLDS.samples,
  );
  if (!Array.isArray(ms)) throw new Error(JSON.stringify(ms));
  return ms;
}
/** Scroll: 200 steps of 40 rows, each timed until its top row is in the DOM. */
async function scrollDefaultSamples(x) {
  const ms = await x.s.execAsync(
    `const [n, step, rowH, done] = arguments;
     (async () => {
       const sc = document.querySelector(".tg-scroll");
       const frame = () => new Promise((r) => requestAnimationFrame(r));
       const out = [];
       for (let i = 0; i < n; i++) {
         const max = sc.scrollHeight - sc.clientHeight;
         sc.scrollTop = sc.scrollTop + step * rowH > max ? 0 : sc.scrollTop + step * rowH;
         const want = Math.floor(sc.scrollTop / rowH);
         const t = performance.now();
         while (!document.getElementById("tg-row-" + want)) {
           await frame();
           if (performance.now() - t > 5000) throw new Error("row " + want + " never shown");
         }
         out.push(performance.now() - t);
         await frame();
       }
       return out;
     })().then(done, (e) => done({ error: String(e) }));`,
    180_000,
    THRESHOLDS.samples,
    40,
    ROW_HEIGHT,
  );
  if (!Array.isArray(ms)) throw new Error(JSON.stringify(ms));
  return ms;
}
/** The two gated default-fixture timings, in the session `x`. `ready` throws when the session
 *  may not be timed (not shown to be untraced, or the wide folder not shown). Thresholds, n and
 *  no retry as registered; each also judges its samples against 0 ms, which must fail. */
async function defaultTimings(x, total, ready) {
  await run.check("timing.ipc_tree_page_p95", async () => {
    ready();
    const ms = await ipcDefaultSamples(x, total);
    const m = stats("ipc_tree_page", ms, THRESHOLDS.ipcP95Ms);
    run.measure(m);
    if (judge(m.id, ms, 0).pass) throw new Error("control: a 0 ms threshold passed these samples");
    if (m.p95 > THRESHOLDS.ipcP95Ms) throw new Error(`p95 ${m.p95.toFixed(1)} ms > ${THRESHOLDS.ipcP95Ms} ms`);
    return m;
  });
  await run.check("timing.scroll_p95", async () => {
    ready();
    const ms = await scrollDefaultSamples(x);
    const m = stats("scroll_rows_shown", ms, THRESHOLDS.scrollP95Ms);
    run.measure(m);
    if (judge(m.id, ms, 0).pass) throw new Error("control: a 0 ms threshold passed these samples");
    if (m.p95 > THRESHOLDS.scrollP95Ms) throw new Error(`p95 ${m.p95.toFixed(1)} ms > ${THRESHOLDS.scrollP95Ms} ms`);
    return m;
  });
}
/** Just before a Scan click: the chosen folders, the git-facts box and the settings the app
 *  reports (the scan reads them from the settings file), so two sessions' scans can be compared. */
async function scanConditions(x) {
  const r = await x.invoke("get_settings");
  if (!r.ok) throw new Error(`get_settings: ${r.error}`);
  const ui = await x.s.exec(
    "return { roots: [...document.querySelectorAll('ul.chosen li')].map((l) => l.firstChild.textContent), gitFacts: document.getElementById('git-facts').checked };",
  );
  return { ...ui, settings: r.value };
}

// Linux: the app's processes in /proc, for the timed sessions' preconditions and the records of
// what a session left after it closed.
const SYNTH_PROCS = ["e2e", "WebKitWebProces", "WebKitNetworkPr"];
// An app instance: the e2e build (comm "e2e", as e2e/check.sh runs it) or the shipped binary.
const APP_NAMES = ["e2e", "prunedisk-deskt"];
/** The app processes started since `before` (pids of the e2e binary), with their TracerPid. */
const newApps = (before) => (tracerState(SYNTH_PROCS)?.procs ?? []).filter((p) => p.name === "e2e" && !before.has(p.pid));
const appPids = () => new Set((tracerState(SYNTH_PROCS)?.procs ?? []).filter((p) => p.name === "e2e").map((p) => p.pid));
/** Precondition of every timed session (pre-registered in WAL w4b6f0f8e4fa): no other app
 *  instance at its start, any /proc entry of the app's names counted, zombies too (as the AC-9
 *  investigation sessions count them). Read once at the session's start, recorded as `id`, never
 *  read again: returns the reason its timings are not measured, or null when it holds. Windows has
 *  no /proc, so it is not checkable there (recorded as such). */
const othersAtStart = (id) => {
  if (!linux) {
    run.measure({ id, checkable: false, reason: "no /proc on this platform" });
    return null;
  }
  const others = (procStates((tracerState(APP_NAMES)?.procs ?? []).map((p) => p.pid)) ?? []).filter((p) => p.alive);
  run.measure({ id, holds: others.length === 0, others });
  if (!others.length) return null;
  return `another app instance is alive at the session start (${others.map((p) => `pid ${p.pid} ${p.comm} state ${p.state}`).join("; ")}); not measured`;
};
// Linux, informational (the side finding of the AC-9 sessions): what is left of a session's
// processes after it closed, at 2 s and at 10 s (the window-close check's limit for the app's
// tree), with state, parent and open-file count, and the process tree around them.
const LEFT_PROCS = [...SYNTH_PROCS, "strace"];
const allPids = () => new Set((tracerState(LEFT_PROCS)?.procs ?? []).map((p) => p.pid));
const afterClose = async (label, pids, group = "synth_1m") => {
  if (!linux) return;
  const id = `info.${group}.after_close.${label}`;
  try {
    const at = [];
    let waited = 0;
    for (const t of [2000, 10_000]) {
      await new Promise((r) => setTimeout(r, t - waited));
      waited = t;
      at.push({ ms: t, procs: procStates(pids), ps: psTree(/e2e|WebKit|strace|tauri-driver|Xvfb/) });
    }
    run.measure({ id, pids, at, gate: false });
  } catch (e) {
    run.measure({ id, pids, error: String(e?.message ?? e) });
  }
};
/** Linux: whether this launch's app processes (since `before`) are all untraced (TracerPid 0).
 *  Throws, so a check fails, when none is found or any is traced: never time a traced app. */
const shownUntraced = (before) => {
  const apps = newApps(before);
  if (!apps.length) throw new Error(`no app process of this launch found in /proc; ${NOT_UNTRACED}`);
  const traced = apps.filter((p) => p.tracerPid !== 0);
  if (traced.length) throw new Error(`traced: ${JSON.stringify(traced)}; ${NOT_UNTRACED}`);
  return apps;
};

// Linux, the default fixture in three launches on the same sandbox, in this order (the stream
// owner's ruling B'):
// 1. first run, traced: the suite's first launch on the pristine sandbox, so its first scan,
//    render, git run and scroll pass strace and the write observer. The default scan, the drill
//    into the wide folder, then the timing checks' steps, not timed (steps.*_traced, gate:false).
// 2. untraced: the gated timing.ipc_tree_page_p95 and timing.scroll_p95, the same scan and steps,
//    thresholds, n and no retry, after the trace-state and other-instance preconditions.
// 3. the main session, traced, whose Settings checks save the active window to the sandbox's
//    settings file (which is why both launches above come before it).
// flow.default_scan_settings_match requires all three scans to read the same conditions.
/** A launch of the default fixture alone (no Open or Export list, no synthetic tree). */
const defaultEnv = (untraced) => {
  const e = { ...env, PRUNEDISK_E2E_FOLDERS: a.fixture };
  delete e.PRUNEDISK_E2E_OPEN;
  delete e.PRUNEDISK_E2E_EXPORT;
  delete e.PRUNEDISK_E2E_SYNTH_NODES;
  if (untraced) e[UNTRACED_VAR] = "1";
  else delete e[UNTRACED_VAR];
  return e;
};
/** Open a default-fixture launch on `port` and wait for the chooser. */
async function openDefault(port) {
  const x = await App.open(`http://127.0.0.1:${port}`, a.app);
  await x.s.cmd("POST", "/timeouts", { script: 600_000 });
  await x.s.until("return [...document.querySelectorAll('h2')].some((e) => e.textContent === 'Choose what to scan')", 60_000);
  return x;
}
/** The default scan: Add folders, the git-facts box as the app starts it (it must be on, the
 *  default: never clicked here, so a changed default shows), the scan conditions read, Scan, the
 *  scan complete and the analysis done (the export button enabled, as the main session waits;
 *  matched by "Export plan" alone, since its label is the main session's check, N2), then into
 *  the wide folder. */
async function defaultScan(x) {
  await x.s.clickText("Add folders...");
  await x.s.until("return document.querySelectorAll('ul.chosen li').length === 1", 10_000);
  const gitFacts = await x.s.exec("return document.getElementById('git-facts')?.checked ?? null");
  if (gitFacts !== true) throw new Error(`the git-facts box is ${gitFacts === null ? "missing" : "off"} before Scan, not on as the app starts it; not scanned`);
  const scan = await scanConditions(x);
  await x.s.clickText("Scan");
  await x.s.until("return [...document.querySelectorAll('.bar span')].some((e) => e.textContent.startsWith('Scan complete.'))", 120_000);
  await x.s.until("return [...document.querySelectorAll('.bar button')].some((b) => b.textContent.startsWith('Export plan') && !b.disabled)", 120_000);
  return { scan, rows: await drillIntoWide(x) };
}

let firstScan = null;
if (linux) {
  let procF;
  let appF;
  let before = new Set();
  let beforeAll = new Set();
  let sessionPids = [];
  let totalF = 0;
  try {
    before = appPids();
    beforeAll = allPids();
    // Recorded only: this launch is not timed.
    othersAtStart("info.default.precondition.first_run");
    ({ proc: procF } = await startDriver({ nativeDriver: a["native-driver"], port: 4452, nativePort: 4453, env: defaultEnv(false) }));
    await run.check("launch.default_first_run", async () => {
      appF = await openDefault(4452);
      return { title: await appF.s.cmd("GET", "/title") };
    });
    sessionPids = procsSince(LEFT_PROCS, beforeAll);
    await run.check("trace.default.first_run_traced", async () => {
      const apps = newApps(before);
      if (!apps.length) throw new Error("no app process of this launch found in /proc");
      const untraced = apps.filter((p) => p.tracerPid === 0);
      if (untraced.length) throw new Error(`not traced, so the first run passed the write observer unseen: ${JSON.stringify(untraced)}`);
      return { apps };
    });
    await run.check("flow.default_first_run_scan", async () => {
      if (!appF) throw new Error("the first-run launch did not open");
      const r = await defaultScan(appF);
      firstScan = r.scan;
      totalF = r.rows;
      return r;
    });
    // The timing checks' steps, traced and not timed: they must complete under the write
    // observer; their numbers are information only.
    await run.check("steps.ipc_tree_page_traced", async () => {
      wideShown(totalF);
      const ms = await ipcDefaultSamples(appF, totalF);
      run.measure({ ...stats("ipc_tree_page_traced", ms, THRESHOLDS.ipcP95Ms), gate: false });
      return { n: ms.length, timed: false };
    });
    await run.check("steps.scroll_traced", async () => {
      wideShown(totalF);
      const ms = await scrollDefaultSamples(appF);
      run.measure({ ...stats("scroll_rows_shown_traced", ms, THRESHOLDS.scrollP95Ms), gate: false });
      return { n: ms.length, timed: false };
    });
  } catch (e) {
    run.results.push({ id: "suite.default_first_run", ok: false, ms: 0, detail: String(e?.stack ?? e) });
    console.log(`FAIL suite.default_first_run ${String(e?.message ?? e)}`);
  } finally {
    if (appF) await appF.close().catch(() => {});
    if (procF) stopDriver(procF);
    await afterClose("first_run", sessionPids, "default");
  }
}

let untracedScan = null;
if (linux) {
  let procU;
  let appU;
  let before = new Set();
  let beforeAll = new Set();
  let sessionPids = [];
  let untraced = false;
  let notAlone = null;
  let totalU = 0;
  try {
    before = appPids();
    beforeAll = allPids();
    notAlone = othersAtStart("info.default.precondition.untraced");
    ({ proc: procU } = await startDriver({ nativeDriver: a["native-driver"], port: 4450, nativePort: 4451, env: defaultEnv(true) }));
    await run.check("launch.default_untraced", async () => {
      appU = await openDefault(4450);
      return { title: await appU.s.cmd("GET", "/title") };
    });
    sessionPids = procsSince(LEFT_PROCS, beforeAll);
    await run.check("trace.default.timed_launch_untraced", async () => {
      const apps = shownUntraced(before);
      untraced = true;
      return { apps };
    });
    await run.check("flow.default_untraced_scan", async () => {
      if (!appU) throw new Error("the untraced launch did not open");
      const r = await defaultScan(appU);
      untracedScan = r.scan;
      totalU = r.rows;
      return r;
    });
    await defaultTimings(appU, totalU, () => {
      // The trace state first: a traced app is never timed, whatever else is wrong.
      if (!untraced) throw new Error(`the timing launch is ${NOT_UNTRACED}`);
      if (notAlone) throw new Error(notAlone);
      wideShown(totalU);
    });
  } catch (e) {
    run.results.push({ id: "suite.default_untraced", ok: false, ms: 0, detail: String(e?.stack ?? e) });
    console.log(`FAIL suite.default_untraced ${String(e?.message ?? e)}`);
  } finally {
    if (appU) await appU.close().catch(() => {});
    if (procU) stopDriver(procU);
    await afterClose("untraced", sessionPids, "default");
  }
}

// The main session (traced on Linux).
const beforeAllMain = allPids();
let mainPids = [];
let mainScan = null;
const { proc, base } = await startDriver({ nativeDriver: a["native-driver"], env });
try {
  await run.check("launch", async () => {
    app = await App.open(base, a.app);
    await waitH2("Choose what to scan");
    return { title: await app.s.cmd("GET", "/title") };
  });
  if (!app) throw new Error("the app did not open");
  mainPids = procsSince(LEFT_PROCS, beforeAllMain);

  await axe("axe.chooser", "body");

  await run.check("focus.chooser_tab_order", async () => {
    await app.s.exec("[...document.querySelectorAll('button')].find((b) => b.textContent === 'Add folders...').focus()");
    const seen = [await active()];
    for (let i = 0; i < 2; i++) {
      await app.s.keys(KEY.tab);
      seen.push(await active());
    }
    const want = ["Add folders...", "Open a scan file...", "git-facts"];
    const got = seen.map((e) => e?.id ?? e?.text);
    if (JSON.stringify(got) !== JSON.stringify(want)) throw new Error(`order ${JSON.stringify(got)}, wanted ${JSON.stringify(want)}`);
    return got;
  });

  // Refused IPC (before the fixture is offered, so the root check is the one under test).
  for (const [id, cmd, payload, expect] of [
    // Refused by the capability list itself, not by the command (a granted path command
    // answers "unknown path" for a bad argument: probe run 44686507550, capability variant).
    ["ipc.refused.not_in_capabilities.dialog_open", "plugin:dialog|open", { options: {} }, "not allowed by ACL"],
    ["ipc.refused.not_in_capabilities.path", "plugin:path|resolve_directory", { directory: 10 }, "not allowed by ACL"],
    ["ipc.refused.not_in_capabilities.window_close", "plugin:window|close", { label: "main" }, "not allowed by ACL"],
    ["ipc.refused.unregistered", "delete_everything", {}, "not allowed by ACL"],
    ["ipc.refused.root_not_offered", "start_scan", { roots: [a.fixture], settings: { collect_git_facts: false } }, "was not offered"],
    ["ipc.refused.too_many_roots", "start_scan", { roots: Array.from({ length: 65 }, () => a.fixture), settings: { collect_git_facts: false } }, "at most 64"],
    ["ipc.refused.oversized_root", "start_scan", { roots: [a.fixture + "/x".repeat(1 << 19)], settings: { collect_git_facts: false } }, '"kind":"refused"'],
  ]) {
    await run.check(id, () => refused(cmd, payload, expect));
  }

  await run.check("flow.add_folders", async () => {
    await app.s.clickText("Add folders...");
    await app.s.until("return document.querySelectorAll('ul.chosen li').length === 2", 10_000);
    return app.s.exec("return [...document.querySelectorAll('ul.chosen li')].map((l) => l.firstChild.textContent)");
  });

  await run.check("ipc.refused.subfolder_of_offered_root", () =>
    refused("start_scan", { roots: [path.join(a.fixture, "proj-web")], settings: { collect_git_facts: false } }, "was not offered"),
  );

  await run.check("flow.cancel_mid_scan", async () => {
    await app.s.exec("const b = document.getElementById('git-facts'); if (!b.checked) b.click();");
    await app.s.clickText("Scan");
    await waitH2("Scanning", 30_000);
    const t = Date.now();
    await app.s.clickText("Cancel");
    await waitH2("Choose what to scan", 10_000);
    const ms = Date.now() - t;
    const s = await state();
    if (s.state === "walking" || s.state === "analysing") throw new Error(`still ${s.state}`);
    return { cancelToChooserMs: ms, state: s.state, status: await live() };
  });

  await run.check("flow.rescan_fixture", async () => {
    const big = path.basename(a.big);
    const n = await app.s.exec(
      `const big = arguments[0];
       const li = [...document.querySelectorAll('ul.chosen li')].filter((l) => l.firstChild.textContent.includes(big));
       if (li.length === 1) li[0].querySelector('button').click();
       return li.length;`,
      big,
    );
    if (n !== 1) throw new Error(`${n} chosen entries match ${big}`);
    await app.s.until("return document.querySelectorAll('ul.chosen li').length === 1", 10_000);
    mainScan = await scanConditions(app);
    await app.s.clickText("Scan");
    await app.s.until("return [...document.querySelectorAll('.bar span')].some((e) => e.textContent.startsWith('Scan complete.'))", 120_000);
    await app.s.until(`return [...document.querySelectorAll('.bar button')].some((b) => b.textContent === '${EXPORT_LABEL}' && !b.disabled)`, 120_000);
    return state();
  });
  if (linux) {
    // The first-run, untraced and main scans of the fixture read the same conditions (owner
    // point 3): the chosen folders, the git-facts box and the settings the app reports, read just
    // before each Scan, are the same bytes as JSON.
    await run.check("flow.default_scan_settings_match", async () => {
      const scans = { first_run: firstScan, untraced: untracedScan, main: mainScan };
      const missing = Object.keys(scans).filter((k) => !scans[k]);
      if (missing.length) throw new Error(`no scan settings recorded for: ${missing.join(", ")}, so the scans cannot be compared`);
      const want = JSON.stringify(mainScan);
      const differ = Object.keys(scans).filter((k) => JSON.stringify(scans[k]) !== want);
      if (differ.length) {
        throw new Error(`scanned under other settings than the main session: ${differ.map((k) => `${k} ${JSON.stringify(scans[k])}`).join("; ")}; main ${want}`);
      }
      return { scan: mainScan, bytes: want.length, sessions: Object.keys(scans) };
    });
  }

  // Each view, its axe check, and the tablist's arrow keys.
  for (const [label, ready] of [
    ["Size tree", ".tg-scroll [role=row][aria-rowindex='2']"],
    ["Treemap", "#panel svg, #panel .treemap, #panel [role=img], #panel p"],
    ["Top files", "#panel table.items, #panel p"],
    ["Developer view", "#panel .dev-groups button"],
    ["Settings", "#panel form"],
  ]) {
    await run.check(`view.${label}`, async () => {
      await tab(label);
      await app.s.until("return !!document.querySelector(arguments[0])", 30_000, ready);
      return null;
    });
    await axe(`axe.${label}`, "body");
  }

  await run.check("focus.tablist_arrow_keys", async () => {
    await tab("Size tree");
    await app.s.exec("document.getElementById('tab-tree').focus()");
    await app.s.keys(KEY.right);
    const a1 = await active();
    await app.s.keys(KEY.end);
    const a2 = await active();
    await app.s.keys(KEY.home);
    const a3 = await active();
    const got = [a1?.id, a2?.id, a3?.id];
    if (JSON.stringify(got) !== JSON.stringify(["tab-treemap", "tab-settings", "tab-tree"])) throw new Error(JSON.stringify(got));
    return got;
  });

  // Size tree: into the wide folder (5,000 rows), then the timings' steps.
  let total = 0;
  await run.check("view.tree_drill_into_wide", async () => {
    total = await drillIntoWide(app);
    return { rows: total };
  });
  // Linux: this session is traced, so it times nothing; the timings are taken in the untraced
  // launch before it, and their steps ran traced in the first-run launch. Windows has no tracer:
  // the timings are taken here, as registered.
  if (!linux) await defaultTimings(app, total, () => wideShown(total));

  // Developer view: a class with more than one page, Next and Previous, then another class.
  await run.check("view.dev_paging_and_class_switch", async () => {
    await tab("Developer view");
    await app.s.until("return document.querySelectorAll('#panel .dev-groups button').length > 0", 30_000);
    const groups = await app.s.exec(
      "return [...document.querySelectorAll('#panel .dev-groups button')].map((b) => b.textContent)",
    );
    const count = (t) => Number(t.split(":")[1].trim().split(" ")[0].replace(/[^0-9]/g, ""));
    const big = groups.findIndex((t) => count(t) > 100);
    const other = groups.findIndex((t, i) => i !== big && count(t) > 0);
    if (big < 0) throw new Error(`no class with more than one page: ${JSON.stringify(groups)}`);
    const buttons = await app.s.findAll("#panel .dev-groups button");
    await app.s.click(buttons[big]);
    const pager = () => app.s.exec("return document.querySelector('#panel .pager')?.textContent.trim() ?? ''");
    await app.s.until("return document.querySelector('#panel .pager')?.textContent.trim().startsWith('Previous 1 to 100 of')", 15_000);
    const first = await pager();
    await app.s.clickText("Next", "#panel .pager button");
    await app.s.until("return document.querySelector('#panel .pager')?.textContent.includes(' 101 to ')", 15_000);
    const second = await pager();
    const focusAfterNext = await active();
    await app.s.clickText("Previous", "#panel .pager button");
    await app.s.until("return document.querySelector('#panel .pager')?.textContent.includes(' 1 to 100 ')", 15_000);
    let switched = null;
    if (other >= 0) {
      const again = await app.s.findAll("#panel .dev-groups button");
      await app.s.click(again[other]);
      await app.s.until(
        "const b = document.querySelectorAll('#panel .dev-groups button')[arguments[0]]; return b && b.getAttribute('aria-pressed') === 'true';",
        15_000,
        other,
      );
      switched = await app.s.exec("return document.querySelector('#panel .dev-items h3')?.textContent ?? null");
    }
    if (!["Next", "Previous"].includes(focusAfterNext?.text)) throw new Error(`focus after Next: ${JSON.stringify(focusAfterNext)}`);
    return { first, second, focusAfterNext: focusAfterNext.text, switched };
  });

  // Settings: lengthen and save; shorten and Escape; shorten and Keep; shorten and confirm.
  await run.check("settings.save_confirm_keep_escape", async () => {
    // This check compares the sentence settings.ts writes after Keep; fail at once if the two
    // disagree (a run.mjs that still expects the old wording, or a settings.ts that changed it).
    const settingsPath = new URL("../src/views/settings.ts", import.meta.url);
    const settingsSrc = existsSync(settingsPath) ? readFileSync(settingsPath, "utf8") : null;
    if (settingsSrc !== null && !settingsSrc.includes("Not saved: the active window in the file is still ${file} days; the field keeps your unsaved ${typed}.")) {
      throw new Error("settings.ts no longer writes the Keep sentence this check expects (click-test N6)");
    }
    await tab("Settings");
    await app.s.until("return document.getElementById('set-window')?.value !== ''", 15_000);
    const setWindow = (v) =>
      app.s.exec(
        `const i = document.getElementById('set-window'); i.value = arguments[0]; i.dispatchEvent(new Event('input', { bubbles: true }));`,
        String(v),
      );
    const message = () => app.s.exec("return document.querySelector('#panel .message')?.textContent ?? ''");
    const save = () => app.s.clickText("Save", "#panel form button");
    const steps = {};
    await setWindow(30);
    await save();
    await app.s.until("const t = document.querySelector('#panel .message')?.textContent ?? ''; return t.startsWith('Saved') || t.startsWith('Plan settings saved') ? t : null;", 30_000);
    steps.lengthen = await message();

    await setWindow(10);
    await save();
    await app.s.until("return !!document.querySelector('[role=alertdialog]')", 15_000);
    const onOpen = await active();
    if (!onOpen?.text?.startsWith("Keep 30")) throw new Error(`focus when the question opens: ${JSON.stringify(onOpen)}`);
    await app.s.keys(KEY.escape);
    await app.s.until("return !document.querySelector('[role=alertdialog]')", 10_000);
    steps.escape = await message();
    // Click-test N6: Escape keeps the user's unsaved edit in the field, and the sentence says so.
    steps.fieldAfterEscape = await app.s.exec("return document.getElementById('set-window').value");
    if (steps.fieldAfterEscape !== "10") throw new Error(`the field after Escape: ${steps.fieldAfterEscape}`);
    const afterEscape = await active();
    if (afterEscape?.id !== "set-window") throw new Error(`focus after Escape: ${JSON.stringify(afterEscape)}`);
    let saved = await app.invoke("get_settings");
    if (saved.value?.core?.active_window_days !== 30) throw new Error(`after Escape: ${JSON.stringify(saved)}`);

    await save();
    await app.s.until("return !!document.querySelector('[role=alertdialog]')", 15_000);
    await app.s.clickText("Keep 30 days", "[role=alertdialog] button");
    await app.s.until("return !document.querySelector('[role=alertdialog]')", 10_000);
    steps.keep = await message();

    await save();
    await app.s.until("return !!document.querySelector('[role=alertdialog]')", 15_000);
    await app.s.clickText("Shorten to 10 days", "[role=alertdialog] button");
    await app.s.until("const t = document.querySelector('#panel .message')?.textContent ?? ''; return t.startsWith('Saved') || t.startsWith('Plan settings saved') ? t : null;", 30_000);
    steps.shorten = await message();
    // Click-test N6 wording (settings.ts keptMessage). The old sentence read as if the field went back.
    const kept = "Not saved: the active window in the file is still 30 days; the field keeps your unsaved 10.";
    if (!steps.escape.startsWith(kept) || !steps.keep.startsWith(kept)) throw new Error(JSON.stringify(steps));
    saved = await app.invoke("get_settings");
    if (saved.value?.core?.active_window_days !== 10) throw new Error(`after Shorten: ${JSON.stringify(saved)}`);
    steps.activeWindowDays = 10;
    return steps;
  });

  // The shorten question is modal (PR gate F8): Tab and Shift+Tab never leave it, nothing behind
  // it takes a click, and it leaves nothing inert behind once answered.
  await run.check("settings.confirm_focus_contained", async () => {
    await app.s.exec(
      `const i = document.getElementById('set-window'); i.value = '5'; i.dispatchEvent(new Event('input', { bubbles: true }));`,
    );
    // A spacer so the page behind the question can scroll (removed after the question closes).
    await app.s.exec("const s = document.createElement('div'); s.id = 'e2e-spacer'; s.style.height = '4000px'; document.getElementById('panel').append(s);");
    await app.s.clickText("Save", "#panel form button");
    await app.s.until("return !!document.querySelector('[role=alertdialog]')", 15_000);
    const inside = () => app.s.exec("const d = document.querySelector('[role=alertdialog]'); return !!d && d.contains(document.activeElement);");
    const seen = [];
    for (let i = 0; i < 6; i++) {
      await app.s.keys(KEY.tab);
      seen.push((await active())?.text ?? null);
      if (!(await inside())) throw new Error(`Tab ${i + 1} left the question: ${JSON.stringify(seen)}`);
    }
    for (let i = 0; i < 6; i++) {
      await app.s.chord(KEY.shift, KEY.tab);
      seen.push((await active())?.text ?? null);
      if (!(await inside())) throw new Error(`Shift+Tab ${i + 1} left the question: ${JSON.stringify(seen)}`);
    }
    // Click-test N1: focus is always on one of the question's buttons (never the dialog box or
    // nowhere), and nothing behind it scrolls: many Shift+Tab and Tab, scroll keys on a button,
    // focus lost to nowhere, and the wheel. A tall spacer makes sure the page behind can scroll.
    const onButton = () =>
      app.s.exec("const d = document.querySelector('[role=alertdialog]'); const e = document.activeElement; return !!d && !!e && e.tagName === 'BUTTON' && d.contains(e);");
    const scroll = () =>
      app.s.exec(
        "const s = document.scrollingElement; return [s.scrollTop, s.scrollLeft, ...[...document.querySelectorAll('body *')].filter((e) => e.scrollHeight > e.clientHeight + 1).map((e) => e.scrollTop)].join(',');",
      );
    const canScroll = await app.s.exec("const s = document.scrollingElement; return s.scrollHeight > s.clientHeight + 50;");
    if (!canScroll) throw new Error("precondition: the page behind the question cannot scroll, so the scroll checks would prove nothing");
    const still = await scroll();
    for (let i = 0; i < 16; i++) {
      await app.s.chord(KEY.shift, KEY.tab);
      if (!(await onButton())) throw new Error(`Shift+Tab ${i + 1} (long run) left the buttons: ${JSON.stringify(await active())}`);
    }
    for (let i = 0; i < 16; i++) {
      await app.s.keys(KEY.tab);
      if (!(await onButton())) throw new Error(`Tab ${i + 1} (long run) left the buttons: ${JSON.stringify(await active())}`);
    }
    for (const [name, k] of [["Down", KEY.down], ["Up", KEY.up], ["End", KEY.end], ["Home", KEY.home], ["Page Down", "\uE00F"], ["Right", KEY.right], ["Left", KEY.left]]) {
      await app.s.keys(k);
      if (!(await app.s.exec("return !!document.querySelector('[role=alertdialog]')"))) throw new Error(`${name} closed the question`);
      if (!(await onButton())) throw new Error(`${name} moved focus off the buttons`);
      const now = await scroll();
      if (now !== still) throw new Error(`${name} scrolled the page behind the question: ${still} -> ${now}`);
    }
    await app.s.exec("document.activeElement?.blur();");
    await new Promise((r) => setTimeout(r, 200));
    if (!(await onButton())) throw new Error(`focus lost to nowhere did not come back: ${JSON.stringify(await active())}`);
    let wheel = "scrolled nothing";
    try {
      await app.s.cmd("POST", "/actions", {
        actions: [{ type: "wheel", id: "wheel", actions: [{ type: "scroll", x: 20, y: 20, deltaX: 0, deltaY: 800, origin: "viewport" }] }],
      });
    } catch (e) {
      wheel = `not supported by this driver: ${String(e?.message ?? e).slice(0, 120)}`;
    }
    await new Promise((r) => setTimeout(r, 300));
    if ((await scroll()) !== still) throw new Error(`the wheel scrolled the page behind the question`);
    // A click on New scan behind the question must do nothing (it is inert).
    const before = await state();
    try {
      await app.s.clickText("New scan");
    } catch {
      // A driver may refuse to click an element it cannot reach; either way nothing happens.
    }
    await new Promise((r) => setTimeout(r, 500));
    const after = await state();
    if (!(await app.s.exec("return !!document.querySelector('[role=alertdialog]')"))) throw new Error("the question closed on a click behind it");
    if (after.view !== before.view || after.state !== before.state) throw new Error(`a click behind the question acted: ${JSON.stringify(after)}`);
    if (!(await inside())) throw new Error("focus left the question after a click behind it");
    await app.s.keys(KEY.escape);
    await app.s.until("return !document.querySelector('[role=alertdialog]')", 10_000);
    const leftInert = await app.s.exec("return document.querySelectorAll('[inert]').length");
    if (leftInert !== 0) throw new Error(`${leftInert} elements left inert`);
    const overflow = await app.s.exec("return [document.documentElement.style.overflow, document.body.style.overflow].join(',')");
    if (overflow !== ",") throw new Error(`the page's scroll lock was left on: ${overflow}`);
    await app.s.exec("document.getElementById('e2e-spacer')?.remove();");
    const saved = await app.invoke("get_settings");
    if (saved.value?.core?.active_window_days !== 10) throw new Error(`after Escape: ${JSON.stringify(saved)}`);
    return { tabCycle: seen, wheel };
  });

  await run.check("flow.export_plan", async () => {
    // Click-test N2: Export picks a folder and the app names the file, so no dialog can offer to
    // replace one. Every stamped name the export can use in the next two minutes is taken
    // beforehand by a planted file the user owns: the app must write the "-2" name beside it and
    // leave every planted file as it was.
    mkdirSync(a.export, { recursive: true });
    if (readdirSync(a.export).length) throw new Error(`${a.export} is not empty before the export`);
    const planted = new Map();
    const t0 = Date.now();
    for (let s = -5; s <= 120; s++) {
      const name = `prunedisk-plan-${stampOf(t0 + s * 1000)}.json`;
      const bytes = `planted by the end-to-end suite: ${name}\n`;
      writeFileSync(path.join(a.export, name), bytes, { flag: "wx" });
      planted.set(name, bytes);
    }
    await app.s.clickText(EXPORT_LABEL);
    await app.s.until("return document.querySelector('p.live')?.textContent.startsWith('Plan exported to ')", 30_000);
    const status = await live();
    // CT-c: the confirmation names the file and says nothing else changed.
    const m = /^Plan exported to (.+)\. Nothing else was changed\.$/.exec(status);
    if (!m) throw new Error(`export said: ${status}`);
    const written = m[1];
    const same = (x, y) => (process.platform === "win32" ? x.toLowerCase() === y.toLowerCase() : x === y);
    if (!same(path.resolve(path.dirname(written)), path.resolve(a.export))) throw new Error(`exported outside the picked folder: ${written}`);
    if (!/^prunedisk-plan-\d{8}-\d{6}-2\.json$/.test(path.basename(written))) throw new Error(`a planted name was not skipped: ${written}`);
    const text = readFileSync(written, "utf8");
    const plan = JSON.parse(text);
    for (const [name, bytes] of planted) {
      if (readFileSync(path.join(a.export, name), "utf8") !== bytes) throw new Error(`the export changed ${name}`);
    }
    const now = readdirSync(a.export).sort();
    const want = [...planted.keys(), path.basename(written)].sort();
    if (JSON.stringify(now) !== JSON.stringify(want)) throw new Error(`the export folder holds ${JSON.stringify(now)}`);
    // CT-F6, first sequence: the confirmation is gone on the chooser after New scan. (No
    // clearing of the status from here: that hid this defect.)
    await app.s.clickText("New scan");
    await waitH2("Choose what to scan");
    const onChooser = await live();
    if (onChooser !== "") throw new Error(`after New scan the chooser shows: ${onChooser}`);
    return { status, written, planted: planted.size, onChooser, bytes: text.length, keys: Object.keys(plan).slice(0, 12) };
  });

  await run.check("flow.open_valid_scan_file", async () => {
    await app.s.clickText("Open a scan file...");
    await app.s.until("return [...document.querySelectorAll('.bar span')].some((e) => e.textContent.startsWith('Imported scan file.'))", 60_000);
    // CT-F6: "Reading the scan file..." ends with the read.
    const afterImport = await live();
    if (afterImport !== "") throw new Error(`after the import the status says: ${afterImport}`);
    const tabs = await app.s.exec("return [...document.querySelectorAll('[role=tab]')].map((t) => t.textContent)");
    if (tabs.includes("Size tree")) throw new Error(`an imported scan shows ${JSON.stringify(tabs)}`);
    // The export folder list is used up: this second export is a cancel, writes nothing and
    // says nothing.
    const before = readdirSync(a.export).length;
    await app.s.clickText(EXPORT_LABEL);
    await new Promise((r) => setTimeout(r, 1500));
    const second = await live();
    if (second !== "") throw new Error(`a cancelled export said: ${second}`);
    if (readdirSync(a.export).length !== before) throw new Error("a cancelled export wrote a file");
    return { tabs, afterImport, second };
  });
  await axe("axe.imported_developer_view", "body");

  await run.check("flow.open_malformed_scan_file", async () => {
    await app.s.clickText("New scan");
    await waitH2("Choose what to scan");
    // CT-F6, second sequence: after an import, New scan and Add folders, the chooser shows no
    // status from the import.
    await app.s.clickText("Add folders...");
    await new Promise((r) => setTimeout(r, 1000));
    const afterAdd = await live();
    if (afterAdd !== "") throw new Error(`after import, New scan and Add folders the status says: ${afterAdd}`);
    await app.s.clickText("Open a scan file...");
    const msg = await app.s.until("const t = document.querySelector('p.live')?.textContent ?? ''; return t.startsWith('Cannot open that scan file:') ? t : null;", 30_000);
    const focus = await active();
    if (focus?.text !== "Open a scan file...") throw new Error(`focus after the error: ${JSON.stringify(focus)}`);
    const s = await state();
    return { msg, focus: focus.text, state: s.state };
  });

  await run.check("flow.open_cancelled", async () => {
    await app.s.clickText("Open a scan file...");
    await new Promise((r) => setTimeout(r, 1500));
    const h = await h2();
    if (h !== "Choose what to scan") throw new Error(`after a cancelled open: ${h}`);
    return { h2: h, status: await live() };
  });

  await run.check("csp.inline_script_blocked", async () => {
    const r = await app.s.execAsync(
      `const done = arguments[0];
       const s = document.createElement("script");
       window.__probe = 0;
       s.textContent = "window.__probe = 1";
       document.head.append(s);
       setTimeout(() => done({ inlineRan: window.__probe === 1 }), 200);`,
      5000,
    );
    if (r.inlineRan) throw new Error("inline script ran");
    return r;
  });
} catch (e) {
  run.results.push({ id: "suite", ok: false, ms: 0, detail: String(e?.stack ?? e) });
  console.log(`FAIL suite ${String(e?.message ?? e)}`);
} finally {
  if (app) await app.close();
  stopDriver(proc);
  await afterClose("traced", mainPids, "default");
}

// Spec AC-9 at one million rows (PR gate F9), as registered in WAL w8d2cd55d39e (built to a
// quoted excerpt of spec v6 AC-9; the spec text was not re-read). A second app session, started
// with PRUNEDISK_E2E_SYNTH_NODES so the e2e build opens on a size tree of one folder holding
// 1,000,000 files (root expanded: 1,000,001 rows). Its own tauri-driver, because the app takes
// its environment from the driver, on its own ports so nothing of the first session is in the
// way. Steps, boundary and verdict: e2e/ac9.mjs, shared with the Playwright check (perf/).
//
// Linux, as ruled after the no-strace controls (docs/DESKTOP.md; registered in WAL w3bd2a24a6c6
// and wad2689e80fc before its first run): the running-app scroll (2) is timed in an untraced
// launch (PRUNEDISK_E2E_UNTRACED=1, which e2e/check.sh's wrapper honours for an e2e build only;
// this session checks the app is not traced). Threshold, n and no retry are unchanged. Since
// main's later ruling every timing assertion is taken untraced, the IPC gate (1) included. A
// second, traced launch then runs the same IPC pages and scroll steps, not timed: they must
// complete within the DOM limit, under the write observer. Windows has no tracer: one session,
// as registered.
const synthEnv = (untraced) => {
  const e = { ...env, PRUNEDISK_E2E_SYNTH_NODES: String(AC9.nodes) };
  if (untraced) e[UNTRACED_VAR] = "1";
  else delete e[UNTRACED_VAR];
  return e;
};
/** (1)'s pages in the page: offset k*200 limit 200, 10 warm-up pages, then k = 0..99 timed. */
const ipcPages = async (app) => {
  const r = await app.invoke("current_state");
  if (!r.ok) throw new Error(`current_state: ${r.error}`);
  const out = await app.s.execAsync(
    `const [view, warm, n, limit, done] = arguments;
     (async () => {
       const inv = window.__TAURI_INTERNALS__.invoke;
       for (let k = 0; k < warm; k++) await inv("tree_page", { view, offset: k * limit, limit });
       const ms = [];
       for (let k = 0; k < n; k++) {
         const t = performance.now();
         const p = await inv("tree_page", { view, offset: k * limit, limit });
         ms.push(performance.now() - t);
         if (p.rows.length !== limit || p.offset !== k * limit) throw new Error("page " + k + ": " + p.rows.length + " rows at " + p.offset);
       }
       return ms;
     })().then(done, (e) => done({ error: String(e) }));`,
    300_000,
    r.value.view,
    AC9.ipcWarmup,
    AC9.ipcSamples,
    AC9.pageLimit,
  );
  if (!Array.isArray(out)) throw new Error(JSON.stringify(out));
  if (out.length !== AC9.ipcSamples) throw new Error(`${out.length} samples, wanted ${AC9.ipcSamples}`);
  return out;
};
const runSteps = (app) =>
  app.s.execAsync(
    `const [o, done] = arguments;
     (${measureScroll.toString()})(o).then(done, (e) => done({ error: String(e), samples: [] }));`,
    600_000,
    scrollOptions(),
  );

{
  const rows = AC9.nodes + 1;
  let proc2;
  let app2;
  let before = new Set();
  let beforeAll = new Set();
  let sessionPids = [];
  try {
    await new Promise((r) => setTimeout(r, 2000));
    before = appPids();
    beforeAll = allPids();
    // Precondition (Linux; WAL w4b6f0f8e4fa): no other app instance alive at the session's start,
    // or none of its timings is measured.
    const notAlone = othersAtStart("info.synth_1m.precondition.untraced");
    ({ proc: proc2 } = await startDriver({
      nativeDriver: a["native-driver"],
      port: 4446,
      nativePort: 4447,
      env: synthEnv(linux),
    }));
    let shown = 0;
    await run.check("launch.synth_1m", async () => {
      app2 = await App.open("http://127.0.0.1:4446", a.app);
      // The drivers' default script timeout is 30 s (variant run 44877270665 hit it); a slow
      // run must end in its measured p95, not in a timeout.
      await app2.s.cmd("POST", "/timeouts", { script: 600_000 });
      shown = await app2.s.until(
        "const s = document.querySelector('.tg-scroll'); return s && document.getElementById('tg-row-0') ? Number(s.getAttribute('aria-rowcount')) - 1 : 0;",
        120_000,
      );
      if (shown !== rows) throw new Error(`${shown} rows shown, wanted ${rows}; not measured`);
      const tab = await app2.s.exec("return document.querySelector('[role=tab][aria-selected=true]')?.textContent ?? null");
      return { rows: shown, tab };
    });
    if (linux) sessionPids = procsSince(LEFT_PROCS, beforeAll);
    // Linux: the timed launch must not be traced (the app refuses the tree if it is, and this
    // reads its TracerPid from /proc).
    let untraced = !linux;
    if (linux) {
      await run.check("trace.synth_1m.timed_launch_untraced", async () => {
        const apps = shownUntraced(before);
        untraced = true;
        return { apps };
      });
    }
    const ready = () => {
      // The trace state first: a traced app is never timed, whatever else is wrong.
      if (!untraced) throw new Error(`the timed launch is ${NOT_UNTRACED}`);
      if (notAlone) throw new Error(notAlone);
      if (shown !== rows) throw new Error(`the one-million-row tree is not shown (${shown} rows); not measured`);
    };
    // Informational, never a pass gate: a probe that fails is recorded, not counted as a failure.
    const info = async (id, f) => {
      try {
        run.measure({ id, ...(await f()) });
      } catch (e) {
        run.measure({ id, error: String(e?.message ?? e) });
      }
    };
    const start = snapshot();
    await info("info.synth_1m.runner", async () => ({ machine: machine(), start }));

    // (1) IPC, Linux, in the registered order ahead of the steps: tree_page offset k*200 limit
    // 200, 10 warm-up pages, then k = 0..99. Timed here, untraced (it was judged in the traced
    // launch until every timing moved out of it); the traced launch below runs the same pages.
    if (linux) {
      await run.check("timing.synth_1m.ipc_tree_page_p95", async () => {
        ready();
        const out = await ipcPages(app2);
        const m = { ...judge("synth_1m_ipc_tree_page", out, AC9.ipcP95Ms), rows, pageLimit: AC9.pageLimit };
        run.measure(m);
        if (judge(m.id, out, 0).pass) throw new Error("control: a 0 ms threshold passed these samples");
        if (!m.pass) throw new Error(`p95 ${m.p95.toFixed(1)} ms, not under ${AC9.ipcP95Ms} ms`);
        return m;
      });
    }

    // (2) Running-app scroll, Linux (untraced) and Windows: 10 warm-up steps, then 200 steps of 10
    // rows.
    await run.check("timing.synth_1m.scroll_p95", async () => {
      ready();
      const r = await runSteps(app2);
      if (r && Array.isArray(r.samples) && r.samples.length) run.measure({ ...judge("synth_1m_scroll_partial", r.samples, AC9.scrollP95Ms), partial: !!r.error });
      const m = { ...scrollVerdict("synth_1m_scroll_rows_shown", r), rows };
      // The raw samples, in step order, so drift within a run shows.
      run.measure({ ...m, samples: r.samples });
      if (!m.pass) throw new Error(`p95 ${m.p95.toFixed(1)} ms, not under ${AC9.scrollP95Ms} ms`);
      return m;
    });

    // The empty-frame floor (informational). After the gated steps: at 0588a6e it ran just before
    // them, and the gated Linux samples moved on both runner classes (docs/DESKTOP.md).
    await info("info.synth_1m.frame_floor", async () => {
      ready();
      const r = await app2.s.execAsync(
        `const [o, done] = arguments;
         (${measureFloor.toString()})(o).then(done, (e) => done({ error: String(e) }));`,
        120_000,
        { warmup: AC9.scrollWarmup, samples: AC9.scrollSamples },
      );
      if (r.error) throw new Error(r.error);
      const f = judge("frame_floor", r.samples, AC9.scrollP95Ms);
      const iv = r.intervals.length ? { n: r.intervals.length, p50: rank(r.intervals, 0.5), p95: rank(r.intervals, 0.95), max: Math.max(...r.intervals) } : null;
      return { n: f.n, p50: f.p50, p95: f.p95, max: f.max, rafInterval: iv, screen: r.screen, scrollerHeight: r.scrollerHeight, gate: false };
    });

    // The same steps again, split into slices (informational; after the gated samples, so the
    // instrumentation never reaches them).
    await info("info.synth_1m.scroll_profile", async () => {
      ready();
      const r = await app2.s.execAsync(
        `const [o, done] = arguments;
         (${profileScroll.toString()})(o).then(done, (e) => done({ error: String(e), steps: [] }));`,
        600_000,
        scrollOptions(),
      );
      return { error: r.error ?? null, slices: summarizeProfile(r.steps), steps: r.steps, gate: false };
    });
    await info("info.synth_1m.runner_end", async () => {
      const end = snapshot();
      return { end, cpuShares: cpuShares(start, end) };
    });
  } catch (e) {
    run.results.push({ id: "suite.synth_1m", ok: false, ms: 0, detail: String(e?.stack ?? e) });
    console.log(`FAIL suite.synth_1m ${String(e?.message ?? e)}`);
  } finally {
    if (app2) await app2.close().catch(() => {});
    if (proc2) stopDriver(proc2);
    await afterClose("untraced", sessionPids);
  }
}

// Linux: the traced launch. The IPC pages of (1), then the same scroll steps as (2), neither
// timed: they must complete within the DOM limit, and the launch must leave its strace log
// and pass the write observer (e2e/check.sh).
if (linux) {
  const rows = AC9.nodes + 1;
  let proc3;
  let app3;
  let before = new Set();
  let beforeAll = new Set();
  let sessionPids = [];
  try {
    await new Promise((r) => setTimeout(r, 2000));
    before = appPids();
    beforeAll = allPids();
    // Recorded only: this launch is not timed, so the other-instance precondition does not gate it.
    othersAtStart("info.synth_1m.precondition.traced");
    ({ proc: proc3 } = await startDriver({ nativeDriver: a["native-driver"], port: 4448, nativePort: 4449, env: synthEnv(false) }));
    let shown = 0;
    await run.check("launch.synth_1m_traced", async () => {
      app3 = await App.open("http://127.0.0.1:4448", a.app);
      await app3.s.cmd("POST", "/timeouts", { script: 600_000 });
      shown = await app3.s.until(
        "const s = document.querySelector('.tg-scroll'); return s && document.getElementById('tg-row-0') ? Number(s.getAttribute('aria-rowcount')) - 1 : 0;",
        120_000,
      );
      if (shown !== rows) throw new Error(`${shown} rows shown, wanted ${rows}`);
      const apps = newApps(before);
      if (!apps.length) throw new Error("no app process of this launch found in /proc");
      const untraced = apps.filter((p) => p.tracerPid === 0);
      if (untraced.length) throw new Error(`not traced: ${JSON.stringify(untraced)}`);
      return { rows: shown, apps };
    });
    sessionPids = procsSince(LEFT_PROCS, beforeAll);
    const ready = () => {
      if (shown !== rows) throw new Error(`the one-million-row tree is not shown (${shown} rows); not measured`);
    };

    // (1)'s pages under the write observer, not timed: they must all come back, each with 200
    // rows at its offset; the numbers are information only.
    await run.check("steps.synth_1m.traced_ipc", async () => {
      ready();
      const out = await ipcPages(app3);
      run.measure({ ...judge("synth_1m_ipc_tree_page_traced", out, AC9.ipcP95Ms), rows, pageLimit: AC9.pageLimit, gate: false });
      return { pages: out.length, timed: false };
    });

    // (2)'s steps under the write observer, not timed.
    await run.check("steps.synth_1m.traced_scroll", async () => {
      ready();
      const r = await runSteps(app3);
      if (!r || !Array.isArray(r.samples)) throw new Error(`no result: ${JSON.stringify(r).slice(0, 300)}`);
      if (r.error) throw new Error(r.error);
      if (r.samples.length !== AC9.scrollSamples) throw new Error(`${r.samples.length} steps, wanted ${AC9.scrollSamples}`);
      return { steps: r.steps, rowsMax: r.rowsMax, rowsMin: r.rowsMin, maxDomRows: AC9.maxDomRows, timed: false };
    });
  } catch (e) {
    run.results.push({ id: "suite.synth_1m_traced", ok: false, ms: 0, detail: String(e?.stack ?? e) });
    console.log(`FAIL suite.synth_1m_traced ${String(e?.message ?? e)}`);
  } finally {
    if (app3) await app3.close().catch(() => {});
    if (proc3) stopDriver(proc3);
    await afterClose("traced", sessionPids);
  }
}

// AC-9 investigation of the Linux running-app scroll (docs/DESKTOP.md), registered before any
// sample in WAL w07983c42191 and wf5f38e99704. Opt-in (AC9_INVESTIGATE=1, Linux only) and never
// a gate: everything goes to run.measure, nothing to a check. The gated sessions above are not
// changed. Six more one-million-row sessions, each a fresh launch, with and without the strace
// wrapper (e2e/check.sh skips it for PRUNEDISK_E2E_UNTRACED=1):
// - steps: floor windows at the start, middle and end, around the scroll steps;
// - idle: the app with the tree shown, no steps, the floor loop alone for the same time;
// - blank: the same launch, navigated to about:blank (no app code), the floor loop likewise.
if (linux && process.env.AC9_INVESTIGATE === "1") {
  const { stepsWithFloors, floorFor, stat, windows, blockMedians } = await import("./ac9x.mjs");
  const NAMES = SYNTH_PROCS;
  const WINDOW = 60;
  const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
  const livePids = () => (tracerState(NAMES)?.procs ?? []).filter((p) => p.name === "e2e").map((p) => p.pid);
  const durations = {};
  const session = async (arm, untraced, body) => {
    const id = `info.ac9x.${arm}.${untraced ? "untraced" : "traced"}`;
    let proc;
    let app;
    try {
      // The previous session's app must be gone, or its trace state would be read for this one.
      for (let i = 0; i < 30 && livePids().length; i++) await sleep(500);
      const leftover = new Set(livePids());
      // Precondition: no other app instance alive at the session's start, or the session is
      // invalid, reported and not measured (an extra instance on the display can slow frames).
      if (leftover.size) {
        run.measure({ id, valid: false, untraced, leftover: procStates([...leftover]), error: "another app instance is alive at the session start; not measured" });
        return;
      }
      await sleep(2000);
      ({ proc } = await startDriver({ nativeDriver: a["native-driver"], port: 4448, nativePort: 4449, env: synthEnv(untraced) }));
      app = await App.open("http://127.0.0.1:4448", a.app);
      await app.s.cmd("POST", "/timeouts", { script: 600_000 });
      const shown = await app.s.until(
        "const s = document.querySelector('.tg-scroll'); return s && document.getElementById('tg-row-0') ? Number(s.getAttribute('aria-rowcount')) - 1 : 0;",
        120_000,
      );
      if (shown !== AC9.nodes + 1) throw new Error(`${shown} rows shown, wanted ${AC9.nodes + 1}`);
      const tracer = tracerState(NAMES);
      const mine = (tracer?.procs ?? []).filter((p) => p.name === "e2e" && !leftover.has(p.pid));
      const valid = mine.length > 0 && mine.every((p) => (untraced ? p.tracerPid === 0 : p.tracerPid > 0));
      if (!valid) {
        run.measure({ id, valid: false, untraced, tracer, leftover: [...leftover], error: "the app's trace state does not match the arm; not measured" });
        return;
      }
      const t = Date.now();
      const out = await body(app);
      run.measure({ id, valid: true, untraced, arm, tracer, leftover: [...leftover], wallMs: Date.now() - t, gate: false, ...out });
    } catch (err) {
      run.measure({ id, valid: false, untraced, arm, error: String(err?.message ?? err) });
    } finally {
      if (app) await app.close().catch(() => {});
      if (proc) stopDriver(proc);
    }
  };
  const floorSummary = (r) => ({
    elapsedMs: r.elapsedMs,
    href: r.href,
    elements: r.elements,
    floor: windows(r.samples, WINDOW),
    raf: windows(r.intervals, WINDOW),
    all: stat(r.samples),
    samples: r.samples,
    at: r.at,
  });
  const steps = (untraced) =>
    session("steps", untraced, async (app) => {
      const r = await app.s.execAsync(
        `const [o, done] = arguments;
         const measureScroll = ${measureScroll.toString()};
         const measureFloor = ${measureFloor.toString()};
         (${stepsWithFloors.toString()})(o, measureScroll, measureFloor).then(done, (e) => done({ error: String(e), samples: [] }));`,
        600_000,
        { scroll: scrollOptions(), half: AC9.scrollSamples / 2, window: WINDOW, floorWarmup: AC9.scrollWarmup },
      );
      if (r.error) throw new Error(r.error);
      durations[untraced] = r.elapsedMs;
      const w = (x) => ({ floor: stat(x.samples), raf: stat(x.intervals) });
      return {
        elapsedMs: r.elapsedMs,
        windows: { start: w(r.start), mid: w(r.mid), end: w(r.end) },
        scroll: stat(r.samples),
        scrollPerBlock50: blockMedians(r.samples, 50),
        rowsMax: r.rowsMax,
        samples: r.samples,
        floorSamples: { start: r.start.samples, mid: r.mid.samples, end: r.end.samples },
      };
    });
  const floorOnly = (arm, untraced, before) =>
    session(arm, untraced, async (app) => {
      const pre = before ? await before(app) : {};
      const durationMs = durations[untraced] ?? 20_000;
      const r = await app.s.execAsync(
        `const [o, done] = arguments;
         (${floorFor.toString()})(o).then(done, (e) => done({ error: String(e) }));`,
        300_000,
        { warmup: AC9.scrollWarmup, durationMs: Math.min(durationMs, 120_000), maxSamples: 20_000 },
      );
      if (r.error) throw new Error(r.error);
      return { ...pre, matchedToMs: durationMs, matchedFrom: durations[untraced] ? `steps.${untraced ? "untraced" : "traced"}` : "fallback 20 s", ...floorSummary(r) };
    });
  // The blank page: the same launch, then about:blank, so no app code runs in the page.
  const blank = async (app) => {
    let how = "navigate about:blank";
    try {
      await app.s.cmd("POST", "/url", { url: "about:blank" });
    } catch (e) {
      how = `document.open replacement (navigation failed: ${String(e?.message ?? e).slice(0, 200)}; the app's timers and window listeners may remain)`;
      await app.s.exec("document.open(); document.write('<!doctype html><html><head></head><body></body></html>'); document.close(); return true;");
    }
    const page = await app.s.exec("return { href: location.href, elements: document.getElementsByTagName('*').length, grid: !!document.querySelector('.tg-scroll'), tauri: typeof window.__TAURI_INTERNALS__ };");
    if (page.grid) throw new Error("the grid is still in the page; not a blank page");
    return { blank: { how, page } };
  };
  await steps(false);
  await steps(true);
  await floorOnly("idle", false);
  await floorOnly("idle", true);
  await floorOnly("blank", false, blank);
  await floorOnly("blank", true, blank);
}
process.exitCode = run.write(a.out ?? "e2e-result.json") ? 1 : 0;
