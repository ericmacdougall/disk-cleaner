// The status line's owner (CT-F6): stale messages, superseded and ended operations, view setters.

import { describe, expect, it } from "vitest";
import { StatusLine, within } from "./status";

describe("StatusLine", () => {
  it("is the one polite status region", () => {
    const s = new StatusLine();
    expect(s.el.getAttribute("role")).toBe("status");
    expect(s.el.getAttribute("aria-live")).toBe("polite");
    expect(s.el.classList.contains("live")).toBe(true);
  });

  it("scopes nest on '/' boundaries only", () => {
    expect(within("results/5/tree", "results/5")).toBe(true);
    expect(within("results/5", "results/5")).toBe(true);
    expect(within("results/50", "results/5")).toBe(false);
    expect(within("chooser", "results/5")).toBe(false);
  });

  it("a message is cleared when its screen is left", () => {
    const s = new StatusLine();
    s.enter("results/5/tree");
    s.begin("results/5").set("Plan exported to /a.json. Nothing else was changed.");
    s.enter("chooser");
    expect(s.text).toBe("");
  });

  it("re-entering the screen shown keeps its message", () => {
    const s = new StatusLine();
    s.enter("chooser");
    s.begin().set("Cannot open that scan file: bad");
    s.enter("chooser");
    expect(s.text).toBe("Cannot open that scan file: bad");
  });

  it("a results-bar message stays across tabs of the same results; a tab's message does not", () => {
    const s = new StatusLine();
    s.enter("results/5/tree");
    s.begin("results/5").set("exported");
    s.enter("results/5/dev");
    expect(s.text).toBe("exported");
    s.setter("results/5/dev")("dev says");
    s.enter("results/5/tree");
    expect(s.text).toBe("");
  });

  it("a newer operation clears the line and supersedes the older one", () => {
    const s = new StatusLine();
    s.enter("chooser");
    const first = s.begin();
    first.set("Reading the scan file...");
    const second = s.begin();
    expect(s.text).toBe("");
    expect(first.live).toBe(false);
    first.set("Cannot open that scan file: late");
    expect(s.text).toBe("");
    second.set("second");
    expect(s.text).toBe("second");
  });

  it("an operation whose screen was left cannot write, even after coming back", () => {
    const s = new StatusLine();
    s.enter("results/5/tree");
    const op = s.begin("results/5");
    s.enter("chooser");
    expect(op.live).toBe(false);
    op.set("late");
    expect(s.text).toBe("");
    s.enter("results/5/tree");
    op.set("late again");
    expect(s.text).toBe("");
  });

  it("an operation lives on while the user moves inside its scope", () => {
    const s = new StatusLine();
    s.enter("results/5/tree");
    const op = s.begin("results/5");
    s.enter("results/5/treemap");
    expect(op.live).toBe(true);
    op.set("done");
    expect(s.text).toBe("done");
  });

  it("a view setter writes only while its scope shows, and again when it shows again", () => {
    const s = new StatusLine();
    const tree = s.setter("results/5/tree");
    s.enter("results/5/treemap");
    tree("late tree");
    expect(s.text).toBe("");
    s.enter("results/5/tree");
    tree("Sorted by size, largest first.");
    expect(s.text).toBe("Sorted by size, largest first.");
    s.enter("results/6/tree");
    expect(s.text).toBe("");
    tree("old results");
    expect(s.text).toBe("");
  });
});
