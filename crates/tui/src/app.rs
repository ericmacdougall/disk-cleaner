//! Screen state and key handling, kept apart from drawing so tests can drive it.

use crate::worker::{Analysis, Msg};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use disk_cleaner_core::{Plan, ScanResult};
use disk_cleaner_view::dev::{DevView, GROUPS};
use disk_cleaner_view::export::export_to_dir;
use disk_cleaner_view::import::{import, Limits};
use disk_cleaner_view::sanitize::display_name;
use disk_cleaner_view::sessions::{self, SessionFolder};
use disk_cleaner_view::table::{Sort, SortKey, TreeView};
use disk_cleaner_view::tree::{EntryKind, Tree};
use disk_cleaner_view::walk::Progress;
use std::path::{Path, PathBuf};

/// Which view is shown.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tab {
    Tree,
    Dev,
}

/// What a key press asks of the loop.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    Continue,
    Quit,
}

/// The size walk's state.
#[derive(Debug, Clone)]
pub enum WalkState {
    Walking(Progress),
    Done,
    Failed(String),
    /// An imported scan has no size tree.
    NotRun,
}

/// The analysis's state.
#[derive(Debug, Clone)]
pub enum AnalysisState {
    Waiting,
    Running,
    Done,
    Failed(String),
}

/// One line of the developer view.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DevRow {
    Header(usize),
    Item(usize, usize),
    SessionsHeader,
    Session(usize),
}

/// The order `s` cycles through.
const SORT_KEYS: [SortKey; 5] = [
    SortKey::Allocated,
    SortKey::Size,
    SortKey::Files,
    SortKey::Modified,
    SortKey::Name,
];

/// The whole screen state.
pub struct App {
    pub now: i64,
    pub tab: Tab,
    pub tree: Option<TreeView>,
    pub sel: usize,
    pub top: usize,
    pub dev: Option<DevView>,
    pub plan: Option<Plan>,
    pub dev_rows: Vec<DevRow>,
    pub dev_sel: usize,
    pub dev_top: usize,
    pub sessions: Vec<SessionFolder>,
    pub help: bool,
    /// The overlay with every notice of the developer view (`i`), and its first line shown.
    pub details: bool,
    pub details_top: usize,
    /// Lines the overlay could show in the last frame (set by drawing; paging uses it).
    pub details_page: usize,
    pub status: String,
    pub walk: WalkState,
    pub analysis: AnalysisState,
    pub imported: Option<String>,
    /// Rows the last frame could show (set by drawing; paging uses it).
    pub page: usize,
    /// Where `e` writes the plan.
    pub export_dir: PathBuf,
}

impl App {
    pub fn new(now: i64) -> App {
        App {
            now,
            tab: Tab::Tree,
            tree: None,
            sel: 0,
            top: 0,
            dev: None,
            plan: None,
            dev_rows: Vec::new(),
            dev_sel: 0,
            dev_top: 0,
            sessions: Vec::new(),
            help: false,
            details: false,
            details_top: 0,
            details_page: 20,
            status: String::new(),
            walk: WalkState::NotRun,
            analysis: AnalysisState::Waiting,
            imported: None,
            page: 20,
            export_dir: PathBuf::from("."),
        }
    }

    /// A live session is starting for these roots.
    pub fn start_live(&mut self, _roots: &[PathBuf]) {
        self.walk = WalkState::Walking(Progress::default());
        self.analysis = AnalysisState::Waiting;
    }

    /// Read a scan file (the import protocol) and plan it.
    pub fn load_import(&mut self, file: &Path) -> anyhow::Result<()> {
        let got = import(file, Limits::default()).map_err(|e| anyhow::anyhow!("{e}"))?;
        let plan = crate::worker::plan_scan(&got.scan, self.now)
            .map_err(|e| anyhow::anyhow!("cannot read the settings: {e}"))?;
        self.imported = Some(display_name(&file.to_string_lossy()));
        self.walk = WalkState::NotRun;
        self.set_analysis(&got.scan, plan, true);
        self.tab = Tab::Dev;
        Ok(())
    }

    /// Show a size tree.
    pub fn set_tree(&mut self, tree: Tree) {
        let mut view = TreeView::new(tree);
        // Open the roots so the first screen shows their contents.
        let roots: Vec<_> = view.tree().roots().to_vec();
        for r in roots {
            view.expand(r);
        }
        self.tree = Some(view);
        self.sel = 0;
        self.top = 0;
        self.walk = WalkState::Done;
        self.refresh_sessions();
    }

