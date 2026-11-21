// Every ordering of the Settings form's reads, saves, edits and outside changes (PR review F13,
// F14: class "async results accepted without request identity"). A fake config file answers
// reads and saves when the test says so; every interleaving of the events below, up to a bounded
// length, is run against the real SettingsForm in jsdom, and after every step the invariants are
// checked:
//   I0 a field the user edited shows what they typed;
//   I1 a field the user did not edit shows the newest value any answer has brought (no answer
//      that is older than one already shown can put an old value back);
//   I2 a save sends as changes only fields the user edited, with what they typed, and every
//      edited field that differs from the file; one save at a time; a confirmed save sends the
//      change the question showed;
//   I3 the message never describes a state the form does not show ("Saved" means the untouched
//      fields show what that save returned; "Not saved" means the last save did not succeed;
//      "Saving..." means a save is in flight);
//   I4 an answer that says the plan was recomputed reaches the app (onSaved(true)), and the
//      message mentions a recomputed plan only after such an answer;
//   I5 a field edited while the file changed under it is named, with both values, in the form's
//      conflict note, and no other field is;
//   I6 the open shorten question names the window the file held when it was asked, and nothing
//      on the form changes under it (an answer that arrives meanwhile is not used; Keep reads
//      again).
// ASSUMPTION: a read's answer describes the files as they were when the read was issued (the
// fake takes its snapshot then). A backend that answered two reads in the opposite order to the
// one they read the file in would defeat a token kept on this side; state.rs serves them in turn.
// The table at the end names the orderings the review and the owner listed, so each has a
// readable test of its own.

import { describe, expect, it } from "vitest";
import type { AppSettings, Backend, CoreChanges, SaveResult, SettingsPayload } from "./api";
import { openModal } from "./modal";
import { mockBackend, SETTINGS } from "./testing";
import { SettingsForm } from "./views/settings";

const MIB = 1024 * 1024;
type Field = "window" | "git" | "top";
const FIELDS: Field[] = ["window", "git", "top"];
const SELECTOR: Record<Field, string> = { window: "#set-window", git: "#set-git", top: "#set-top" };
const LABEL: Record<Field, string> = { window: "Active window", git: "Git folder limit", top: "Top files" };
const SUCCESS = /^(Saved|Nothing to save|Plan settings saved)/;

/** What the files hold: the two core keys in config.toml, Top files in the app's own settings. */
interface Snap {
  window: number;
  git: number; // MiB
  top: number;
  version: number;
}

interface Deferred<T> {
  promise: Promise<T>;
  resolve: (v: T) => void;
  reject: (e: unknown) => void;
}
function deferred<T>(): Deferred<T> {
  let resolve!: (v: T) => void;
  let reject!: (e: unknown) => void;
  const promise = new Promise<T>((a, b) => {
    resolve = a;
    reject = b;
  });
  return { promise, resolve, reject };
}

/** Let every continuation the last step started run to its end. The form and the fake use only
 * promises (no timers), so draining the microtask queue is enough; 64 turns is far more than the
 * longest chain (an answer, the form's continuation, a confirmed save's `.then`). */
const settleTask = async () => {
  for (let i = 0; i < 64; i++) await Promise.resolve();
};

function payload(s: Snap, replanned: boolean): SettingsPayload {
  const p = structuredClone(SETTINGS);
  p.core.active_window_days = s.window;
  p.core.max_git_admin_bytes = s.git * MIB;
  p.app.top_files = s.top;
  p.replanned = replanned;
  return p;
}

