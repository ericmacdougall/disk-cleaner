// The size tree (spec AC-2, AC-4, AC-9): a virtualized tree grid. Only the visible rows (at
// most MAX_DOM_ROWS) are in the DOM; pages come from the backend by offset. Keyboard: Up/Down,
// PageUp/PageDown, Home/End move; Right opens a folder, Left closes it or goes to its parent;
// Enter drills into a folder; Backspace goes up one folder.

import type { Backend, Row, SortKey, TreePage } from "../api";
import { errorKind, errorMessage } from "../api";
import { h, replace } from "../dom";
import { formatBytes, formatCount, formatDate, formatPercent } from "../format";

export const ROW_HEIGHT = 28;
export const MAX_DOM_ROWS = 200;

const COLUMNS: { key: SortKey | null; label: string; cls: string }[] = [
  { key: "name", label: "Name", cls: "c-name" },
  { key: "allocated", label: "Allocated", cls: "c-num" },
  { key: "size", label: "Size", cls: "c-num" },
  { key: "files", label: "Files", cls: "c-num" },
  { key: "modified", label: "Modified", cls: "c-date" },
  // Percent of parent is in allocated order; sorting by Allocated sorts it.
  { key: null, label: "% of parent", cls: "c-pct" },
];

export function allocatedLabel(platform: string): string {
  // "Windows" only: "win" alone also matches "Darwin".
  return /windows/i.test(platform) ? "Allocated (file length)" : "Allocated";
}

export class TreeGrid {
  readonly el: HTMLElement;
  private scroller: HTMLDivElement;
  private spacer: HTMLDivElement;
  private body: HTMLDivElement;
  private head: HTMLDivElement;
  private crumbs: HTMLElement;
  private sourceLine: HTMLElement;
  private page: TreePage | null = null;
  private selected = 0;
  private request = 0;

  constructor(
    private backend: Backend,
    private view: number,
    private status: (text: string) => void,
    private platform: string = navigator.userAgent,
  ) {
    this.crumbs = h("nav", { "aria-label": "Folder path", class: "crumbs" });
    this.sourceLine = h("p", { class: "source" });
    this.head = h("div", { role: "row", class: "tg-row tg-head" });
    this.spacer = h("div", { class: "tg-spacer" });
    this.body = h("div", { class: "tg-body", role: "rowgroup" });
    this.scroller = h(
      "div",
      {
        class: "tg-scroll",
        role: "treegrid",
        tabindex: 0,
        "aria-label": "Size tree",
        "aria-multiselectable": "false",
        onkeydown: (e) => void this.onKey(e as KeyboardEvent),
        onscroll: () => void this.load(),
      },
      this.spacer,
      this.body,
    );
    this.el = h("section", { class: "tree", "aria-label": "Size tree" }, this.sourceLine, this.crumbs, h("div", { class: "tg" }, this.head, this.scroller));
  }

  /** Visible row count for the scroller's height. */
  private visibleRows(): number {
    const height = this.scroller.clientHeight || ROW_HEIGHT * 30;
    return Math.min(MAX_DOM_ROWS, Math.ceil(height / ROW_HEIGHT) + 10);
  }

  async load(): Promise<void> {
    const first = Math.max(0, Math.floor(this.scroller.scrollTop / ROW_HEIGHT) - 5);
    const id = ++this.request;
    try {
      const page = await this.backend.treePage(this.view, first, this.visibleRows());
      if (id !== this.request) return;
      this.page = page;
      this.render();
    } catch (e) {
      if (id !== this.request) return;
      this.status(errorKind(e) === "stale" ? "These results were replaced." : errorMessage(e));
    }
  }

  private render(): void {
    const p = this.page;
    if (!p) return;
    this.selected = Math.min(this.selected, Math.max(0, p.total - 1));
    const finished = p.source.finished_at ? new Date(p.source.finished_at * 1000).toLocaleString() : "not finished";
    this.sourceLine.textContent = `Source: size walk, finished ${finished}. Allocated totals of the size walk; the developer view has its own numbers.`;
    replace(
      this.crumbs,
      h("button", { type: "button", class: "link", onclick: () => void this.focus(null) }, "All scanned folders"),
      ...p.breadcrumb.flatMap((c) => [
        " / ",
        h("button", { type: "button", class: "link", onclick: () => void this.focus(c.id) }, c.name),
      ]),
    );
    replace(
      this.head,
      ...COLUMNS.map((c, i) => {
        const key = c.key;
        const active = key !== null && p.sort.key === key;
        const label = i === 1 ? allocatedLabel(this.platform) : c.label;
        const arrow = active ? (p.sort.descending ? " ↓" : " ↑") : "";
        return h(
          "div",
          {
            role: "columnheader",
            class: c.cls,
            "aria-sort": key === null ? null : active ? (p.sort.descending ? "descending" : "ascending") : "none",
          },
          key === null
            ? label
            : h("button", { type: "button", class: "sort", onclick: () => void this.sortBy(key) }, label, arrow),
        );
      }),
    );
    this.spacer.style.height = `${p.total * ROW_HEIGHT}px`;
    this.body.style.transform = `translateY(${p.offset * ROW_HEIGHT}px)`;
    this.scroller.setAttribute("aria-rowcount", String(p.total));
    replace(this.body, ...p.rows.map((r, i) => this.row(r, p.offset + i)));
    const sel = this.body.querySelector<HTMLElement>(`[data-index="${this.selected}"]`);
    if (sel) this.scroller.setAttribute("aria-activedescendant", sel.id);
    else this.scroller.removeAttribute("aria-activedescendant");
    if (p.total === 0) this.status("Nothing to show: the scanned folders are empty.");
  }

