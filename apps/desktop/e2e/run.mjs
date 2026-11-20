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
//          --export <path the save dialog names> --out <result.json>

import { existsSync, readFileSync } from "node:fs";
import path from "node:path";
import { App, Run, args, startDriver, stopDriver } from "./driver.mjs";
import { KEY } from "./webdriver.mjs";

// Registered before the first measurement (WAL w0ce4fd7355e): p95 by nearest rank.
const THRESHOLDS = { ipcP95Ms: 50, scrollP95Ms: 50, samples: 200, warmup: 10 };
const ROW_HEIGHT = 28;

const a = args(process.argv.slice(2));
for (const k of ["app", "fixture", "big", "valid", "malformed", "export"]) {
  if (!a[k]) throw new Error(`--${k} is required`);
}
const run = new Run("e2e");
const env = {
  ...process.env,
  PRUNEDISK_E2E_FOLDERS: [a.fixture, a.big].join(path.delimiter),
  // Valid, then malformed, then the list is used up: the third Open is a cancel.
  PRUNEDISK_E2E_OPEN: [a.valid, a.malformed].join(path.delimiter),
  // One export; the second is a cancel.
  PRUNEDISK_E2E_EXPORT: a.export,
};

/** Nearest-rank percentile: the smallest sample with at least `q` of the samples at or below it. */
function rank(xs, q) {
  const s = [...xs].sort((x, y) => x - y);
  return s[Math.ceil(q * s.length) - 1];
}

const { proc, base } = await startDriver({ nativeDriver: a["native-driver"], env });
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

