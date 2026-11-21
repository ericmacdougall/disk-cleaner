// The size tree (spec AC-2, AC-4, AC-9): a virtualized WAI-ARIA treegrid. Only the rows near the
// view are in the DOM, plus the focused row when it is scrolled away; pages come from the backend
// by offset. At most MAX_DOM_ROWS `.tg-row` elements are in the DOM in every state: the header
// row, the page and a pinned focused row together. A slot is kept for the pin only while one is
// shown.
//
// Keyboard (APG treegrid pattern, CT-F4): one focus owner by roving tabindex. Focus is on a row,
// or on one of its cells after Right. Up/Down, PageUp/PageDown move a row (keeping the cell
// column); Home/End go to the first/last row, or the first/last cell when a cell has focus;
// Ctrl+Home/Ctrl+End go to the first/last row. Right opens a closed folder, otherwise moves to
// the first cell and then cell by cell. Left moves cell by cell back to the row, closes an open
// folder, or moves to the parent row (as the APG treegrid example does). Enter drills into a
// folder; Backspace goes up one folder. The column headers' sort buttons come before the rows in
// Tab order; the rows are one Tab stop.
//
// The focused row's node is kept across renders and pinned in the DOM while scrolled out of the
// rendered rows, so focus (and a screen reader's place) never drops to the container (gate F6).

import type { Backend, Row, SortKey, TreePage } from "../api";
import { errorKind, errorMessage } from "../api";
import { h, replace } from "../dom";
import { formatBytes, formatCount, formatDate, formatPercent } from "../format";

export const ROW_HEIGHT = 28;
export const MAX_DOM_ROWS = 200;
const COLS = 6;

const COLUMNS: { sort: SortKey; label: string; cls: string; hint?: string }[] = [
  { sort: "name", label: "Name", cls: "c-name" },
  { sort: "allocated", label: "Allocated", cls: "c-num" },
  { sort: "size", label: "Size", cls: "c-num" },
  { sort: "files", label: "Files", cls: "c-num" },
  { sort: "modified", label: "Modified", cls: "c-date" },
  // Percent of parent is in allocated order: its button sorts by Allocated.
  { sort: "allocated", label: "% of parent", cls: "c-pct", hint: "Sorts by allocated size." },
];

export function allocatedLabel(platform: string): string {
  // "Windows" only: "win" alone also matches "Darwin".
  return /windows/i.test(platform) ? "Allocated (file length)" : "Allocated";
}

/** The visible text of a row's cells, in column order. */
function cellTexts(r: Row): [string, string, string, string, string, string] {
  const notes = [
    r.kind === "symlink" ? "symbolic link" : "",
    r.unreadable ? "unreadable" : "",
    r.mount_skipped ? "other file system, not entered" : "",
    r.hardlink_duplicate ? "hard link counted once" : "",
    r.tag ? r.tag : "",
  ].filter(Boolean);
  return [
    notes.length ? `${r.name} [${notes.join(", ")}]` : r.name,
    formatBytes(r.alloc),
    formatBytes(r.size),
    r.kind === "dir" ? formatCount(r.files) : "",
    formatDate(r.mtime),
    formatPercent(r.percent_of_parent),
  ];
}

const isParent = (r: Row) => r.kind === "dir" && r.has_children;

export class TreeGrid {
  readonly el: HTMLElement;
  private scroller: HTMLDivElement;
  private spacer: HTMLDivElement;
  private body: HTMLDivElement;
  private head: HTMLDivElement;
  private heads: { cell: HTMLElement; button: HTMLButtonElement; arrow: HTMLElement }[];
  private crumbs: HTMLElement;
  private crumbKey = "";
  private sourceLine: HTMLElement;
  private page: TreePage | null = null;
  /** The selected row's data when it is outside the rendered page. */
  private pinned: Row | null = null;
  private selected = 0;
  /** The focused cell's column, or null when the row itself has focus. */
  private col: number | null = null;
  /** The selected row's node: kept across renders so focus stays on it. */
  private focusRow: HTMLElement | null = null;
  /** Put focus on the selected row at the next render (after a key, a click or a crumb). */
  private wantFocus = false;
  private request = 0;
  private keys: Promise<void> = Promise.resolve();