class World {
  // The files, and the backend's own view of them.
  disk = { window: 30, git: 64, top: 200 };
  version = 0;
  planned = 0;
  /** Reads in flight. `outdated`: the shorten question opened while it was in flight, so its
   * answer is not used (Keep reads again). */
  reads: { d: Deferred<SettingsPayload>; snap: Snap; replanned: boolean; outdated?: boolean }[] = [];
  save: { d: Deferred<SaveResult>; core: CoreChanges; app: AppSettings | null; confirm: boolean } | null = null;
  // What the form has been told, and what the user did: the oracle.
  latest: Snap | null = null;
  typed: Partial<Record<Field, string>> = {};
  editFrom: Partial<Record<Field, string>> = {};
  lastSaved: Snap | null = null;
  lastSaveOk = false;
  /** The shorten question the backend asked, and what the form showed while it was open. */
  question: { old: number; new: number; shown?: string[] } | null = null;
  answering = false;
  replanDelivered = false;
  recomputedSinceSave = false;
  onSavedTrue = 0;
  attached = true;
  violations: string[] = [];
  form: SettingsForm;
  shell: HTMLElement;

  constructor() {
    const backend: Backend = mockBackend({
      getSettings: () => {
        const snap = { ...this.disk, version: this.version };
        const replanned = this.version !== this.planned;
        this.planned = this.version;
        const d = deferred<SettingsPayload>();
        this.reads.push({ d, snap, replanned });
        return d.promise;
      },
      saveSettings: (core, app, confirm) => {
        this.checkSubmit(core, app, confirm);
        const d = deferred<SaveResult>();
        if (this.save) this.violations.push("I2 a second save was sent while one was in flight");
        this.save = { d, core, app, confirm };
        this.lastSaveOk = false;
        this.recomputedSinceSave = false;
        return d.promise;
      },
    });
    this.form = new SettingsForm(backend, (replanned) => {
      if (replanned) this.onSavedTrue++;
    });
    this.shell = document.createElement("div");
    this.shell.append(this.form.el);
    openModal()?.close();
    document.body.replaceChildren(this.shell);
  }

  input(f: Field): HTMLInputElement {
    return this.form.el.querySelector(SELECTOR[f]) as HTMLInputElement;
  }
  shown(f: Field): string {
    return this.input(f).value;
  }
  message(): string {
    return this.form.el.querySelector(".message")?.textContent ?? "";
  }
  locked(): boolean {
    return !!this.form.el.querySelector("fieldset")?.disabled;
  }
  static text(s: Snap, f: Field): string {
    return String(s[f]);
  }

  /** The form learned `s` (a read's or a save's answer). While the shorten question is open the
   * form shows what it showed when the question was asked: a read's answer then is not used
   * (Keep reads again), though a recomputed plan in it still reaches the app. */
  private learn(s: Snap, replanned: boolean, outdated = false): void {
    if (replanned) {
      this.replanDelivered = true;
      this.recomputedSinceSave = true;
    }
    if (this.question || outdated) return;
    if (!this.latest || s.version >= this.latest.version) this.latest = s;
    for (const f of FIELDS) {
      if (this.typed[f] === World.text(this.latest, f)) {
        delete this.typed[f];
        delete this.editFrom[f];
      }
    }
  }

  deliverRead(i: number): void {
    const [r] = this.reads.splice(i, 1);
    if (!r) throw new Error("no read to deliver");
    this.learn(r.snap, r.replanned, r.outdated);
    r.d.resolve(payload(r.snap, r.replanned));
  }

  failRead(i: number): void {
    const [r] = this.reads.splice(i, 1);
    r?.d.reject({ kind: "failed", message: "cannot read config.toml: permission denied" });
  }