try {
  await run.check("launch", async () => {
    app = await App.open(base, a.app);
    await waitH2("Choose what to scan");
    return { title: await app.s.cmd("GET", "/title") };
  });
  if (!app) throw new Error("the app did not open");

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
    await app.s.clickText("Scan");
    await app.s.until("return [...document.querySelectorAll('.bar span')].some((e) => e.textContent.startsWith('Scan complete.'))", 120_000);
    await app.s.until("return [...document.querySelectorAll('.bar button')].some((b) => b.textContent === 'Export plan...' && !b.disabled)", 120_000);
    return state();
  });

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

  // Size tree: into the wide folder (5,000 rows), then the timings.
  let total = 0;
  await run.check("view.tree_drill_into_wide", async () => {
    await tab("Size tree");
    // Rows are re-rendered as pages arrive, so a row found a moment ago can be gone when the
    // pointer reaches it (run 44688993733: "stale element reference"). The driver refuses
    // such an action before it clicks, so trying again cannot click twice.
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
    await again(async () => app.s.dblclick(await app.s.find(".tg-scroll [role=row][aria-rowindex='2']")));
    await app.s.until("return document.querySelectorAll(\"nav[aria-label='Folder path'] button\").length >= 2", 15_000);
    await again(async () => app.s.dblclick(await app.s.findText(".tg-scroll [role=row] .name", "wide")));
    total = await app.s.until(
      "const n = Number(document.querySelector('.tg-scroll').getAttribute('aria-rowcount')) - 1; return n >= 1000 ? n : 0;",
      30_000,
    );
    return { rows: total };
  });

  // The timings mean something only over the wide folder (probe run 44686507550 measured a
  // short tree when the drill failed).
  const wideShown = () => {
    if (total < 1000) throw new Error(`the wide folder is not shown (${total} rows); not measured`);
  };
  await run.check("timing.ipc_tree_page_p95", async () => {
    wideShown();
    const view = (await state()).view;
    const ms = await app.s.execAsync(
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
      view,
      total,
      THRESHOLDS.warmup,
      THRESHOLDS.samples,
    );
    if (!Array.isArray(ms)) throw new Error(JSON.stringify(ms));
    const m = { id: "ipc_tree_page", n: ms.length, p50: rank(ms, 0.5), p95: rank(ms, 0.95), max: Math.max(...ms), thresholdMs: THRESHOLDS.ipcP95Ms };
    run.measure(m);
    if (m.p95 > THRESHOLDS.ipcP95Ms) throw new Error(`p95 ${m.p95.toFixed(1)} ms > ${THRESHOLDS.ipcP95Ms} ms`);
    return m;
  });

  await run.check("timing.scroll_p95", async () => {
    wideShown();
    const ms = await app.s.execAsync(
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
    const m = { id: "scroll_rows_shown", n: ms.length, p50: rank(ms, 0.5), p95: rank(ms, 0.95), max: Math.max(...ms), thresholdMs: THRESHOLDS.scrollP95Ms };
    run.measure(m);
    if (m.p95 > THRESHOLDS.scrollP95Ms) throw new Error(`p95 ${m.p95.toFixed(1)} ms > ${THRESHOLDS.scrollP95Ms} ms`);
    return m;
  });

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
    await app.s.until("return !!document.querySelector('#panel [role=alertdialog]')", 15_000);
    const onOpen = await active();
    if (!onOpen?.text?.startsWith("Keep 30")) throw new Error(`focus when the question opens: ${JSON.stringify(onOpen)}`);
    await app.s.keys(KEY.escape);
    await app.s.until("return !document.querySelector('#panel [role=alertdialog]')", 10_000);
    steps.escape = await message();
    const afterEscape = await active();
    if (afterEscape?.id !== "set-window") throw new Error(`focus after Escape: ${JSON.stringify(afterEscape)}`);
    let saved = await app.invoke("get_settings");
    if (saved.value?.core?.active_window_days !== 30) throw new Error(`after Escape: ${JSON.stringify(saved)}`);

    await save();
    await app.s.until("return !!document.querySelector('#panel [role=alertdialog]')", 15_000);
    await app.s.clickText("Keep 30 days", "#panel [role=alertdialog] button");
    await app.s.until("return !document.querySelector('#panel [role=alertdialog]')", 10_000);
    steps.keep = await message();

    await save();
    await app.s.until("return !!document.querySelector('#panel [role=alertdialog]')", 15_000);
    await app.s.clickText("Shorten to 10 days", "#panel [role=alertdialog] button");
    await app.s.until("const t = document.querySelector('#panel .message')?.textContent ?? ''; return t.startsWith('Saved') || t.startsWith('Plan settings saved') ? t : null;", 30_000);
    steps.shorten = await message();
    if (steps.escape !== "Not saved: the active window was kept." || steps.keep !== steps.escape) throw new Error(JSON.stringify(steps));
    saved = await app.invoke("get_settings");
    if (saved.value?.core?.active_window_days !== 10) throw new Error(`after Shorten: ${JSON.stringify(saved)}`);
    steps.activeWindowDays = 10;
    return steps;
  });

  await run.check("flow.export_plan", async () => {
    if (existsSync(a.export)) throw new Error(`${a.export} exists before the export`);
    await app.s.clickText("Export plan...");
    await app.s.until("return document.querySelector('p.live')?.textContent.startsWith('Plan exported to ')", 30_000);
    const text = readFileSync(a.export, "utf8");
    const plan = JSON.parse(text);
    const status = await live();
    // The save dialog's list is used up: the second export is a cancel and writes nothing.
    await app.s.exec("document.querySelector('p.live').textContent = ''");
    await app.s.clickText("Export plan...");
    await new Promise((r) => setTimeout(r, 1500));
    const second = await live();
    if (second !== "") throw new Error(`second export said: ${second}`);
    return { status, bytes: text.length, keys: Object.keys(plan).slice(0, 12) };
  });

  await run.check("flow.open_valid_scan_file", async () => {
    await app.s.clickText("New scan");
    await waitH2("Choose what to scan");
    await app.s.clickText("Open a scan file...");
    await app.s.until("return [...document.querySelectorAll('.bar span')].some((e) => e.textContent.startsWith('Imported scan file.'))", 60_000);
    const tabs = await app.s.exec("return [...document.querySelectorAll('[role=tab]')].map((t) => t.textContent)");
    if (tabs.includes("Size tree")) throw new Error(`an imported scan shows ${JSON.stringify(tabs)}`);
    return { tabs };
  });
  await axe("axe.imported_developer_view", "body");

  await run.check("flow.open_malformed_scan_file", async () => {
    await app.s.clickText("New scan");
    await waitH2("Choose what to scan");
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
}
process.exitCode = run.write(a.out ?? "e2e-result.json") ? 1 : 0;
