//! The commands the webview may call (spec AC-12). No path ever comes from the webview except
//! scan roots, which are checked; import and export paths come from native dialogs opened here
//! (built by dialogs.rs, which also picks the folder each one starts in).
//! View commands take a view id, node ids, sort keys and bounded counts (at most 1,000).
//! Every command runs off the UI thread (`async`).

use crate::dialogs;
use crate::state::{AppError, Emit, Inner, SaveReport, ScanEvent, ScanSettings};
use disk_cleaner_view::dev::{DevItem, DevSummary};
use disk_cleaner_view::export::{stamp, write_plan_new};
use disk_cleaner_view::jobs::{JobId, JobState};
use disk_cleaner_view::sanitize::display_name;
use disk_cleaner_view::sessions::SessionFolder;
use disk_cleaner_view::settings::{AppSettings, CoreChanges, SettingsView};
use disk_cleaner_view::table::{Crumb, Row, Sort, SortKey, Tile, MAX_PAGE};
use disk_cleaner_view::tree::{NodeId, TopFile, WalkStats};
use serde::Serialize;
use std::path::PathBuf;
use std::sync::Arc;
use tauri::{AppHandle, Emitter, State};

/// The managed state.
pub struct AppState(pub Arc<Inner>);

type Res<T> = Result<T, AppError>;

fn cap(limit: usize) -> usize {
    limit.min(MAX_PAGE)
}

fn emitter(app: &AppHandle) -> Emit {
    let app = app.clone();
    Arc::new(move |e: &ScanEvent| {
        let _ = app.emit("scan", e);
    })
}

/// Run blocking work off the async runtime's threads.
async fn blocking<T: Send + 'static>(f: impl FnOnce() -> Res<T> + Send + 'static) -> Res<T> {
    tauri::async_runtime::spawn_blocking(f)
        .await
        .map_err(|e| AppError::failed(format!("internal error: {e}")))?
}

#[derive(Serialize)]
pub struct AppInfo {
    pub version: &'static str,
    pub schema_version: u32,
    pub read_only: bool,
}

#[tauri::command]
pub async fn app_info() -> Res<AppInfo> {
    Ok(AppInfo {
        version: env!("CARGO_PKG_VERSION"),
        schema_version: disk_cleaner_core::SCHEMA_VERSION,
        read_only: true,
    })
}

/// A folder or drive the user can scan. `path` is what Start sends back; `label` is shown.
#[derive(Serialize)]
pub struct Place {
    pub path: String,
    pub label: String,
    pub total_bytes: Option<u64>,
    pub available_bytes: Option<u64>,
    pub file_system: Option<String>,
    pub removable: bool,
}

fn place(p: PathBuf) -> Res<Place> {
    let path = p.to_str().map(str::to_string).ok_or_else(|| {
        AppError::refused(format!(
            "{} is not valid Unicode; scan it with the command line",
            display_name(&p.to_string_lossy())
        ))
    })?;
    Ok(Place {
        label: display_name(&path),
        path,
        total_bytes: None,
        available_bytes: None,
        file_system: None,
        removable: false,
    })
}

/// The mounted drives. Each one listed may be scanned (Inner::offer_roots).
#[tauri::command]
pub async fn list_volumes(state: State<'_, AppState>) -> Res<Vec<Place>> {
    let inner = Arc::clone(&state.0);
    blocking(move || {
        let disks = sysinfo::Disks::new_with_refreshed_list();
        let mut out = Vec::new();
        for d in disks.list() {
            let Ok(mut p) = place(d.mount_point().to_path_buf()) else {
                continue;
            };
            let name = d.name().to_string_lossy();
            if !name.is_empty() && name != p.path {
                p.label = format!("{} ({})", display_name(&p.path), display_name(&name));
            }
            p.total_bytes = Some(d.total_space());
            p.available_bytes = Some(d.available_space());
            p.file_system = Some(display_name(&d.file_system().to_string_lossy()));
            p.removable = d.is_removable();
            out.push(p);
        }
        out.sort_by(|a, b| a.path.cmp(&b.path));
        out.dedup_by(|a, b| a.path == b.path);
        inner.offer_roots(out.iter().map(|p| p.path.as_str()));
        Ok(out)
    })
    .await
}

