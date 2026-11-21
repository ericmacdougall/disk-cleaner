// AC-9 investigation of the Linux running-app scroll (docs/DESKTOP.md), informational and never
// a gate. Registered before any sample (WAL w07983c42191, controls in the entry after it). The
// gated measurement is e2e/ac9.mjs and is not changed; these page functions reuse its step and
// floor code as given, so a step here is the same step as a gated one.

/**
 * Steps with floors between them, in the page. `measureScroll` and `measureFloor` are e2e/ac9.mjs's
 * functions, passed in. Order: a floor window (after `floorWarmup` unrecorded frames), half the
 * steps (with the usual warm-up steps), a floor window, the other half (no warm-up), a floor
 * window. The clock covers all of it, so a control can run for the same time.
 */
export async function stepsWithFloors(o, measureScroll, measureFloor) {
  const t0 = performance.now();
  const win = async (warmup) => {
    const r = await measureFloor({ warmup, samples: o.window });
    return { samples: r.samples, intervals: r.intervals };
  };
  const start = await win(o.floorWarmup);
  const a = await measureScroll({ ...o.scroll, samples: o.half });
  if (a.error) return { error: `first half: ${a.error}`, start, samples: a.samples };
  const mid = await win(0);
  const b = await measureScroll({ ...o.scroll, warmup: 0, samples: o.half });
  if (b.error) return { error: `second half: ${b.error}`, start, mid, samples: [...a.samples, ...b.samples] };
  const end = await win(0);
  return {
    elapsedMs: performance.now() - t0,
    start,
    mid,
    end,
    samples: [...a.samples, ...b.samples],
    rowsMax: Math.max(a.rowsMax, b.rowsMax),
  };
}

/**
 * The floor loop alone (requestAnimationFrame, then a MessageChannel message, nothing changed),
 * run for `durationMs` after `warmup` unrecorded frames, at most `maxSamples` frames. Returns each
 * sample with its start time and the rAF timestamp intervals. Self-contained.
 */
export async function floorFor(o) {
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
  const at = [];
  const t0 = performance.now();
  for (let i = 0; ; i++) {
    const s = performance.now();
    await frame();
    await postPaint();
    const e = performance.now();
    if (i >= o.warmup) {
      samples.push(e - s);
      at.push(s - t0);
    }
    if (e - t0 >= o.durationMs || samples.length >= o.maxSamples) break;
  }
  const intervals = [];
  for (let i = o.warmup + 1; i < stamps.length; i++) intervals.push(stamps[i] - stamps[i - 1]);
  return { samples, at, intervals, elapsedMs: performance.now() - t0, href: location.href, elements: document.getElementsByTagName("*").length };
}

/** p50/p95/max by nearest rank, as e2e/ac9.mjs ranks. */
export function stat(xs) {
  if (!xs?.length) return null;
  const s = [...xs].sort((x, y) => x - y);
  const r = (q) => s[Math.ceil(q * s.length) - 1];
  return { n: s.length, p50: r(0.5), p95: r(0.95), max: s[s.length - 1] };
}

/** Start, middle and end windows of `w` samples of one continuous series. */
export function windows(xs, w) {
  const n = xs.length;
  const m = Math.max(0, Math.floor(n / 2 - w / 2));
  return { start: stat(xs.slice(0, w)), mid: stat(xs.slice(m, m + w)), end: stat(xs.slice(Math.max(0, n - w))) };
}

/** Medians of consecutive blocks of `k` samples (the drift within a run). */
export function blockMedians(xs, k) {
  const out = [];
  for (let i = 0; i < xs.length; i += k) out.push(stat(xs.slice(i, i + k)).p50);
  return out;
}