    /// Show an analysis.
    pub fn set_analysis(&mut self, scan: &ScanResult, plan: Plan, imported: bool) {
        self.dev = Some(DevView::new(scan, &plan, imported, self.now));
        self.plan = Some(plan);
        self.analysis = AnalysisState::Done;
        self.refresh_sessions();
    }

    fn refresh_sessions(&mut self) {
        if let Some(t) = &self.tree {
            let items = self
                .plan
                .as_ref()
                .map(|p| p.items.as_slice())
                .unwrap_or(&[]);
            self.sessions = sessions::find(t.tree(), &sessions::builtin(), items);
        }
        self.rebuild_dev_rows();
    }

    fn rebuild_dev_rows(&mut self) {
        self.dev_rows.clear();
        if let Some(dev) = &self.dev {
            for g in 0..GROUPS.len() {
                let items = dev.items(g);
                if items.is_empty() {
                    continue;
                }
                self.dev_rows.push(DevRow::Header(g));
                self.dev_rows
                    .extend((0..items.len()).map(|i| DevRow::Item(g, i)));
            }
        }
        if !self.sessions.is_empty() {
            self.dev_rows.push(DevRow::SessionsHeader);
            self.dev_rows
                .extend((0..self.sessions.len()).map(DevRow::Session));
        }
        self.dev_sel = self.dev_sel.min(self.dev_rows.len().saturating_sub(1));
    }

    /// A message from the background thread.
    pub fn on_message(&mut self, msg: Msg) {
        match msg {
            Msg::Progress(p) => {
                if matches!(self.walk, WalkState::Walking(_)) {
                    self.walk = WalkState::Walking(p);
                }
            }
            Msg::Tree(t) => self.set_tree(*t),
            Msg::WalkFailed(e) => self.walk = WalkState::Failed(e),
            Msg::Analysing => self.analysis = AnalysisState::Running,
            Msg::Analysis(a) => {
                let Analysis { scan, plan } = *a;
                // The clock was read when the screen opened, and a live scan finishes later:
                // without this its time would read as in the future, shown as unknown.
                self.now = self.now.max(scan.generated_at).max(plan.generated_at);
                self.set_analysis(&scan, plan, false);
            }
            Msg::AnalysisFailed(e) => self.analysis = AnalysisState::Failed(e),
        }
    }

    fn rows(&self) -> usize {
        match self.tab {
            Tab::Tree => self.tree.as_ref().map(|t| t.row_count()).unwrap_or(0),
            Tab::Dev => self.dev_rows.len(),
        }
    }

    fn move_to(&mut self, target: usize) {
        let n = self.rows();
        let target = if n == 0 { 0 } else { target.min(n - 1) };
        match self.tab {
            Tab::Tree => self.sel = target,
            Tab::Dev => self.dev_sel = target,
        }
    }

    fn selected(&self) -> usize {
        match self.tab {
            Tab::Tree => self.sel,
            Tab::Dev => self.dev_sel,
        }
    }

    /// Keep the selection on screen.
    pub fn scroll_into_view(&mut self) {
        let page = self.page.max(1);
        let (sel, top) = match self.tab {
            Tab::Tree => (self.sel, &mut self.top),
            Tab::Dev => (self.dev_sel, &mut self.dev_top),
        };
        if sel < *top {
            *top = sel;
        } else if sel >= *top + page {
            *top = sel + 1 - page;
        }
    }

    fn selected_tree_id(&self) -> Option<u32> {
        let t = self.tree.as_ref()?;
        t.page(self.sel, 1).first().map(|r| r.id)
    }

    fn drill_in(&mut self) {
        let Some(id) = self.selected_tree_id() else {
            return;
        };
        if let Some(t) = &mut self.tree {
            if t.tree().kind(id) == EntryKind::Dir && t.set_focus(Some(id)) {
                self.sel = 0;
                self.top = 0;
            }
        }
    }

    fn drill_out(&mut self) {
        if let Some(t) = &mut self.tree {
            if let Some(old) = t.focus_parent() {
                self.sel = t.row_of(old).unwrap_or(0);
            }
        }
    }

    fn toggle(&mut self) {
        let Some(id) = self.selected_tree_id() else {
            return;
        };
        if let Some(t) = &mut self.tree {
            t.toggle(id);
        }
    }

