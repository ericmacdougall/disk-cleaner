// Spec AC-9 (PR gate F9), the mocked-backend check: the real size tree (src/views/tree.ts)
// over 1,000,000 synthetic rows served by offset from src/testing.ts. Test page only; the app's
// own build (index.html, dist/) never includes it.
//
// ?delay=<ms> makes every page answer that much later. The check's control uses it to show the
// timing sees the backend's answer (perf/tree.spec.ts).

import { mockBackend, syntheticPage } from "../../src/testing";
import { TreeGrid } from "../../src/views/tree";

const ROWS = 1_000_000;

async function boot(): Promise<void> {
  const delay = Number(new URLSearchParams(location.search).get("delay") ?? "0");
  const status = document.getElementById("status");
  const backend = mockBackend({
    treePage: async (view, offset, limit) => {
      if (delay > 0) await new Promise((r) => setTimeout(r, delay));
      return syntheticPage(view, ROWS, offset, limit);
    },
  });
  const grid = new TreeGrid(backend, 1, (text) => {
    if (status) status.textContent = text;
  }, "Linux");
  document.getElementById("app")?.append(grid.el);
  await grid.load();
  (window as unknown as { __perfReady: boolean }).__perfReady = true;
}

void boot();
