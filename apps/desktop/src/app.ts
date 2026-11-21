// The app shell: chooser, scan progress with Cancel, and the result tabs (size tree, treemap,
// top files, developer view) plus Settings. Every control is a native button, link-button,
// input or tab with a label; the tab list follows the ARIA tabs pattern (arrow keys move).

import type { Backend, Place, TopFile } from "./api";
import { errorMessage } from "./api";
import { h, replace } from "./dom";
import { countOf, formatBytes, formatCount } from "./format";
import { ScanController, type ScanSnapshot } from "./scan";
import { StatusLine } from "./status";
import { DevView } from "./views/dev";
import { SettingsForm } from "./views/settings";
import { TreeGrid } from "./views/tree";
import { Treemap } from "./views/treemap";

type Tab = "tree" | "treemap" | "top" | "dev" | "settings";
const TABS: [Tab, string][] = [
  ["tree", "Size tree"],
  ["treemap", "Treemap"],
  ["top", "Top files"],
  ["dev", "Developer view"],
  ["settings", "Settings"],
];

export class App {
  readonly el: HTMLElement;
  private main: HTMLElement;
  /** The one owner of the status line (CT-F6): every status message goes through it. */
  private line = new StatusLine();
  private scan: ScanController;
  private chosen = new Map<string, Place>();
  private volumes: Place[] = [];
  private gitFacts = true;
  private tab: Tab = "tree";
  private shownView: number | null = null;
  private shownPhase: string | null = null;
  /** A scan file is being read. */
  private opening = false;
  private openButton: HTMLButtonElement | null = null;
  /** A plan export is running: Export is disabled until it ends (one export at a time). */
  private exporting = false;
  private exportButton: HTMLButtonElement | null = null;
  private views: { tree?: TreeGrid; treemap?: Treemap; dev?: DevView; settings?: SettingsForm } = {};

  constructor(private backend: Backend) {
    this.scan = new ScanController(backend);
    this.main = h("main", { id: "main", tabindex: -1 });
    this.el = h(
      "div",
      { class: "shell" },
      h("a", { href: "#main", class: "skip" }, "Skip to content"),
      h("header", {}, h("h1", {}, "PruneDisk"), h("p", { class: "tagline" }, "Read-only disk view: nothing here deletes, moves or backs up anything.")),
      this.main,
      this.line.el,
    );
    this.scan.subscribe((s) => this.onScan(s));
  }

  async start(): Promise<void> {
    await this.backend.onScanEvent((e) => this.scan.onEvent(e));
    try {
      const cur = await this.backend.currentState();
      if (cur.view !== null && (cur.state === "done" || cur.state === "idle")) {
        this.scan.showLoaded(cur.view, cur.has_analysis, cur.imported);
        return;
      }
    } catch {
      // Show the chooser.
    }
    this.renderChooser();
    // Not an operation: the drive list does not depend on what the user starts meanwhile. Its
    // error shows only while the chooser does.
    const say = this.line.setter("chooser");
    try {
      this.volumes = await this.backend.listVolumes();
      this.renderChooser();
    } catch (e) {
      say(`Cannot list drives: ${errorMessage(e)}`);
    }
  }

  private onScan(s: ScanSnapshot): void {
    switch (s.phase) {
      case "chooser":
        if (this.shownPhase !== "chooser") this.renderChooser();
        // The scan controller's own error (a refused Start); it drops late replies itself.
        if (s.error) this.line.setter("chooser")(s.error);
        break;
      case "starting":
      case "walking":
        this.renderProgress(s);
        break;
      case "analysing":
      case "done":
        this.renderResults(s);
        break;
      case "failed":
        this.renderFailed(s);
        break;
    }
    this.shownPhase = s.phase;
  }