    fn resort(&mut self, next_key: bool) {
        let keep = self.selected_tree_id();
        if let Some(t) = &mut self.tree {
            let cur = t.sort();
            let sort = if next_key {
                let i = SORT_KEYS.iter().position(|k| *k == cur.key).unwrap_or(0);
                let key = SORT_KEYS[(i + 1) % SORT_KEYS.len()];
                Sort {
                    key,
                    descending: key != SortKey::Name,
                }
            } else {
                Sort {
                    key: cur.key,
                    descending: !cur.descending,
                }
            };
            t.set_sort(sort);
            if let Some(id) = keep {
                self.sel = t.row_of(id).unwrap_or(0);
            }
        }
    }

    /// Export the plan JSON to a new file in the export folder.
    pub fn export(&mut self) {
        let Some(plan) = &self.plan else {
            self.status = "The plan is not ready yet; nothing was written.".to_string();
            return;
        };
        self.status = match export_to_dir(&self.export_dir, plan, self.now) {
            Ok(p) => format!(
                "Plan written to {} (a new file; nothing else was changed).",
                display_name(&p.to_string_lossy())
            ),
            Err(e) => format!("Export failed: {e}"),
        };
    }

    /// Handle a key press.
    pub fn on_key(&mut self, k: KeyEvent) -> Outcome {
        if k.modifiers.contains(KeyModifiers::CONTROL) && k.code == KeyCode::Char('c') {
            return Outcome::Quit;
        }
        if self.help {
            if matches!(
                k.code,
                KeyCode::Esc | KeyCode::Char('?') | KeyCode::Char('q')
            ) {
                self.help = false;
            }
            return Outcome::Continue;
        }
        if self.details {
            self.details_key(k.code);
            return Outcome::Continue;
        }
        let page = self.page.max(1);
        match k.code {
            KeyCode::Char('q') => return Outcome::Quit,
            KeyCode::Char('?') => self.help = true,
            KeyCode::Tab | KeyCode::BackTab => {
                self.tab = match self.tab {
                    Tab::Tree => Tab::Dev,
                    Tab::Dev => Tab::Tree,
                }
            }
            KeyCode::Char('1') => self.tab = Tab::Tree,
            KeyCode::Char('2') => self.tab = Tab::Dev,
            KeyCode::Up | KeyCode::Char('k') => {
                let s = self.selected();
                self.move_to(s.saturating_sub(1));
            }
            KeyCode::Down | KeyCode::Char('j') => {
                let s = self.selected();
                self.move_to(s + 1);
            }
            KeyCode::PageUp => {
                let s = self.selected();
                self.move_to(s.saturating_sub(page));
            }
            KeyCode::PageDown => {
                let s = self.selected();
                self.move_to(s + page);
            }
            KeyCode::Home | KeyCode::Char('g') => self.move_to(0),
            KeyCode::End | KeyCode::Char('G') => self.move_to(usize::MAX),
            KeyCode::Enter | KeyCode::Right | KeyCode::Char('l') if self.tab == Tab::Tree => {
                self.drill_in()
            }
            KeyCode::Left | KeyCode::Backspace | KeyCode::Char('h') if self.tab == Tab::Tree => {
                self.drill_out()
            }
            KeyCode::Char(' ') if self.tab == Tab::Tree => self.toggle(),
            KeyCode::Char('s') if self.tab == Tab::Tree => self.resort(true),
            KeyCode::Char('r') if self.tab == Tab::Tree => self.resort(false),
            KeyCode::Char('e') => self.export(),
            KeyCode::Char('i') => {
                self.details = true;
                self.details_top = 0;
            }
            _ => {}
        }
        self.scroll_into_view();
        Outcome::Continue
    }

    /// Keys while the notices overlay is open: scroll it, or close it (Esc, `i`, `q`). Drawing
    /// keeps the first line in range.
    fn details_key(&mut self, code: KeyCode) {
        let page = self.details_page.max(1);
        let top = self.details_top;
        self.details_top = match code {
            KeyCode::Esc | KeyCode::Char('i') | KeyCode::Char('q') => {
                self.details = false;
                0
            }
            KeyCode::Up | KeyCode::Char('k') => top.saturating_sub(1),
            KeyCode::Down | KeyCode::Char('j') => top.saturating_add(1),
            KeyCode::PageUp => top.saturating_sub(page),
            KeyCode::PageDown => top.saturating_add(page),
            KeyCode::Home | KeyCode::Char('g') => 0,
            KeyCode::End | KeyCode::Char('G') => usize::MAX,
            _ => top,
        };
    }
}
