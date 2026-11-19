// The backend's commands and events, typed. Every view talks to a `Backend`, so tests can use a
// mock. Shapes mirror apps/desktop/src-tauri/src/commands.rs and the view crate.

export type JobState = "idle" | "walking" | "analysing" | "done" | "failed" | "cancelled";
export type SortKey = "size" | "allocated" | "files" | "modified" | "name";
export type EntryKind = "dir" | "file" | "symlink" | "other";

export interface Sort {
  key: SortKey;
  descending: boolean;
}

export interface Progress {
  files: number;
  dirs: number;
  bytes: number;
  elapsed_ms: number;
  current: string;
}

export interface ScanEvent {
  job: number;
  state: JobState;
  progress?: Progress;
  view?: number;
  error?: string;
}

export interface Place {
  path: string;
  label: string;
  total_bytes: number | null;
  available_bytes: number | null;
  file_system: string | null;
  removable: boolean;
}

export interface CurrentState {
  job: number | null;
  state: JobState;
  view: number | null;
  imported: boolean;
  has_tree: boolean;
  has_analysis: boolean;
  roots: string[];
}

export interface Row {
  id: number;
  depth: number;
  name: string;
  kind: EntryKind;
  size: number;
  alloc: number;
  files: number;
  mtime: number | null;
  percent_of_parent: number;
  has_children: boolean;
  expanded: boolean;
  unreadable: boolean;
  mount_skipped: boolean;
  hardlink_duplicate: boolean;
  summary: boolean;
  tag: string | null;
}

export interface Crumb {
  id: number;
  name: string;
}

export interface WalkStats {
  files: number;
  dirs: number;
  symlinks: number;
  allocated_bytes: number;
  logical_bytes: number;
  hardlink_duplicates_skipped: number;
  unreadable: number;
  mount_points_skipped: number;
  elapsed_ms: number;
}

export interface TreePage {
  view: number;
  total: number;
  offset: number;
  rows: Row[];
  sort: Sort;
  focus: number | null;
  breadcrumb: Crumb[];
  source: { kind: string; finished_at: number | null; roots: string[]; stats: WalkStats | null };
}

export interface Tile {
  id: number | null;
  name: string;
  alloc: number;
  is_dir: boolean;
  tag: string | null;
}

export interface Tiles {
  node: number | null;
  breadcrumb: Crumb[];
  total_alloc: number;
  tiles: Tile[];
}

export interface TopFile {
  id: number | null;
  path: string;
  bytes: number;
}

export interface DevGroup {
  class: string;
  label: string;
  count: number;
  class_bytes: number;
}

export interface DevSummary {
  source: string;
  scan_generated_at: number;
  plan_generated_at: number;
  imported: boolean;
  scan_age_days: number;
  stale_warning: string | null;
  imported_note: string | null;
  groups: DevGroup[];
  bytes_by_class: Record<string, number>;
  plan_totals: {
    reclaimable_bytes: number;
    ready_bytes: number;
    by_action: Record<string, { items: number; reclaimable_bytes: number }>;
  };
  coverage_complete: boolean;
  coverage_note: string | null;
  dry_run: boolean;
  remote_facts_verified: boolean;
  remote_facts_note: string;
  process_check: string;
  process_check_note: string | null;
  active_window_days: number;
  window_source: string;
  roots: string[];
}

export interface DevItem {
  node: number;
  path: string;
  class: string;
  action: string;
  removes: boolean;
  reason: string;
  state: string;
  state_reason: string;
  reclaimable_bytes: number;
  bytes_total: number;
  files_total: number;
  newest_mtime: number | null;
  days_since_activity: number | null;
  facts_no: string[];
  facts_unknown: string[];
}

export interface SessionFolder {
  id: number;
  layout: string;
  path: string;
  alloc: number;
  size: number;
  files: number;
  newest_mtime: number | null;
  plan_note: string;
  enclosing: { action: string; path: string } | null;
}

export interface CoreSettingsView {
  path: string;
  file_exists: boolean;
  active_window_days: number;
  active_window_in_range: boolean;
  max_git_admin_bytes: number;
  max_git_admin_in_range: boolean;
  trust_partial_process_check: boolean;
  accept_incomplete_coverage: boolean;
  source: string;
}

export interface AppSettings {
  top_files: number;
  min_output_mib: number;
  one_file_system: boolean;
}

