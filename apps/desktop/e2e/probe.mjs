// Harness probe (not a gate): can the packaged app be launched through tauri-driver, read,
// called through its IPC bridge, refused by its capability list, and checked with axe?
// usage: node e2e/probe.mjs --app <binary> --out <result.json> [--native-driver <path>]

import { App, Run, args, startDriver, stopDriver } from "./driver.mjs";

const a = args(process.argv.slice(2));
const run = new Run("probe");
const { proc, base } = await startDriver({ nativeDriver: a["native-driver"], env: process.env });
let app;
try {
  await run.check("launch", async () => {
    app = await App.open(base, a.app);
    return { title: await app.s.cmd("GET", "/title") };
  });
  if (app) {
    await run.check("ipc.app_info", async () => {
      const r = await app.invoke("app_info");
      if (!r.ok || r.value.read_only !== true) throw new Error(JSON.stringify(r));
      return r.value;
    });
    await run.check("ipc.list_volumes", async () => {
      const r = await app.invoke("list_volumes");
      if (!r.ok) throw new Error(JSON.stringify(r));
      return r.value.map((p) => `${p.path} ${p.file_system}`);
    });
    for (const [id, cmd, payload] of [
      ["ipc.refused.plugin_dialog_open", "plugin:dialog|open", { options: {} }],
      ["ipc.refused.core_path", "plugin:path|resolve_directory", { directory: 10 }],
      ["ipc.refused.unregistered", "delete_everything", {}],
    ]) {
      await run.check(id, async () => {
        const r = await app.invoke(cmd, payload);
        if (r.ok) throw new Error(`allowed: ${JSON.stringify(r.value)}`);
        return r.error;
      });
    }
    await run.check("axe.chooser", async () => {
      const r = await app.axe("body");
      if (!r.ok) throw new Error(r.error);
      return r;
    });
    await run.check("csp.inline_script_blocked", async () => {
      // The page itself cannot run inline script: CSP script-src 'self'.
      return app.s.execAsync(
        `const done = arguments[0];
         const s = document.createElement("script");
         window.__probe = 0;
         s.textContent = "window.__probe = 1";
         document.head.append(s);
         setTimeout(() => done({ inlineRan: window.__probe === 1 }), 200);`,
        5000,
      );
    });
  }
} finally {
  if (app) await app.close();
  stopDriver(proc);
}
process.exitCode = run.write(a.out ?? "probe-result.json") ? 1 : 0;
