//! The app's state and its scan jobs, with no Tauri types, so the job flow is testable.
//!
//! Rules (spec AC-3, NEG-1, NEG-2, NEG-4):
//! - A result is accepted, and Cancel decided, under the session lock, so a cancelled job's
//!   result can never be stored after Cancel returned.
//! - Expensive work (tree index, parse, plan, import) runs outside the session lock; the result
//!   is swapped in under it after the job id and the settings generation are checked.
//! - The view commands name a view id; a stale id is refused, never answered from newer data.

use crate::worker::{spawn_worker, worker_exe, WorkerArgs, WorkerHandle, GO};
use disk_cleaner_core::{running_cwds, Plan, ScanResult};
use disk_cleaner_view::dev::{make_plan, DevView, PlanInputs};
use disk_cleaner_view::import::{self, Limits, MAX_IMPORT_BYTES};
use disk_cleaner_view::jobs::{JobId, JobState, Jobs, Ticket};
use disk_cleaner_view::sanitize::display_name;
use disk_cleaner_view::sessions::{self, SessionFolder};
use disk_cleaner_view::settings::{
    self, AppSettings, CoreChanges, Loaded, SaveHooks, SaveOutcome, SettingsError,
};
use disk_cleaner_view::table::TreeView;
use disk_cleaner_view::tree::{NodeId, TopFile, WalkStats};
use disk_cleaner_view::units;
use disk_cleaner_view::walk::{check_root, walk, Progress, WalkError, WalkOptions};
use serde::Serialize;
use std::collections::HashMap;
use std::ffi::OsString;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

/// At most this many roots per scan.
pub const MAX_ROOTS: usize = 64;

/// What the screen hears about a job.
#[derive(Debug, Clone, Serialize)]
pub struct ScanEvent {
    pub job: JobId,
    pub state: JobState,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub progress: Option<Progress>,
    /// Set once the job's results can be paged.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub view: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

pub type Emit = Arc<dyn Fn(&ScanEvent) + Send + Sync>;

/// A scan and its plan, as shown.
pub struct Analysis {
    pub scan: Arc<ScanResult>,
    pub plan: Arc<Plan>,
    pub dev: DevView,
    pub imported: bool,
    pub settings_gen: u64,
}

/// What the screen shows: one scan's tree and analysis, or one imported scan.
#[derive(Default)]
pub struct Session {
    /// 0 when nothing is loaded.
    pub view_id: u64,
    pub job: Option<JobId>,
    pub tree: Option<TreeView>,
    pub top_files: Vec<TopFile>,
    pub stats: Option<WalkStats>,
    pub walk_finished_at: Option<i64>,
    pub analysis: Option<Analysis>,
    pub sessions: Vec<SessionFolder>,
    pub roots: Vec<String>,
}

/// Errors the commands return. Messages are display-safe.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum AppError {
    Refused { message: String },
    Stale { message: String },
    NeedsConfirmation { message: String, old: u64, new: u64 },
    Failed { message: String },
}

impl AppError {
    pub fn refused(m: impl Into<String>) -> Self {
        AppError::Refused { message: m.into() }
    }
    pub fn failed(m: impl Into<String>) -> Self {
        AppError::Failed { message: m.into() }
    }
    pub fn stale() -> Self {
        AppError::Stale {
            message: "these results were replaced; showing the current ones".into(),
        }
    }
}

impl std::fmt::Display for AppError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AppError::Refused { message }
            | AppError::Stale { message }
            | AppError::NeedsConfirmation { message, .. }
            | AppError::Failed { message } => f.write_str(message),
        }
    }
}

impl From<SettingsError> for AppError {
    fn from(e: SettingsError) -> Self {
        match e {
            SettingsError::NeedsConfirmation { old, new } => AppError::NeedsConfirmation {
                message: e.to_string(),
                old,
                new,
            },
            SettingsError::Invalid(m) | SettingsError::Refused(m) => AppError::Refused { message: m },
            SettingsError::Io(m) => AppError::Failed { message: m },
        }
    }
}

/// The scan settings the screen sends with Start.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Deserialize)]
pub struct ScanSettings {
    pub collect_git_facts: bool,
}

/// Shared state.
pub struct Inner {
    pub jobs: Jobs,
    session: Mutex<Session>,
    workers: Mutex<HashMap<JobId, Arc<WorkerHandle>>>,
    next_view: AtomicU64,
    /// Core's config file, or why it cannot be found.
    config_path: Result<PathBuf, String>,
    /// The config file as Settings last loaded or saved it.
    loaded: Mutex<Option<Loaded>>,
    settings_gen: AtomicU64,
    app_settings: Mutex<AppSettings>,
    /// The worker executable and git program (tests use the built app and a fake git).
    worker_exe: Mutex<Option<PathBuf>>,
    git_program: Mutex<OsString>,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|p| p.into_inner())
}

