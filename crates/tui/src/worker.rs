//! The background thread of a live `tui` session: the size walk first, then core's scan and
//! plan. Messages go to the screen loop over a channel.

use disk_cleaner_core::{
    running_cwds, scan, FactsConfig, Git, Plan, ProcessCheck, ScanOptions, ScanResult,
};
use disk_cleaner_view::dev::{make_plan, PlanInputs};
use disk_cleaner_view::settings::{default_core_path, load_core};
use disk_cleaner_view::tree::Tree;
use disk_cleaner_view::walk::{walk, Progress, WalkOptions};
use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::sync::mpsc::{channel, Receiver};
use std::sync::Arc;
use std::time::Duration;

/// A scan and the plan made from it.
#[derive(Debug)]
pub struct Analysis {
    pub scan: ScanResult,
    pub plan: Plan,
}

/// What the background thread reports.
#[derive(Debug)]
pub enum Msg {
    Progress(Progress),
    Tree(Box<Tree>),
    WalkFailed(String),
    /// The size walk is done and the analysis (git included) has started.
    Analysing,
    Analysis(Box<Analysis>),
    AnalysisFailed(String),
}

/// The plan settings from core's config file (the one the CLI reads without `--config`). A
/// file that exists but cannot be read is an error, as in the CLI.
pub fn plan_inputs() -> Result<PlanInputs, String> {
    let path = default_core_path().map_err(|e| e.to_string())?;
    let loaded = load_core(&path).map_err(|e| e.to_string())?;
    Ok(PlanInputs {
        window_source: loaded.window_source(),
        config: loaded.config,
    })
}

/// Plan a scan with the config file's settings and this machine's running processes.
pub fn plan_scan(scan: &ScanResult, now: i64) -> Result<Plan, String> {
    let inputs = plan_inputs()?;
    Ok(make_plan(scan, &inputs, now, running_cwds()))
}

/// Plan without looking at processes (tests).
pub fn plan_scan_with(scan: &ScanResult, inputs: &PlanInputs, now: i64) -> Plan {
    make_plan(scan, inputs, now, ProcessCheck::none_running())
}

/// Start the walk and the analysis for `roots` on a background thread.
pub fn spawn(
    roots: Vec<PathBuf>,
    one_file_system: bool,
    git: bool,
    cancel: Arc<AtomicBool>,
) -> Receiver<Msg> {
    let (tx, rx) = channel();
    std::thread::spawn(move || {
        let mut opts = WalkOptions::new(roots.clone());
        opts.one_file_system = one_file_system;
        let progress_tx = tx.clone();
        let result = walk(&opts, &cancel, &mut |p| {
            let _ = progress_tx.send(Msg::Progress(p.clone()));
        });
        match result {
            Ok(tree) => {
                let _ = tx.send(Msg::Tree(Box::new(tree)));
            }
            Err(e) => {
                let _ = tx.send(Msg::WalkFailed(e.to_string()));
                if cancel.load(std::sync::atomic::Ordering::Relaxed) {
                    return;
                }
            }
        }
        let _ = tx.send(Msg::Analysing);
        let mut sopts = ScanOptions::new(roots);
        sopts.one_file_system = one_file_system;
        sopts.collect_facts = git;
        sopts.facts = FactsConfig::default().with_git(Git::new("git", Duration::from_secs(30)));
        let msg = match scan(&sopts) {
            Ok(s) => match plan_scan(&s, disk_cleaner_view::units::now()) {
                Ok(plan) => Msg::Analysis(Box::new(Analysis { scan: s, plan })),
                Err(e) => Msg::AnalysisFailed(format!("cannot read the settings: {e}")),
            },
            Err(e) => Msg::AnalysisFailed(disk_cleaner_view::sanitize::display_name(&format!(
                "the analysis failed: {e}"
            ))),
        };
        let _ = tx.send(msg);
    });
    rx
}