  constructor(
    private backend: Backend,
    private view: number,
    private status: (text: string) => void,
    private platform: string = navigator.userAgent,
  ) {
    this.crumbs = h("nav", { "aria-label": "Folder path", class: "crumbs" });
    this.sourceLine = h("p", { class: "source" });
    // One treegrid owns the header row (row 1) and the data rows (rows 2 on), each in a
    // rowgroup (PR gate F6). The header stays in view while the rows scroll under it. Header
    // cells are built once and updated in place, so a focused sort button keeps focus.
    this.heads = COLUMNS.map((c, i) => {
      const label = i === 1 ? allocatedLabel(this.platform) : c.label;
      const arrow = h("span", { "aria-hidden": "true", class: "arrow" });
      const button = h(
        "button",
        { type: "button", class: "sort", "aria-label": label, title: c.hint ?? null, onclick: () => void this.sortBy(c.sort) },
        label,
        arrow,
      );
      const cell = h("div", { role: "columnheader", class: c.cls, "aria-colindex": i + 1, "aria-label": label }, button);
      return { cell, button, arrow };
    });
    this.head = h("div", { role: "row", class: "tg-row tg-head", "aria-rowindex": 1 }, ...this.heads.map((x) => x.cell));
    this.spacer = h("div", { class: "tg-spacer", "aria-hidden": "true" });
    this.body = h("div", { class: "tg-body", role: "rowgroup" });
    this.scroller = h(
      "div",
      {
        class: "tg-scroll",
        role: "treegrid",
        "aria-label": "Size tree",
        "aria-multiselectable": "false",
        "aria-readonly": "true",
        "aria-colcount": COLS,
        onkeydown: (e) => void this.onKey(e as KeyboardEvent),
        onscroll: () => void this.load(),
      },
      h("div", { class: "tg-headgroup", role: "rowgroup" }, this.head),
      this.spacer,
      this.body,
    );
    this.el = h("section", { class: "tree", "aria-label": "Size tree" }, this.sourceLine, this.crumbs, h("div", { class: "tg" }, this.scroller));
  }

  /** Data rows for the scroller's height: at most MAX_DOM_ROWS - 1, as the header row counts. */
  private visibleRows(): number {
    const height = this.scroller.clientHeight || ROW_HEIGHT * 30;
    return Math.min(MAX_DOM_ROWS - 1, Math.ceil(height / ROW_HEIGHT) + 10);
  }

  async load(): Promise<void> {
    const first = Math.max(0, Math.floor(this.scroller.scrollTop / ROW_HEIGHT) - 5);
    const id = ++this.request;
    try {
      const wanted = this.selected;
      // The selected row is scrolled away: fetch it alone (alongside the page), to keep its
      // node and so focus. Only then is a slot kept for it.
      const visible = this.visibleRows();
      const away = wanted < first || wanted >= first + visible;
      const limit = away ? Math.min(visible, MAX_DOM_ROWS - 2) : visible;
      const [page, alone] = await Promise.all([
        this.backend.treePage(this.view, first, limit),
        away ? this.backend.treePage(this.view, wanted, 1) : Promise.resolve(null),
      ]);
      if (id !== this.request) return;
      const sel = Math.min(this.selected, Math.max(0, page.total - 1));
      let pinned: Row | null = null;
      if (page.total > 0 && (sel < page.offset || sel >= page.offset + page.rows.length)) {
        pinned = alone && alone.offset === sel ? (alone.rows[0] ?? null) : (await this.backend.treePage(this.view, sel, 1)).rows[0] ?? null;
        if (id !== this.request) return;
      }
      // The cap holds whatever page came back: the header, the pinned row and the page together
      // are at most MAX_DOM_ROWS rows (spec AC-9).
      const room = MAX_DOM_ROWS - 1 - (pinned ? 1 : 0);
      this.page = page.rows.length > room ? { ...page, rows: page.rows.slice(0, room) } : page;
      this.pinned = pinned;
      this.render();
    } catch (e) {
      if (id !== this.request) return;
      this.status(errorKind(e) === "stale" ? "These results were replaced." : errorMessage(e));
    }
  }

  /** The row data at an index, when it is loaded. */
  private rowAt(index: number): Row | null {
    const p = this.page;
    if (!p) return null;
    const r = p.rows[index - p.offset];
    if (r) return r;
    return index === this.selected ? this.pinned : null;
  }

