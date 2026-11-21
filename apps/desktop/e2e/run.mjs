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
import { AC9, judge, measureFloor, measureScroll, profileScroll, scrollOptions, scrollVerdict, summarizeProfile } from "./ac9.mjs";
import { cpuShares, machine, snapshot } from "./runner_info.mjs";

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
    await app.s.until("return !!document.querySelector('[role=alertdialog]')", 15_000);
    const onOpen = await active();
    if (!onOpen?.text?.startsWith("Keep 30")) throw new Error(`focus when the question opens: ${JSON.stringify(onOpen)}`);
    await app.s.keys(KEY.escape);
    await app.s.until("return !document.querySelector('[role=alertdialog]')", 10_000);
    steps.escape = await message();
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
    if (steps.escape !== "Not saved: the active window was kept." || steps.keep !== steps.escape) throw new Error(JSON.stringify(steps));
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
    const saved = await app.invoke("get_settings");
    if (saved.value?.core?.active_window_days !== 10) throw new Error(`after Escape: ${JSON.stringify(saved)}`);
    return { tabCycle: seen };
  });

  await run.check("flow.export_plan", async () => {
    if (existsSync(a.export)) throw new Error(`${a.export} exists before the export`);
    await app.s.clickText("Export plan...");
    await app.s.until("return document.querySelector('p.live')?.textContent.startsWith('Plan exported to ')", 30_000);
    const text = readFileSync(a.export, "utf8");
    const plan = JSON.parse(text);
    const status = await live();
    // CT-c: the confirmation names the file and says nothing else changed.
    if (!/^Plan exported to .+\. Nothing else was changed\.$/.test(status)) throw new Error(`export said: ${status}`);
    // CT-F6, first sequence: the confirmation is gone on the chooser after New scan. (No
    // clearing of the status from here: that hid this defect.)
    await app.s.clickText("New scan");
    await waitH2("Choose what to scan");
    const onChooser = await live();
    if (onChooser !== "") throw new Error(`after New scan the chooser shows: ${onChooser}`);
    return { status, onChooser, bytes: text.length, keys: Object.keys(plan).slice(0, 12) };
  });

  await run.check("flow.open_valid_scan_file", async () => {
    await app.s.clickText("Open a scan file...");
    await app.s.until("return [...document.querySelectorAll('.bar span')].some((e) => e.textContent.startsWith('Imported scan file.'))", 60_000);
    // CT-F6: "Reading the scan file..." ends with the read.
    const afterImport = await live();
    if (afterImport !== "") throw new Error(`after the import the status says: ${afterImport}`);
    const tabs = await app.s.exec("return [...document.querySelectorAll('[role=tab]')].map((t) => t.textContent)");
    if (tabs.includes("Size tree")) throw new Error(`an imported scan shows ${JSON.stringify(tabs)}`);
    // The save dialog's list is used up: this second export is a cancel, writes nothing and
    // says nothing.
    await app.s.clickText("Export plan...");
    await new Promise((r) => setTimeout(r, 1500));
    const second = await live();
    if (second !== "") throw new Error(`a cancelled export said: ${second}`);
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
}

// Spec AC-9 at one million rows (PR gate F9), as registered in WAL w8d2cd55d39e (built to a
// quoted excerpt of spec v6 AC-9; the spec text was not re-read). A second app session, started
// with PRUNEDISK_E2E_SYNTH_NODES so the e2e build opens on a size tree of one folder holding
// 1,000,000 files (root expanded: 1,000,001 rows). Its own tauri-driver, because the app takes
// its environment from the driver, on its own ports so nothing of the first session is in the
// way. Steps, boundary and verdict: e2e/ac9.mjs, shared with the Playwright check (perf/).
{
  const rows = AC9.nodes + 1;
  let proc2;
  let app2;
  try {
    await new Promise((r) => setTimeout(r, 2000));
    ({ proc: proc2 } = await startDriver({
      nativeDriver: a["native-driver"],
      port: 4446,
      nativePort: 4447,
      env: { ...env, PRUNEDISK_E2E_SYNTH_NODES: String(AC9.nodes) },
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
    const ready = () => {
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

    // (1) IPC, Linux: tree_page offset k*200 limit 200, 10 warm-up pages, then k = 0..99.
    if (process.platform === "linux") {
      await run.check("timing.synth_1m.ipc_tree_page_p95", async () => {
        ready();
        const r = await app2.invoke("current_state");
        if (!r.ok) throw new Error(`current_state: ${r.error}`);
        const view = r.value.view;
        const out = await app2.s.execAsync(
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
          view,
          AC9.ipcWarmup,
          AC9.ipcSamples,
          AC9.pageLimit,
        );
        if (!Array.isArray(out)) throw new Error(JSON.stringify(out));
        if (out.length !== AC9.ipcSamples) throw new Error(`${out.length} samples, wanted ${AC9.ipcSamples}`);
        const m = { ...judge("synth_1m_ipc_tree_page", out, AC9.ipcP95Ms), rows, pageLimit: AC9.pageLimit };
        run.measure(m);
        if (judge(m.id, out, 0).pass) throw new Error("control: a 0 ms threshold passed these samples");
        if (!m.pass) throw new Error(`p95 ${m.p95.toFixed(1)} ms, not under ${AC9.ipcP95Ms} ms`);
        return m;
      });
    }

    // (2) Running-app scroll, Linux and Windows: 10 warm-up steps, then 200 steps of 10 rows.
    await run.check("timing.synth_1m.scroll_p95", async () => {
      ready();
      const r = await app2.s.execAsync(
        `const [o, done] = arguments;
         (${measureScroll.toString()})(o).then(done, (e) => done({ error: String(e), samples: [] }));`,
        600_000,
        scrollOptions(),
      );
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
  }
}

// AC-9 investigation of the Linux running-app scroll (docs/DESKTOP.md), registered before any
// sample in WAL w07983c42191 and wf5f38e99704. Opt-in (AC9_INVESTIGATE=1, Linux only) and never
// a gate: everything goes to run.measure, nothing to a check. The gated session above is not
// changed. Six more one-million-row sessions, each a fresh launch, with and without the strace
// wrapper (e2e/check.sh skips it for PRUNEDISK_AC9_UNTRACED=1 with PRUNEDISK_E2E_SYNTH_NODES):
// - steps: floor windows at the start, middle and end, around the scroll steps;
// - idle: the app with the tree shown, no steps, the floor loop alone for the same time;
// - blank: the same launch, navigated to about:blank (no app code), the floor loop likewise.
if (process.platform === "linux" && process.env.AC9_INVESTIGATE === "1") {
  const { stepsWithFloors, floorFor, stat, windows, blockMedians } = await import("./ac9x.mjs");
  const { tracerState } = await import("./runner_info.mjs");
  const NAMES = ["e2e", "WebKitWebProces", "WebKitNetworkPr"];
  const WINDOW = 60;
  const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
  const appPids = () => (tracerState(NAMES)?.procs ?? []).filter((p) => p.name === "e2e").map((p) => p.pid);
  const durations = {};
  const session = async (arm, untraced, body) => {
    const id = `info.ac9x.${arm}.${untraced ? "untraced" : "traced"}`;
    let proc;
    let app;
    try {
      // The previous session's app must be gone, or its trace state would be read for this one.
      for (let i = 0; i < 30 && appPids().length; i++) await sleep(500);
      const leftover = new Set(appPids());
      await sleep(2000);
      const e = { ...env, PRUNEDISK_E2E_SYNTH_NODES: String(AC9.nodes) };
      if (untraced) e.PRUNEDISK_AC9_UNTRACED = "1";
      else delete e.PRUNEDISK_AC9_UNTRACED;
      ({ proc } = await startDriver({ nativeDriver: a["native-driver"], port: 4448, nativePort: 4449, env: e }));
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
