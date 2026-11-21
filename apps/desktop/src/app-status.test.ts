// The app's status line through real App flows (click test CT-F6, CT-c): a message describes the
// screen it is on, a newer operation replaces an older one, and a late answer from an operation
// the user has left cannot write. Each case here failed against 0112370.

import { describe, expect, it } from "vitest";
import type { Backend, CurrentState, Place } from "./api";
import { App } from "./app";
import { flush, mockBackend, syntheticPage } from "./testing";

const LOADED: CurrentState = { job: null, state: "idle", view: 5, imported: false, has_tree: true, has_analysis: true, roots: ["/syn"] };
const IMPORTED: CurrentState = { job: null, state: "idle", view: 7, imported: true, has_tree: false, has_analysis: true, roots: [] };
const PLACE: Place = { path: "/syn/a", label: "a", total_bytes: null, available_bytes: null, file_system: null, removable: false };

function deferred<T>() {
  let resolve!: (v: T) => void;
  let reject!: (e: unknown) => void;
  const promise = new Promise<T>((res, rej) => {
    resolve = res;
    reject = rej;
  });
  return { promise, resolve, reject };
}

async function mount(over: Partial<Backend>): Promise<App> {
  const app = new App(mockBackend(over));
  document.body.replaceChildren(app.el);
  await app.start();
  await flush(20);
  return app;
}

const live = (app: App) => app.el.querySelector("p.live")?.textContent ?? null;
const h2 = (app: App) => app.el.querySelector("h2")?.textContent ?? null;
function button(app: App, text: string): HTMLButtonElement {
  const b = [...app.el.querySelectorAll("button")].find((x) => x.textContent === text);
  if (!b) throw new Error(`no button ${text}`);
  return b;
}

