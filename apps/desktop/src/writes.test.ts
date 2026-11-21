// The app's list of what it writes (src/writes.ts) and docs/DESKTOP.md say the same thing
// (PR gate F10, click-test CT-F7), and Settings shows it.
import { readFileSync } from "node:fs";
import { describe, expect, it } from "vitest";
import { mockBackend } from "./testing";
import { SettingsForm } from "./views/settings";
import { TAGLINE, WRITES } from "./writes";

// Paths are relative to apps/desktop, where the tests run (as contrast.test.ts reads styles.css).
const doc = readFileSync("../../docs/DESKTOP.md", "utf8");
const section = doc.slice(doc.indexOf("### Files the desktop app writes"), doc.indexOf("\n## Tests"));

describe("what the app writes (src/writes.ts)", () => {
  it("docs/DESKTOP.md lists every entry, path and text, under its group", () => {
    expect(section.length).toBeGreaterThan(100);
    for (const g of WRITES) {
      expect(section, g.title).toContain(`**${g.title}**`);
      for (const w of g.writes) {
        expect(section, w.path).toContain(`\`${w.path}\`: ${w.what}`);
      }
    }
  });

  it("the docs list nothing the app does not", () => {
    const listed = [...section.matchAll(/^- `([^`]+)`: /gm)].map((m) => m[1]);
    const known = WRITES.flatMap((g) => g.writes.map((w) => w.path));
    expect(listed.sort()).toEqual([...known].sort());
  });

  it("Settings shows every entry", () => {
    const form = new SettingsForm(mockBackend({}));
    const shown = form.el.querySelector("section.writes");
    expect(shown?.querySelector("h3")?.textContent).toBe("What this app writes");
    const items = [...(shown?.querySelectorAll("li") ?? [])].map((li) => li.textContent);
    expect(items).toEqual(WRITES.flatMap((g) => g.writes.map((w) => `${w.path}: ${w.what}`)));
  });

  it("the tagline points to the list", () => {
    expect(TAGLINE).toContain("listed in Settings");
    expect(doc.replace(/\s+/g, " ")).toContain(TAGLINE);
  });
});
