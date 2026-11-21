// The size tree as a WAI-ARIA treegrid (CT-F4, CT-F5, PR gate F6): every key of the pattern,
// one focus owner that survives virtualisation, header order and names, and the row and cell
// attributes a screen reader reads. Keys are dispatched on the focused element, as a user's are.

import { describe, expect, it } from "vitest";
import type { Backend, Row, TreePage } from "./api";
import { flush, mockBackend, row } from "./testing";
import { TreeGrid } from "./views/tree";

interface Node {
  id: number;
  name: string;
  kids?: Node[];
}

/** A small stateful tree served like the backend: roots A (a1 (x1, x2), a2, a3), B (1000 files), c.txt. */
function model() {
  let next = 0;
  const node = (name: string, kids?: (() => Node)[]): (() => Node) => () => {
    const id = next++;
    return { id, name, kids: kids?.map((k) => k()) };
  };
  const roots: Node[] = [
    node("A", [node("a1", [node("x1"), node("x2")]), node("a2"), node("a3")])(),
    node("B", Array.from({ length: 1000 }, (_, i) => node(`b${i}`)))(),
    node("c.txt")(),
  ];
  const byId = new Map<number, Node>();
  const index = (n: Node) => {
    byId.set(n.id, n);
    n.kids?.forEach(index);
  };
  roots.forEach(index);
  const expanded = new Set<number>();
  let focus: Node | null = null;
  const flat = (): Row[] => {
    const out: Row[] = [];
    const walk = (list: Node[], depth: number, parent: number | null) => {
      list.forEach((n, i) => {
        const at = out.length;
        out.push(
          row(n.id, n.name, {
            depth,
            kind: n.kids ? "dir" : "file",
            has_children: !!n.kids?.length,
            expanded: expanded.has(n.id),
            parent_row: parent,
            posinset: i + 1,
            setsize: list.length,
          }),
        );
        if (n.kids && expanded.has(n.id)) walk(n.kids, depth + 1, at);
      });
    };
    walk(focus ? (focus.kids ?? []) : roots, 0, null);
    return out;
  };
  const page = (view: number, offset: number, limit: number): TreePage => {
    const all = flat();
    return {
      view,
      total: all.length,
      offset,
      rows: all.slice(offset, offset + Math.min(limit, 1000)),
      sort: { key: "allocated", descending: true },
      focus: focus?.id ?? null,
      breadcrumb: focus ? [{ id: focus.id, name: focus.name }] : [],
      source: { kind: "size walk", finished_at: 1_767_225_600, roots: ["/syn"], stats: null },
    };
  };
  const over: Partial<Backend> = {
    treePage: async (view, offset, limit) => page(view, offset, limit),
    treeToggle: async (_v, id, expand) => {
      if (expand ?? !expanded.has(id)) expanded.add(id);
      else expanded.delete(id);
      return flat().length;
    },
    treeFocus: async (_v, id) => {
      focus = id === null ? null : (byId.get(id) ?? null);
      expanded.clear();
      return flat().length;
    },
  };
  return { backend: mockBackend(over), expanded, flat };
}

async function setup(open: number[] = []) {
  const m = model();
  for (const id of open) m.expanded.add(id);
  const g = new TreeGrid(m.backend, 1, () => undefined, "Linux");
  document.body.replaceChildren(g.el);
  await g.load();
  const grid = g.el.querySelector<HTMLElement>('[role="treegrid"]')!;
  // Enter the grid as a user does: click the first data row.
  grid.querySelector<HTMLElement>('.tg-body [role="row"]')!.click();
  await flush(10);
  return { ...m, g, grid };
}

async function press(key: string, opts: KeyboardEventInit = {}): Promise<void> {
  const target = document.activeElement ?? document.body;
  target.dispatchEvent(new KeyboardEvent("keydown", { key, bubbles: true, cancelable: true, ...opts }));
  await flush(10);
}