  private renderChrome(p: TreePage): void {
    const finished = p.source.finished_at ? new Date(p.source.finished_at * 1000).toLocaleString() : "not finished";
    this.sourceLine.textContent = `Source: size walk, finished ${finished}. Allocated totals of the size walk; the developer view has its own numbers.`;
    // Rebuilt only when the path changes, so a focused crumb keeps focus while rows scroll.
    const key = JSON.stringify(p.breadcrumb.map((c) => [c.id, c.name]));
    if (key !== this.crumbKey) {
      this.crumbKey = key;
      const go = (node: number | null) => {
        this.wantFocus = true;
        void this.focus(node);
      };
      replace(
        this.crumbs,
        h("button", { type: "button", class: "link", onclick: () => go(null) }, "All scanned folders"),
        ...p.breadcrumb.flatMap((c) => [" / ", h("button", { type: "button", class: "link", onclick: () => go(c.id) }, c.name)]),
      );
    }
    COLUMNS.forEach((c, i) => {
      const x = this.heads[i]!;
      // The % column sorts by Allocated but does not repeat its sort state: one sorted header.
      const active = i !== 5 && p.sort.key === c.sort;
      if (i === 5) x.cell.removeAttribute("aria-sort");
      else x.cell.setAttribute("aria-sort", active ? (p.sort.descending ? "descending" : "ascending") : "none");
      x.arrow.textContent = active ? (p.sort.descending ? " ↓" : " ↑") : "";
    });
  }

  private render(): void {
    const p = this.page;
    if (!p) return;
    const active = document.activeElement;
    const hadFocus = this.wantFocus || (active !== null && this.body.contains(active));
    this.wantFocus = false;
    this.selected = Math.min(this.selected, Math.max(0, p.total - 1));
    const sel = this.selected;
    this.renderChrome(p);
    this.spacer.style.height = `${p.total * ROW_HEIGHT}px`;
    this.body.style.transform = `translateY(${p.offset * ROW_HEIGHT}px)`;
    // The header row counts.
    this.scroller.setAttribute("aria-rowcount", String(p.total + 1));
    // An empty grid stays reachable; otherwise the focused row is the grid's Tab stop.
    if (p.total === 0) this.scroller.setAttribute("tabindex", "0");
    else this.scroller.removeAttribute("tabindex");

    // The selected row: the node that has (or is to get) focus, updated in place.
    const data = this.rowAt(sel);
    let selEl: HTMLElement | null = null;
    if (data) {
      const keep =
        this.focusRow && this.focusRow.dataset.index === String(sel)
          ? this.focusRow
          : this.body.querySelector<HTMLElement>(`:scope > [data-index="${sel}"]`);
      selEl = keep ? this.patch(keep, data, sel) : this.row(data, sel);
      if (!selEl.parentElement) this.body.append(selEl);
      const inWindow = sel >= p.offset && sel < p.offset + p.rows.length;
      // Out of the window it is pinned where it belongs, out of view, and out of the flow.
      selEl.style.position = inWindow ? "" : "absolute";
      selEl.style.top = inWindow ? "" : `${(sel - p.offset) * ROW_HEIGHT}px`;
      selEl.style.left = inWindow ? "" : "0";
      selEl.style.right = inWindow ? "" : "0";
      if (this.col !== null && this.col >= COLS) this.col = COLS - 1;
      // Move focus before the old node goes, so focus never passes through the body.
      const target = this.col === null ? selEl : (selEl.children[this.col] as HTMLElement | undefined);
      if (hadFocus && target && document.activeElement !== target) target.focus({ preventScroll: true });
    }
    this.focusRow = selEl;
    // Keyed by row index: a row node still in the window is kept and updated in place, so a key
    // press or a scroll of a few rows adds and removes only the rows that enter or leave. Every
    // node replaced is a burst of accessibility events a screen reader works through before it
    // speaks the next focus (gate F6: Orca fell silent after a wheel over a 5,000-row folder).
    const kept = new Map<number, HTMLElement>();
    for (const c of [...this.body.children] as HTMLElement[]) {
      if (c === selEl) continue;
      const i = Number(c.dataset.index);
      if (i >= p.offset && i < p.offset + p.rows.length && i !== sel && !kept.has(i)) kept.set(i, c);
      else c.remove();
    }
    const before: HTMLElement[] = [];
    const after: HTMLElement[] = [];
    p.rows.forEach((r, i) => {
      const index = p.offset + i;
      if (index === sel && selEl) return;
      const old = kept.get(index);
      // A row pinned while it was selected and scrolled away rejoins the flow.
      if (old && old.style.cssText) old.style.cssText = "";
      (index < sel || !selEl ? before : after).push(old ? this.patch(old, r, index) : this.row(r, index));
    });
    // Place in index order around the selected row, which never moves (moving it would drop
    // focus); a kept node already in its place is not touched.
    const place = (list: HTMLElement[], start: Element | null, stop: Element | null) => {
      let at = start;
      for (const n of list) {
        if (n === at) at = at.nextElementSibling;
        else this.body.insertBefore(n, at ?? stop);
      }
    };
    if (selEl) {
      place(before, this.body.firstElementChild, selEl);
      place(after, selEl.nextElementSibling, null);
    } else {
      place(before, this.body.firstElementChild, null);
    }
    if (p.total === 0) this.status("Nothing to show: the scanned folders are empty.");
  }

