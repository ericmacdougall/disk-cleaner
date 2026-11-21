//! `disk-cleaner tui`: a read-only terminal view of a scan. It shows a size tree with
//! drill-down and a developer view of the plan. It deletes, moves and backs up nothing; the
//! only file it writes is a plan export (`e`), always to a new file.

#![forbid(unsafe_code)]

pub mod app;
pub mod ui;
pub mod worker;

use anyhow::Context;
use app::{App, Outcome};
use clap::Args;
use crossterm::event::{self, Event, KeyEventKind};
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use crossterm::ExecutableCommand;
use ratatui::backend::CrosstermBackend;
use ratatui::Terminal;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

/// Arguments of `disk-cleaner tui`.
#[derive(Args, Debug, Clone)]
pub struct TuiArgs {
    /// Folders to scan and browse.
    pub paths: Vec<PathBuf>,
    /// Browse a scan file written by `disk-cleaner scan --out` instead of scanning.
    #[arg(long, value_name = "FILE", conflicts_with = "paths")]
    pub scan: Option<PathBuf>,
    /// Do not cross mount points (Unix), in the size walk and the analysis.
    #[arg(long)]
    pub one_file_system: bool,
    /// Skip git; every repository fact is then unknown, so repositories ask.
    #[arg(long)]
    pub no_git: bool,
}

/// Restores the terminal however `run` ends.
struct TerminalGuard;

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let _ = std::io::stdout().execute(LeaveAlternateScreen);
    }
}

/// Run the terminal view.
pub fn run(args: TuiArgs) -> anyhow::Result<()> {
    if args.paths.is_empty() && args.scan.is_none() {
        anyhow::bail!("give one or more folders to scan, or --scan FILE");
    }
    let mut app = App::new(disk_cleaner_view::units::now());
    let cancel = Arc::new(AtomicBool::new(false));
    let rx = match &args.scan {
        Some(file) => {
            // Imports are parsed here, before the screen changes, so errors print plainly.
            app.load_import(file).context("cannot open the scan file")?;
            None
        }
        None => {
            let mut roots = Vec::new();
            for p in &args.paths {
                roots.push(
                    std::path::absolute(p)
                        .with_context(|| format!("cannot resolve {}", p.display()))?,
                );
            }
            for r in &roots {
                disk_cleaner_view::walk::check_root(r).map_err(|e| anyhow::anyhow!("{e}"))?;
            }
            app.start_live(&roots);
            Some(worker::spawn(
                roots,
                args.one_file_system,
                !args.no_git,
                Arc::clone(&cancel),
            ))
        }
    };

    enable_raw_mode().context("cannot switch the terminal to raw mode")?;
    let _guard = TerminalGuard;
    std::io::stdout().execute(EnterAlternateScreen)?;
    let mut terminal = Terminal::new(CrosstermBackend::new(std::io::stdout()))?;
    terminal.clear()?;
    loop {
        if let Some(rx) = &rx {
            while let Ok(msg) = rx.try_recv() {
                app.on_message(msg);
            }
        }
        terminal.draw(|f| ui::draw(f, &mut app))?;
        // A resize needs nothing more than the next draw.
        if event::poll(Duration::from_millis(100))? {
            if let Event::Key(k) = event::read()? {
                if k.kind == KeyEventKind::Press && app.on_key(k) == Outcome::Quit {
                    break;
                }
            }
        }
    }
    // Stop the size walk; the analysis thread ends with the process. A documented limit
    // (docs/DESKTOP.md, "Terminal view"): git commands the analysis already started are not
    // stopped. They are read-only, and normally end within a second, but core's 30 s git
    // timeout is enforced by this process, so once it has exited nothing ends a git that hangs
    // (on a stuck network file system, say). The desktop app runs its analysis in a contained
    // worker instead (apps/desktop/src-tauri/src/worker.rs).
    cancel.store(true, Ordering::Relaxed);
    Ok(())
}