/// Folders picked in the system dialog. Each one picked may be scanned (Inner::offer_roots).
#[tauri::command]
pub async fn choose_folders(app: AppHandle, state: State<'_, AppState>) -> Res<Vec<Place>> {
    let inner = Arc::clone(&state.0);
    blocking(move || {
        let picked: Vec<PathBuf> = pick_folders(&app)?;
        let places: Vec<Place> = picked.into_iter().map(place).collect::<Res<_>>()?;
        inner.offer_roots(places.iter().map(|p| p.path.as_str()));
        Ok(places)
    })
    .await
}

/// The folder dialog, or (test builds with the `e2e` feature) the folders it is told to pick.
fn pick_folders(app: &AppHandle) -> Res<Vec<PathBuf>> {
    #[cfg(feature = "e2e")]
    if let Some(p) = crate::e2e::folders() {
        return Ok(p);
    }
    dialogs::file_dialog(app, "Choose folders to scan")
        .blocking_pick_folders()
        .unwrap_or_default()
        .into_iter()
        .map(|f| {
            f.into_path()
                .map_err(|e| AppError::refused(format!("cannot use that folder: {e}")))
        })
        .collect()
}

/// What the screen needs after an import or at start.
#[derive(Serialize)]
pub struct CurrentState {
    pub job: Option<JobId>,
    pub state: JobState,
    pub view: Option<u64>,
    pub imported: bool,
    pub has_tree: bool,
    pub has_analysis: bool,
    pub roots: Vec<String>,
}

fn current(inner: &Inner) -> CurrentState {
    let (job, state) = inner.jobs.current();
    let s = inner.session();
    CurrentState {
        job,
        state,
        view: (s.view_id != 0).then_some(s.view_id),
        imported: s.analysis.as_ref().is_some_and(|a| a.imported),
        has_tree: s.tree.is_some(),
        has_analysis: s.analysis.is_some(),
        roots: s.roots.clone(),
    }
}

#[tauri::command]
pub async fn current_state(state: State<'_, AppState>) -> Res<CurrentState> {
    Ok(current(&state.0))
}

/// Pick a scan JSON file (written by `disk-cleaner scan`) and show it.
#[tauri::command]
pub async fn open_scan_file(app: AppHandle, state: State<'_, AppState>) -> Res<Option<CurrentState>> {
    let inner = Arc::clone(&state.0);
    blocking(move || {
        let Some(path) = pick_scan_file(&app)? else {
            return Ok(None);
        };
        inner.import_file(&path)?;
        Ok(Some(current(&inner)))
    })
    .await
}

/// Save the shown plan as JSON to a new file the user names. An existing file is never
/// replaced (spec NEG-6).
#[tauri::command]
pub async fn export_plan(app: AppHandle, state: State<'_, AppState>, view: u64) -> Res<Option<String>> {
    let inner = Arc::clone(&state.0);
    blocking(move || {
        let plan = {
            let s = inner.view(view)?;
            let a = s
                .analysis
                .as_ref()
                .ok_or_else(|| AppError::refused("the plan is not ready yet"))?;
            Arc::clone(&a.plan)
        };
        let Some(path) = pick_export_file(&app)? else {
            return Ok(None);
        };
        write_plan_new(&path, &plan).map_err(|e| AppError::refused(e.to_string()))?;
        Ok(Some(display_name(&path.to_string_lossy())))
    })
    .await
}

/// The open dialog, or (test builds with the `e2e` feature) the next file it is told to pick.
fn pick_scan_file(app: &AppHandle) -> Res<Option<PathBuf>> {
    #[cfg(feature = "e2e")]
    if let Some(answer) = crate::e2e::next_open() {
        return Ok(answer);
    }
    let Some(picked) = dialogs::file_dialog(app, "Open a scan file")
        .add_filter("Scan JSON", &["json"])
        .blocking_pick_file()
    else {
        return Ok(None);
    };
    picked
        .into_path()
        .map(Some)
        .map_err(|e| AppError::refused(format!("cannot use that file: {e}")))
}

/// The save dialog, or (test builds with the `e2e` feature) the next file it is told to name.
fn pick_export_file(app: &AppHandle) -> Res<Option<PathBuf>> {
    #[cfg(feature = "e2e")]
    if let Some(answer) = crate::e2e::next_export() {
        return Ok(answer);
    }
    let name = format!("prunedisk-plan-{}.json", stamp(disk_cleaner_view::units::now()));
    let Some(picked) = dialogs::file_dialog(app, "Export the plan")
        .set_file_name(&name)
        .add_filter("JSON", &["json"])
        .blocking_save_file()
    else {
        return Ok(None);
    };
    picked
        .into_path()
        .map(Some)
        .map_err(|e| AppError::refused(format!("cannot use that file: {e}")))
}