  /** Attributes of a data row (aria-selected and tabindex follow the selection). */
  private rowAttrs(r: Row, index: number): Record<string, string | null> {
    const texts = cellTexts(r);
    const on = index === this.selected;
    return {
      role: "row",
      id: `tg-row-${index}`,
      class: `tg-row${on ? " selected" : ""}`,
      "data-index": String(index),
      // Row 1 is the header.
      "aria-rowindex": String(index + 2),
      "aria-level": String(r.depth + 1),
      "aria-posinset": String(r.posinset),
      "aria-setsize": String(r.setsize),
      "aria-expanded": isParent(r) ? String(r.expanded) : null,
      "aria-selected": String(on),
      "aria-label": `${texts[0]}, ${texts[1]} allocated, ${texts[2]} size${r.kind === "dir" ? `, ${texts[3]} files` : ""}, modified ${texts[4]}, ${texts[5]} of parent`,
      tabindex: on ? "0" : "-1",
    };
  }

  private cells(r: Row, indent: number): HTMLElement[] {
    const texts = cellTexts(r);
    const marker = isParent(r) ? (r.expanded ? "open" : "closed") : "none";
    return [
      h(
        "div",
        { role: "gridcell", class: "c-name", "aria-colindex": 1, "aria-label": texts[0], tabindex: -1, style: `padding-left:${8 + indent * 16}px` },
        // Drawn by CSS: a text glyph would reach the accessible text even when aria-hidden.
        h("span", { "aria-hidden": "true", class: "marker", "data-marker": marker }),
        h("span", { class: "name" }, r.name),
        texts[0] !== r.name ? h("span", { class: "note" }, texts[0].slice(r.name.length)) : null,
      ),
      h("div", { role: "gridcell", class: "c-num", "aria-colindex": 2, "aria-label": texts[1], tabindex: -1 }, texts[1]),
      h("div", { role: "gridcell", class: "c-num", "aria-colindex": 3, "aria-label": texts[2], tabindex: -1 }, texts[2]),
      h("div", { role: "gridcell", class: "c-num", "aria-colindex": 4, "aria-label": texts[3] || "none", tabindex: -1 }, texts[3]),
      h("div", { role: "gridcell", class: "c-date", "aria-colindex": 5, "aria-label": texts[4], tabindex: -1 }, texts[4]),
      h(
        "div",
        { role: "gridcell", class: "c-pct", "aria-colindex": 6, "aria-label": texts[5], tabindex: -1 },
        h("span", { class: "bar", "aria-hidden": "true", style: `width:${Math.max(0, Math.min(100, r.percent_of_parent))}%` }),
        h("span", { class: "pct" }, texts[5]),
      ),
    ];
  }

  private row(r: Row, index: number): HTMLElement {
    const el = h(
      "div",
      {
        ...this.rowAttrs(r, index),
        onclick: (e) => {
          this.selected = index;
          this.col = null;
          this.focusRow = e.currentTarget as HTMLElement;
          this.wantFocus = true;
          this.render();
        },
        ondblclick: () => {
          const d = this.rowAt(index);
          if (d) void this.drill(d);
        },
      },
      ...this.cells(r, r.depth),
    );
    return el;
  }

  /** Update a kept row node in place: same row and cell nodes, new attributes and text. A cell
   *  whose content did not change keeps its child nodes, so a click's target stays inside the
   *  row and the double click that follows reaches the row's handler. */
  private patch(el: HTMLElement, r: Row, index: number): HTMLElement {
    for (const [k, v] of Object.entries(this.rowAttrs(r, index))) {
      if (v === null) el.removeAttribute(k);
      else if (el.getAttribute(k) !== v) el.setAttribute(k, v);
    }
    this.cells(r, r.depth).forEach((fresh, i) => {
      const cell = el.children[i] as HTMLElement | undefined;
      if (!cell || cell.isEqualNode(fresh)) return;
      for (const a of [...fresh.attributes]) if (cell.getAttribute(a.name) !== a.value) cell.setAttribute(a.name, a.value);
      cell.style.cssText = fresh.style.cssText;
      cell.replaceChildren(...fresh.childNodes);
    });
    return el;
  }

  private current(): Row | null {
    return this.rowAt(this.selected);
  }

