// Settings (spec AC-5, NEG-4, NEG-5, NEG-14). Two core keys (active window, git admin limit)
// are saved to core's config file only on Save and only when changed; the app's scan settings
// are its own. While a save is pending the form is disabled (no double save, no edit lost in
// flight). Shortening the active window asks first, in the app's one modal question
// (src/modal.ts); Escape or "Keep" keeps the old window and the draft. After a save the form
// shows what the file now holds.
//
// One owner of the form's state (PR review F13/F14, class "async results accepted without request
// identity"). The rules, which src/settings-orderings.test.ts checks over every interleaving:
// - Every read takes a generation number. A read's answer is used only if no newer answer (a
//   later read, or any successful save) has been used; a successful save outdates every read
//   issued before it ended. No read is started while a save is in flight. The shorten question
//   owns the form while it is open: answers in flight are outdated when it opens, no read
//   starts, and Keep reads the files again (Shorten's save answer is the truth).
// - Each field remembers the value the file last held for it (`filled`). A field whose text is
//   that value is untouched: it always shows the newest value read. A field whose text differs
//   is the user's edit: a read never changes it, and only edited fields are sent as changes.
// - A save's answer is the truth: every field shows it, and the message describes it. A later
//   read that changes what is shown replaces a "Saved" message.
// - If the file changes under a field the user is editing, the form keeps the user's text, says
//   so next to the form with both values, and Save writes what the user typed.

import type { AppSettings, Backend, CoreChanges, SettingsPayload } from "../api";
import { errorKind, errorMessage } from "../api";
import { h, replace } from "../dom";
import { formatBytes } from "../format";
import { showModal } from "../modal";
import { WRITES } from "../writes";

/** "What this app writes": the list in src/writes.ts, which docs/DESKTOP.md repeats. */
function writesSection(): HTMLElement {
  return h(
    "section",
    { class: "writes", "aria-labelledby": "writes-title" },
    h("h3", { id: "writes-title" }, "What this app writes"),
    h("p", {}, "It never deletes, moves or backs up a scanned file. Outside the folders it scans, it writes only this:"),
    ...WRITES.map((g) =>
      h(
        "div",
        { class: "writes-group" },
        h("h4", {}, g.title),
        h("ul", {}, ...g.writes.map((w) => h("li", {}, h("code", {}, w.path), `: ${w.what}`))),
      ),
    ),
  );
}

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

const FIELDS: (keyof Draft)[] = ["activeWindowDays", "gitAdminMib", "topFiles", "minOutputMib", "oneFileSystem"];
const LABELS: Record<keyof Draft, string> = {
  activeWindowDays: "Active window (days)",
  gitAdminMib: "Git folder limit (MiB)",
  topFiles: "Top files to list",
  minOutputMib: "Output files from (MiB)",
  oneFileSystem: "Stay on one file system",
};
const SUCCESS = /^(Saved|Nothing to save|Plan settings saved)/;

/** What the files hold, as the form's fields show it. */
export function draftOf(p: SettingsPayload): Draft {
  return {
    activeWindowDays: String(p.core.active_window_days),
    gitAdminMib: String(Math.round(p.core.max_git_admin_bytes / MIB)),
    topFiles: String(p.app.top_files),
    minOutputMib: String(p.app.min_output_mib),
    oneFileSystem: p.app.one_file_system,
  };
}

/** An answer from the backend: a read's (with the generation it was issued under) or a save's. */
type Answer = { gen: number; p: SettingsPayload; saved: boolean };