impl Inner {
    /// `config_path`: core's config file (`None`: core's default path).
    pub fn new(config_path: Option<PathBuf>) -> Arc<Inner> {
        let config_path = match config_path {
            Some(p) => Ok(p),
            None => settings::default_core_path().map_err(|e| e.to_string()),
        };
        let app = match &config_path {
            Ok(p) => settings::load_app_settings(&settings::app_settings_path(p)).0,
            Err(_) => AppSettings::default(),
        };
        Arc::new(Inner {
            jobs: Jobs::new(),
            session: Mutex::new(Session::default()),
            workers: Mutex::new(HashMap::new()),
            next_view: AtomicU64::new(1),
            config_path,
            loaded: Mutex::new(None),
            settings_gen: AtomicU64::new(1),
            app_settings: Mutex::new(app),
            worker_exe: Mutex::new(None),
            git_program: Mutex::new("git".into()),
        })
    }

    /// Tests: run this executable as the worker, with this git program.
    pub fn set_worker(&self, exe: PathBuf, git: OsString) {
        *lock(&self.worker_exe) = Some(exe);
        *lock(&self.git_program) = git;
    }

    pub fn session(&self) -> MutexGuard<'_, Session> {
        lock(&self.session)
    }

    /// The session, if `view` is the current view.
    pub fn view(&self, view: u64) -> Result<MutexGuard<'_, Session>, AppError> {
        let s = self.session();
        if view == 0 || s.view_id != view {
            return Err(AppError::stale());
        }
        Ok(s)
    }

    fn new_view_id(&self) -> u64 {
        self.next_view.fetch_add(1, Ordering::Relaxed)
    }

    pub fn app_settings(&self) -> AppSettings {
        *lock(&self.app_settings)
    }

    /// Workers still running (tests).
    pub fn worker_pids(&self) -> Vec<u32> {
        lock(&self.workers).values().map(|h| h.pid()).collect()
    }

    // ---- settings -------------------------------------------------------------------

    fn config_path(&self) -> Result<PathBuf, AppError> {
        self.config_path.clone().map_err(AppError::failed)
    }

    /// Load (or reload) the config file as Settings shows it.
    pub fn load_settings(&self) -> Result<Loaded, AppError> {
        let loaded = settings::load_core(&self.config_path()?)?;
        *lock(&self.loaded) = Some(loaded.clone());
        Ok(loaded)
    }

    /// The config file as last loaded or saved (loaded now if it never was).
    pub fn load_settings_cached(&self) -> Result<Loaded, AppError> {
        if let Some(l) = lock(&self.loaded).clone() {
            return Ok(l);
        }
        self.load_settings()
    }

    /// The plan settings and their generation.
    fn plan_inputs(&self) -> Result<(PlanInputs, u64), AppError> {
        let gen = self.settings_gen.load(Ordering::SeqCst);
        let loaded = self.load_settings_cached()?;
        Ok((
            PlanInputs {
                window_source: loaded.window_source(),
                config: loaded.config,
            },
            gen,
        ))
    }

    /// Save Settings: the two core keys (when changed) and the app's scan settings. A saved
    /// plan setting recomputes the shown plan without rescanning.
    pub fn save_settings(
        &self,
        changes: CoreChanges,
        app: Option<AppSettings>,
        confirm_shorten: bool,
    ) -> Result<SaveReport, AppError> {
        let path = self.config_path()?;
        let mut outcome = "unchanged";
        let mut note = None;
        if let Some(a) = app {
            a.validate()?;
        }
        if changes.active_window_days.is_some() || changes.max_git_admin_bytes.is_some() {
            let loaded = self.load_settings_cached()?;
            let saved = settings::save_core_config(
                &loaded,
                &changes,
                confirm_shorten,
                &SaveHooks::default(),
            )?;
            outcome = match &saved {
                SaveOutcome::Saved(_) => "saved",
                SaveOutcome::Unchanged(_) => "unchanged",
                SaveOutcome::SavedUnconfirmed(_, m) => {
                    note = Some(format!("saved; could not confirm it is on disk ({m})"));
                    "saved_unconfirmed"
                }
            };
            *lock(&self.loaded) = Some(saved.loaded().clone());
            if outcome != "unchanged" {
                self.settings_gen.fetch_add(1, Ordering::SeqCst);
            }
        }
        if let Some(a) = app {
            if a != self.app_settings() {
                settings::save_app_settings(&settings::app_settings_path(&path), &a)?;
                *lock(&self.app_settings) = a;
                if outcome == "unchanged" {
                    outcome = "saved";
                }
            }
        }
        let replanned = self.replan()?;
        Ok(SaveReport {
            outcome,
            note,
            replanned,
        })
    }

    /// Recompute the shown plan with the current settings (no rescan). Returns whether it did.
    pub fn replan(&self) -> Result<bool, AppError> {
        for _ in 0..3 {
            let (view, scan, imported, gen_shown) = {
                let s = self.session();
                match &s.analysis {
                    Some(a) => (s.view_id, Arc::clone(&a.scan), a.imported, a.settings_gen),
                    None => return Ok(false),
                }
            };
            let (inputs, gen) = self.plan_inputs()?;
            if gen == gen_shown {
                return Ok(false);
            }
            let built = build_analysis(scan, &inputs, gen, imported, units::now());
            let mut s = self.session();
            if s.view_id != view {
                return Ok(false);
            }
            if self.settings_gen.load(Ordering::SeqCst) != gen {
                continue;
            }
            install_analysis(&mut s, built);
            return Ok(true);
        }
        Ok(false)
    }

    // ---- jobs -------------------------------------------------------------------------

    /// Check roots from the webview: absolute, existing folders, not links; at most 64.
    pub fn check_roots(roots: &[String]) -> Result<Vec<PathBuf>, AppError> {
        if roots.is_empty() {
            return Err(AppError::refused("choose at least one folder or drive"));
        }
        if roots.len() > MAX_ROOTS {
            return Err(AppError::refused(format!("at most {MAX_ROOTS} folders per scan")));
        }
        let mut out = Vec::new();
        for r in roots {
            let p = PathBuf::from(r);
            check_root(&p).map_err(|e| AppError::refused(e.to_string()))?;
            out.push(p);
        }
        Ok(out)
    }

    /// Start a scan of `roots`. The walk and the analysis run on a new thread.
    pub fn start_scan(
        self: &Arc<Self>,
        roots: &[String],
        scan: ScanSettings,
        emit: Emit,
    ) -> Result<JobId, AppError> {
        let roots = Inner::check_roots(roots)?;
        let ticket = self
            .jobs
            .start()
            .map_err(|e| AppError::refused(e.to_string()))?;
        let id = ticket.id;
        let me = Arc::clone(self);
        std::thread::Builder::new()
            .name(format!("scan-{id}"))
            .spawn(move || me.run_job(ticket, roots, scan, emit))
            .map_err(|e| AppError::failed(format!("cannot start the scan: {e}")))?;
        Ok(id)
    }

    /// Cancel `job`. Its id stops being current at once; its worker is ended in the
    /// background. Returns false when `job` was not running.
    pub fn cancel(&self, job: JobId, emit: &Emit) -> bool {
        let cancelled = {
            let _s = self.session();
            self.jobs.cancel(job)
        };
        if cancelled {
            if let Some(h) = lock(&self.workers).get(&job).cloned() {
                std::thread::spawn(move || {
                    h.terminate();
                });
            }
            emit(&ScanEvent {
                job,
                state: JobState::Cancelled,
                progress: None,
                view: None,
                error: None,
            });
        }
        cancelled
    }

    /// End every worker now (the app is closing).
    pub fn shutdown(&self) {
        let (cur, _) = self.jobs.current();
        if let Some(id) = cur {
            let _s = self.session();
            self.jobs.cancel(id);
        }
        let handles: Vec<_> = lock(&self.workers).values().cloned().collect();
        for h in handles {
            h.terminate();
        }
    }

    fn fail(&self, id: JobId, message: String, emit: &Emit) {
        let failed = {
            let _s = self.session();
            self.jobs.advance(id, JobState::Failed)
        };
        if failed {
            emit(&ScanEvent {
                job: id,
                state: JobState::Failed,
                progress: None,
                view: None,
                error: Some(display_name(&message)),
            });
        }
    }

    fn run_job(self: Arc<Self>, ticket: Ticket, roots: Vec<PathBuf>, scan: ScanSettings, emit: Emit) {
        let Ticket { id, cancel, guard } = ticket;
        // The slot is released only when this thread ends: after the walk returned and the
        // worker was swept and reaped.
        let _guard = guard;
        let app = self.app_settings();
        let mut opts = WalkOptions::new(roots.clone());
        opts.one_file_system = app.one_file_system;
        opts.top_files = app.top_files;
        let walked = walk(&opts, &cancel, &mut |p| {
            if self.jobs.is_current(id) {
                emit(&ScanEvent {
                    job: id,
                    state: JobState::Walking,
                    progress: Some(p.clone()),
                    view: None,
                    error: None,
                });
            }
        });
        let tree = match walked {
            Ok(t) => t,
            Err(WalkError::Cancelled) => return,
            Err(e) => return self.fail(id, e.to_string(), &emit),
        };
        let top_files = tree.top_files.clone();
        let stats = tree.stats.clone();
        let mut view = TreeView::new(tree);
        for r in view.tree().roots().to_vec() {
            view.expand(r);
        }
        let found = sessions::find(view.tree(), &sessions::builtin(), &[]);
        let view_id = self.new_view_id();
        let old = {
            let mut s = self.session();
            if !self.jobs.advance(id, JobState::Analysing) {
                return;
            }
            std::mem::replace(
                &mut *s,
                Session {
                    view_id,
                    job: Some(id),
                    tree: Some(view),
                    top_files,
                    stats: Some(stats),
                    walk_finished_at: Some(units::now()),
                    analysis: None,
                    sessions: found,
                    roots: roots
                        .iter()
                        .map(|r| display_name(&r.to_string_lossy()))
                        .collect(),
                },
            )
        };
        drop(old);
        emit(&ScanEvent {
            job: id,
            state: JobState::Analysing,
            progress: None,
            view: Some(view_id),
            error: None,
        });
        let scan_result = match self.run_worker(id, &roots, scan, &app) {
            Ok(Some(s)) => s,
            Ok(None) => return,
            Err(m) => return self.fail(id, m, &emit),
        };
        let scan_result = Arc::new(scan_result);
        for _ in 0..3 {
            let (inputs, gen) = match self.plan_inputs() {
                Ok(x) => x,
                Err(e) => return self.fail(id, format!("cannot read the settings: {e}"), &emit),
            };
            let built = build_analysis(Arc::clone(&scan_result), &inputs, gen, false, units::now());
            let mut s = self.session();
            if !self.jobs.is_current(id) || s.view_id != view_id {
                return;
            }
            if self.settings_gen.load(Ordering::SeqCst) != gen {
                continue;
            }
            install_analysis(&mut s, built);
            self.jobs.advance(id, JobState::Done);
            drop(s);
            emit(&ScanEvent {
                job: id,
                state: JobState::Done,
                progress: None,
                view: Some(view_id),
                error: None,
            });
            return;
        }
        self.fail(id, "the settings kept changing; scan again".into(), &emit);
    }

    /// Run the analysis worker; `Ok(None)` when the job was cancelled.
    fn run_worker(
        &self,
        id: JobId,
        roots: &[PathBuf],
        scan: ScanSettings,
        app: &AppSettings,
    ) -> Result<Option<ScanResult>, String> {
        let exe = match lock(&self.worker_exe).clone() {
            Some(e) => e,
            None => worker_exe().map_err(|e| format!("cannot find the app's executable: {e}"))?,
        };
        let args = WorkerArgs {
            roots: roots.to_vec(),
            one_file_system: app.one_file_system,
            collect_facts: scan.collect_git_facts,
            top_files: app.top_files,
            min_output_bytes: app.min_output_mib << 20,
            git: lock(&self.git_program).clone(),
        };
        let mut proc = spawn_worker(&exe, &args)
            .map_err(|e| format!("cannot start the analysis process: {e}"))?;
        lock(&self.workers).insert(id, Arc::clone(&proc.handle));
        let out = self.drive_worker(id, &mut proc);
        // The worker has been reaped; closing stdin now is harmless.
        drop(proc.stdin.take());
        lock(&self.workers).remove(&id);
        let bytes = match out? {
            Some(b) => b,
            None => return Ok(None),
        };
        // Parsed here, not in the worker: the value-count estimate of spec AC-11 is checked
        // first, and the parse runs on this job thread, off the UI thread.
        let imported = import::parse_bytes(bytes, Limits::default(), "the analysis output")
            .map_err(|e| e.to_string())?;
        Ok(Some(imported.scan))
    }

    /// Start the worker, read its output and reap it. Whatever happens, the worker has been
    /// reaped when this returns, and a worker that was cancelled or whose output was refused
    /// was swept before being reaped.
    fn drive_worker(
        &self,
        id: JobId,
        proc: &mut crate::worker::WorkerProcess,
    ) -> Result<Option<Vec<u8>>, String> {
        let handle = Arc::clone(&proc.handle);
        let end = || {
            handle.terminate();
            handle.wait();
        };
        if !self.jobs.is_current(id) {
            end();
            return Ok(None);
        }
        let Some(mut stdin) = proc.stdin.take() else {
            end();
            return Err("the analysis process has no input".into());
        };
        if let Err(e) = stdin.write_all(&[GO]).and_then(|_| stdin.flush()) {
            end();
            return Err(format!("cannot start the analysis process: {e}"));
        }
        // Keep stdin open while the worker runs; closing it tells the worker to stop.
        proc.stdin = Some(stdin);
        let stderr = proc.stderr.take();
        let err_reader = std::thread::spawn(move || {
            use std::io::Read;
            let mut text = Vec::new();
            if let Some(mut e) = stderr {
                // Keep the first 64 KiB and drain the rest, so the worker never blocks.
                let _ = (&mut e).take(64 * 1024).read_to_end(&mut text);
                let _ = std::io::copy(&mut e, &mut std::io::sink());
            }
            text
        });
        let Some(mut stdout) = proc.stdout.take() else {
            end();
            return Err("the analysis process has no output".into());
        };
        let bytes = import::read_capped(&mut stdout, MAX_IMPORT_BYTES, "the analysis output");
        if !self.jobs.is_current(id) {
            end();
            return Ok(None);
        }
        let bytes = match bytes {
            Ok(b) => b,
            Err(e) => {
                end();
                return Err(e.to_string());
            }
        };
        let status = handle.wait();
        let messages = err_reader.join().unwrap_or_default();
        if !self.jobs.is_current(id) {
            return Ok(None);
        }
        match status {
            Some(s) if s.success() => Ok(Some(bytes)),
            _ => {
                let text = display_name(&String::from_utf8_lossy(&messages));
                let first = text.lines().next().unwrap_or("").trim().to_string();
                Err(if first.is_empty() {
                    "the analysis process stopped unexpectedly".to_string()
                } else {
                    format!("the analysis failed: {first}")
                })
            }
        }
    }

    /// Show a scan file: import protocol of spec AC-11, then plan. Refused while a scan runs.
    pub fn import_file(&self, path: &Path) -> Result<u64, AppError> {
        self.refuse_while_running()?;
        let imported = import::import(path, Limits::default()).map_err(|e| AppError::refused(e.to_string()))?;
        let scan = Arc::new(imported.scan);
        let roots = scan.roots.iter().map(|r| display_name(r)).collect();
        for _ in 0..3 {
            let (inputs, gen) = self.plan_inputs()?;
            let built = build_analysis(Arc::clone(&scan), &inputs, gen, true, units::now());
            let mut s = self.session();
            self.refuse_while_running()?;
            if self.settings_gen.load(Ordering::SeqCst) != gen {
                continue;
            }
            let view_id = self.new_view_id();
            let old = std::mem::replace(
                &mut *s,
                Session {
                    view_id,
                    roots,
                    ..Session::default()
                },
            );
            install_analysis(&mut s, built);
            drop(s);
            drop(old);
            return Ok(view_id);
        }
        Err(AppError::failed("the settings kept changing; open the file again"))
    }

    fn refuse_while_running(&self) -> Result<(), AppError> {
        match self.jobs.current().1 {
            JobState::Walking | JobState::Analysing => {
                Err(AppError::refused("a scan is running; cancel it first"))
            }
            _ => Ok(()),
        }
    }
}