describe("status line (CT-F6, CT-c)", () => {
  it("the export confirmation names the file and says nothing else changed (CT-c)", async () => {
    const app = await mount({ currentState: async () => LOADED, exportPlan: async () => "/out/plan2.json" });
    button(app, "Export plan...").click();
    await flush(5);
    expect(live(app)).toBe("Plan exported to /out/plan2.json. Nothing else was changed.");
  });

  it("'Plan exported' does not stay on the chooser after New scan (CT-F6, first sequence)", async () => {
    const app = await mount({ currentState: async () => LOADED, exportPlan: async () => "/out/plan2.json" });
    button(app, "Export plan...").click();
    await flush(5);
    expect(live(app)).toContain("Plan exported to");
    button(app, "New scan").click();
    await flush(5);
    expect(h2(app)).toBe("Choose what to scan");
    expect(live(app)).toBe("");
  });

  it("'Reading the scan file...' is gone after an import, New scan and Add folders (CT-F6, second sequence)", async () => {
    const app = await mount({ openScanFile: async () => IMPORTED, chooseFolders: async () => [PLACE] });
    button(app, "Open a scan file...").click();
    await flush(10);
    expect(app.el.textContent).toContain("Imported scan file.");
    expect(live(app)).toBe("");
    button(app, "New scan").click();
    await flush(5);
    expect(live(app)).toBe("");
    button(app, "Add folders...").click();
    await flush(5);
    expect(h2(app)).toBe("Choose what to scan");
    expect(app.el.querySelector("ul.chosen")?.textContent).toContain("a");
    expect(live(app)).toBe("");
  });

  it("a double click on Export plan exports once; the button is disabled until the export ends", async () => {
    const d = deferred<string | null>();
    let calls = 0;
    const app = await mount({
      currentState: async () => LOADED,
      exportPlan: () => {
        calls += 1;
        return d.promise;
      },
    });
    button(app, "Export plan...").click();
    button(app, "Export plan...").click();
    await flush(5);
    expect(calls).toBe(1);
    expect(button(app, "Export plan...").disabled).toBe(true);
    d.resolve("/out/one.json");
    await flush(5);
    expect(button(app, "Export plan...").disabled).toBe(false);
    expect(live(app)).toBe("Plan exported to /out/one.json. Nothing else was changed.");
  });

  it("a slow export that finishes after New scan writes nothing under the chooser", async () => {
    const d = deferred<string | null>();
    const app = await mount({ currentState: async () => LOADED, exportPlan: () => d.promise });
    button(app, "Export plan...").click();
    await flush(2);
    button(app, "New scan").click();
    await flush(2);
    d.resolve("/out/late.json");
    await flush(5);
    expect(h2(app)).toBe("Choose what to scan");
    expect(live(app)).toBe("");
  });

  it("a slow export that fails after New scan writes nothing under the chooser", async () => {
    const d = deferred<string | null>();
    const app = await mount({ currentState: async () => LOADED, exportPlan: () => d.promise });
    button(app, "Export plan...").click();
    await flush(2);
    button(app, "New scan").click();
    await flush(2);
    d.reject({ kind: "refused", message: "disk full" });
    await flush(5);
    expect(live(app)).toBe("");
  });

  it("an import that answers after a scan started writes nothing on the scan screen", async () => {
    const d = deferred<CurrentState | null>();
    const app = await mount({ openScanFile: () => d.promise, startScan: async () => 3 });
    button(app, "Open a scan file...").click();
    await flush(2);
    // A scan started by any path while the file is read (the Scan button is disabled then, so
    // this drives the controller the button uses).
    await (app as unknown as { scan: { start(r: string[], g: boolean): Promise<boolean> } }).scan.start(["/syn"], true);
    await flush(2);
    expect(h2(app)).toBe("Scanning");
    expect(live(app)).toBe("");
    d.reject({ kind: "refused", message: "a scan is running" });
    await flush(5);
    expect(h2(app)).toBe("Scanning");
    expect(live(app)).toBe("");
  });

  it("an import that succeeds after a scan started neither shows its results nor writes", async () => {
    const d = deferred<CurrentState | null>();
    const app = await mount({ openScanFile: () => d.promise, startScan: async () => 3 });
    button(app, "Open a scan file...").click();
    await flush(2);
    await (app as unknown as { scan: { start(r: string[], g: boolean): Promise<boolean> } }).scan.start(["/syn"], true);
    await flush(2);
    d.resolve(IMPORTED);
    await flush(5);
    expect(h2(app)).toBe("Scanning");
    expect(live(app)).toBe("");
  });

  it("a size-tree message does not follow the user to another tab", async () => {
    let first = true;
    const app = await mount({
      currentState: async () => LOADED,
      treePage: async (view, offset, limit) => {
        if (first) {
          first = false;
          throw { kind: "failed", message: "tree page failed" };
        }
        return syntheticPage(view, 5, offset, limit);
      },
    });
    expect(live(app)).toBe("tree page failed");
    ([...app.el.querySelectorAll("[role=tab]")].find((t) => t.textContent === "Treemap") as HTMLButtonElement).click();
    await flush(5);
    expect(live(app)).toBe("");
  });

  it("a size-tree answer that arrives after New scan writes nothing under the chooser", async () => {
    const d = deferred<never>();
    const app = await mount({ currentState: async () => LOADED, treePage: () => d.promise });
    button(app, "New scan").click();
    await flush(2);
    d.reject({ kind: "stale", message: "replaced" });
    await flush(5);
    expect(h2(app)).toBe("Choose what to scan");
    expect(live(app)).toBe("");
  });

  it("a plan-export confirmation stays while the user changes tab in the same results", async () => {
    const app = await mount({ currentState: async () => LOADED, exportPlan: async () => "/out/p.json" });
    button(app, "Export plan...").click();
    await flush(5);
    ([...app.el.querySelectorAll("[role=tab]")].find((t) => t.textContent === "Treemap") as HTMLButtonElement).click();
    await flush(5);
    expect(live(app)).toContain("Plan exported to /out/p.json.");
  });
});
