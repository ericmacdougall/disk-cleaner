// The chooser's lists stay lists for assistive technology. WebKit drops a <ul>'s list role when its
// list style is removed (`list-style: none`, as styles.css does for .places and .chosen), so the AppImage
// probe heard the Drives list as a group (bundle fixer's AT-SPI roles probe, every WebKit). An explicit
// role keeps it a list.

import { readFileSync } from "node:fs";
import { describe, expect, it } from "vitest";
import type { Place } from "./api";
import { App } from "./app";
import { flush, mockBackend } from "./testing";

const DRIVE: Place = { path: "/", label: "Root", total_bytes: 100, available_bytes: 50, file_system: "ext4", removable: false };

describe("chooser lists (AT-SPI roles probe)", () => {
  it("every list class styles.css strips the list style from is rendered with role list", () => {
    // Paths are relative to apps/desktop, where the tests run (as contrast.test.ts reads styles.css).
    const css = readFileSync("src/styles.css", "utf8");
    const sources = ["src/app.ts", "src/views/dev.ts", "src/views/settings.ts", "src/views/tree.ts"].map((f) => readFileSync(f, "utf8")).join("\n");
    const stripped = [...css.matchAll(/([^{}]+)\{[^}]*list-style:\s*none/g)].flatMap((m) => [...(m[1] ?? "").matchAll(/\.([\w-]+)/g)].map((c) => c[1] ?? ""));
    expect(stripped.length).toBeGreaterThan(0);
    for (const cls of stripped) {
      expect(sources, cls).toMatch(new RegExp(`class: "${cls}", role: "list"`));
    }
  });

  it("the Drives and Chosen folders lists say they are lists, with list items", async () => {
    const app = new App(mockBackend({ listVolumes: async () => [DRIVE] }));
    document.body.replaceChildren(app.el);
    await app.start();
    await flush(20);
    (app.el.querySelector('input[type="checkbox"]') as HTMLInputElement).click();
    await flush(5);
    for (const name of ["Drives", "Chosen folders"]) {
      const ul = app.el.querySelector(`ul[aria-label="${name}"]`);
      expect(ul, name).not.toBeNull();
      expect(ul?.getAttribute("role"), name).toBe("list");
      expect(ul?.querySelectorAll(":scope > li").length, name).toBeGreaterThan(0);
    }
  });
});