  private renderChooser(): void {
    this.line.enter("chooser");
    this.shownView = null;
    this.views = {};
    const list = h(
      "ul",
      { class: "places", "aria-label": "Drives" },
      ...this.volumes.map((v) => {
        const id = `vol-${v.path}`;
        const box = h("input", {
          type: "checkbox",
          id,
          checked: this.chosen.has(v.path),
          onchange: (e) => {
            if ((e.target as HTMLInputElement).checked) this.chosen.set(v.path, v);
            else this.chosen.delete(v.path);
            this.renderChooser();
          },
        });
        const space = v.total_bytes !== null ? `, ${formatBytes(v.available_bytes ?? 0)} free of ${formatBytes(v.total_bytes)}` : "";
        return h("li", {}, box, h("label", { for: id }, ` ${v.label}${space}${v.removable ? ", removable" : ""}`));
      }),
    );
    const chosenList = h(
      "ul",
      { class: "chosen", "aria-label": "Chosen folders" },
      ...[...this.chosen.values()].map((p) =>
        h("li", {}, p.label, " ", h("button", { type: "button", class: "link", "aria-label": `Remove ${p.label}`, onclick: () => { this.chosen.delete(p.path); this.renderChooser(); } }, "Remove")),
      ),
    );
    const starting = this.scan.snapshot.phase === "starting";
    // Disabled while a file is read; the backend also refuses a second file or a scan then.
    this.openButton = h("button", { type: "button", disabled: this.opening, onclick: () => void this.openFile() }, "Open a scan file...");
    replace(
      this.main,
      h("h2", {}, "Choose what to scan"),
      h("p", {}, "Choose one or more drives or folders. The scan reads sizes and metadata only."),
      list,
      // Disabled while a file is read too: one chooser operation at a time, so the read's
      // answer is not superseded (CT-F6).
      h("button", { type: "button", disabled: this.opening, onclick: () => void this.addFolders() }, "Add folders..."),
      " ",
      this.openButton,
      h("h3", {}, "Chosen"),
      this.chosen.size ? chosenList : h("p", {}, "Nothing chosen yet."),
      h(
        "div",
        { class: "field" },
        h("input", { type: "checkbox", id: "git-facts", checked: this.gitFacts, onchange: (e) => { this.gitFacts = (e.target as HTMLInputElement).checked; } }),
        h("label", { for: "git-facts" }, " Read git facts (local, read-only git commands)"),
      ),
      h(
        "button",
        { type: "button", class: "primary", disabled: this.chosen.size === 0 || starting || this.opening, onclick: () => void this.startScan() },
        "Scan",
      ),
    );
  }

  private async addFolders(): Promise<void> {
    const op = this.line.begin("chooser");
    try {
      for (const p of await this.backend.chooseFolders()) this.chosen.set(p.path, p);
    } catch (e) {
      op.set(errorMessage(e));
    }
    // Only the chooser lists the chosen folders; it shows them when it is next drawn.
    if (this.line.scope === "chooser") this.renderChooser();
  }

  /** Open a scan file. One at a time (PR gate F5): Open and Scan are disabled until it ends. */
  private async openFile(): Promise<void> {
    if (this.scan.running || this.opening) return;
    this.opening = true;
    if (this.openButton?.isConnected) this.renderChooser();
    const op = this.line.begin("chooser");
    op.set("Reading the scan file...");
    try {
      const cur = await this.backend.openScanFile();
      // Showing the results leaves the chooser, which ends this operation and its message.
      // showLoaded refuses while a scan runs.
      if (cur && cur.view !== null) this.scan.showLoaded(cur.view, cur.has_analysis, true);
      else op.set("");
    } catch (e) {
      op.set(`Cannot open that scan file: ${errorMessage(e)}`);
    } finally {
      this.opening = false;
      // Still on the chooser (nothing was opened): enable Open again and put focus back on it.
      if (this.openButton?.isConnected) {
        this.renderChooser();
        this.openButton.focus();
      }
    }
  }

  private async startScan(): Promise<void> {
    const roots = [...this.chosen.keys()];
    await this.scan.start(roots, this.gitFacts);
  }

  private renderProgress(s: ScanSnapshot): void {
    this.line.enter("scan");
    const p = s.progress;
    const secs = p ? Math.round(p.elapsed_ms / 1000) : 0;
    const body = p
      ? `${countOf(p.files, "file", "files")}, ${countOf(p.dirs, "folder", "folders")}, ${formatBytes(p.bytes)}, ${secs} s. Reading ${p.current}`
      : "Starting...";
    if (this.shownPhase === "walking" || this.shownPhase === "starting") {
      const line = this.main.querySelector(".progress-line");
      if (line) {
        line.textContent = body;
        return;
      }
    }
    replace(
      this.main,
      h("h2", {}, "Scanning"),
      h("p", { class: "progress-line", role: "status", "aria-live": "off" }, body),
      h("p", { class: "help" }, "A folder on a network share that does not answer can delay Cancel."),
      h("button", { type: "button", onclick: () => this.scan.cancel() }, "Cancel"),
    );
    this.main.querySelector<HTMLButtonElement>("button")?.focus();
  }

  private renderFailed(s: ScanSnapshot): void {
    this.line.enter("failed");
    replace(
      this.main,
      h("h2", {}, "The scan failed"),
      h("p", { role: "alert" }, s.error ?? "Unknown error."),
      h("button", { type: "button", onclick: () => this.scan.newScan() }, "Back"),
    );
  }