#[tauri::command]
pub async fn start_scan(
    app: AppHandle,
    state: State<'_, AppState>,
    roots: Vec<String>,
    settings: ScanSettings,
) -> Res<JobId> {
    let inner = Arc::clone(&state.0);
    let emit = emitter(&app);
    blocking(move || inner.start_scan(&roots, settings, emit)).await
}

#[tauri::command]
pub async fn cancel_scan(app: AppHandle, state: State<'_, AppState>, job: JobId) -> Res<bool> {
    Ok(state.0.cancel(job, &emitter(&app)))
}

/// Where the size tree's numbers come from.
#[derive(Serialize)]
pub struct TreeSource {
    /// Always "size walk".
    pub kind: &'static str,
    pub finished_at: Option<i64>,
    pub roots: Vec<String>,
    pub stats: Option<WalkStats>,
}

#[derive(Serialize)]
pub struct TreePage {
    pub view: u64,
    pub total: usize,
    pub offset: usize,
    pub rows: Vec<Row>,
    pub sort: Sort,
    pub focus: Option<NodeId>,
    pub breadcrumb: Vec<Crumb>,
    pub source: TreeSource,
}

#[tauri::command]
pub async fn tree_page(state: State<'_, AppState>, view: u64, offset: usize, limit: usize) -> Res<TreePage> {
    let inner = Arc::clone(&state.0);
    blocking(move || {
        let s = inner.view(view)?;
        let t = s
            .tree
            .as_ref()
            .ok_or_else(|| AppError::refused("an imported scan has no size tree"))?;
        Ok(TreePage {
            view,
            total: t.row_count(),
            offset,
            rows: t.page(offset, cap(limit)),
            sort: t.sort(),
            focus: t.focus(),
            breadcrumb: t.breadcrumb(),
            source: TreeSource {
                kind: "size walk",
                finished_at: s.walk_finished_at,
                roots: s.roots.clone(),
                stats: s.stats.clone(),
            },
        })
    })
    .await
}

/// Expand (`true`), collapse (`false`) or toggle (absent) a folder. Returns the row count.
#[tauri::command]
pub async fn tree_toggle(
    state: State<'_, AppState>,
    view: u64,
    node: NodeId,
    expand: Option<bool>,
) -> Res<usize> {
    let inner = Arc::clone(&state.0);
    blocking(move || {
        let mut s = inner.view(view)?;
        let t = s.tree.as_mut().ok_or_else(|| AppError::refused("no size tree"))?;
        match expand {
            Some(true) => t.expand(node),
            Some(false) => t.collapse(node),
            None => t.toggle(node),
        };
        Ok(t.row_count())
    })
    .await
}

#[tauri::command]
pub async fn tree_sort(state: State<'_, AppState>, view: u64, key: SortKey, descending: bool) -> Res<usize> {
    let inner = Arc::clone(&state.0);
    blocking(move || {
        let mut s = inner.view(view)?;
        let t = s.tree.as_mut().ok_or_else(|| AppError::refused("no size tree"))?;
        t.set_sort(Sort { key, descending });
        Ok(t.row_count())
    })
    .await
}

/// Drill into a folder (`Some`) or back to every root (`None`). Returns the row count.
#[tauri::command]
pub async fn tree_focus(state: State<'_, AppState>, view: u64, node: Option<NodeId>) -> Res<usize> {
    let inner = Arc::clone(&state.0);
    blocking(move || {
        let mut s = inner.view(view)?;
        let t = s.tree.as_mut().ok_or_else(|| AppError::refused("no size tree"))?;
        if !t.set_focus(node) {
            return Err(AppError::refused("that entry is not a folder"));
        }
        Ok(t.row_count())
    })
    .await
}

#[derive(Serialize)]
pub struct Tiles {
    pub node: Option<NodeId>,
    pub breadcrumb: Vec<Crumb>,
    pub total_alloc: u64,
    pub tiles: Vec<Tile>,
}

#[tauri::command]
pub async fn tree_tiles(
    state: State<'_, AppState>,
    view: u64,
    node: Option<NodeId>,
    limit: usize,
) -> Res<Tiles> {
    let inner = Arc::clone(&state.0);
    blocking(move || {
        let s = inner.view(view)?;
        let t = s.tree.as_ref().ok_or_else(|| AppError::refused("no size tree"))?;
        let tree = t.tree();
        if let Some(n) = node {
            if n as usize >= tree.len() {
                return Err(AppError::refused("no such entry"));
            }
        }
        let breadcrumb = match node {
            None => Vec::new(),
            Some(n) => tree
                .ancestry(n)
                .into_iter()
                .map(|id| Crumb {
                    id,
                    name: display_name(tree.name(id)),
                })
                .collect(),
        };
        let total_alloc = match node {
            Some(n) => tree.alloc(n),
            None => tree.total_alloc(),
        };
        Ok(Tiles {
            node,
            breadcrumb,
            total_alloc,
            tiles: t.tiles(node, cap(limit)),
        })
    })
    .await
}