  endSave(fail: boolean): void {
    const s = this.save;
    if (!s) throw new Error("no save to end");
    this.save = null;
    if (fail) {
      s.d.reject({ kind: "failed", message: "not saved: cannot replace config.toml" });
      return;
    }
    const days = s.core.active_window_days;
    if (days !== null && days < this.disk.window && !s.confirm) {
      this.question = { old: this.disk.window, new: days };
      for (const r of this.reads) r.outdated = true;
      s.d.reject({
        kind: "needs_confirmation",
        message: `shortening the active window from ${this.disk.window} to ${days} days`,
        old: this.disk.window,
        new: days,
      });
      return;
    }
    let core = false;
    let any = false;
    if (days !== null && days !== this.disk.window) {
      this.disk.window = days;
      core = true;
    }
    const git = s.core.max_git_admin_bytes;
    if (git !== null && git / MIB !== this.disk.git) {
      this.disk.git = git / MIB;
      core = true;
    }
    if (s.app && s.app.top_files !== this.disk.top) {
      this.disk.top = s.app.top_files;
      any = true;
    }
    if (core || any) this.version++;
    const replanned = core || this.version !== this.planned;
    this.planned = this.version;
    const snap = { ...this.disk, version: this.version };
    this.typed = {};
    this.editFrom = {};
    this.latest = snap;
    this.lastSaved = snap;
    this.lastSaveOk = true;
    this.question = null;
    if (replanned) {
      this.replanDelivered = true;
      this.recomputedSinceSave = true;
    }
    s.d.resolve({ report: { outcome: core || any ? "saved" : "unchanged", note: null, replanned }, settings: payload(snap, replanned) });
  }

  /** Another program edits config.toml (the app's own settings live only in the app). */
  external(): void {
    this.disk.window = this.disk.window === 30 ? 45 : 30;
    this.disk.git = this.disk.git === 64 ? 128 : 64;
    this.version++;
  }

  type(f: Field, v: string): void {
    const input = this.input(f);
    input.value = v;
    input.dispatchEvent(new Event("input", { bubbles: true }));
    const disk = this.latest ? World.text(this.latest, f) : "";
    if (v === disk) {
      delete this.typed[f];
      delete this.editFrom[f];
    } else {
      if (this.typed[f] === undefined) this.editFrom[f] = disk;
      this.typed[f] = v;
    }
  }

  private checkSubmit(core: CoreChanges, app: AppSettings | null, confirm: boolean): void {
    const L = this.latest;
    if (!L) {
      this.violations.push("I2 a save was sent before the form had read the file");
      return;
    }
    if (this.question && !(confirm && this.answering)) this.violations.push("I2 a save was sent while the question was open");
    if (confirm && (!this.question || core.active_window_days !== this.question.new)) {
      this.violations.push(`I2 the confirmed save sent window ${core.active_window_days}, the question showed ${this.question?.new}`);
    }
    const sent: Record<"window" | "git", number | null> = {
      window: core.active_window_days,
      git: core.max_git_admin_bytes === null ? null : core.max_git_admin_bytes / MIB,
    };
    for (const f of ["window", "git"] as const) {
      const t = this.typed[f];
      if (sent[f] !== null && (t === undefined || Number(t) !== sent[f])) {
        this.violations.push(`I2 sent ${f}=${sent[f]} but the user ${t === undefined ? "did not edit it" : `typed ${t}`}`);
      }
      if (t !== undefined && Number(t) !== L[f] && sent[f] !== Number(t)) {
        this.violations.push(`I2 the user's ${f}=${t} was not sent (sent ${sent[f]})`);
      }
    }
    if (app) {
      const want = Number(this.typed.top ?? L.top);
      if (app.top_files !== want) this.violations.push(`I2 sent top=${app.top_files}, want ${want}`);
    }
  }

