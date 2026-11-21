// What machine a timing ran on (PR gate F9, informational): CPU model, logical CPUs, load,
// CPU time split (Linux: steal and iowait between two snapshots), memory, and the X screen.
// Every probe is best effort; a probe that is not available says so instead of failing.

import { spawnSync } from "node:child_process";
import { mkdtempSync, readdirSync, readFileSync } from "node:fs";
import os from "node:os";
import path from "node:path";

// On Windows the suite runs with APPDATA in the watched sandbox (e2e/check.sh); a probe must not
// write there, so its children get a throwaway folder outside every watched root instead.
let childEnv = process.env;
if (process.platform === "win32") {
  const scratch = mkdtempSync(path.join(os.tmpdir(), "prunedisk-runner-info-"));
  childEnv = { ...process.env, APPDATA: scratch, LOCALAPPDATA: scratch };
}

const run = (cmd, argv, ms = 15_000, max = 2000) => {
  try {
    const r = spawnSync(cmd, argv, { encoding: "utf8", timeout: ms, env: childEnv });
    if (r.error) return { error: String(r.error.code ?? r.error.message) };
    return { status: r.status, out: (r.stdout || "").trim().slice(0, max) };
  } catch (e) {
    return { error: String(e?.message ?? e) };
  }
};
const read = (p) => {
  try {
    return readFileSync(p, "utf8");
  } catch {
    return null;
  }
};

/** /proc/stat's first line as numbers: user nice system idle iowait irq softirq steal. */
function cpuTimes() {
  const line = read("/proc/stat")?.split("\n")[0];
  if (!line?.startsWith("cpu ")) return null;
  const [user, nice, system, idle, iowait, irq, softirq, steal] = line.trim().split(/\s+/).slice(1).map(Number);
  return { user, nice, system, idle, iowait, irq, softirq, steal };
}

/** A snapshot taken at the start or end of a session. */
export function snapshot() {
  const s = { at: new Date().toISOString(), loadavg: os.loadavg(), freeMem: os.freemem(), cpuTimes: cpuTimes() };
  if (process.platform === "linux") {
    s.memAvailableKb = Number(/MemAvailable:\s+(\d+)/.exec(read("/proc/meminfo") ?? "")?.[1] ?? NaN);
  } else if (process.platform === "win32") {
    // Windows has no load average (os.loadavg() is zeros); the processor's LoadPercentage instead.
    s.windowsLoadPercentage = run("powershell", ["-NoProfile", "-Command", "(Get-CimInstance Win32_Processor | Measure-Object -Property LoadPercentage -Average).Average"]).out ?? null;
  }
  return s;
}

/** The machine, once per session. */
export function machine() {
  const cpus = os.cpus();
  const m = {
    platform: `${process.platform} ${os.release()}`,
    arch: process.arch,
    cpuModel: cpus[0]?.model ?? null,
    logicalCpus: cpus.length,
    availableParallelism: os.availableParallelism?.() ?? null,
    totalMem: os.totalmem(),
  };
  if (process.platform === "linux") {
    const info = read("/proc/cpuinfo") ?? "";
    m.procCpuinfoModel = /^model name\s*:\s*(.+)$/m.exec(info)?.[1] ?? null;
    m.procCpuinfoMhz = /^cpu MHz\s*:\s*(.+)$/m.exec(info)?.[1] ?? null;
    m.nproc = run("nproc", []).out ?? null;
    m.display = process.env.DISPLAY ?? null;
    // Xvfb's screen as the X server reports it (xdpyinfo or xrandr, when installed).
    const dpy = run("xdpyinfo", []);
    m.xdpyinfo = dpy.out ? dpy.out.split("\n").filter((l) => /dimensions|depth of root|vendor string|version number/.test(l)).map((l) => l.trim()) : dpy;
    const xr = run("xrandr", ["--current"]);
    m.xrandr = xr.out ? xr.out.split("\n").slice(0, 4) : xr;
  } else if (process.platform === "win32") {
    const w = run("wmic", ["cpu", "get", "Name,NumberOfCores,NumberOfLogicalProcessors", "/format:list"]);
    m.wmic = w.out ? w.out.split(/\r?\n/).filter(Boolean) : w;
    const p = run("powershell", ["-NoProfile", "-Command", "Get-CimInstance Win32_Processor | ForEach-Object { \"$($_.Name)|$($_.NumberOfCores)|$($_.NumberOfLogicalProcessors)|$($_.MaxClockSpeed)\" }"]);
    m.win32Processor = p.out ?? p;
    const v = run("powershell", ["-NoProfile", "-Command", "Get-CimInstance Win32_VideoController | ForEach-Object { \"$($_.Name)|$($_.CurrentRefreshRate)|$($_.CurrentHorizontalResolution)x$($_.CurrentVerticalResolution)\" }"]);
    m.win32Video = v.out ?? v;
  }
  return m;
}