export class SettingsForm {
  readonly el: HTMLElement;
  private form: HTMLFormElement;
  private fields: HTMLFieldSetElement;
  private message: HTMLElement;
  private conflict: HTMLElement;
  private facts: HTMLElement;
  private inputs: Record<keyof Draft, HTMLInputElement>;
  /** The newest answer used: what the files held, as far as the form knows. */
  private current: SettingsPayload | null = null;
  /** For each field, the value the files held in the newest answer used. A field whose text is
   * that value is untouched; any other text is the user's edit. */
  private filled: Draft;
  /** Edited fields the file changed under: the value the file held when the edit began. */
  private changedUnder: Partial<Record<keyof Draft, string | boolean>> = {};
  /** Generation numbers. Every read takes one when it starts; `used` is the newest answer used.
   * A save's answer takes a fresh one when it arrives, so every read issued before is older. */
  private issued = 0;
  private used = 0;
  /** A save call is in flight. */
  private saving = false;
  /** The shorten question is open; the form stays locked until it is answered. */
  private confirming = false;
  /** The message is the "Not saved" sentence for a kept-back shortening (see `say`). */
  private kept = false;

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
    this.conflict = h("div", { class: "conflict", "aria-live": "polite" });
    this.facts = h("div", { class: "facts" });
    this.fields = h(
      "fieldset",
      {},
      h("legend", {}, "Plan settings (core's config file)"),
      row("set-window", LABELS.activeWindowDays, this.inputs.activeWindowDays, "Projects changed within this many days count as active."),
      row("set-git", LABELS.gitAdminMib, this.inputs.gitAdminMib, "Largest git admin folder the plan backs up whole."),
      h("legend", { class: "sub" }, "Scan settings (this app)"),
      row("set-top", LABELS.topFiles, this.inputs.topFiles, "10 to 1,000."),
      row("set-out", LABELS.minOutputMib, this.inputs.minOutputMib, "Output files at least this big are listed."),
      h("div", { class: "field" }, this.inputs.oneFileSystem, h("label", { for: "set-onefs" }, ` ${LABELS.oneFileSystem}`)),
      h("button", { type: "submit", class: "primary" }, "Save"),
    );
    this.form = h(
      "form",
      {
        "aria-label": "Settings",
        onsubmit: (e: Event) => {
          e.preventDefault();
          void this.save(false);
        },
        oninput: () => this.edits(),
        onchange: () => this.edits(),
      },
      this.fields,
    );
    this.filled = this.draft();
    this.el = h(
      "section",
      { class: "settings", "aria-label": "Settings" },
      h("h2", {}, "Settings"),
      this.form,
      this.conflict,
      this.message,
      this.facts,
      writesSection(),
    );
  }

  /** The form is locked: a save is in flight or the shorten question is open. */
  get pending(): boolean {
    return this.saving || this.confirming;
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

  private value(k: keyof Draft): string | boolean {
    return k === "oneFileSystem" ? this.inputs[k].checked : this.inputs[k].value;
  }

  /** The user changed this field from what the files hold. */
  private edited(k: keyof Draft): boolean {
    return this.value(k) !== this.filled[k];
  }

  /** Read the files. Settings is read each time it is shown and on every re-render of the app (a
   * scan event). No read starts while a save is in flight: the save's answer is newer. */
  async load(): Promise<void> {
    // A save in flight: its answer is newer than any read. The shorten question open: it was
    // asked about the file as the save found it, so nothing changes under it; Keep reads again.
    if (this.pending) return;
    const gen = ++this.issued;
    let p: SettingsPayload;
    try {
      p = await this.backend.getSettings();
    } catch (e) {
      if (gen > this.used && !this.pending) this.say(errorMessage(e));
      return;
    }
    // Opening Settings found the file changed and recomputed the plan (PR gate F4). The app is
    // told even when this answer is too old to show: the plan changed either way.
    if (p.replanned) this.onSaved(true);
    this.use({ gen, p, saved: false });
  }

  /** The one place an answer reaches the form. A read's answer older than one already used is
   * dropped. Untouched fields show the answer; edited fields keep the user's text. A save's
   * answer is the truth for every field (the form was locked while it was in flight). */
  private use(a: Answer): void {
    if (!a.saved && a.gen <= this.used) return;
    this.used = a.gen;
    const first = this.current === null;
    this.current = a.p;
    const disk = draftOf(a.p);
    let changed = false;
    for (const k of FIELDS) {
      const input = this.inputs[k];
      if (a.saved || !this.edited(k)) {
        if (this.value(k) !== disk[k]) changed = true;
        if (k === "oneFileSystem") input.checked = disk[k];
        else input.value = disk[k] as string;
        delete this.changedUnder[k];
      } else if (this.value(k) === disk[k]) {
        delete this.changedUnder[k]; // the file now holds what the user typed
      } else if (!first && this.filled[k] !== disk[k]) {
        // The file changed under the user's edit: keep the edit and say so.
        if (!(k in this.changedUnder)) this.changedUnder[k] = this.filled[k];
        else if (this.changedUnder[k] === disk[k]) delete this.changedUnder[k];
      }
    }
    this.filled = disk;
    this.showFacts(a.p);
    this.edits();
    if (!a.saved && !this.pending) {
      // A kept-back shortening stays said (it is still unsaved); the recomputed plan is added.
      if (a.p.replanned && this.kept) this.say(`${this.keptMessage()} The config file changed; the plan was recomputed.`, true);
      else if (a.p.replanned) this.say("The config file changed; the plan was recomputed.");
      else if (changed && SUCCESS.test(this.message.textContent ?? "")) {
        this.say("The settings changed after the save; the form shows what the files hold now.");
      }
    }
  }

  /** The one writer of the message. `kept` marks the "Not saved" sentence for a shortening the
   * user kept back: it names the file's window and the field's, so it is rewritten whenever
   * either changes, and goes when the field no longer holds an unsaved edit. */
  private say(text: string, kept = false): void {
    this.kept = kept;
    this.message.textContent = text;
  }

  private refreshKept(): void {
    if (!this.kept) return;
    if (this.edited("activeWindowDays")) this.say(this.keptMessage(), true);
    else this.say("");
  }

  /** After the user typed or an answer was used: what the conflict note and a "Not saved" sentence
   * say follows what the fields now show. */
  private edits(): void {
    this.showConflicts();
    this.refreshKept();
  }

  /** Name every edited field the file changed under, with both values. */
  private showConflicts(): void {
    const show = (v: string | boolean | undefined) => (typeof v === "boolean" ? (v ? "on" : "off") : String(v));
    const items: HTMLElement[] = [];
    for (const k of FIELDS) {
      if (!(k in this.changedUnder)) continue;
      if (!this.edited(k)) {
        delete this.changedUnder[k];
        continue;
      }
      items.push(
        h(
          "li",
          {},
          `${LABELS[k]}: the file now holds ${show(this.filled[k])} (it held ${show(this.changedUnder[k])} when you started editing). ` +
            `The field keeps what you typed, ${show(this.value(k))}, and Save writes it.`,
        ),
      );
    }
    if (items.length === 0) replace(this.conflict);
    else replace(this.conflict, h("p", {}, "Another program changed these settings while you were editing them:"), h("ul", {}, ...items));
  }

  private showFacts(p: SettingsPayload): void {
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

  private lock(): void {
    this.fields.disabled = this.pending;
    this.form.setAttribute("aria-busy", String(this.pending));
  }

  /** The changes to send, from the form as it is now: a field the user did not edit is sent as
   * the files hold it, so only edited fields can differ. */
  private changes(): { core: CoreChanges; app: AppSettings } | { error: string } | null {
    if (!this.current) return null;
    const d = this.draft();
    const sent: Draft = {
      activeWindowDays: this.edited("activeWindowDays") ? d.activeWindowDays : this.filled.activeWindowDays,
      gitAdminMib: this.edited("gitAdminMib") ? d.gitAdminMib : this.filled.gitAdminMib,
      topFiles: this.edited("topFiles") ? d.topFiles : this.filled.topFiles,
      minOutputMib: this.edited("minOutputMib") ? d.minOutputMib : this.filled.minOutputMib,
      oneFileSystem: this.edited("oneFileSystem") ? d.oneFileSystem : this.filled.oneFileSystem,
    };
    return changesFrom(sent, this.current);
  }

  /** Save. Ignored while a save is in flight or the shorten question is open. */
  async save(confirmShorten: boolean): Promise<void> {
    if (this.pending) return;
    const c = this.changes();
    if (c === null) return;
    if ("error" in c) {
      this.say(c.error);
      return;
    }
    await this.submit(c, confirmShorten);
  }

  /** Send these exact changes. The form stays locked from here until the save ends or the
   * shorten question is answered, so a confirmation always applies to the change it showed. */
  private async submit(c: { core: CoreChanges; app: AppSettings }, confirmShorten: boolean): Promise<void> {
    this.saving = true;
    this.lock();
    this.say("Saving...");
    try {
      const r = await this.backend.saveSettings(c.core, c.app, confirmShorten);
      this.saving = false;
      this.use({ gen: ++this.issued, p: r.settings, saved: true });
      const what = {
        saved: "Saved.",
        unchanged: "Nothing to save: the file already holds these values.",
        saved_unconfirmed: "Saved; could not confirm it is on disk.",
        partial: "Plan settings saved.",
      }[r.report.outcome];
      this.say(`${what}${r.report.note ? ` ${r.report.note}` : ""}${r.report.replanned ? " The plan was recomputed." : ""}`);
      this.onSaved(r.report.replanned);
    } catch (e) {
      this.saving = false;
      if (errorKind(e) === "needs_confirmation" && !confirmShorten) {
        this.confirm(e as { old?: number; new?: number; message: string }, c);
        return;
      }
      this.say(`Not saved: ${errorMessage(e)}`);
    } finally {
      this.lock();
    }
  }

  /** What "Keep" leaves: the file's window and the field's unsaved one, so the sentence cannot
   * read as if the field went back (click-test N6: the field keeps the user's edit). */
  private keptMessage(): string {
    const file = this.current?.core.active_window_days;
    const typed = this.inputs.activeWindowDays.value.trim();
    return `Not saved: the active window in the file is still ${file} days; the field keeps your unsaved ${typed}.`;
  }

  /** Ask before shortening the active window, through the app's one modal primitive (PR gate
   * F8): nothing behind the question can be reached until it is answered. While it is open the
   * form stays locked and shows what it showed when the question was asked: reads in flight are
   * outdated, none starts, and Keep reads the files again. Shorten sends exactly the changes the
   * question was asked about, and its answer is the truth. */
  private confirm(e: { old?: number; new?: number; message: string }, c: { core: CoreChanges; app: AppSettings }): void {
    this.confirming = true;
    this.used = ++this.issued;
    this.lock();
    this.say("");
    const answer = (go: boolean) => {
      if (!this.confirming) return;
      this.confirming = false;
      modal.close(this.inputs.activeWindowDays);
      if (go) {
        void this.submit(c, true).then(() => this.inputs.activeWindowDays.focus());
        return;
      }
      this.lock();
      this.inputs.activeWindowDays.focus();
      this.say(this.keptMessage(), true);
      void this.load();
    };
    const modal = showModal({
      title: "Shorten the active window?",
      text: e.message,
      buttons: [
        { label: `Keep ${e.old ?? ""} days`, initial: true, onClick: () => answer(false) },
        { label: `Shorten to ${e.new ?? ""} days`, primary: true, onClick: () => answer(true) },
      ],
      onCancel: () => answer(false),
    });
  }
}