  check(where: string): void {
    const out: string[] = [];
    for (const f of FIELDS) {
      const t = this.typed[f];
      if (t !== undefined && this.shown(f) !== t) out.push(`I0 ${f} shows ${this.shown(f)}, the user typed ${t}`);
      if (t === undefined && this.latest && this.shown(f) !== World.text(this.latest, f)) {
        out.push(`I1 untouched ${f} shows ${this.shown(f)}, the newest answer says ${World.text(this.latest, f)}`);
      }
    }
    const m = this.message();
    if (SUCCESS.test(m)) {
      if (!this.lastSaveOk || !this.lastSaved) out.push(`I3 "${m}" but the last save did not succeed`);
      else {
        for (const f of FIELDS) {
          if (this.typed[f] === undefined && this.shown(f) !== World.text(this.lastSaved, f)) {
            out.push(`I3 "${m}" but untouched ${f} shows ${this.shown(f)}, the save returned ${World.text(this.lastSaved, f)}`);
          }
        }
      }
    }
    if (m.startsWith("Not saved") && this.lastSaveOk) out.push(`I3 "${m}" after a save that succeeded`);
    // Click-test N6: after Keep, the sentence says what the file holds and what the field shows.
    if (m.startsWith("Not saved") && /active window/.test(m) && this.latest) {
      const has = (v: string) => new RegExp(`\\b${v}\\b`).test(m);
      if (!has(String(this.latest.window)) || !has(this.shown("window"))) {
        out.push(`I3 "${m}" does not name the file's window ${this.latest.window} and the field's ${this.shown("window")}`);
      }
    }
    if (m.startsWith("Saving") && !this.save) out.push(`I3 "${m}" with no save in flight`);
    if (this.save && !this.locked()) out.push("I2 the form is editable while a save is in flight");
    if (this.replanDelivered && this.onSavedTrue === 0) out.push("I4 an answer said the plan was recomputed; the app was not told");
    if (/recomputed/.test(m) && !this.recomputedSinceSave) out.push(`I4 "${m}" with no recomputed plan since the last save`);
    if (this.question && !openModal()) out.push("I3 the shorten question is not open");
    // I6: the open question names the window the file held when it was asked, and nothing on the
    // form changes under it.
    const dialog = openModal()?.el;
    if (this.question && dialog) {
      const text = dialog.textContent ?? "";
      if (!text.includes(`Keep ${this.question.old} days`) || !text.includes(`Shorten to ${this.question.new} days`)) {
        out.push(`I6 the question says ${JSON.stringify(text)}, the file held ${this.question.old} and the change is ${this.question.new}`);
      }
      const now = FIELDS.map((f) => this.shown(f));
      if (!this.question.shown) this.question.shown = now;
      else if (now.join() !== this.question.shown.join()) out.push(`I6 the form changed under the open question: ${this.question.shown.join()} -> ${now.join()}`);
    }
    const note = this.form.el.querySelector(".conflict");
    const items = note ? [...note.querySelectorAll("li")].map((li) => li.textContent ?? "") : [];
    for (const f of FIELDS) {
      const t = this.typed[f];
      const from = this.editFrom[f];
      const conflict = t !== undefined && from !== undefined && from !== "" && this.latest !== null && from !== World.text(this.latest, f);
      const line = items.find((s) => s.includes(LABEL[f]));
      if (conflict && this.latest) {
        const file = World.text(this.latest, f);
        const has = (v: string) => !!line && new RegExp(`\\b${v}\\b`).test(line);
        if (!line || !has(file) || !has(t)) out.push(`I5 ${f}: the file changed to ${file} under the edit ${t}; the note says ${JSON.stringify(line ?? null)}`);
      } else if (line) {
        out.push(`I5 ${f} is named in the conflict note with no conflict: ${JSON.stringify(line)}`);
      }
    }
    for (const o of out) this.violations.push(`${o} [after ${where}]`);
  }
}