/** The focused element's row, and the column of the focused cell (null for row focus). */
function focusAt(): { rowindex: string | null; col: number | null; role: string | null } {
  const el = document.activeElement as HTMLElement | null;
  const r = el?.closest('[role="row"]');
  const role = el?.getAttribute("role") ?? null;
  const col = role === "gridcell" && r ? [...r.children].indexOf(el!) : null;
  return { rowindex: r?.getAttribute("aria-rowindex") ?? null, col, role };
}

/** aria-rowindex of data row i (row 1 is the header). */
const ri = (i: number) => String(i + 2);

describe("treegrid keyboard (CT-F4, APG treegrid pattern)", () => {
  it("puts DOM focus on the clicked row, not on the grid container", async () => {
    await setup();
    expect(focusAt()).toEqual({ rowindex: ri(0), col: null, role: "row" });
  });

  it("Down and Up move focus one row and stop at the ends", async () => {
    await setup();
    await press("ArrowUp");
    expect(focusAt().rowindex).toBe(ri(0));
    await press("ArrowDown");
    expect(focusAt()).toEqual({ rowindex: ri(1), col: null, role: "row" });
    await press("ArrowDown");
    await press("ArrowDown");
    expect(focusAt().rowindex).toBe(ri(2));
    await press("ArrowUp");
    expect(focusAt().rowindex).toBe(ri(1));
  });

  it("Home and End move to the first and last row; Ctrl+Home and Ctrl+End too", async () => {
    const { flat } = await setup([6]);
    const last = flat().length - 1;
    expect(last).toBe(1002);
    await press("End");
    expect(focusAt()).toEqual({ rowindex: ri(last), col: null, role: "row" });
    await press("Home");
    expect(focusAt().rowindex).toBe(ri(0));
    await press("End", { ctrlKey: true });
    expect(focusAt().rowindex).toBe(ri(last));
    await press("Home", { ctrlKey: true });
    expect(focusAt().rowindex).toBe(ri(0));
  });

  it("PageDown and PageUp move by a page of rows and stop at the ends", async () => {
    const { grid } = await setup([6]);
    const pageRows = Math.max(1, Math.floor((grid.clientHeight || 28 * 20) / 28) - 1);
    await press("PageDown");
    expect(focusAt().rowindex).toBe(ri(pageRows));
    await press("PageDown");
    expect(focusAt().rowindex).toBe(ri(2 * pageRows));
    await press("PageUp");
    await press("PageUp");
    await press("PageUp");
    expect(focusAt().rowindex).toBe(ri(0));
  });

  it("Right opens a closed folder and keeps focus on its row", async () => {
    const { backend } = await setup();
    await press("ArrowRight");
    expect(backend.calls.filter((c) => c.name === "treeToggle").map((c) => c.args)).toEqual([[1, 0, true]]);
    expect(focusAt()).toEqual({ rowindex: ri(0), col: null, role: "row" });
    expect(document.activeElement?.getAttribute("aria-expanded")).toBe("true");
  });

  it("Right on an open folder or a file moves to the first cell, then cell by cell, stopping at the last", async () => {
    const { backend } = await setup([0]);
    await press("ArrowRight");
    expect(backend.calls.filter((c) => c.name === "treeToggle")).toEqual([]);
    expect(focusAt()).toEqual({ rowindex: ri(0), col: 0, role: "gridcell" });
    for (let i = 0; i < 8; i++) await press("ArrowRight");
    expect(focusAt()).toEqual({ rowindex: ri(0), col: 5, role: "gridcell" });
  });

  it("Right on a file row moves to its first cell and opens nothing", async () => {
    const { backend } = await setup([0]);
    // Rows: A, a1, a2 (a file), ...
    await press("ArrowDown");
    await press("ArrowDown");
    await press("ArrowRight");
    expect(focusAt()).toEqual({ rowindex: ri(2), col: 0, role: "gridcell" });
    expect(backend.calls.filter((c) => c.name === "treeToggle")).toEqual([]);
  });

  it("Left moves cell by cell, then from the first cell back to the row", async () => {
    await setup([0]);
    await press("ArrowRight");
    await press("ArrowRight");
    await press("ArrowRight");
    expect(focusAt().col).toBe(2);
    await press("ArrowLeft");
    expect(focusAt().col).toBe(1);
    await press("ArrowLeft");
    await press("ArrowLeft");
    expect(focusAt()).toEqual({ rowindex: ri(0), col: null, role: "row" });
  });

  it("Home and End in a cell move to the first and last cell of the row; Up and Down keep the column", async () => {
    await setup([0]);
    await press("ArrowRight");
    await press("End");
    expect(focusAt()).toEqual({ rowindex: ri(0), col: 5, role: "gridcell" });
    await press("ArrowDown");
    expect(focusAt()).toEqual({ rowindex: ri(1), col: 5, role: "gridcell" });
    await press("Home");
    expect(focusAt()).toEqual({ rowindex: ri(1), col: 0, role: "gridcell" });
    await press("End", { ctrlKey: true });
    expect(focusAt().col).toBe(0);
    await press("ArrowUp");
    expect(focusAt().col).toBe(0);
  });

  it("Left closes an open folder and keeps focus on it", async () => {
    const { backend } = await setup([0]);
    await press("ArrowLeft");
    expect(backend.calls.filter((c) => c.name === "treeToggle").map((c) => c.args)).toEqual([[1, 0, false]]);
    expect(focusAt()).toEqual({ rowindex: ri(0), col: null, role: "row" });
    expect(document.activeElement?.getAttribute("aria-expanded")).toBe("false");
  });

  it("Left on a closed child moves to its parent row (APG treegrid example)", async () => {
    const { backend } = await setup([0, 1]);
    // Rows: A, a1, x1, x2, a2 ... ; x2 is row 3, its parent a1 is row 1.
    await press("ArrowDown");
    await press("ArrowDown");
    await press("ArrowDown");
    expect(focusAt().rowindex).toBe(ri(3));
    await press("ArrowLeft");
    expect(focusAt()).toEqual({ rowindex: ri(1), col: null, role: "row" });
    await press("ArrowLeft"); // a1 is open: closes it
    await press("ArrowLeft"); // a1 is closed: to A
    expect(focusAt().rowindex).toBe(ri(0));
    expect(backend.calls.filter((c) => c.name === "treeFocus")).toEqual([]);
  });

  it("Left on a child whose parent row is far outside the rendered rows still moves to the parent", async () => {
    const { g, grid } = await setup([6]);
    // B is row 1; its children are rows 2..1001. Go far down, then Left.
    await press("End"); // c.txt, row 1002
    await press("ArrowUp"); // b999, row 1001
    expect(focusAt().rowindex).toBe(ri(1001));
    expect(grid.querySelector(`[aria-rowindex="${ri(1)}"]`)).toBeNull();
    await press("ArrowLeft");
    expect(focusAt()).toEqual({ rowindex: ri(1), col: null, role: "row" });
    expect(g.el.contains(document.activeElement)).toBe(true);
  });

  it("Left on a closed top-level row does nothing: it neither closes nor leaves the folder", async () => {
    const { backend } = await setup();
    await press("ArrowDown");
    await press("ArrowDown"); // c.txt
    await press("ArrowLeft");
    expect(focusAt().rowindex).toBe(ri(2));
    expect(backend.calls.filter((c) => c.name === "treeFocus" || c.name === "treeToggle")).toEqual([]);
  });

  it("Enter drills into a folder and Backspace goes back up, with focus kept on a row", async () => {
    const { backend } = await setup();
    await press("Enter");
    expect(backend.calls.filter((c) => c.name === "treeFocus").map((c) => c.args)).toEqual([[1, 0]]);
    expect(focusAt()).toEqual({ rowindex: ri(0), col: null, role: "row" });
    expect(document.activeElement?.textContent).toContain("a1");
    await press("Backspace");
    expect(backend.calls.filter((c) => c.name === "treeFocus").map((c) => c.args)).toEqual([
      [1, 0],
      [1, null],
    ]);
    expect(focusAt().rowindex).toBe(ri(0));
  });

  it("a double click on a row's name drills in: the first click keeps the name node in its row", async () => {
    // CI e2e (run 44877158666, view.tree_drill_into_wide): the click re-render moved the name
    // span out of its cell, so the browser's dblclick on that span never reached the row.
    const { g, backend } = await setup();
    const name = [...g.el.querySelectorAll<HTMLElement>('.tg-body [role="row"] .name')].find((x) => x.textContent === "B")!;
    name.dispatchEvent(new MouseEvent("click", { bubbles: true }));
    await flush(10);
    name.dispatchEvent(new MouseEvent("click", { bubbles: true }));
    await flush(10);
    expect(name.closest('[role="row"]')?.isConnected).toBe(true);
    name.dispatchEvent(new MouseEvent("dblclick", { bubbles: true }));
    await flush(10);
    expect(backend.calls.filter((c) => c.name === "treeFocus").map((c) => c.args)).toEqual([[1, 6]]);
  });

  it("keys on a column header's sort button do not move or open rows", async () => {
    const { g, backend } = await setup();
    const sort = g.el.querySelector<HTMLButtonElement>('[role="columnheader"] button')!;
    sort.focus();
    await press("ArrowRight");
    await press("ArrowDown");
    await press("Enter");
    expect(backend.calls.filter((c) => c.name === "treeToggle" || c.name === "treeFocus")).toEqual([]);
    expect(document.activeElement).toBe(sort);
  });
});