/// What a save did.
#[derive(Debug, Clone, Serialize)]
pub struct SaveReport {
    pub outcome: &'static str,
    pub note: Option<String>,
    pub replanned: bool,
}

/// Plan a scan and build its developer view (outside any lock).
fn build_analysis(
    scan: Arc<ScanResult>,
    inputs: &PlanInputs,
    gen: u64,
    imported: bool,
    now: i64,
) -> Analysis {
    let plan = make_plan(&scan, inputs, now, running_cwds());
    let dev = DevView::new(&scan, &plan, imported, now);
    Analysis {
        scan,
        plan: Arc::new(plan),
        dev,
        imported,
        settings_gen: gen,
    }
}

/// Store an analysis in the session: class tags on the tree and session folders' enclosing
/// plan items.
fn install_analysis(s: &mut Session, a: Analysis) {
    if let Some(t) = s.tree.as_mut() {
        let mut tags: HashMap<NodeId, String> = HashMap::new();
        for it in &a.plan.items {
            if let Some(id) = t.tree().find_path(Path::new(&it.path)) {
                tags.insert(id, it.class.as_str().to_string());
            }
        }
        s.sessions = sessions::find(t.tree(), &sessions::builtin(), &a.plan.items);
        for f in &s.sessions {
            tags.entry(f.id).or_insert_with(|| "agent-session".to_string());
        }
        t.set_tags(tags);
    }
    s.analysis = Some(a);
}