interface Ev {
  name: string;
  ok: (w: World) => boolean;
  run: (w: World) => void;
}
const typing = (f: Field, v: string): Ev => ({
  name: `type ${f}=${v}`,
  ok: (w) => w.attached && w.latest !== null && !w.locked() && !openModal() && w.shown(f) !== v,
  run: (w) => w.type(f, v),
});
const EVENTS: Ev[] = [
  { name: "read", ok: (w) => w.attached, run: (w) => void w.form.load() }, // a re-render of the app
  {
    name: "leave",
    ok: (w) => w.attached,
    run: (w) => {
      w.form.el.remove();
      w.attached = false;
    },
  },
  {
    name: "enter",
    ok: (w) => !w.attached,
    run: (w) => {
      w.shell.append(w.form.el);
      w.attached = true;
      void w.form.load();
    },
  },
  { name: "readEnd", ok: (w) => w.reads.length > 0, run: (w) => w.deliverRead(0) },
  { name: "readEndNewest", ok: (w) => w.reads.length > 1, run: (w) => w.deliverRead(w.reads.length - 1) },
  { name: "readFail", ok: (w) => w.reads.length > 0, run: (w) => w.failRead(0) },
  typing("window", "60"),
  typing("window", "7"),
  typing("git", "100"),
  typing("top", "300"),
  { name: "save", ok: (w) => w.attached && !openModal(), run: (w) => void w.form.save(false) },
  {
    name: "saveTwice", // a double click
    ok: (w) => w.attached && !openModal() && !w.locked(),
    run: (w) => {
      void w.form.save(false);
      void w.form.save(false);
    },
  },
  { name: "saveEnd", ok: (w) => w.save !== null, run: (w) => w.endSave(false) },
  { name: "saveFail", ok: (w) => w.save !== null, run: (w) => w.endSave(true) },
  { name: "external", ok: () => true, run: (w) => w.external() },
  {
    name: "escape",
    ok: (w) => w.attached || !!openModal(),
    run: (w) => {
      const target = openModal()?.el ?? w.form.el.querySelector("form");
      target?.dispatchEvent(new KeyboardEvent("keydown", { key: "Escape", bubbles: true, cancelable: true }));
      if (openModal() === null) w.question = null;
    },
  },
  {
    name: "shorten",
    ok: () => !!openModal(),
    run: (w) => {
      w.answering = true;
      (openModal()?.el.querySelector("button.primary") as HTMLButtonElement).click();
      w.answering = false;
      if (!w.save) w.question = null;
    },
  },
];
const BY_NAME = new Map(EVENTS.map((e) => [e.name, e]));

async function step(w: World, e: Ev, where: string): Promise<void> {
  w.replanDelivered = false;
  const before = w.onSavedTrue;
  w.onSavedTrue = 0;
  e.run(w);
  await settleTask();
  // The save that a "Shorten" answer starts is now in flight: the question is over.
  if (w.save?.confirm) w.question = null;
  w.check(where);
  w.onSavedTrue += before;
}

type Start = "filled" | "opening";

/** A fresh form, shown as the app shows it (appended, then loaded). */
async function begin(start: Start): Promise<World> {
  const w = new World();
  void w.form.load();
  await settleTask();
  if (start === "filled") await step(w, BY_NAME.get("readEnd")!, "the first read");
  return w;
}

/** Run `seq` from a fresh form, then settle everything and read once more. The exploration stops
 * at the first violation; `all` (the named rows) goes on and collects every one, so a row shows
 * each invariant it breaks (on 87221db the F14 row breaks I1 at the read and I2 at the save). */
async function run(seq: string[], start: Start, settle = true, all = false): Promise<{ w: World; enabled: Ev[] }> {
  const w = await begin(start);
  for (let i = 0; i < seq.length; i++) {
    const e = BY_NAME.get(seq[i]!);
    const where = seq.slice(0, i).join(", ");
    if (!e || !e.ok(w)) {
      if (!all) throw new Error(`event ${seq[i]} is not possible after ${where}`);
      w.violations.push(`event ${seq[i]} is not possible after ${where}`);
      return { w, enabled: [] };
    }
    await step(w, e, seq.slice(0, i + 1).join(", "));
    if (w.violations.length && !all) return { w, enabled: [] };
  }
  const enabled = EVENTS.filter((e) => e.ok(w));
  if (settle) await finish(w, seq, all);
  return { w, enabled };
}

