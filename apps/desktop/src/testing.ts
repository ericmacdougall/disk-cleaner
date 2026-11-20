// A scriptable mock backend for the frontend tests (and the perf page).

import type { Backend, Row, ScanEvent, SettingsPayload, TreePage } from "./api";

export function row(id: number, name: string, extra: Partial<Row> = {}): Row {
  return {
    id,
    depth: 0,
    name,
    kind: "dir",
    size: 1000 * (id + 1),
    alloc: 4096 * (id + 1),
    files: 1,
    mtime: 1_767_225_600,
    percent_of_parent: 10,
    has_children: true,
    expanded: false,
    unreadable: false,
    mount_skipped: false,
    hardlink_duplicate: false,
    summary: false,
    tag: null,
    ...extra,
  };
}

/** A page of `total` synthetic rows, served by offset like the backend. */
export function syntheticPage(view: number, total: number, offset: number, limit: number, name = (i: number) => `entry-${i}`): TreePage {
  const n = Math.max(0, Math.min(limit, 1000, total - offset));
  return {
    view,
    total,
    offset,
    rows: Array.from({ length: n }, (_, i) => row(offset + i, name(offset + i))),
    sort: { key: "allocated", descending: true },
    focus: null,
    breadcrumb: [],
    source: { kind: "size walk", finished_at: 1_767_225_600, roots: ["/syn"], stats: null },
  };
}

export const SETTINGS: SettingsPayload = {
  core: {
    path: "/syn/config.toml",
    file_exists: true,
    active_window_days: 30,
    active_window_in_range: true,
    max_git_admin_bytes: 64 * 1024 * 1024,
    max_git_admin_in_range: true,
    trust_partial_process_check: false,
    accept_incomplete_coverage: false,
    source: "/syn/config.toml",
  },
  app: { top_files: 200, min_output_mib: 100, one_file_system: true },
  ranges: {
    active_window_days: [1, 3650],
    max_git_admin_bytes: [1 << 20, 2 ** 40],
    top_files: [10, 1000],
    min_output_mib: [1, 1_048_576],
  },
  replanned: false,
};

export interface Mock extends Backend {
  calls: { name: string; args: unknown[] }[];
  emit(e: ScanEvent): void;
}

/** A mock whose methods can be overridden. Unknown calls reject. */
export function mockBackend(over: Partial<Backend> = {}): Mock {
  const calls: { name: string; args: unknown[] }[] = [];
  let handler: ((e: ScanEvent) => void) | null = null;
  const base: Backend = {
    appInfo: async () => ({ version: "0.1.0", schema_version: 15, read_only: true }),
    listVolumes: async () => [],
    chooseFolders: async () => [],
    openScanFile: async () => null,
    exportPlan: async () => null,
    startScan: async () => 1,
    cancelScan: async () => true,
    currentState: async () => ({ job: null, state: "idle", view: null, imported: false, has_tree: false, has_analysis: false, roots: [] }),
    treePage: async (view, offset, limit) => syntheticPage(view, 50, offset, limit),
    treeToggle: async () => 50,
    treeSort: async () => 50,
    treeFocus: async () => 50,
    treeTiles: async (_v, node) => ({ node, breadcrumb: [], total_alloc: 0, tiles: [] }),
    topFiles: async () => [],
    devSummary: async () => null,
    devPage: async () => [],
    sessionFolders: async () => [],
    getSettings: async () => structuredClone(SETTINGS),
    saveSettings: async () => ({ report: { outcome: "saved", note: null, replanned: false }, settings: structuredClone(SETTINGS) }),
    onScanEvent: async (h) => {
      handler = h;
      return () => {
        handler = null;
      };
    },
  };
  const merged = { ...base, ...over } as Backend;
  const recorded = Object.fromEntries(
    Object.entries(merged).map(([name, f]) => [
      name,
      (...args: unknown[]) => {
        calls.push({ name, args });
        return (f as (...a: unknown[]) => unknown)(...args);
      },
    ]),
  ) as unknown as Backend;
  return {
    ...recorded,
    calls,
    emit: (e) => handler?.(e),
  };
}

/** Resolve pending promises. */
export async function flush(times = 5): Promise<void> {
  for (let i = 0; i < times; i++) await new Promise((r) => setTimeout(r, 0));
}