  private row(r: Row, index: number): HTMLElement {
    const notes = [
      r.unreadable ? "unreadable" : "",
      r.mount_skipped ? "other file system, not entered" : "",
      r.hardlink_duplicate ? "hard link counted once" : "",
      r.tag ? r.tag : "",
    ].filter(Boolean);
    const marker = r.kind === "dir" ? (r.has_children ? (r.expanded ? "▾ " : "▸ ") : "  ") : r.kind === "symlink" ? "@ " : "  ";
    return h(
      "div",
      {
        role: "row",
        id: `tg-row-${index}`,
        class: `tg-row${index === this.selected ? " selected" : ""}`,
        "data-index": index,
        "aria-rowindex": index + 1,
        "aria-level": r.depth + 1,
        "aria-expanded": r.kind === "dir" && r.has_children ? String(r.expanded) : null,
        "aria-selected": String(index === this.selected),
        onclick: () => {
          this.selected = index;
          this.render();
          this.scroller.focus();
        },
        ondblclick: () => void this.drill(r),
      },
      h(
        "div",
        { role: "gridcell", class: "c-name", style: `padding-left:${8 + r.depth * 16}px` },
        h("span", { "aria-hidden": "true", class: "marker" }, marker),
        h("span", { class: "name" }, r.name),
        notes.length ? h("span", { class: "note" }, ` [${notes.join(", ")}]`) : null,
      ),
      h("div", { role: "gridcell", class: "c-num" }, formatBytes(r.alloc)),
      h("div", { role: "gridcell", class: "c-num" }, formatBytes(r.size)),
      h("div", { role: "gridcell", class: "c-num" }, r.kind === "dir" ? formatCount(r.files) : ""),
      h("div", { role: "gridcell", class: "c-date" }, formatDate(r.mtime)),
      h(
        "div",
        { role: "gridcell", class: "c-pct" },
        h("span", { class: "bar", "aria-hidden": "true", style: `width:${Math.max(0, Math.min(100, r.percent_of_parent))}%` }),
        h("span", { class: "pct" }, formatPercent(r.percent_of_parent)),
      ),
    );
  }

  private current(): Row | null {
    const p = this.page;
    if (!p) return null;
    return p.rows[this.selected - p.offset] ?? null;
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
    this.scroller.scrollTop = 0;
    await this.run(() => this.backend.treeFocus(this.view, node));
  }

  private async drill(r: Row): Promise<void> {
    if (r.kind === "dir" && r.has_children) await this.focus(r.id);
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
    const top = this.selected * ROW_HEIGHT;
    const view = this.scroller.clientHeight || ROW_HEIGHT * 20;
    if (top < this.scroller.scrollTop) this.scroller.scrollTop = top;
    else if (top + ROW_HEIGHT > this.scroller.scrollTop + view) this.scroller.scrollTop = top + ROW_HEIGHT - view;
    await this.load();
  }

  async onKey(e: KeyboardEvent): Promise<void> {
    const pageRows = Math.max(1, Math.floor((this.scroller.clientHeight || ROW_HEIGHT * 20) / ROW_HEIGHT) - 1);
    const r = this.current();
    const keys: Record<string, () => Promise<void>> = {
      ArrowDown: () => this.move(this.selected + 1),
      ArrowUp: () => this.move(this.selected - 1),
      PageDown: () => this.move(this.selected + pageRows),
      PageUp: () => this.move(this.selected - pageRows),
      Home: () => this.move(0),
      End: () => this.move((this.page?.total ?? 1) - 1),
      ArrowRight: async () => {
        if (r && r.kind === "dir" && r.has_children && !r.expanded) await this.run(() => this.backend.treeToggle(this.view, r.id, true));
      },
      ArrowLeft: async () => {
        if (r && r.expanded) await this.run(() => this.backend.treeToggle(this.view, r.id, false));
        else await this.up();
      },
      Enter: async () => {
        if (r) await this.drill(r);
      },
      Backspace: () => this.up(),
    };
    const f = keys[e.key];
    if (!f) return;
    e.preventDefault();
    await f();
  }

  private async up(): Promise<void> {
    const crumbs = this.page?.breadcrumb ?? [];
    if (crumbs.length === 0) return;
    const parent = crumbs.length >= 2 ? crumbs[crumbs.length - 2] : undefined;
    await this.focus(parent ? parent.id : null);
  }
}
