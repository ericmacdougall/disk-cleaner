import { describe, expect, it } from "vitest";
import { ScanController } from "./scan";
import { flush, mockBackend } from "./testing";

function deferred<T>() {
  let resolve!: (v: T) => void;
  let reject!: (e: unknown) => void;
  const promise = new Promise<T>((a, b) => {
    resolve = a;
    reject = b;
  });
  return { promise, resolve, reject };
}

describe("scan state (spec AC-3, NEG-1, NEG-2)", () => {
  it("a double click starts one scan", async () => {
    const d = deferred<number>();
    const b = mockBackend({ startScan: () => d.promise });
    const c = new ScanController(b);
    const first = c.start(["/syn"], true);
    const second = await c.start(["/syn"], true);
    expect(second).toBe(false);
    d.resolve(7);
    expect(await first).toBe(true);
    expect(b.calls.filter((x) => x.name === "startScan")).toHaveLength(1);
    expect(c.snapshot).toMatchObject({ phase: "walking", job: 7 });
  });

  it("events of another job are never shown", async () => {
    const c = new ScanController(mockBackend({ startScan: async () => 2 }));
    await c.start(["/syn"], true);
    c.onEvent({ job: 1, state: "done", view: 9 });
    c.onEvent({ job: 1, state: "walking", progress: { files: 5, dirs: 1, bytes: 1, elapsed_ms: 1, current: "x" } });
    expect(c.snapshot).toMatchObject({ phase: "walking", job: 2, view: null, progress: null });
    c.onEvent({ job: 2, state: "analysing", view: 3 });
    c.onEvent({ job: 2, state: "done", view: 3 });
    expect(c.snapshot).toMatchObject({ phase: "done", view: 3, analysed: true });
  });

  it("cancel returns to the chooser at once and drops the job's late results", async () => {
    const b = mockBackend({ startScan: async () => 4 });
    const c = new ScanController(b);
    await c.start(["/syn"], true);
    c.onEvent({ job: 4, state: "analysing", view: 8 });
    c.cancel();
    expect(c.snapshot).toMatchObject({ phase: "chooser", job: null, view: null });
    expect(b.calls.some((x) => x.name === "cancelScan" && x.args[0] === 4)).toBe(true);
    c.onEvent({ job: 4, state: "done", view: 8 });
    expect(c.snapshot.phase).toBe("chooser");
  });

  it("cancel before Start replied cancels the job when its id arrives, even after a new Start", async () => {
    const first = deferred<number>();
    const second = deferred<number>();
    const replies = [first, second];
    const b = mockBackend({ startScan: () => replies.shift()!.promise });
    const c = new ScanController(b);
    const p1 = c.start(["/syn"], true);
    c.cancel();
    expect(c.snapshot.phase).toBe("chooser");
    const p2 = c.start(["/syn"], true);
    first.resolve(10);
    expect(await p1).toBe(false);
    second.resolve(11);
    expect(await p2).toBe(true);
    expect(b.calls.filter((x) => x.name === "cancelScan").map((x) => x.args[0])).toEqual([10]);
    expect(c.snapshot).toMatchObject({ phase: "walking", job: 11 });
  });

  it("events that arrive before Start replied are kept for that job only", async () => {
    const d = deferred<number>();
    const c = new ScanController(mockBackend({ startScan: () => d.promise }));
    const p = c.start(["/syn"], true);
    c.onEvent({ job: 3, state: "walking", progress: { files: 9, dirs: 2, bytes: 3, elapsed_ms: 4, current: "a" } });
    c.onEvent({ job: 99, state: "done", view: 1 });
    d.resolve(3);
    await p;
    expect(c.snapshot).toMatchObject({ phase: "walking", job: 3 });
    expect(c.snapshot.progress?.files).toBe(9);
  });

  it("a refused Start shows why and allows another", async () => {
    const c = new ScanController(
      mockBackend({
        startScan: async () => {
          throw { kind: "refused", message: "the previous scan is still stopping" };
        },
      }),
    );
    expect(await c.start(["/syn"], true)).toBe(false);
    expect(c.snapshot).toMatchObject({ phase: "chooser", error: "the previous scan is still stopping" });
    await flush();
    expect(c.running).toBe(false);
  });
});
