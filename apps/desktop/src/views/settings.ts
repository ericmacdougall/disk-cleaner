// Settings (spec AC-5, NEG-4, NEG-5, NEG-14). Two core keys (active window, git admin limit)
// are saved to core's config file only on Save and only when changed; the app's scan settings
// are its own. While a save is pending the form is disabled (no double save, no edit lost in
// flight). Shortening the active window asks first; Escape or "Keep" keeps the old window and
// the draft. After a save the form shows what the file now holds.

import type { AppSettings, Backend, CoreChanges, SettingsPayload } from "../api";
import { errorKind, errorMessage } from "../api";
import { h, replace } from "../dom";
import { formatBytes } from "../format";

const MIB = 1024 * 1024;

export interface Draft {
  activeWindowDays: string;
  gitAdminMib: string;
  topFiles: string;
  minOutputMib: string;
  oneFileSystem: boolean;
}

/** Whole numbers only: "12" is 12; "", "1.5", "-3", "1e3", "abc" are not numbers. */
export function parseWhole(text: string): number | null {
  const t = text.trim();
  if (!/^\d{1,16}$/.test(t)) return null;
  const n = Number(t);
  return Number.isSafeInteger(n) ? n : null;
}

/** Turn a draft into the changes to save, or a message saying what is wrong. Unchanged core keys
 * are not sent, so a value outside the app's ranges that the file already holds is kept. */
export function changesFrom(
  draft: Draft,
  current: SettingsPayload,
): { core: CoreChanges; app: AppSettings } | { error: string } {
  const r = current.ranges;
  const days = parseWhole(draft.activeWindowDays);
  const git = parseWhole(draft.gitAdminMib);
  const top = parseWhole(draft.topFiles);
  const out = parseWhole(draft.minOutputMib);
  const core: CoreChanges = { active_window_days: null, max_git_admin_bytes: null };
  if (days === null) return { error: "The active window must be a whole number of days." };
  if (days !== current.core.active_window_days) {
    if (days < r.active_window_days[0] || days > r.active_window_days[1]) {
      return { error: `The active window must be ${r.active_window_days[0]} to ${r.active_window_days[1]} days.` };
    }
    core.active_window_days = days;
  }
  if (git === null) return { error: "The git folder limit must be a whole number of MiB." };
  const gitBytes = git * MIB;
  if (gitBytes !== current.core.max_git_admin_bytes && git !== Math.round(current.core.max_git_admin_bytes / MIB)) {
    if (gitBytes < r.max_git_admin_bytes[0] || gitBytes > r.max_git_admin_bytes[1]) {
      return { error: `The git folder limit must be ${formatBytes(r.max_git_admin_bytes[0])} to ${formatBytes(r.max_git_admin_bytes[1])}.` };
    }
    core.max_git_admin_bytes = gitBytes;
  }
  if (top === null || top < r.top_files[0] || top > r.top_files[1]) {
    return { error: `Top files must be ${r.top_files[0]} to ${r.top_files[1]}.` };
  }
  if (out === null || out < r.min_output_mib[0] || out > r.min_output_mib[1]) {
    return { error: `The output size must be ${r.min_output_mib[0]} to ${r.min_output_mib[1]} MiB.` };
  }
  return { core, app: { top_files: top, min_output_mib: out, one_file_system: draft.oneFileSystem } };
}

export class SettingsForm {
  readonly el: HTMLElement;
  private form: HTMLFormElement;
  private fields: HTMLFieldSetElement;
  private message: HTMLElement;
  private facts: HTMLElement;
  private dialog: HTMLElement;
  private inputs: Record<keyof Draft, HTMLInputElement>;
  private current: SettingsPayload | null = null;
  pending = false;
  /** The shorten question is open; the form stays locked until it is answered. */
  private confirming = false;

  constructor(
    private backend: Backend,
    private onSaved: (replanned: boolean) => void = () => undefined,
  ) {
    const num = (id: string) => h("input", { id, type: "text", inputmode: "numeric", autocomplete: "off", "aria-describedby": `${id}-help` });
    this.inputs = {
      activeWindowDays: num("set-window"),
      gitAdminMib: num("set-git"),
      topFiles: num("set-top"),
      minOutputMib: num("set-out"),
      oneFileSystem: h("input", { id: "set-onefs", type: "checkbox" }),
    };
    const row = (id: string, label: string, input: HTMLInputElement, help: string) =>
      h("div", { class: "field" }, h("label", { for: id }, label), input, h("p", { id: `${id}-help`, class: "help" }, help));
    this.message = h("p", { role: "status", "aria-live": "polite", class: "message" });
    this.facts = h("div", { class: "facts" });
    this.dialog = h("div", { class: "confirm", hidden: true });
    this.fields = h(
      "fieldset",
      {},
      h("legend", {}, "Plan settings (core's config file)"),
      row("set-window", "Active window (days)", this.inputs.activeWindowDays, "Projects changed within this many days count as active."),
      row("set-git", "Git folder limit (MiB)", this.inputs.gitAdminMib, "Largest git admin folder the plan backs up whole."),
      h("legend", { class: "sub" }, "Scan settings (this app)"),
      row("set-top", "Top files to list", this.inputs.topFiles, "10 to 1,000."),
      row("set-out", "Output files from (MiB)", this.inputs.minOutputMib, "Output files at least this big are listed."),
      h("div", { class: "field" }, this.inputs.oneFileSystem, h("label", { for: "set-onefs" }, " Stay on one file system")),
      h("button", { type: "submit", class: "primary" }, "Save"),
    );
    this.form = h("form", { "aria-label": "Settings", onsubmit: (e) => { e.preventDefault(); void this.save(false); } }, this.fields);
    this.el = h("section", { class: "settings", "aria-label": "Settings" }, h("h2", {}, "Settings"), this.form, this.dialog, this.message, this.facts);
  }

