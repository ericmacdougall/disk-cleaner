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

/**
 * Informational, not a pass gate: the empty-frame floor. With nothing changed, `samples` times
 * of one requestAnimationFrame and then one MessageChannel message, on the same clock and from
 * the same starting point as a scroll step (right after the previous one ended), after `warmup`
 * unrecorded ones. Also the rAF timestamp interval (the frame period the webview runs at).
 * Self-contained, like measureScroll.
 */
export async function measureFloor(o) {
  const stamps = [];
  const frame = () =>
    new Promise((r) =>
      requestAnimationFrame((ts) => {
        stamps.push(ts);
        r();
      }),
    );
  const postPaint = () =>
    new Promise((r) => {
      const ch = new MessageChannel();
      ch.port1.onmessage = () => r();
      ch.port2.postMessage(0);
    });
  const samples = [];
  for (let i = 0; i < o.warmup + o.samples; i++) {
    const t0 = performance.now();
    await frame();
    await postPaint();
    if (i >= o.warmup) samples.push(performance.now() - t0);
  }
  const intervals = [];
  for (let i = o.warmup + 1; i < stamps.length; i++) intervals.push(stamps[i] - stamps[i - 1]);
  return {
    samples,
    intervals,
    screen: { width: screen.width, height: screen.height, colorDepth: screen.colorDepth, dpr: devicePixelRatio, inner: [innerWidth, innerHeight] },
    scrollerHeight: document.querySelector(".tg-scroll")?.clientHeight ?? null,
  };
}

/**
 * Informational, not a pass gate: the scroll steps of measureScroll again (same steps, target,
 * clock and stop), each split into slices. Runs after the gated measurement, so its own cost
 * never reaches the gated samples; its totals show what the instrumentation adds.
 * - toScroll: scrollTop set to the first scroll event on `.tg-scroll`;
 * - toIpc: that event to the page's IPC request (tree.ts load() before it fetches; tauri 2 sends
 *   IPC with fetch, which is wrapped here; `ipcCalls` counts them);
 * - ipc: the request to its response headers; body: reading the JSON body;
 * - toDom: the body read to the DOM holding the target row (tauri's callback, tree.ts render;
 *   seen by a MutationObserver on the grid);
 * - toStop: that to the stop (the next animation frame and the post-paint task);
 * - load = toIpc + ipc + body + toDom (the time inside tree.ts load());
 * - layoutReads: getBoundingClientRect, clientHeight, offsetHeight and scrollTop reads from the
 *   scroll event to the DOM change (each may force a synchronous layout).
 * Without fetch-based IPC (the mocked page) the IPC slices are null and toDom starts at the
 * scroll event.
 */
