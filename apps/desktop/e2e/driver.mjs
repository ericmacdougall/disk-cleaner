// Start tauri-driver, open the app under test in a WebDriver session, and give checks a small
// API: page scripts, IPC through the page's own bridge, axe, and named results.

import { spawn } from "node:child_process";
import { readFileSync, writeFileSync } from "node:fs";
import { createRequire } from "node:module";
import path from "node:path";
import { Session } from "./webdriver.mjs";

const require = createRequire(import.meta.url);

export function args(argv) {
  const out = {};
  for (let i = 0; i < argv.length; i++) {
    const a = argv[i];
    if (a.startsWith("--")) out[a.slice(2)] = argv[i + 1]?.startsWith("--") || argv[i + 1] === undefined ? "true" : argv[++i];
  }
  return out;
}

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

/** Start tauri-driver on `port`; resolves once it answers /status. */
export async function startDriver({ driver = "tauri-driver", port = 4444, nativeDriver, env }) {
  // Windows runners name the Edge driver's folder in EDGEWEBDRIVER (Git Bash does not pass it
  // on reliably; Node reads it case-insensitively).
  if (!nativeDriver && process.platform === "win32" && process.env.EDGEWEBDRIVER) {
    nativeDriver = path.join(process.env.EDGEWEBDRIVER, "msedgedriver.exe");
  }
  if (nativeDriver) console.log(`native driver: ${nativeDriver}`);
  const argv = ["--port", String(port)];
  if (nativeDriver) argv.push("--native-driver", nativeDriver);
  const proc = spawn(driver, argv, { env, stdio: ["ignore", "inherit", "inherit"] });
  const base = `http://127.0.0.1:${port}`;
  for (let i = 0; i < 100; i++) {
    if (proc.exitCode !== null) throw new Error(`tauri-driver exited with ${proc.exitCode}`);
    try {
      const r = await fetch(`${base}/status`, { signal: AbortSignal.timeout(1000) });
      if (r.ok) return { proc, base };
    } catch {
      // Not up yet.
    }
    await sleep(200);
  }
  proc.kill();
  throw new Error("tauri-driver did not start");
}

export class Run {
  constructor(name) {
    this.name = name;
    this.results = [];
    this.measurements = [];
  }

  /** Run one named check; a throw is a failure with its message. */
  async check(id, f) {
    const t = Date.now();
    try {
      const detail = await f();
      this.results.push({ id, ok: true, ms: Date.now() - t, detail: detail ?? null });
      console.log(`PASS ${id}${detail ? ` ${JSON.stringify(detail)}` : ""}`);
    } catch (e) {
      this.results.push({ id, ok: false, ms: Date.now() - t, detail: String(e?.stack ?? e) });
      console.log(`FAIL ${id} ${String(e?.message ?? e)}`);
    }
  }

  measure(m) {
    this.measurements.push(m);
    console.log(`MEASURE ${JSON.stringify(m)}`);
  }

  write(path) {
    const failed = this.results.filter((r) => !r.ok).map((r) => r.id);
    writeFileSync(path, JSON.stringify({ suite: this.name, failed, results: this.results, measurements: this.measurements }, null, 2));
    console.log(`${this.name}: ${this.results.length - failed.length} passed, ${failed.length} failed${failed.length ? `: ${failed.join(", ")}` : ""}`);
    return failed.length;
  }
}

/** The app in a WebDriver session. */
export class App {
  constructor(session) {
    this.s = session;
  }

  static async open(base, application) {
    const s = await Session.start(base, { "tauri:options": { application } });
    const app = new App(s);
    await s.until("return document.querySelector('h1')?.textContent === 'PruneDisk' && !!window.__TAURI_INTERNALS__", 60_000);
    return app;
  }

  /** Call a command through the page's IPC bridge, as the app's own code does. */
  async invoke(cmd, payload = {}, timeoutMs = 60_000) {
    return this.s.execAsync(
      `const [cmd, payload, done] = arguments;
       window.__TAURI_INTERNALS__.invoke(cmd, payload).then(
         (v) => done({ ok: true, value: v }),
         (e) => done({ ok: false, error: typeof e === "string" ? e : JSON.stringify(e) }));`,
      timeoutMs,
      cmd,
      payload,
    );
  }

  /** Inject axe-core (the version in package-lock.json) and run it on `selector`. */
  async axe(selector = "body") {
    const loaded = await this.s.exec("return typeof window.axe === 'object'");
    if (!loaded) {
      const src = readFileSync(require.resolve("axe-core/axe.min.js"), "utf8");
      await this.s.exec(`${src}\nreturn typeof window.axe;`);
    }
    return this.s.execAsync(
      `const [sel, done] = arguments;
       window.axe.run(document.querySelector(sel), { resultTypes: ["violations"] }).then(
         (r) => done({ ok: true, version: window.axe.version, violations: r.violations.map((v) => ({ id: v.id, impact: v.impact, nodes: v.nodes.map((n) => n.target.join(" ")).slice(0, 5) })) }),
         (e) => done({ ok: false, error: String(e) }));`,
      120_000,
      selector,
    );
  }

  close() {
    return this.s.end();
  }
}