  draft(): Draft {
    return {
      activeWindowDays: this.inputs.activeWindowDays.value,
      gitAdminMib: this.inputs.gitAdminMib.value,
      topFiles: this.inputs.topFiles.value,
      minOutputMib: this.inputs.minOutputMib.value,
      oneFileSystem: this.inputs.oneFileSystem.checked,
    };
  }

  async load(): Promise<void> {
    try {
      this.show(await this.backend.getSettings());
    } catch (e) {
      this.message.textContent = errorMessage(e);
    }
  }

  private show(p: SettingsPayload): void {
    this.current = p;
    this.inputs.activeWindowDays.value = String(p.core.active_window_days);
    this.inputs.gitAdminMib.value = String(Math.round(p.core.max_git_admin_bytes / MIB));
    this.inputs.topFiles.value = String(p.app.top_files);
    this.inputs.minOutputMib.value = String(p.app.min_output_mib);
    this.inputs.oneFileSystem.checked = p.app.one_file_system;
    const notes: string[] = [];
    if (!p.core.active_window_in_range) notes.push("The active window in the file is outside this app's range; it is kept unless you change it.");
    if (!p.core.max_git_admin_in_range) notes.push("The git folder limit in the file is outside this app's range; it is kept unless you change it.");
    replace(
      this.facts,
      h("p", {}, `Source: ${p.core.source}.`),
      h("p", {}, `Trust a partial process check: ${p.core.trust_partial_process_check ? "yes" : "no"} (read only here; ${p.core.source}).`),
      h("p", {}, `Accept incomplete coverage: ${p.core.accept_incomplete_coverage ? "yes" : "no"} (read only here; ${p.core.source}).`),
      h("p", {}, "If another program edits the config file at the moment of saving, its edit can be lost; the previous text is kept in config.toml.bak."),
      ...notes.map((n) => h("p", {}, n)),
    );
  }

  private setPending(p: boolean): void {
    this.pending = p;
    this.fields.disabled = p;
    this.form.setAttribute("aria-busy", String(p));
  }

  /** Save. Ignored while a save is pending or the shorten question is open. */
  async save(confirmShorten: boolean): Promise<void> {
    if (this.pending || this.confirming || !this.current) return;
    const c = changesFrom(this.draft(), this.current);
    if ("error" in c) {
      this.message.textContent = c.error;
      return;
    }
    await this.submit(c, confirmShorten);
  }

  /** Send these exact changes. The form stays locked from here until the save ends or the
   * shorten question is answered, so a confirmation always applies to the change it showed. */
  private async submit(c: { core: CoreChanges; app: AppSettings }, confirmShorten: boolean): Promise<void> {
    this.setPending(true);
    this.message.textContent = "Saving...";
    try {
      const r = await this.backend.saveSettings(c.core, c.app, confirmShorten);
      this.show(r.settings);
      const what = { saved: "Saved.", unchanged: "Nothing to save: the file already holds these values.", saved_unconfirmed: "Saved; could not confirm it is on disk." }[r.report.outcome];
      this.message.textContent = `${what}${r.report.note ? ` ${r.report.note}` : ""}${r.report.replanned ? " The plan was recomputed." : ""}`;
      this.onSaved(r.report.replanned);
    } catch (e) {
      if (errorKind(e) === "needs_confirmation" && !confirmShorten) {
        this.confirm(e as { old?: number; new?: number; message: string }, c);
        return;
      }
      this.message.textContent = `Not saved: ${errorMessage(e)}`;
    } finally {
      if (!this.confirming) this.setPending(false);
    }
  }

  private confirm(e: { old?: number; new?: number; message: string }, c: { core: CoreChanges; app: AppSettings }): void {
    this.confirming = true;
    this.message.textContent = "";
    const keep = h("button", { type: "button", onclick: () => close(false) }, `Keep ${e.old ?? ""} days`);
    const shorten = h("button", { type: "button", class: "primary", onclick: () => close(true) }, `Shorten to ${e.new ?? ""} days`);
    const close = (go: boolean) => {
      if (!this.confirming) return;
      this.confirming = false;
      this.dialog.hidden = true;
      replace(this.dialog);
      if (go) {
        void this.submit(c, true).then(() => this.inputs.activeWindowDays.focus());
      } else {
        this.setPending(false);
        this.inputs.activeWindowDays.focus();
        this.message.textContent = "Not saved: the active window was kept.";
      }
    };
    replace(
      this.dialog,
      h(
        "div",
        {
          role: "alertdialog",
          "aria-modal": "true",
          "aria-labelledby": "confirm-title",
          "aria-describedby": "confirm-text",
          onkeydown: (ev) => {
            if ((ev as KeyboardEvent).key === "Escape") close(false);
          },
        },
        h("h3", { id: "confirm-title" }, "Shorten the active window?"),
        h("p", { id: "confirm-text" }, e.message),
        keep,
        shorten,
      ),
    );
    this.dialog.hidden = false;
    keep.focus();
  }
}
