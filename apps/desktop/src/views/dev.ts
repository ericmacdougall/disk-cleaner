// The developer view (spec AC-4, AC-11, AC-13): core's analysis, labelled as such, with its
// own totals (never the size walk's): per-class totals, reclaimable totals by action, the
// coverage note, dry run and remote facts, items per class with action, reason and state,
// and agent session folders ("no plan action yet"). The Apply area is a disabled explanation.

import type { Backend, DevItem, DevSummary, SessionFolder } from "../api";
import { errorMessage } from "../api";
import { h, replace } from "../dom";
import { formatBytes, formatCount, formatDate } from "../format";

export const DEV_PAGE = 100;

export function summaryLines(s: DevSummary): string[] {
  const t = s.plan_totals;
  const lines = [
    `Source: analysis (core scan and plan). Scan made ${formatDate(s.scan_generated_at)}, plan made ${formatDate(s.plan_generated_at)}.`,
    `Dry run: ${s.dry_run ? "yes, nothing is changed" : "no"}. Remote facts verified: ${s.remote_facts_verified ? "yes" : "no"}. ${s.remote_facts_note}`,
    `Reclaimable if every removing action completed: ${formatBytes(t.reclaimable_bytes)} (ready now, regenerable only: ${formatBytes(t.ready_bytes)}).`,
    `By action: ${Object.entries(t.by_action)
      .map(([a, x]) => `${a} ${formatCount(x.items)} (${formatBytes(x.reclaimable_bytes)})`)
      .join(", ") || "none"}.`,
    `Active window: ${s.active_window_days} days (${s.window_source}). Process check: ${s.process_check}.`,
  ];
  if (s.process_check_note) lines.push(s.process_check_note);
  lines.push(s.coverage_complete ? "Coverage: complete." : `Coverage: incomplete. ${s.coverage_note ?? ""}`);
  if (s.imported_note) lines.push(s.imported_note);
  if (s.stale_warning) lines.push(`Warning: ${s.stale_warning}`);
  return lines;
}

export class DevView {
  readonly el: HTMLElement;
  private summary: HTMLElement;
  private groups: HTMLElement;
  private items: HTMLElement;
  private sessions: HTMLElement;
  private cls: string | null = null;
  private offset = 0;
  private data: DevSummary | null = null;

  constructor(
    private backend: Backend,
    private view: number,
    private status: (text: string) => void,
  ) {
    this.summary = h("div", { class: "dev-summary" });
    this.groups = h("div", { class: "dev-groups", role: "group", "aria-label": "Classes" });
    this.items = h("div", { class: "dev-items" });
    this.sessions = h("section", { class: "dev-sessions", "aria-label": "Agent session folders" });
    const apply = h(
      "section",
      { class: "apply", "aria-label": "Apply" },
      h("h3", {}, "Apply"),
      h("button", { type: "button", disabled: true, "aria-describedby": "apply-why" }, "Apply plan"),
      h(
        "p",
        { id: "apply-why" },
        "Not available: this app is read-only. It never deletes, moves or backs up anything. Export the plan and review it; applying a plan is a separate, later step.",
      ),
    );
    this.el = h("section", { class: "dev", "aria-label": "Developer view" }, this.summary, this.groups, this.items, this.sessions, apply);
  }

  async load(): Promise<void> {
    try {
      const s = await this.backend.devSummary(this.view);
      if (!s) {
        replace(this.summary, h("p", {}, "The analysis is still running (repositories, git facts and the plan)."));
        return;
      }
      this.data = s;
      replace(this.summary, h("h2", {}, "Developer view"), ...summaryLines(s).map((l) => h("p", {}, l)));
      const first = s.groups.find((g) => g.count > 0) ?? s.groups[0];
      if (this.cls === null && first) this.cls = first.class;
      this.renderGroups();
      await this.loadItems();
      await this.loadSessions();
    } catch (e) {
      this.status(errorMessage(e));
    }
  }

  private renderGroups(): void {
    const s = this.data;
    if (!s) return;
    replace(
      this.groups,
      ...s.groups.map((g) =>
        h(
          "button",
          {
            type: "button",
            class: "group",
            "aria-pressed": String(g.class === this.cls),
            onclick: () => {
              this.cls = g.class;
              this.offset = 0;
              this.renderGroups();
              void this.loadItems();
            },
          },
          `${g.label}: ${formatCount(g.count)} (${formatBytes(g.class_bytes)})`,
        ),
      ),
    );
  }

  private async loadItems(): Promise<void> {
    const cls = this.cls;
    const g = this.data?.groups.find((x) => x.class === cls);
    if (!cls || !g) return;
    const list = await this.backend.devPage(this.view, cls, this.offset, DEV_PAGE);
    if (cls !== this.cls) return;
    const pager = h(
      "div",
      { class: "pager" },
      h("button", { type: "button", disabled: this.offset === 0, onclick: () => void this.pageBy(-DEV_PAGE) }, "Previous"),
      ` ${g.count === 0 ? 0 : this.offset + 1} to ${Math.min(g.count, this.offset + DEV_PAGE)} of ${formatCount(g.count)} `,
      h("button", { type: "button", disabled: this.offset + DEV_PAGE >= g.count, onclick: () => void this.pageBy(DEV_PAGE) }, "Next"),
    );
    replace(
      this.items,
      h("h3", {}, g.label),
      list.length === 0 ? h("p", {}, "None.") : this.table(list),
      pager,
    );
  }

  private table(list: DevItem[]): HTMLElement {
    return h(
      "table",
      { class: "items" },
      h("caption", {}, "Plan items: action, reason and state"),
      h(
        "thead",
        {},
        h("tr", {}, ...["Path", "Action", "Reclaimable", "State", "Reason"].map((c) => h("th", { scope: "col" }, c))),
      ),
      h(
        "tbody",
        {},
        ...list.map((it) =>
          h(
            "tr",
            {},
            h("td", { class: "path" }, it.path),
            h("td", { class: `action action-${it.action}` }, it.action, it.removes ? " (removes)" : ""),
            h("td", { class: "c-num" }, formatBytes(it.reclaimable_bytes)),
            h("td", {}, it.state, it.state_reason ? `: ${it.state_reason}` : ""),
            h(
              "td",
              {},
              it.reason,
              it.facts_no.length ? ` Facts that are no: ${it.facts_no.join(", ")}.` : "",
              it.facts_unknown.length ? ` Unknown: ${it.facts_unknown.join(", ")}.` : "",
            ),
          ),
        ),
      ),
    );
  }

  private async pageBy(delta: number): Promise<void> {
    this.offset = Math.max(0, this.offset + delta);
    await this.loadItems();
  }

  private async loadSessions(): Promise<void> {
    let list: SessionFolder[] = [];
    try {
      list = await this.backend.sessionFolders(this.view);
    } catch {
      list = [];
    }
    replace(
      this.sessions,
      h("h3", {}, "Agent session folders"),
      list.length === 0
        ? h("p", {}, this.data?.imported ? "Not available for an imported scan (they come from the size walk)." : "None found.")
        : h(
            "ul",
            {},
            ...list.map((s) =>
              h(
                "li",
                {},
                `${s.layout}: ${s.path}, ${formatCount(s.files)} files, ${formatBytes(s.alloc)}, last changed ${formatDate(s.newest_mtime)}. ${s.plan_note}.`,
                s.enclosing ? ` Inside ${s.enclosing.path} (${s.enclosing.action}).` : "",
              ),
            ),
          ),
    );
  }
}
