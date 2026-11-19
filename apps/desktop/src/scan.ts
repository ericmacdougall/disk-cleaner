// The scan's screen state (spec AC-3, NEG-1, NEG-2). Rules:
// - Start is refused while starting, walking or analysing (a double click starts one scan).
// - Events are accepted only for the current job id; events that arrive before Start's reply
//   are held and replayed once the id is known.
// - Cancel returns to the chooser at once and forgets the job and its results; a Cancel
//   pressed before Start replied cancels the job as soon as its id arrives.

import { type Backend, type Progress, type ScanEvent, errorMessage } from "./api";

export type Phase = "chooser" | "starting" | "walking" | "analysing" | "done" | "failed";

export interface ScanSnapshot {
  phase: Phase;
  job: number | null;
  /** The results to page, once the size tree is ready. */
  view: number | null;
  progress: Progress | null;
  error: string | null;
  /** The analysis (developer view) is ready. */
  analysed: boolean;
  imported: boolean;
}

export class ScanController {
  private s: ScanSnapshot = {
    phase: "chooser",
    job: null,
    view: null,
    progress: null,
    error: null,
    analysed: false,
    imported: false,
  };
  private held: ScanEvent[] = [];
  /** Each Start gets a number; a reply for an older or cancelled Start is cancelled. */
  private startGen = 0;
  private cancelledGen = 0;
  private listeners = new Set<(s: ScanSnapshot) => void>();

  constructor(private backend: Backend) {}

  get snapshot(): ScanSnapshot {
    return { ...this.s };
  }

  subscribe(fn: (s: ScanSnapshot) => void): () => void {
    this.listeners.add(fn);
    return () => this.listeners.delete(fn);
  }

  private set(patch: Partial<ScanSnapshot>): void {
    this.s = { ...this.s, ...patch };
    for (const fn of this.listeners) fn(this.snapshot);
  }

  get running(): boolean {
    return ["starting", "walking", "analysing"].includes(this.s.phase);
  }

  /** Start a scan. Returns false (and changes nothing) while one is running. */
  async start(roots: string[], collectGitFacts: boolean): Promise<boolean> {
    if (this.running) return false;
    this.held = [];
    const gen = ++this.startGen;
    this.set({ phase: "starting", job: null, view: null, progress: null, error: null, analysed: false, imported: false });
    let job: number;
    try {
      job = await this.backend.startScan(roots, collectGitFacts);
    } catch (e) {
      if (gen === this.startGen && this.s.phase === "starting") {
        this.set({ phase: "chooser", error: errorMessage(e) });
      }
      return false;
    }
    if (gen !== this.startGen || this.cancelledGen === gen) {
      void this.backend.cancelScan(job).catch(() => undefined);
      return false;
    }
    this.set({ phase: "walking", job });
    const held = this.held;
    this.held = [];
    for (const e of held) this.onEvent(e);
    return true;
  }

  /** Cancel: back to the chooser now; the backend ends the job in the background. */
  cancel(): void {
    if (this.s.phase === "starting") {
      this.cancelledGen = this.startGen;
    } else if (this.s.job !== null && (this.s.phase === "walking" || this.s.phase === "analysing")) {
      void this.backend.cancelScan(this.s.job).catch(() => undefined);
    } else {
      return;
    }
    this.held = [];
    this.set({ phase: "chooser", job: null, view: null, progress: null, analysed: false, error: null });
  }

  /** A backend event. Anything not for the current job is dropped. */
  onEvent(e: ScanEvent): void {
    if (this.s.phase === "starting" && this.s.job === null) {
      this.held.push(e);
      return;
    }
    if (e.job !== this.s.job || !(this.s.phase === "walking" || this.s.phase === "analysing")) return;
    switch (e.state) {
      case "walking":
        if (e.progress) this.set({ progress: e.progress });
        break;
      case "analysing":
        this.set({ phase: "analysing", view: e.view ?? this.s.view });
        break;
      case "done":
        this.set({ phase: "done", view: e.view ?? this.s.view, analysed: true });
        break;
      case "failed":
        this.set({ phase: "failed", error: e.error ?? "the scan failed" });
        break;
      default:
        break;
    }
  }

  /** Show results that are already loaded (an imported scan, or the e2e start file). */
  showLoaded(view: number, analysed: boolean, imported: boolean): void {
    if (this.running) return;
    this.set({ phase: "done", job: null, view, analysed, imported, error: null, progress: null });
  }

  /** Back to the chooser from the results (nothing is cancelled). */
  newScan(): void {
    if (this.running) return;
    this.set({ phase: "chooser", error: null });
  }
}