export interface SettingsPayload {
  core: CoreSettingsView;
  app: AppSettings;
  ranges: {
    active_window_days: [number, number];
    max_git_admin_bytes: [number, number];
    top_files: [number, number];
    min_output_mib: [number, number];
  };
}

export interface CoreChanges {
  active_window_days: number | null;
  max_git_admin_bytes: number | null;
}

export interface SaveResult {
  report: { outcome: "saved" | "unchanged" | "saved_unconfirmed"; note: string | null; replanned: boolean };
  settings: SettingsPayload;
}

/** An error from a command (state.rs `AppError`). */
export interface AppError {
  kind: "refused" | "stale" | "needs_confirmation" | "failed";
  message: string;
  old?: number;
  new?: number;
}

export function errorMessage(e: unknown): string {
  if (e && typeof e === "object" && "message" in e && typeof (e as AppError).message === "string") {
    return (e as AppError).message;
  }
  return String(e);
}

export function errorKind(e: unknown): AppError["kind"] | null {
  return e && typeof e === "object" && "kind" in e ? (e as AppError).kind : null;
}

export interface Backend {
  appInfo(): Promise<{ version: string; schema_version: number; read_only: boolean }>;
  listVolumes(): Promise<Place[]>;
  chooseFolders(): Promise<Place[]>;
  openScanFile(): Promise<CurrentState | null>;
  exportPlan(view: number): Promise<string | null>;
  startScan(roots: string[], collectGitFacts: boolean): Promise<number>;
  cancelScan(job: number): Promise<boolean>;
  currentState(): Promise<CurrentState>;
  treePage(view: number, offset: number, limit: number): Promise<TreePage>;
  treeToggle(view: number, node: number, expand?: boolean): Promise<number>;
  treeSort(view: number, key: SortKey, descending: boolean): Promise<number>;
  treeFocus(view: number, node: number | null): Promise<number>;
  treeTiles(view: number, node: number | null, limit: number): Promise<Tiles>;
  topFiles(view: number, limit: number): Promise<TopFile[]>;
  devSummary(view: number): Promise<DevSummary | null>;
  devPage(view: number, cls: string, offset: number, limit: number): Promise<DevItem[]>;
  sessionFolders(view: number): Promise<SessionFolder[]>;
  getSettings(): Promise<SettingsPayload>;
  saveSettings(core: CoreChanges, app: AppSettings | null, confirmShorten: boolean): Promise<SaveResult>;
  onScanEvent(handler: (e: ScanEvent) => void): Promise<() => void>;
}

/** The real backend, over Tauri IPC. */
export async function tauriBackend(): Promise<Backend> {
  const { invoke } = await import("@tauri-apps/api/core");
  const { listen } = await import("@tauri-apps/api/event");
  return {
    appInfo: () => invoke("app_info"),
    listVolumes: () => invoke("list_volumes"),
    chooseFolders: () => invoke("choose_folders"),
    openScanFile: () => invoke("open_scan_file"),
    exportPlan: (view) => invoke("export_plan", { view }),
    startScan: (roots, collectGitFacts) =>
      invoke("start_scan", { roots, settings: { collect_git_facts: collectGitFacts } }),
    cancelScan: (job) => invoke("cancel_scan", { job }),
    currentState: () => invoke("current_state"),
    treePage: (view, offset, limit) => invoke("tree_page", { view, offset, limit }),
    treeToggle: (view, node, expand) => invoke("tree_toggle", { view, node, expand: expand ?? null }),
    treeSort: (view, key, descending) => invoke("tree_sort", { view, key, descending }),
    treeFocus: (view, node) => invoke("tree_focus", { view, node }),
    treeTiles: (view, node, limit) => invoke("tree_tiles", { view, node, limit }),
    topFiles: (view, limit) => invoke("top_files", { view, limit }),
    devSummary: (view) => invoke("dev_summary", { view }),
    devPage: (view, cls, offset, limit) => invoke("dev_page", { view, class: cls, offset, limit }),
    sessionFolders: (view) => invoke("session_folders", { view }),
    getSettings: () => invoke("get_settings"),
    saveSettings: (core, app, confirmShorten) => invoke("save_settings", { core, app, confirmShorten }),
    onScanEvent: async (handler) => {
      const un = await listen<ScanEvent>("scan", (e) => handler(e.payload));
      return un;
    },
  };
}
