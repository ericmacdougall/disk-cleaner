import { describe, expect, it } from "vitest";
import { App } from "./app";
import { flush, mockBackend, syntheticPage, SETTINGS } from "./testing";
import { changesFrom, parseWhole, SettingsForm } from "./views/settings";
import { allocatedLabel, MAX_DOM_ROWS, TreeGrid } from "./views/tree";
import type { DevItem, DevSummary, SaveResult } from "./api";
import { DEV_PAGE, DevView } from "./views/dev";
import { layout } from "./views/treemap";

const HOSTILE = '<img src=x onerror="window.__xss=1">';

const SUMMARY: DevSummary = {
  source: "analysis",
  scan_generated_at: 1_767_225_600,
  plan_generated_at: 1_767_225_600,
  imported: true,
  scan_age_days: 0,
  stale_warning: null,
  imported_note: "The process check describes this machine.",
  groups: [{ class: "repo", label: "Repositories", count: 0, class_bytes: 0 }],
  bytes_by_class: {},
  plan_totals: { reclaimable_bytes: 0, ready_bytes: 0, by_action: {} },
  coverage_complete: true,
  coverage_note: null,
  dry_run: true,
  remote_facts_verified: false,
  remote_facts_note: "Remote facts are not verified.",
  process_check: "none",
  process_check_note: null,
  active_window_days: 30,
  window_source: "default",
  roots: [],
};

function item(cls: string, i: number): DevItem {
  return {
    node: i,
    path: `/syn/${cls}/item-${i}`,
    class: cls,
    action: "keep",
    removes: false,
    reason: "synthetic",
    state: "ready",
    state_reason: "",
    reclaimable_bytes: 0,
    bytes_total: 1,
    files_total: 1,
    newest_mtime: null,
    days_since_activity: null,
    facts_no: [],
    facts_unknown: [],
  };
}

function deferred<T>() {
  let resolve!: (v: T) => void;
  const promise = new Promise<T>((r) => {
    resolve = r;
  });
  return { promise, resolve };
}

function button(root: HTMLElement, text: string): HTMLButtonElement {
  const b = [...root.querySelectorAll("button")].find((x) => x.textContent?.startsWith(text));
  if (!b) throw new Error(`no button ${text}`);
  return b;
}

describe("hostile names stay text (spec AC-10, NEG-8)", () => {
  it("a name holding markup renders as text, with no element created", async () => {
    const b = mockBackend({
      treePage: async (view, offset, limit) => syntheticPage(view, 3, offset, limit, (i) => (i === 0 ? HOSTILE : `n${i}`)),
    });
    const g = new TreeGrid(b, 1, () => undefined, "Linux");
    document.body.replaceChildren(g.el);
    await g.load();
    expect(g.el.querySelector("img")).toBeNull();
    expect(g.el.textContent).toContain(HOSTILE);
    expect((window as unknown as { __xss?: number }).__xss).toBeUndefined();
  });
});

describe("tree grid (spec AC-4, AC-9, NEG-12)", () => {
  it("never holds more than 200 rows, even over a million entries in a very tall window", async () => {
    const b = mockBackend({ treePage: async (view, offset, limit) => syntheticPage(view, 1_000_000, offset, limit) });
    const g = new TreeGrid(b, 1, () => undefined, "Linux");
    document.body.replaceChildren(g.el);
    // jsdom has no layout: pretend the window is a million pixels tall.
    const scroller = g.el.querySelector('[role="treegrid"]') as HTMLElement;
    Object.defineProperty(scroller, "clientHeight", { value: 1_000_000 });
    await g.load();
    expect(g.el.querySelectorAll('.tg-body [role="row"]').length).toBe(MAX_DOM_ROWS);
    expect(scroller.getAttribute("aria-rowcount")).toBe("1000000");
    for (const call of b.calls.filter((c) => c.name === "treePage")) expect(call.args[2]).toBeLessThanOrEqual(MAX_DOM_ROWS);
  });

  it("is operable by keyboard: Right expands, Enter drills in, Backspace goes up", async () => {
    const b = mockBackend({
      treePage: async (view, offset, limit) => ({
        ...syntheticPage(view, 50, offset, limit),
        breadcrumb: [
          { id: 40, name: "outer" },
          { id: 41, name: "inner" },
        ],
      }),
    });
    const g = new TreeGrid(b, 1, () => undefined, "Linux");
    document.body.replaceChildren(g.el);
    await g.load();
    await g.onKey(new KeyboardEvent("keydown", { key: "ArrowRight" }));
    await g.onKey(new KeyboardEvent("keydown", { key: "Enter" }));
    await g.onKey(new KeyboardEvent("keydown", { key: "Backspace" }));
    expect(b.calls.find((c) => c.name === "treeToggle")?.args).toEqual([1, 0, true]);
    expect(b.calls.filter((c) => c.name === "treeFocus").map((c) => c.args)).toEqual([
      [1, 0],
      [1, 40],
    ]);
  });

  it("says file length, not allocated size, only on Windows", () => {
    expect(allocatedLabel("Mozilla/5.0 (Windows NT 10.0; Win64; x64)")).toBe("Allocated (file length)");
    expect(allocatedLabel("Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) Darwin")).toBe("Allocated");
    expect(allocatedLabel("Mozilla/5.0 (X11; Linux x86_64)")).toBe("Allocated");
  });

  it("labels sortable columns and says which sort is active", async () => {
    const g = new TreeGrid(mockBackend(), 1, () => undefined, "Windows NT");
    document.body.replaceChildren(g.el);
    await g.load();
    const heads = [...g.el.querySelectorAll('[role="columnheader"]')];
    expect(heads.map((x) => x.textContent?.replace(/[↑↓]/g, "").trim())).toEqual([
      "Name",
      "Allocated (file length)",
      "Size",
      "Files",
      "Modified",
      "% of parent",
    ]);
    expect(heads[1]?.getAttribute("aria-sort")).toBe("descending");
  });
});