/** Answer everything still open (Escape a question, end a save, end reads), show Settings again
 * and read: the form must then show what the files hold. */
async function finish(w: World, seq: string[], all = false): Promise<void> {
  const tail = `${seq.join(", ")}; then settle`;
  const go = () => all || !w.violations.length;
  for (let i = 0; i < 20 && go(); i++) {
    const e = openModal() ? "escape" : w.save ? "saveEnd" : w.reads.length ? "readEnd" : null;
    if (!e) break;
    await step(w, BY_NAME.get(e)!, tail);
  }
  if (go()) {
    await step(w, BY_NAME.get(w.attached ? "read" : "enter")!, tail);
    if (w.reads.length === 0) w.violations.push(`settle: showing Settings again did not read the file [after ${tail}]`);
    while (w.reads.length && go()) await step(w, BY_NAME.get("readEnd")!, tail);
    if (go() && w.latest?.version !== w.version) w.violations.push(`settle: the form never learned the file's last state [after ${tail}]`);
  }
}

/** A seeded random walk of `length` possible events, for orderings longer than the exhaustive
 * bound (mulberry32; the seed is in every violation). */
async function walk(seed: number, length: number, start: Start): Promise<World> {
  let s = seed >>> 0;
  const rand = () => {
    s = (s + 0x6d2b79f5) >>> 0;
    let t = s;
    t = Math.imul(t ^ (t >>> 15), t | 1);
    t ^= t + Math.imul(t ^ (t >>> 7), t | 61);
    return ((t ^ (t >>> 14)) >>> 0) / 4294967296;
  };
  const w = await begin(start);
  const seq: string[] = [];
  for (let i = 0; i < length && !w.violations.length; i++) {
    const enabled = EVENTS.filter((e) => e.ok(w));
    const e = enabled[Math.floor(rand() * enabled.length)]!;
    seq.push(e.name);
    await step(w, e, `seed ${seed}: ${seq.join(", ")}`);
  }
  if (!w.violations.length) await finish(w, [`seed ${seed}: ${seq.join(", ")}`]);
  return w;
}

/** Every sequence of possible events of exactly `depth` steps (or fewer, if nothing is possible). */
async function explore(start: Start, depth: number) {
  const found = new Map<string, string>(); // invariant -> shortest example
  let runs = 0;
  const visit = async (prefix: string[]): Promise<void> => {
    const { w, enabled } = await run(prefix, start, prefix.length === depth);
    runs++;
    if (w.violations.length) {
      const v = w.violations[0]!;
      const key = v.split(" [after")[0]!.replace(/\d+/g, "#").replace(/(shows|says|typed|sent|want) \S+/g, "$1 _");
      const id = v.slice(0, 2);
      const k = `${id} ${key}`;
      if (!found.has(k)) found.set(k, v);
      return;
    }
    if (prefix.length === depth) return;
    for (const e of enabled) await visit([...prefix, e.name]);
  };
  await visit([]);
  return { runs, found };
}

// Depth 4 is exhaustive in about 40 s in jsdom (6,362 runs, each a fresh form); SETTINGS_ORDER_DEPTH=5
// runs the next bound (about ten times as many). Longer orderings: the walks and the table.
const ENV = (globalThis as { process?: { env: Record<string, string | undefined> } }).process?.env ?? {};
const DEPTH = Number(ENV.SETTINGS_ORDER_DEPTH ?? 4);
const WALKS = Number(ENV.SETTINGS_ORDER_WALKS ?? 400);
const WALK_LENGTH = 16;