  private renderResults(s: ScanSnapshot): void {
    const view = s.view;
    if (view === null) return;
    if (this.shownView !== view) {
      this.shownView = view;
      this.views = {};
      if (s.imported) this.tab = "dev";
    }
    this.line.enter(`results/${view}/${this.tab}`);
    const tabs = TABS.filter(([t]) => !(s.imported && (t === "tree" || t === "treemap" || t === "top")));
    const list = h(
      "div",
      { role: "tablist", "aria-label": "Results", onkeydown: (e) => this.tabKey(e as KeyboardEvent, tabs.map(([t]) => t)) },
      ...tabs.map(([t, label]) =>
        h("button", { type: "button", role: "tab", id: `tab-${t}`, "aria-selected": String(t === this.tab), "aria-controls": "panel", tabindex: t === this.tab ? 0 : -1, onclick: () => this.select(t) }, label),
      ),
    );
    const panel = h("div", { id: "panel", role: "tabpanel", "aria-labelledby": `tab-${this.tab}`, tabindex: -1 });
    const bar = h(
      "div",
      { class: "bar" },
      s.phase === "analysing"
        ? h("span", {}, "Analysing repositories and building the plan... ", h("button", { type: "button", onclick: () => this.scan.cancel() }, "Cancel"))
        : h("span", {}, s.imported ? "Imported scan file. " : "Scan complete. "),
      (this.exportButton = h("button", { type: "button", disabled: !s.analysed || this.exporting, onclick: () => void this.exportPlan(view) }, "Export plan...")),
      " ",
      h("button", { type: "button", disabled: s.phase === "analysing", onclick: () => this.scan.newScan() }, "New scan"),
    );
    replace(this.main, bar, list, panel);
    void this.fill(panel, view, s);
  }

  private tabKey(e: KeyboardEvent, order: Tab[]): void {
    const i = order.indexOf(this.tab);
    const next = e.key === "ArrowRight" ? order[(i + 1) % order.length] : e.key === "ArrowLeft" ? order[(i - 1 + order.length) % order.length] : e.key === "Home" ? order[0] : e.key === "End" ? order[order.length - 1] : undefined;
    if (!next) return;
    e.preventDefault();
    this.select(next);
    document.getElementById(`tab-${next}`)?.focus();
  }

  private select(t: Tab): void {
    this.tab = t;
    this.renderResults(this.scan.snapshot);
    document.getElementById(`tab-${t}`)?.focus();
  }

  private async fill(panel: HTMLElement, view: number, s: ScanSnapshot): Promise<void> {
    // Each view writes the status line only while its own tab of these results shows.
    const st = this.line.setter(`results/${view}/${this.tab}`);
    switch (this.tab) {
      case "tree": {
        const v = (this.views.tree ??= new TreeGrid(this.backend, view, st));
        panel.append(v.el);
        await v.load();
        break;
      }
      case "treemap": {
        const v = (this.views.treemap ??= new Treemap(this.backend, view, st));
        panel.append(v.el);
        await v.load();
        break;
      }
      case "top":
        panel.append(await this.topFiles(view));
        break;
      case "dev": {
        if (!s.analysed) {
          panel.append(h("p", {}, "The analysis is still running (repositories, git facts and the plan)."));
          break;
        }
        const v = (this.views.dev ??= new DevView(this.backend, view, st));
        panel.append(v.el);
        await v.load();
        break;
      }
      case "settings": {
        const v = (this.views.settings ??= new SettingsForm(this.backend, (replanned) => {
          if (replanned) this.views.dev = undefined;
        }));
        panel.append(v.el);
        await v.load();
        break;
      }
    }
  }

  private async topFiles(view: number): Promise<HTMLElement> {
    let files: TopFile[] = [];
    try {
      files = await this.backend.topFiles(view, 1000);
    } catch (e) {
      return h("p", {}, errorMessage(e));
    }
    return h(
      "section",
      { "aria-label": "Top files" },
      h("p", { class: "source" }, "Source: size walk. Largest files by allocated size."),
      files.length === 0
        ? h("p", {}, "No files.")
        : h(
            "table",
            { class: "items" },
            h("caption", {}, files.length === 1 ? "The largest file" : `The ${formatCount(files.length)} largest files`),
            h("thead", {}, h("tr", {}, h("th", { scope: "col" }, "Allocated"), h("th", { scope: "col" }, "Path"))),
            h("tbody", {}, ...files.map((f) => h("tr", {}, h("td", { class: "c-num" }, formatBytes(f.bytes)), h("td", { class: "path" }, f.path)))),
          ),
    );
  }

  /** Export the plan. One at a time: a click while one runs does nothing. */
  private async exportPlan(view: number): Promise<void> {
    if (this.exporting) return;
    this.exporting = true;
    this.syncExportButton();
    // Bound to these results, not to a tab: the answer still shows after a tab change in the
    // same results, and never after New scan.
    const op = this.line.begin(`results/${view}`);
    try {
      const where = await this.backend.exportPlan(view);
      op.set(where ? exportedText(where) : "");
    } catch (e) {
      op.set(`Not exported: ${errorMessage(e)}`);
    } finally {
      this.exporting = false;
      this.syncExportButton();
    }
  }

  /** Enable or disable the Export button shown, in place (a re-render would reload the tab). */
  private syncExportButton(): void {
    const b = this.exportButton;
    if (b?.isConnected) b.disabled = this.exporting || !this.scan.snapshot.analysed;
  }
}

/** The export confirmation (CT-c): the file written, and that nothing else changed. */
export function exportedText(where: string): string {
  return `Plan exported to ${where}. Nothing else was changed.`;
}