describe("treemap (spec NEG-11)", () => {
  it("a zero-byte tree shows nothing and divides by nothing", () => {
    expect(layout([{ id: 1, name: "a", alloc: 0, is_dir: true, tag: null }], 800, 400)).toEqual([]);
    expect(layout([], 800, 400)).toEqual([]);
  });

  it("tiles fill the box in proportion", () => {
    const placed = layout(
      [
        { id: 1, name: "a", alloc: 300, is_dir: true, tag: null },
        { id: 2, name: "b", alloc: 100, is_dir: false, tag: null },
      ],
      400,
      100,
    );
    expect(placed).toHaveLength(2);
    const area = (p: (typeof placed)[number]) => p.w * p.h;
    expect(area(placed[0]!) / area(placed[1]!)).toBeGreaterThan(2);
  });
});

describe("settings (spec AC-5, NEG-5)", () => {
  const draft = { activeWindowDays: "30", gitAdminMib: "64", topFiles: "200", minOutputMib: "100", oneFileSystem: true };

  it("refuses zero, negative, non-numeric and over-range values", () => {
    for (const bad of ["0", "-3", "abc", "1.5", "1e3", "", "3651"]) {
      expect("error" in changesFrom({ ...draft, activeWindowDays: bad }, SETTINGS), bad).toBe(true);
    }
    expect(parseWhole(" 12 ")).toBe(12);
  });

  it("sends only the keys that changed", () => {
    expect(changesFrom(draft, SETTINGS)).toMatchObject({ core: { active_window_days: null, max_git_admin_bytes: null } });
    expect(changesFrom({ ...draft, activeWindowDays: "60" }, SETTINGS)).toMatchObject({ core: { active_window_days: 60 } });
  });

  it("keeps an out-of-range value the file holds when it is not changed", () => {
    const s = structuredClone(SETTINGS);
    s.core.active_window_days = 9000;
    s.core.active_window_in_range = false;
    expect(changesFrom({ ...draft, activeWindowDays: "9000" }, s)).toMatchObject({ core: { active_window_days: null } });
  });

  it("a double Save sends one save; the form is disabled while it is pending", async () => {
    let release!: () => void;
    const b = mockBackend({
      saveSettings: () =>
        new Promise<SaveResult>((r) => {
          release = () => r({ report: { outcome: "saved", note: null, replanned: true }, settings: structuredClone(SETTINGS) });
        }),
    });
    const f = new SettingsForm(b);
    document.body.replaceChildren(f.el);
    await f.load();
    const s1 = f.save(false);
    const s2 = f.save(false);
    expect(f.el.querySelector("fieldset")?.disabled).toBe(true);
    release();
    await Promise.all([s1, s2]);
    expect(b.calls.filter((c) => c.name === "saveSettings")).toHaveLength(1);
    expect(f.el.querySelector("fieldset")?.disabled).toBe(false);
    expect(f.el.textContent).toContain("The plan was recomputed.");
  });

  it("shortening the window asks first; Escape keeps it and saves nothing", async () => {
    const b = mockBackend({
      saveSettings: async (_core, _app, confirm) => {
        if (!confirm) throw { kind: "needs_confirmation", message: "shortening the active window from 30 to 7 days", old: 30, new: 7 };
        return { report: { outcome: "saved", note: null, replanned: false }, settings: structuredClone(SETTINGS) };
      },
    });
    const f = new SettingsForm(b);
    document.body.replaceChildren(f.el);
    await f.load();
    (f.el.querySelector("#set-window") as HTMLInputElement).value = "7";
    await f.save(false);
    const dialog = f.el.querySelector('[role="alertdialog"]') as HTMLElement;
    expect(dialog).not.toBeNull();
    expect(document.activeElement?.textContent).toBe("Keep 30 days");
    dialog.dispatchEvent(new KeyboardEvent("keydown", { key: "Escape", bubbles: true }));
    expect(f.el.querySelector('[role="alertdialog"]')).toBeNull();
    expect(b.calls.filter((c) => c.name === "saveSettings")).toHaveLength(1);
    expect((f.el.querySelector("#set-window") as HTMLInputElement).value).toBe("7");
    await f.save(false);
    (f.el.querySelector('[role="alertdialog"] button.primary') as HTMLButtonElement).click();
    await flush();
    expect(b.calls.filter((c) => c.name === "saveSettings").map((c) => c.args[2])).toEqual([false, false, true]);
  });

  it("confirming saves exactly the change that was shown, and the form is locked while asking", async () => {
    const b = mockBackend({
      saveSettings: async (_core, _app, confirm) => {
        if (!confirm) throw { kind: "needs_confirmation", message: "shortening the active window from 30 to 7 days", old: 30, new: 7 };
        return { report: { outcome: "saved", note: null, replanned: false }, settings: structuredClone(SETTINGS) };
      },
    });
    const f = new SettingsForm(b);
    document.body.replaceChildren(f.el);
    await f.load();
    const input = f.el.querySelector("#set-window") as HTMLInputElement;
    input.value = "7";
    await f.save(false);
    expect(f.el.querySelector('[role="alertdialog"]')).not.toBeNull();
    // The draft cannot be edited, nor another save started, while the question is open.
    expect(f.el.querySelector("fieldset")?.disabled).toBe(true);
    input.value = "1";
    await f.save(false);
    expect(b.calls.filter((c) => c.name === "saveSettings")).toHaveLength(1);
    (f.el.querySelector('[role="alertdialog"] button.primary') as HTMLButtonElement).click();
    await flush();
    const saves = b.calls.filter((c) => c.name === "saveSettings");
    expect(saves).toHaveLength(2);
    expect(saves[1]?.args[0]).toMatchObject({ active_window_days: 7 });
    expect(saves[1]?.args[2]).toBe(true);
    expect(f.el.querySelector("fieldset")?.disabled).toBe(false);
  });
});

