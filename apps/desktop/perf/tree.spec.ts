// Spec AC-9 at one million rows (PR gate F9), check (3): headless Chromium, the real size tree
// over the mocked backend's 1,000,000 rows. Same steps, timing boundary, samples, threshold and
// DOM-row limit as the running-app check (both use e2e/ac9.mjs). Registered before any sample
// (WAL w8d2cd55d39e), built to a quoted excerpt of spec v6 AC-9; the spec text was not re-read.

import { expect, test } from "@playwright/test";
import { mkdirSync, writeFileSync } from "node:fs";
import path from "node:path";
import { AC9, judge, measureFloor, measureScroll, profileScroll, rank, scrollOptions, scrollVerdict, summarizeProfile } from "../e2e/ac9.mjs";

const ROWS = AC9.nodes;
const OUT = path.resolve("perf-results");
const results: Record<string, unknown> = {};

test.afterAll(() => {
  mkdirSync(OUT, { recursive: true });
  writeFileSync(path.join(OUT, "ac9-playwright.json"), JSON.stringify({ suite: "ac9-playwright", ...results }, null, 2));
});

async function open(page: import("@playwright/test").Page, delay: number): Promise<void> {
  await page.goto(`/perf/index.html?delay=${delay}`);
  await page.waitForFunction(() => (window as unknown as { __perfReady?: boolean }).__perfReady === true, null, { timeout: 60_000 });
  const count = await page.locator(".tg-scroll").getAttribute("aria-rowcount");
  // The header row counts.
  expect(Number(count) - 1, "rows in the tree").toBe(ROWS);
}

test("timing.playwright_1m.scroll_p95", async ({ page, browser }) => {
  await open(page, 0);
  const r = await page.evaluate(measureScroll, scrollOptions());
  results.browser = browser.version();
  results.raw = { error: r.error ?? null, step: r.step ?? null, rowsMax: r.rowsMax ?? null, samples: r.samples };
  const m = scrollVerdict("playwright_1m_scroll_rows_shown", r);
  results.scroll = m;
  console.log(`MEASURE ${JSON.stringify(m)}`);
  expect(m.p95, `p95 ${m.p95.toFixed(1)} ms over ${m.n} samples`).toBeLessThan(AC9.scrollP95Ms);
});

// Informational, never a failure: the empty-frame floor and the per-slice profile, as the
// running-app session records them (e2e/run.mjs), for a browser baseline.
test("info.playwright_1m.floor_and_profile", async ({ page }) => {
  await open(page, 0);
  try {
    const f = await page.evaluate(measureFloor, { warmup: AC9.scrollWarmup, samples: AC9.scrollSamples });
    const p = await page.evaluate(profileScroll, scrollOptions());
    results.floor = { ...judge("playwright_1m_frame_floor", f.samples, AC9.scrollP95Ms), intervalP50: f.intervals.length ? rank(f.intervals, 0.5) : null, screen: f.screen, gate: false };
    results.profile = { error: p.error, slices: summarizeProfile(p.steps), gate: false };
  } catch (e) {
    results.floor = { error: String(e) };
  }
  console.log(`MEASURE ${JSON.stringify({ floor: results.floor, profile: (results.profile as { slices?: unknown } | undefined)?.slices })}`);
});

// Control: the same measurement over a backend that answers each page 100 ms late must miss the
// threshold. A timing that stopped on a row already in the DOM, or before the page answered,
// would pass here, and this test would fail.
test("control.playwright_1m.slow_backend_is_seen", async ({ page }) => {
  await open(page, 100);
  const r = await page.evaluate(measureScroll, scrollOptions());
  if (r.error) throw new Error(r.error);
  const m = judge("playwright_1m_scroll_backend_100ms", r.samples, AC9.scrollP95Ms);
  results.control = { ...m, rowsMax: r.rowsMax };
  console.log(`MEASURE ${JSON.stringify(results.control)}`);
  expect(m.n).toBe(AC9.scrollSamples);
  expect(m.pass, `a 100 ms backend gave p95 ${m.p95.toFixed(1)} ms`).toBe(false);
  expect(m.p50).toBeGreaterThanOrEqual(100);
});