export async function profileScroll(o) {
  const sc = document.querySelector(".tg-scroll");
  if (!sc) return { error: "no .tg-scroll in the page", steps: [] };
  const now = () => performance.now();
  const frame = () => new Promise((r) => requestAnimationFrame(r));
  const postPaint = () =>
    new Promise((r) => {
      const ch = new MessageChannel();
      ch.port1.onmessage = () => r();
      ch.port2.postMessage(0);
    });
  // Originals, for this function's own reads and for restoring.
  const scrollTopD = Object.getOwnPropertyDescriptor(Element.prototype, "scrollTop");
  const clientHD = Object.getOwnPropertyDescriptor(Element.prototype, "clientHeight");
  const offsetHD = Object.getOwnPropertyDescriptor(HTMLElement.prototype, "offsetHeight");
  const gbcr = Element.prototype.getBoundingClientRect;
  const origFetch = window.fetch;
  const top0 = () => scrollTopD.get.call(sc);
  let cur = null;
  let reads = 0;
  const counting = (d) => ({
    ...d,
    get() {
      if (cur && cur.scroll !== null && cur.dom === null) reads++;
      return d.get.call(this);
    },
  });
  Object.defineProperty(Element.prototype, "scrollTop", counting(scrollTopD));
  Object.defineProperty(Element.prototype, "clientHeight", counting(clientHD));
  Object.defineProperty(HTMLElement.prototype, "offsetHeight", counting(offsetHD));
  Element.prototype.getBoundingClientRect = function () {
    if (cur && cur.scroll !== null && cur.dom === null) reads++;
    return gbcr.call(this);
  };
  window.fetch = function (input, init) {
    const c = cur;
    const isIpc = /^(ipc:|http:\/\/ipc\.localhost)/.test(String(input?.url ?? input));
    if (c && isIpc) {
      c.ipcCalls++;
      if (c.ipcStart === null) c.ipcStart = now();
    }
    const p = origFetch.call(window, input, init);
    if (!(c && isIpc)) return p;
    return p.then((res) => {
      if (c.ipcHeaders === null) c.ipcHeaders = now();
      const json = res.json.bind(res);
      res.json = () =>
        json().then((v) => {
          if (c.ipcBody === null) c.ipcBody = now();
          return v;
        });
      return res;
    });
  };
  // Capture on the document: it runs before the grid's own scroll handler, so the handler's work
  // (load() up to its request) falls after this timestamp.
  const onScroll = (e) => {
    if (e.target === sc && cur && cur.scroll === null) cur.scroll = now();
  };
  document.addEventListener("scroll", onScroll, { capture: true, passive: true });
  const mo = new MutationObserver(() => {
    if (cur && cur.dom === null && document.getElementById("tg-row-" + cur.target)) cur.dom = now();
  });
  mo.observe(sc, { childList: true, subtree: true });
  const steps = [];
  let error = null;
  try {
    for (let i = 0; i < o.warmup + o.samples; i++) {
      const top = top0() + o.stepRows * o.rowHeight;
      const target = Math.floor((top + clientHD.get.call(sc)) / o.rowHeight) - 2;
      if (i >= o.warmup && document.getElementById("tg-row-" + target)) {
        error = `profile step ${i + 1}: row ${target} is in the DOM before the step`;
        break;
      }
      cur = { target, t0: 0, scroll: null, ipcStart: null, ipcHeaders: null, ipcBody: null, dom: null, ipcCalls: 0 };
      reads = 0;
      cur.t0 = now();
      scrollTopD.set.call(sc, top);
      for (;;) {
        await frame();
        if (document.getElementById("tg-row-" + target)) break;
        if (now() - cur.t0 > o.stepTimeoutMs) {
          error = `profile step ${i + 1}: row ${target} not shown within ${o.stepTimeoutMs} ms`;
          break;
        }
      }
      if (error) break;
      await postPaint();
      const c = cur;
      const stop = now();
      cur = null;
      if (i < o.warmup) continue;
      const d = (a, b) => (a === null || b === null ? null : b - a);
      const ipc = c.ipcStart !== null;
      steps.push({
        total: stop - c.t0,
        toScroll: d(c.t0, c.scroll),
        toIpc: ipc ? d(c.scroll, c.ipcStart) : null,
        ipc: ipc ? d(c.ipcStart, c.ipcHeaders) : null,
        body: ipc ? d(c.ipcHeaders, c.ipcBody) : null,
        toDom: d(ipc ? c.ipcBody : c.scroll, c.dom),
        load: d(c.scroll, c.dom),
        toStop: d(c.dom, stop),
        ipcCalls: c.ipcCalls,
        layoutReads: reads,
      });
    }
  } finally {
    cur = null;
    mo.disconnect();
    document.removeEventListener("scroll", onScroll, { capture: true });
    Object.defineProperty(Element.prototype, "scrollTop", scrollTopD);
    Object.defineProperty(Element.prototype, "clientHeight", clientHD);
    Object.defineProperty(HTMLElement.prototype, "offsetHeight", offsetHD);
    Element.prototype.getBoundingClientRect = gbcr;
    window.fetch = origFetch;
  }
  return { error, steps };
}

/** Per-slice p50, p95 and max of profileScroll's steps (nearest rank; nulls left out). */
export function summarizeProfile(steps) {
  const out = {};
  for (const k of ["total", "toScroll", "toIpc", "ipc", "body", "toDom", "load", "toStop", "layoutReads", "ipcCalls"]) {
    const xs = steps.map((s) => s[k]).filter((x) => typeof x === "number");
    out[k] = xs.length ? { n: xs.length, p50: rank(xs, 0.5), p95: rank(xs, 0.95), max: Math.max(...xs) } : null;
  }
  return out;
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