describe("developer view paging (spec AC-4)", () => {
  const groups = [
    { class: "repo", label: "Repositories", count: 350, class_bytes: 1 },
    { class: "cache", label: "Caches", count: 3, class_bytes: 1 },
  ];

  it("a late page never replaces a newer one, and the range shown is the page shown", async () => {
    const pending: { args: [string, number]; d: ReturnType<typeof deferred<DevItem[]>> }[] = [];
    const b = mockBackend({
      devSummary: async () => ({ ...SUMMARY, imported: false, groups }),
      devPage: (_v, cls, offset) => {
        const d = deferred<DevItem[]>();
        pending.push({ args: [cls, offset], d });
        return d.promise;
      },
    });
    const v = new DevView(b, 1, () => undefined);
    document.body.replaceChildren(v.el);
    const loading = v.load();
    await flush();
    pending[0]!.d.resolve(Array.from({ length: DEV_PAGE }, (_, i) => item("repo", i)));
    await loading;
    expect(v.el.textContent).toContain("1 to 100 of 350");
    button(v.el, "Next").click();
    await flush();
    // While that page is on its way, the user picks the same class again (back to its start).
    button(v.el, "Repositories").click();
    await flush();
    expect(pending.map((p) => p.args)).toEqual([
      ["repo", 0],
      ["repo", 100],
      ["repo", 0],
    ]);
    pending[2]!.d.resolve(Array.from({ length: DEV_PAGE }, (_, i) => item("repo", i)));
    await flush();
    pending[1]!.d.resolve(Array.from({ length: DEV_PAGE }, (_, i) => item("repo", 100 + i)));
    await flush();
    const items = v.el.querySelector(".dev-items")?.textContent ?? "";
    expect(items).toContain("/syn/repo/item-0");
    expect(items).not.toContain("/syn/repo/item-100");
    expect(items).toContain("1 to 100 of 350");
  });

  it("Next pressed twice while a page loads moves one page, and never past the last page", async () => {
    const gate: { release: (() => void) | null } = { release: null };
    const b = mockBackend({
      devSummary: async () => ({ ...SUMMARY, imported: false, groups }),
      devPage: (_v, cls, offset, limit) =>
        new Promise<DevItem[]>((r) => {
          const n = Math.max(0, Math.min(limit, 350 - offset));
          const page = Array.from({ length: n }, (_, i) => item(cls, offset + i));
          if (offset === 0) r(page);
          else gate.release = () => r(page);
        }),
    });
    const v = new DevView(b, 1, () => undefined);
    document.body.replaceChildren(v.el);
    await v.load();
    const next = button(v.el, "Next");
    next.click();
    next.click();
    await flush();
    gate.release?.();
    await flush();
    const offsets = b.calls.filter((c) => c.name === "devPage").map((c) => c.args[2]);
    expect(offsets).toEqual([0, 100]);
    expect(v.el.textContent).toContain("101 to 200 of 350");
    // Go to the last page; Next is then disabled.
    for (let i = 0; i < 5; i++) {
      const n = button(v.el, "Next");
      if (n.disabled) break;
      n.click();
      await flush();
      gate.release?.();
      await flush();
    }
    expect(v.el.textContent).toContain("301 to 350 of 350");
    expect(button(v.el, "Next").disabled).toBe(true);
    expect(Math.max(...b.calls.filter((c) => c.name === "devPage").map((c) => c.args[2] as number))).toBe(300);
  });

  it("keyboard focus stays on the class chosen and on the pager button pressed (spec AC-7)", async () => {
    const b = mockBackend({
      devSummary: async () => ({ ...SUMMARY, imported: false, groups }),
      devPage: async (_v, cls, offset, limit) => {
        const count = cls === "repo" ? 350 : 3;
        const n = Math.max(0, Math.min(limit, count - offset));
        return Array.from({ length: n }, (_, i) => item(cls, offset + i));
      },
    });
    const v = new DevView(b, 1, () => undefined);
    document.body.replaceChildren(v.el);
    await v.load();
    const caches = button(v.el, "Caches");
    caches.focus();
    caches.click();
    await flush();
    expect(button(v.el, "Caches").getAttribute("aria-pressed")).toBe("true");
    expect(document.activeElement).toBe(button(v.el, "Caches"));
    const repos = button(v.el, "Repositories");
    repos.focus();
    repos.click();
    await flush();
    expect(document.activeElement).toBe(button(v.el, "Repositories"));
    const next = button(v.el, "Next");
    next.focus();
    next.click();
    await flush();
    expect(v.el.textContent).toContain("101 to 200 of 350");
    expect(document.activeElement).toBe(button(v.el, "Next"));
    for (let i = 0; i < 2; i++) {
      button(v.el, "Next").click();
      await flush();
    }
    // On the last page Next is disabled: focus moves to Previous instead of being lost.
    expect(v.el.textContent).toContain("301 to 350 of 350");
    expect(button(v.el, "Next").disabled).toBe(true);
    expect(document.activeElement).toBe(button(v.el, "Previous"));
  });

  it("a failed page keeps the page shown, says so, and Next asks for that page again", async () => {
    let failNext = true;
    const statuses: string[] = [];
    const b = mockBackend({
      devSummary: async () => ({ ...SUMMARY, imported: false, groups }),
      devPage: async (_v, cls, offset, limit) => {
        if (offset === 100 && failNext) {
          failNext = false;
          throw { kind: "io", message: "the page could not be read" };
        }
        const n = Math.max(0, Math.min(limit, 350 - offset));
        return Array.from({ length: n }, (_, i) => item(cls, offset + i));
      },
    });
    const v = new DevView(b, 1, (t) => statuses.push(t));
    document.body.replaceChildren(v.el);
    await v.load();
    button(v.el, "Next").click();
    await flush();
    expect(statuses).toContain("the page could not be read");
    const items = v.el.querySelector(".dev-items")?.textContent ?? "";
    expect(items).toContain("/syn/repo/item-0");
    expect(items).toContain("1 to 100 of 350");
    const alert = v.el.querySelector('[role="alert"]')?.textContent ?? "";
    expect(alert).toContain("Could not load Repositories 101 to 200");
    expect(alert).toContain("the page could not be read");
    // Next still means the page after the one shown, not the one after the failed one.
    button(v.el, "Next").click();
    await flush();
    const offsets = b.calls.filter((c) => c.name === "devPage").map((c) => c.args[2]);
    expect(offsets).toEqual([0, 100, 100]);
    expect(v.el.textContent).toContain("101 to 200 of 350");
    expect(v.el.querySelector('[role="alert"]')).toBeNull();
  });

  it("a failed class switch keeps the class shown selected and offers to try again", async () => {
    let failCache = true;
    const b = mockBackend({
      devSummary: async () => ({ ...SUMMARY, imported: false, groups }),
      devPage: async (_v, cls, offset, limit) => {
        if (cls === "cache" && failCache) {
          failCache = false;
          throw { kind: "io", message: "the class could not be read" };
        }
        const count = cls === "repo" ? 350 : 3;
        const n = Math.max(0, Math.min(limit, count - offset));
        return Array.from({ length: n }, (_, i) => item(cls, offset + i));
      },
    });
    const v = new DevView(b, 1, () => undefined);
    document.body.replaceChildren(v.el);
    await v.load();
    button(v.el, "Caches").click();
    await flush();
    expect(button(v.el, "Repositories").getAttribute("aria-pressed")).toBe("true");
    expect(button(v.el, "Caches").getAttribute("aria-pressed")).toBe("false");
    expect(v.el.querySelector(".dev-items")?.textContent).toContain("/syn/repo/item-0");
    expect(v.el.querySelector('[role="alert"]')?.textContent).toContain("Could not load Caches 1 to 3");
    const retry = button(v.el, "Try again");
    retry.focus();
    retry.click();
    await flush();
    expect(button(v.el, "Caches").getAttribute("aria-pressed")).toBe("true");
    // The Try again button is gone; focus moves to the heading of the page now shown.
    expect(document.activeElement?.tagName).toBe("H3");
    expect(document.activeElement?.textContent).toBe("Caches");
    const items = v.el.querySelector(".dev-items")?.textContent ?? "";
    expect(items).toContain("/syn/cache/item-0");
    expect(items).toContain("1 to 3 of 3");
    expect(v.el.querySelector('[role="alert"]')).toBeNull();
  });
});