#[tauri::command]
pub async fn top_files(state: State<'_, AppState>, view: u64, limit: usize) -> Res<Vec<TopFile>> {
    let inner = Arc::clone(&state.0);
    blocking(move || {
        let s = inner.view(view)?;
        Ok(s.top_files.iter().take(cap(limit)).cloned().collect())
    })
    .await
}

/// The developer view's summary, or `None` while the analysis runs.
#[tauri::command]
pub async fn dev_summary(state: State<'_, AppState>, view: u64) -> Res<Option<DevSummary>> {
    let inner = Arc::clone(&state.0);
    blocking(move || {
        let s = inner.view(view)?;
        Ok(s.analysis.as_ref().map(|a| a.dev.summary.clone()))
    })
    .await
}

#[tauri::command]
pub async fn dev_page(
    state: State<'_, AppState>,
    view: u64,
    class: String,
    offset: usize,
    limit: usize,
) -> Res<Vec<DevItem>> {
    let inner = Arc::clone(&state.0);
    blocking(move || {
        let s = inner.view(view)?;
        let a = s
            .analysis
            .as_ref()
            .ok_or_else(|| AppError::refused("the analysis is not ready yet"))?;
        Ok(a.dev.page(&class, offset, cap(limit)))
    })
    .await
}

#[tauri::command]
pub async fn session_folders(state: State<'_, AppState>, view: u64) -> Res<Vec<SessionFolder>> {
    let inner = Arc::clone(&state.0);
    blocking(move || Ok(inner.view(view)?.sessions.clone())).await
}

#[derive(Serialize)]
pub struct Ranges {
    pub active_window_days: [u64; 2],
    pub max_git_admin_bytes: [u64; 2],
    pub top_files: [u64; 2],
    pub min_output_mib: [u64; 2],
}

#[derive(Serialize)]
pub struct SettingsPayload {
    pub core: SettingsView,
    pub app: AppSettings,
    pub ranges: Ranges,
    /// A reload found the file changed in a way that changes the plan, and the shown plan was
    /// recomputed (PR gate F4).
    pub replanned: bool,
}

fn settings_payload(inner: &Inner, reload: bool) -> Res<SettingsPayload> {
    use disk_cleaner_view::settings::{GIT_ADMIN_BYTES, WINDOW_DAYS};
    let (loaded, replanned) = if reload {
        let loaded = inner.load_settings()?;
        (loaded, inner.replan()?)
    } else {
        (inner.load_settings_cached()?, false)
    };
    Ok(SettingsPayload {
        replanned,
        core: loaded.view(),
        app: inner.app_settings(),
        ranges: Ranges {
            active_window_days: [*WINDOW_DAYS.start(), *WINDOW_DAYS.end()],
            max_git_admin_bytes: [*GIT_ADMIN_BYTES.start(), *GIT_ADMIN_BYTES.end()],
            top_files: [10, 1000],
            min_output_mib: [1, 1_048_576],
        },
    })
}

/// Settings as the file holds them now (reloaded each time Settings opens). A file another
/// program changed recomputes the shown plan.
#[tauri::command]
pub async fn get_settings(state: State<'_, AppState>) -> Res<SettingsPayload> {
    let inner = Arc::clone(&state.0);
    blocking(move || settings_payload(&inner, true)).await
}

#[derive(Serialize)]
pub struct SaveResult {
    pub report: SaveReport,
    pub settings: SettingsPayload,
}

/// Save Settings. Shortening the active window needs `confirm_shorten` (the screen asks).
#[tauri::command]
pub async fn save_settings(
    state: State<'_, AppState>,
    core: CoreChanges,
    app: Option<AppSettings>,
    confirm_shorten: bool,
) -> Res<SaveResult> {
    let inner = Arc::clone(&state.0);
    blocking(move || {
        let report = inner.save_settings(core, app, confirm_shorten)?;
        Ok(SaveResult {
            report,
            settings: settings_payload(&inner, false)?,
        })
    })
    .await
}
