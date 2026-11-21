// Spec AC-9 at one million rows (PR gate F9): the steps, timing boundary, samples, thresholds
// and verdict of the scroll timing, shared by the running app (e2e/run.mjs, WebDriver on Linux
// and Windows) and the mocked page (perf/, Playwright on headless Chromium), so both measure
// exactly the same thing. Registered before any sample (WAL w8d2cd55d39e), built to a quoted
// excerpt of spec v6 AC-9; the spec text itself was not re-read. See docs/DESKTOP.md.

export const AC9 = Object.freeze({
  /** Files under the one synthetic root (crates/view synth::wide_tree, seed 16452). */
  nodes: 1_000_000,
  seed: 16452,
  /** IPC: tree_page offset k * pageLimit, limit pageLimit; warm-up pages, then k = 0..99. */
  ipcWarmup: 10,
  ipcSamples: 100,
  pageLimit: 200,
  ipcP95Ms: 50,
  /** Scroll: warm-up steps, then measured steps, each stepRows rows (stepRows * rowHeight px). */
  scrollWarmup: 10,
  scrollSamples: 200,
  stepRows: 10,
  rowHeight: 28,
  scrollP95Ms: 50,
  /** Hard limit on every step: document.querySelectorAll('.tg-row').length. */
  maxDomRows: 200,
  /** A step whose row never shows fails the check. */
  stepTimeoutMs: 5000,
});

/** Nearest-rank percentile: the smallest sample with at least `q` of the samples at or below it. */
export function rank(xs, q) {
  const s = [...xs].sort((x, y) => x - y);
  return s[Math.ceil(q * s.length) - 1];
}

/** The verdict on `ms`: pass when the nearest-rank p95 is strictly under `thresholdMs`. */
export function judge(id, ms, thresholdMs) {
  const p95 = rank(ms, 0.95);
  return { id, n: ms.length, p50: rank(ms, 0.5), p95, max: Math.max(...ms), thresholdMs, pass: ms.length > 0 && p95 < thresholdMs };
}

/**
 * The scroll measurement. Runs in the page and must stay self-contained: WebDriver receives its
 * source text, Playwright serializes it.
 *
 * Each step moves `.tg-scroll` down by `stepRows` rows. Its target is the last row fully in view
 * after the step: a row the step brings in, which must not be in the DOM before a measured step (the
 * grid fetches a few rows past each edge, so the new top row is already there and would time
 * one frame whatever the backend does). The clock starts just before scrollTop is set and stops
 * after the first animation frame in which `#tg-row-<target>` is in the DOM plus one post-paint
 * task (requestAnimationFrame, then a MessageChannel message). After every step, warm-up steps
 * included, `.tg-row` elements (the header row is one) must number at most `maxDomRows`; the
 * first step over it ends the measurement and is named.
 *
 * Returns { samples, rowsMax, rowsMin, steps } or { error, step, samples }.
 */
export async function measureScroll(o) {
  const sc = document.querySelector(".tg-scroll");
  if (!sc) return { error: "no .tg-scroll in the page", step: null, samples: [] };
  const frame = () => new Promise((r) => requestAnimationFrame(r));
  const postPaint = () =>
    new Promise((r) => {
      const ch = new MessageChannel();
      ch.port1.onmessage = () => r();
      ch.port2.postMessage(0);
    });
  const rowCount = () => document.querySelectorAll(".tg-row").length;
  const samples = [];
  let rowsMax = 0;
  let rowsMin = Infinity;
  const total = o.warmup + o.samples;
  const name = (i) => (i < o.warmup ? `warm-up step ${i + 1} of ${o.warmup}` : `step ${i - o.warmup + 1} of ${o.samples}`);
  const domCheck = (where) => {
    const n = rowCount();
    rowsMax = Math.max(rowsMax, n);
    rowsMin = Math.min(rowsMin, n);
    return n > o.maxDomRows ? { error: `${where}: ${n} .tg-row elements in the DOM, more than ${o.maxDomRows}`, step: where, samples, rows: n } : null;
  };
  const initial = domCheck("before the first step (initial render)");
  if (initial) return initial;
  for (let i = 0; i < total; i++) {
    const top = sc.scrollTop + o.stepRows * o.rowHeight;
    // Row r's top edge is at rowHeight * (r + 1): the header row sits above row 0.
    const target = Math.floor((top + sc.clientHeight) / o.rowHeight) - 2;
    // Measured steps only: the first warm-up step starts at the top, where the grid has no rows
    // above to fetch and so already holds the target.
    if (i >= o.warmup && document.getElementById("tg-row-" + target)) {
      return { error: `${name(i)}: row ${target} is in the DOM before the step, so the step would time nothing`, step: name(i), samples };
    }
    const t0 = performance.now();
    sc.scrollTop = top;
    if (Math.abs(sc.scrollTop - top) > 1) {
      return { error: `${name(i)}: scrollTop is ${sc.scrollTop}, not ${top}`, step: name(i), samples };
    }
    for (;;) {
      await frame();
      if (document.getElementById("tg-row-" + target)) break;
      if (performance.now() - t0 > o.stepTimeoutMs) {
        return { error: `${name(i)}: row ${target} not shown within ${o.stepTimeoutMs} ms`, step: name(i), samples };
      }
    }
    await postPaint();
    const ms = performance.now() - t0;
    const over = domCheck(name(i));
    if (over) return over;
    if (i >= o.warmup) samples.push(ms);
  }
  return { samples, rowsMax, rowsMin, steps: total, lastTop: sc.scrollTop };
}

/** The options measureScroll takes, from AC9. */
export function scrollOptions() {
  return {
    warmup: AC9.scrollWarmup,
    samples: AC9.scrollSamples,
    stepRows: AC9.stepRows,
    rowHeight: AC9.rowHeight,
    maxDomRows: AC9.maxDomRows,
    stepTimeoutMs: AC9.stepTimeoutMs,
  };
}

/**
 * Turn a measureScroll result into a check result: throws on a DOM-row violation, a step that
 * failed, the wrong sample count, or a p95 at or over the threshold. The control (the same
 * samples judged against a 0 ms threshold must fail) shows the verdict is not fixed to pass.
 */
export function scrollVerdict(id, r) {
  if (!r || !Array.isArray(r.samples)) throw new Error(`no result: ${JSON.stringify(r).slice(0, 300)}`);
  if (r.error) throw new Error(r.error);
  if (r.samples.length !== AC9.scrollSamples) throw new Error(`${r.samples.length} samples, wanted ${AC9.scrollSamples}`);
  const m = { ...judge(id, r.samples, AC9.scrollP95Ms), rowsMax: r.rowsMax, rowsMin: r.rowsMin, maxDomRows: AC9.maxDomRows };
  if (judge(id, r.samples, 0).pass) throw new Error("control: a 0 ms threshold passed these samples");
  return m;
}