describe("agent session folders (spec AC-13)", () => {
  it("are listed with size, files and age, labelled with no plan action, and name the enclosing item", async () => {
    const b = mockBackend({
      devSummary: async () => ({ ...SUMMARY, imported: false }),
      sessionFolders: async () => [
        {
          id: 7,
          layout: ".claude/projects",
          path: "/syn/home/.claude/projects",
          alloc: 3 * 1024 * 1024,
          size: 3 * 1024 * 1024,
          files: 12,
          // Noon UTC on 2026-01-01: the same calendar day in any time zone from UTC-11 to UTC+11.
          newest_mtime: 1_767_268_800,
          plan_note: "no plan action yet",
          enclosing: { action: "keep", path: "/syn/home" },
        },
      ],
    });
    const v = new DevView(b, 1, () => undefined);
    document.body.replaceChildren(v.el);
    await v.load();
    const text = v.el.querySelector(".dev-sessions")?.textContent ?? "";
    expect(text).toContain(".claude/projects: /syn/home/.claude/projects, 12 files, 3.0 MiB");
    expect(text).toContain("last changed 2026-01-01");
    expect(text).toContain("no plan action yet");
    expect(text).toContain("Inside /syn/home (keep).");
  });

  it("say why there are none for an imported scan", async () => {
    const v = new DevView(mockBackend({ devSummary: async () => SUMMARY }), 1, () => undefined);
    document.body.replaceChildren(v.el);
    await v.load();
    expect(v.el.querySelector(".dev-sessions")?.textContent).toContain("Not available for an imported scan");
  });
});

describe("app shell (spec AC-4, AC-7)", () => {
  it("Apply exists only as a disabled explanation", async () => {
    const b = mockBackend({
      currentState: async () => ({ job: null, state: "idle", view: 5, imported: true, has_tree: false, has_analysis: true, roots: [] }),
      devSummary: async () => SUMMARY,
    });
    const app = new App(b);
    document.body.replaceChildren(app.el);
    await app.start();
    await flush(20);
    const apply = [...app.el.querySelectorAll("button")].find((x) => x.textContent === "Apply plan");
    expect(apply?.disabled).toBe(true);
    expect(app.el.textContent).toContain("Dry run: yes, nothing is changed");
    expect(app.el.textContent).toContain("Remote facts verified: no");
    // Every button has a name.
    for (const btn of app.el.querySelectorAll("button")) {
      expect((btn.getAttribute("aria-label") ?? btn.textContent ?? "").trim().length).toBeGreaterThan(0);
    }
  });
});