describe("one focus owner under virtualisation (CT-F4, gate F6)", () => {
  it("keeps the same focused row node while it is scrolled out of the rendered rows and back", async () => {
    const { g, grid } = await setup([6]);
    await press("ArrowDown"); // B, row 1
    const focusedRow = document.activeElement as HTMLElement;
    expect(focusedRow.getAttribute("aria-rowindex")).toBe(ri(1));
    // Wheel far away: rows 600 and on are rendered, row 1 is not in that window.
    grid.scrollTop = 600 * 28;
    grid.dispatchEvent(new Event("scroll"));
    await flush(10);
    expect(grid.querySelector(`[aria-rowindex="${ri(600)}"]`)).not.toBeNull();
    expect(document.activeElement).toBe(focusedRow);
    expect(focusedRow.isConnected).toBe(true);
    expect(focusedRow.getAttribute("tabindex")).toBe("0");
    expect(grid.querySelectorAll('[role="row"][tabindex="0"]').length).toBe(1);
    // And back.
    grid.scrollTop = 0;
    grid.dispatchEvent(new Event("scroll"));
    await flush(10);
    expect(document.activeElement).toBe(focusedRow);
    expect(g.el.querySelector(`[aria-rowindex="${ri(1)}"]`)).toBe(focusedRow);
    // Rows are in order in the DOM around it.
    const order = [...grid.querySelectorAll('.tg-body [role="row"]')].map((r) => Number(r.getAttribute("aria-rowindex")));
    expect(order).toEqual([...order].sort((a, b) => a - b));
    await press("ArrowDown");
    expect(focusAt().rowindex).toBe(ri(2));
  });

  it("keeps a focused cell while its row is scrolled away", async () => {
    const { grid } = await setup([6]);
    await press("ArrowDown");
    await press("ArrowRight"); // B is open: first cell
    await press("ArrowRight");
    const cell = document.activeElement as HTMLElement;
    expect(cell.getAttribute("role")).toBe("gridcell");
    grid.scrollTop = 700 * 28;
    grid.dispatchEvent(new Event("scroll"));
    await flush(10);
    expect(document.activeElement).toBe(cell);
  });

  // Each replaced row node is a burst of accessibility events that a screen reader works through
  // before it speaks the next focus: Orca 46 fell silent after a wheel over a 5,000-row folder.
  const rowNodes = (grid: HTMLElement) =>
    new Map([...grid.querySelectorAll<HTMLElement>('.tg-body [role="row"]')].map((r) => [r.getAttribute("aria-rowindex"), r]));

  it("a key press keeps every rendered row node; only the selection's attributes change", async () => {
    const { grid } = await setup([6]);
    const before = rowNodes(grid);
    const removed: unknown[] = [];
    const watch = new MutationObserver((ms) => ms.forEach((m) => removed.push(...m.removedNodes)));
    watch.observe(grid.querySelector(".tg-body")!, { childList: true });
    await press("ArrowDown");
    await press("ArrowDown");
    watch.disconnect();
    expect(focusAt().rowindex).toBe(ri(2));
    expect(removed).toEqual([]);
    for (const [k, el] of rowNodes(grid)) expect(el, `row ${k}`).toBe(before.get(k));
    expect(grid.querySelectorAll('.tg-body [role="row"][aria-selected="true"]').length).toBe(1);
    expect(grid.querySelectorAll('.tg-body [role="row"][tabindex="0"]').length).toBe(1);
  });

  it("a scroll of a few rows keeps the nodes still in the window, in order", async () => {
    const { grid } = await setup([6]);
    grid.scrollTop = 20 * 28;
    grid.dispatchEvent(new Event("scroll"));
    await flush(10);
    const before = rowNodes(grid);
    grid.scrollTop = 23 * 28;
    grid.dispatchEvent(new Event("scroll"));
    await flush(10);
    const after = rowNodes(grid);
    // The window moved by three rows: every row in both windows is the same node.
    const both = [...after.keys()].filter((k) => before.has(k) && k !== ri(0));
    expect(both.length).toBeGreaterThan(30);
    for (const k of both) expect(after.get(k), `row ${k}`).toBe(before.get(k));
    const order = [...grid.querySelectorAll('.tg-body [role="row"]')].map((r) => Number(r.getAttribute("aria-rowindex")));
    expect(order).toEqual([...order].sort((a, b) => a - b));
    // A kept node shows the row now at its index.
    const r30 = after.get(ri(30))!;
    expect(r30.getAttribute("aria-label")).toMatch(/^b28,/);
    await press("ArrowDown");
    expect(focusAt().rowindex).toBe(ri(1));
  });

  it("keeps the DOM under the row cap with the focused row pinned", async () => {
    const { grid } = await setup([6]);
    Object.defineProperty(grid, "clientHeight", { value: 1_000_000 });
    await press("ArrowDown");
    grid.scrollTop = 800 * 28;
    grid.dispatchEvent(new Event("scroll"));
    await flush(10);
    expect(grid.querySelectorAll('.tg-body [role="row"]').length).toBeLessThanOrEqual(201);
  });

  it("a crumb click leaves focus on the first row, not on the document body", async () => {
    const { g } = await setup();
    await press("Enter"); // into A
    const crumb = [...g.el.querySelectorAll<HTMLButtonElement>(".crumbs button")].find((b) => b.textContent === "All scanned folders")!;
    crumb.focus();
    crumb.click();
    await flush(10);
    expect(document.activeElement).not.toBe(document.body);
    expect(g.el.contains(document.activeElement)).toBe(true);
  });
});