  private async sortBy(key: SortKey): Promise<void> {
    if (!this.page) return;
    const same = this.page.sort.key === key;
    const descending = same ? !this.page.sort.descending : key !== "name";
    await this.run(() => this.backend.treeSort(this.view, key, descending));
    this.status(`Sorted by ${key}, ${descending ? "largest" : "smallest"} first.`);
  }

  async focus(node: number | null): Promise<void> {
    this.selected = 0;
    this.col = null;
    this.scroller.scrollTop = 0;
    await this.run(() => this.backend.treeFocus(this.view, node));
  }

  private async drill(r: Row): Promise<void> {
    if (isParent(r)) {
      if (this.body.contains(document.activeElement)) this.wantFocus = true;
      await this.focus(r.id);
    }
  }

  private async run(f: () => Promise<unknown>): Promise<void> {
    try {
      await f();
    } catch (e) {
      this.status(errorMessage(e));
    }
    await this.load();
  }

  private async move(to: number): Promise<void> {
    const total = this.page?.total ?? 0;
    if (total === 0) return;
    this.selected = Math.max(0, Math.min(total - 1, to));
    this.wantFocus = true;
    // Row i sits at ROW_HEIGHT * (i + 1): below the header row, which covers the top of the view.
    const top = this.selected * ROW_HEIGHT;
    const view = this.scroller.clientHeight || ROW_HEIGHT * 20;
    if (top < this.scroller.scrollTop) this.scroller.scrollTop = top;
    else if (top + 2 * ROW_HEIGHT > this.scroller.scrollTop + view) this.scroller.scrollTop = top + 2 * ROW_HEIGHT - view;
    await this.load();
  }

  /** Focus another cell of the selected row (or the row, for null) without a reload. */
  private toCell(col: number | null): void {
    this.col = col === null ? null : Math.max(0, Math.min(COLS - 1, col));
    this.wantFocus = true;
    this.render();
  }

  private async toggle(r: Row, open: boolean): Promise<void> {
    this.wantFocus = this.body.contains(document.activeElement);
    await this.run(() => this.backend.treeToggle(this.view, r.id, open));
  }

  /** The action for a key, or null when the grid does not handle it. */
  private action(e: KeyboardEvent): (() => Promise<void>) | null {
    const ctrl = e.ctrlKey || e.metaKey;
    if (e.altKey || e.shiftKey) return null;
    const pageRows = () => Math.max(1, Math.floor((this.scroller.clientHeight || ROW_HEIGHT * 20) / ROW_HEIGHT) - 1);
    const last = () => (this.page?.total ?? 1) - 1;
    const r = () => this.current();
    switch (e.key) {
      case "ArrowDown":
        return () => this.move(this.selected + 1);
      case "ArrowUp":
        return () => this.move(this.selected - 1);
      case "PageDown":
        return () => this.move(this.selected + pageRows());
      case "PageUp":
        return () => this.move(this.selected - pageRows());
      case "Home":
        return async () => (ctrl || this.col === null ? this.move(0) : this.toCell(0));
      case "End":
        return async () => (ctrl || this.col === null ? this.move(last()) : this.toCell(COLS - 1));
      case "ArrowRight":
        return async () => {
          const row = r();
          if (this.col !== null) this.toCell(this.col + 1);
          else if (row && isParent(row) && !row.expanded) await this.toggle(row, true);
          else this.toCell(0);
        };
      case "ArrowLeft":
        return async () => {
          const row = r();
          if (this.col !== null) this.toCell(this.col === 0 ? null : this.col - 1);
          else if (row && isParent(row) && row.expanded) await this.toggle(row, false);
          else if (row && row.parent_row !== null) await this.move(row.parent_row);
        };
      case "Enter":
        return async () => {
          const row = r();
          if (row) await this.drill(row);
        };
      case "Backspace":
        return () => this.up();
      default:
        return null;
    }
  }

  async onKey(e: KeyboardEvent): Promise<void> {
    // The sort buttons handle their own keys.
    if (e.target instanceof Node && this.head.contains(e.target)) return;
    const f = this.action(e);
    if (!f) return;
    e.preventDefault();
    // One key at a time, in order: each acts on the selection the previous one left.
    const step = () => f().catch((err: unknown) => this.status(errorMessage(err)));
    this.keys = this.keys.then(step);
    await this.keys;
  }

  private async up(): Promise<void> {
    const crumbs = this.page?.breadcrumb ?? [];
    if (crumbs.length === 0) return;
    const parent = crumbs.length >= 2 ? crumbs[crumbs.length - 2] : undefined;
    if (this.body.contains(document.activeElement)) this.wantFocus = true;
    await this.focus(parent ? parent.id : null);
  }
}