/**
 * Linux: the app's processes and whether each is traced (TracerPid in /proc/<pid>/status), so an
 * AC-9 investigation arm can show it ran with or without strace. `names` are the comm names
 * (at most 15 characters) to look for. Elsewhere: null.
 */
export function tracerState(names) {
  if (process.platform !== "linux") return null;
  let pids;
  try {
    pids = readdirSync("/proc").filter((d) => /^\d+$/.test(d));
  } catch {
    return null;
  }
  const procs = [];
  for (const pid of pids) {
    const st = read(`/proc/${pid}/status`);
    if (!st) continue;
    const name = /^Name:\s*(.*)$/m.exec(st)?.[1];
    if (!names.includes(name)) continue;
    procs.push({ pid: Number(pid), name, tracerPid: Number(/^TracerPid:\s*(\d+)/m.exec(st)?.[1] ?? NaN) });
  }
  return { procs, traced: procs.filter((p) => p.tracerPid > 0).length, untraced: procs.filter((p) => p.tracerPid === 0).length };
}

/**
 * Linux: the processes `names` names (comm) that are not in `before` (a Set of pids). For the
 * side finding of the AC-9 sessions: what is left of a session after it closed.
 */
export function procsSince(names, before) {
  return (tracerState(names)?.procs ?? []).filter((p) => !before.has(p.pid)).map((p) => p.pid);
}

/**
 * Linux: for each pid, whether it still exists and, if so, its state and parent from
 * /proc/<pid>/stat (state Z is a zombie), the parent's name, its TracerPid, and how many files it
 * holds open (/proc/<pid>/fd, a count only). Elsewhere: null.
 */
export function procStates(pids) {
  if (process.platform !== "linux") return null;
  return pids.map((pid) => {
    const stat = read(`/proc/${pid}/stat`);
    if (!stat) return { pid, alive: false };
    // comm is in parentheses and may hold spaces; the fields after the last ')' are fixed.
    const close = stat.lastIndexOf(")");
    const comm = stat.slice(stat.indexOf("(") + 1, close);
    const [state, ppid] = stat.slice(close + 2).split(" ");
    const parent = read(`/proc/${ppid}/stat`);
    const ppidComm = parent ? parent.slice(parent.indexOf("(") + 1, parent.lastIndexOf(")")) : null;
    const tracerPid = Number(/^TracerPid:\s*(\d+)/m.exec(read(`/proc/${pid}/status`) ?? "")?.[1] ?? NaN);
    let fds;
    try {
      fds = { count: readdirSync(`/proc/${pid}/fd`).length };
    } catch (e) {
      fds = { error: String(e.code ?? e.message) };
    }
    return { pid, alive: true, statLine: stat.trim().slice(0, 300), comm, state, ppid: Number(ppid), ppidComm, tracerPid, fds };
  });
}

/** Linux: `ps -eo pid,ppid,stat,args`, the lines matching `re` (the header kept). */
export function psTree(re) {
  if (process.platform !== "linux") return null;
  const r = run("ps", ["-eo", "pid,ppid,stat,args"], 15_000, 1_000_000);
  if (!r.out) return r;
  const lines = r.out.split("\n");
  return [lines[0], ...lines.slice(1).filter((l) => re.test(l))].map((l) => l.slice(0, 300));
}

/** CPU time shares between two snapshots (Linux), in percent of all CPU time. */
export function cpuShares(a, b) {
  if (!a?.cpuTimes || !b?.cpuTimes) return null;
  const d = {};
  let total = 0;
  for (const k of Object.keys(a.cpuTimes)) {
    d[k] = (b.cpuTimes[k] ?? 0) - (a.cpuTimes[k] ?? 0);
    total += d[k];
  }
  if (total <= 0) return null;
  return Object.fromEntries(Object.entries(d).map(([k, v]) => [k, Math.round((1000 * v) / total) / 10]));
}