describe("treegrid semantics (CT-F5, Core-AAM)", () => {
  it("names every column header and puts all of them before the rows in Tab order", async () => {
    const { grid } = await setup();
    const tabbable = [...grid.querySelectorAll<HTMLElement>("button, [tabindex]")].filter((e) => e.tabIndex >= 0 && !(e as HTMLButtonElement).disabled);
    const heads = [...grid.querySelectorAll<HTMLElement>('[role="columnheader"]')];
    expect(heads.map((h) => h.getAttribute("aria-label"))).toEqual(["Name", "Allocated", "Size", "Files", "Modified", "% of parent"]);
    // Six sort buttons, then the one focusable row; the grid container is not a stop of its own.
    expect(tabbable.map((e) => e.getAttribute("role") ?? e.tagName)).toEqual(["BUTTON", "BUTTON", "BUTTON", "BUTTON", "BUTTON", "BUTTON", "row"]);
    expect(tabbable.slice(0, 6).every((b) => (b.textContent ?? "").trim().length > 0)).toBe(true);
    expect(grid.getAttribute("tabindex")).toBeNull();
  });

  it("makes % of parent operable: its button sorts by allocated size", async () => {
    const { g, backend } = await setup();
    const pct = [...g.el.querySelectorAll<HTMLButtonElement>('[role="columnheader"] button')].find((b) => b.textContent?.startsWith("% of parent"))!;
    expect(pct).toBeDefined();
    pct.focus();
    pct.click();
    await flush(10);
    expect(backend.calls.find((c) => c.name === "treeSort")?.args).toEqual([1, "allocated", false]);
    // One header carries the sort state (ARIA: aria-sort on one header at a time): Allocated's.
    const sorted = [...g.el.querySelectorAll('[role="columnheader"]')].filter((x) => ["ascending", "descending"].includes(x.getAttribute("aria-sort") ?? ""));
    expect(sorted.map((x) => x.getAttribute("aria-label"))).toEqual(["Allocated"]);
    expect(document.activeElement).toBe(pct);
  });

  it("gives each row level, expanded state, place among siblings, index and a name", async () => {
    const { grid } = await setup([0]);
    expect(grid.getAttribute("aria-rowcount")).toBe("7"); // header + A, a1, a2, a3, B, c.txt
    expect(grid.getAttribute("aria-colcount")).toBe("6");
    const rows = [...grid.querySelectorAll<HTMLElement>('.tg-body [role="row"]')];
    const attrs = (r: HTMLElement) =>
      ["aria-rowindex", "aria-level", "aria-posinset", "aria-setsize", "aria-expanded"].map((k) => r.getAttribute(k));
    expect(rows.map(attrs)).toEqual([
      ["2", "1", "1", "3", "true"],
      ["3", "2", "1", "3", "false"],
      ["4", "2", "2", "3", null],
      ["5", "2", "3", "3", null],
      ["6", "1", "2", "3", "false"],
      ["7", "1", "3", "3", null],
    ]);
    expect(rows[1]?.getAttribute("aria-label")).toMatch(/^a1, /);
    expect(rows[0]?.getAttribute("aria-selected")).toBe("true");
    expect(rows[1]?.getAttribute("aria-selected")).toBe("false");
  });

  it("names every cell and gives it a column index; no marker glyph reaches the accessible text", async () => {
    const { grid } = await setup([0]);
    const first = grid.querySelector<HTMLElement>('.tg-body [role="row"]')!;
    const cells = [...first.querySelectorAll<HTMLElement>('[role="gridcell"]')];
    expect(cells.map((c) => c.getAttribute("aria-colindex"))).toEqual(["1", "2", "3", "4", "5", "6"]);
    for (const c of cells) expect(c.getAttribute("aria-label")?.trim()).toBeTruthy();
    expect(cells[0]?.getAttribute("aria-label")).toBe("A");
    expect(grid.textContent).not.toMatch(/[▸▾]/);
  });

  it("an empty tree keeps the grid reachable", async () => {
    const b = mockBackend({
      treePage: async (view, offset) => ({ ...(await mockBackend().treePage(view, offset, 0)), total: 0, rows: [] }),
    });
    const g = new TreeGrid(b, 1, () => undefined, "Linux");
    document.body.replaceChildren(g.el);
    await g.load();
    const grid = g.el.querySelector<HTMLElement>('[role="treegrid"]')!;
    expect(grid.getAttribute("tabindex")).toBe("0");
  });
});