describe("Settings form: every ordering of reads, saves, edits and outside changes (PR review F13, F14)", () => {
  it(`no interleaving of up to ${DEPTH} events after the first read breaks an invariant`, async () => {
    const { runs, found } = await explore("filled", DEPTH);
    console.log(`settings orderings from a filled form: ${runs} runs to depth ${DEPTH}`);
    expect(runs).toBeGreaterThan(1000);
    expect([...found.values()]).toEqual([]);
  }, 600_000);

  it(`no interleaving of up to ${DEPTH - 1} events while the first read is in flight breaks an invariant`, async () => {
    const { runs, found } = await explore("opening", DEPTH - 1);
    console.log(`settings orderings from an opening form: ${runs} runs to depth ${DEPTH - 1}`);
    expect(runs).toBeGreaterThan(100);
    expect([...found.values()]).toEqual([]);
  }, 600_000);

  it(`${WALKS} seeded walks of ${WALK_LENGTH} events break no invariant`, async () => {
    const found: string[] = [];
    for (let seed = 1; seed <= WALKS; seed++) {
      const w = await walk(seed, WALK_LENGTH, seed % 4 === 0 ? "opening" : "filled");
      if (w.violations.length && found.length < 10) found.push(w.violations[0]!);
    }
    expect(found).toEqual([]);
  }, 600_000);

  // The named orderings (the review's F13 and F14, and the owner's list). Each must be possible
  // as written (an impossible event is a violation), so a change to the events cannot make a row
  // pass by skipping it. A save answer arriving after a newer save cannot happen: one save is in
  // flight at a time ("a double click", and I2 in every run).
  const ROWS: [string, string[], (w: World) => void][] = [
    ["F13: a read started before Save ends after it", ["read", "type window=60", "save", "saveEnd", "readEnd"], (w) => expect(w.shown("window")).toBe("60")],
    [
      "F14: Top files edited, the git limit changed by another program, Settings shown again, Save",
      ["type top=300", "external", "leave", "enter", "readEnd", "save", "saveEnd"],
      (w) => expect(w.disk).toMatchObject({ git: 128, top: 300 }),
    ],
    ["a read during a save", ["type window=60", "read", "save", "readEnd", "saveEnd"], () => undefined],
    ["a read after a save", ["type window=60", "save", "saveEnd", "read", "readEnd"], () => undefined],
    ["two saves in a row", ["type window=60", "save", "saveEnd", "type git=100", "save", "saveEnd"], (w) => expect(w.disk).toMatchObject({ window: 60, git: 100 })],
    ["a save, an outside edit, then Settings shown again", ["type window=60", "save", "saveEnd", "external", "leave", "enter", "readEnd"], (w) => expect(w.shown("window")).toBe("30")],
    [
      // The newer read (issued second) answers first and changes the fields without a recomputed
      // plan (the first read already caused it): "Saved." must not stay.
      "a save, an outside edit, two reads answered newest first",
      ["type window=60", "save", "saveEnd", "external", "read", "read", "readEndNewest", "readEnd"],
      (w) => {
        expect(w.shown("git")).toBe("128");
        expect(w.message()).not.toMatch(/^Saved/);
      },
    ],
    ["a double click on Save sends one save", ["type window=60", "saveTwice", "saveEnd"], (w) => expect(w.disk.window).toBe(60)],
    ["the question is open when a read ends", ["read", "type window=7", "save", "saveEnd", "readEnd", "shorten", "saveEnd"], (w) => expect(w.disk.window).toBe(7)],
    [
      // The read asked for while the question is open waits; Keep reads, and the form then says
      // the file changed under the kept edit.
      "the file changes between the question and Keep",
      ["type window=7", "save", "saveEnd", "external", "read", "escape", "readEnd"],
      (w) => {
        expect(w.shown("window")).toBe("7");
        // (The read after Keep also recomputed the plan and said so; the settle step's later read
        // rewrote the sentence for the fields as they are.)
        expect(w.message()).toMatch(/^Not saved: the active window in the file is still 45 days; the field keeps your unsaved 7\./);
        expect(w.form.el.querySelector(".conflict")?.textContent).toMatch(/Active window.*\b45\b.*\b30\b.*\b7\b/);
      },
    ],
    [
      "the file changes between the question and Shorten",
      ["type window=7", "type top=300", "save", "saveEnd", "external", "read", "shorten", "saveEnd"],
      (w) => expect(w.disk).toMatchObject({ window: 7, git: 128, top: 300 }),
    ],
    [
      "Escape on the question: the field keeps the unsaved edit and the message says both values (click-test N6)",
      ["type window=7", "save", "saveEnd", "escape"],
      (w) => {
        expect(w.shown("window")).toBe("7");
        expect(w.message()).toBe("Not saved: the active window in the file is still 30 days; the field keeps your unsaved 7.");
      },
    ],
    ["Escape while a save is in flight", ["type window=60", "save", "escape", "saveEnd"], (w) => expect(w.disk.window).toBe(60)],
    ["a failed save keeps the edit", ["type window=60", "save", "saveFail", "read", "readEnd"], (w) => expect(w.shown("window")).toBe("60")],
    ["a failed read, then a save", ["read", "readFail", "type window=60", "save", "saveEnd"], (w) => expect(w.disk.window).toBe(60)],
    ["an older read ends after a newer one", ["read", "external", "read", "readEndNewest", "readEnd"], (w) => expect(w.shown("git")).toBe("128")],
    [
      "the file changes under an edited field: both values shown, Save writes the user's",
      ["type git=100", "external", "read", "readEnd", "save", "saveEnd"],
      (w) => expect(w.disk.git).toBe(100),
    ],
  ];
  for (const [name, seq, then] of ROWS) {
    it(name, async () => {
      const { w } = await run(seq, "filled", true, true);
      expect([...new Set(w.violations)]).toEqual([]);
      then(w);
    });
  }

  it("the conflict note names the field and both values while the edit is kept", async () => {
    const { w } = await run(["type git=100", "external", "read", "readEnd"], "filled", false);
    expect(w.violations).toEqual([]);
    const note = w.form.el.querySelector(".conflict")?.textContent ?? "";
    expect(note).toContain("Git folder limit");
    expect(note).toMatch(/\b128\b/);
    expect(note).toMatch(/\b100\b/);
    expect(w.shown("git")).toBe("100");
  });

  it("a re-render or Settings shown again while the question is open starts no read; Keep reads", async () => {
    // The question owns the form, so no answer can change it under the question (I6).
    const { w } = await run(["type window=7", "save", "saveEnd", "external", "read", "leave", "enter"], "filled", false, true);
    expect([...new Set(w.violations)]).toEqual([]);
    expect(w.reads).toHaveLength(0);
    await step(w, BY_NAME.get("escape")!, "Keep");
    expect(w.reads).toHaveLength(1);
    await step(w, BY_NAME.get("readEnd")!, "Keep, then the read");
    expect([...new Set(w.violations)]).toEqual([]);
    expect(w.shown("git")).toBe("128");
  });

  it("after a save the fields show what the save returned, also when that is not what was typed (partial save)", async () => {
    const result = payload({ window: 60, git: 64, top: 200, version: 1 }, true);
    const b = mockBackend({
      getSettings: async () => payload({ window: 30, git: 64, top: 200, version: 0 }, false),
      saveSettings: async () => ({
        report: { outcome: "partial", note: "scan settings were not saved: cannot replace prunedisk-app.json", replanned: true },
        settings: result,
      }),
    });
    const f = new SettingsForm(b);
    document.body.replaceChildren(f.el);
    await f.load();
    const window = f.el.querySelector("#set-window") as HTMLInputElement;
    const top = f.el.querySelector("#set-top") as HTMLInputElement;
    window.value = "60";
    top.value = "300";
    await f.save(false);
    expect(window.value).toBe("60");
    expect(top.value).toBe("200"); // the scan settings were not saved: the field says so by showing the file
    expect(f.el.querySelector(".message")?.textContent).toMatch(/^Plan settings saved\. scan settings were not saved/);
  });
});
