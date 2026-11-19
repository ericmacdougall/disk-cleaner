import { describe, expect, it } from "vitest";
import { App } from "./app";
import { flush, mockBackend, syntheticPage, SETTINGS } from "./testing";
import { changesFrom, parseWhole, SettingsForm } from "./views/settings";
import { allocatedLabel, MAX_DOM_ROWS, TreeGrid } from "./views/tree";
import type { SaveResult } from "./api";
import { layout } from "./views/treemap";

const HOSTILE = '<img src=x onerror="window.__xss=1">';

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
});

describe("app shell (spec AC-4, AC-7)", () => {
  it("Apply exists only as a disabled explanation", async () => {
    const b = mockBackend({
      currentState: async () => ({ job: null, state: "idle", view: 5, imported: true, has_tree: false, has_analysis: true, roots: [] }),
      devSummary: async () => ({
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
      }),
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
