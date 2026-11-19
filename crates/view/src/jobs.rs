//! Scan job bookkeeping: one active job and at most one retiring job at a time.
//!
//! A job is *active* from Start until it is cancelled or its result is delivered. Cancel retires
//! it at once (its id is no longer current, so anything it reports later is dropped) but its
//! threads may still be finishing; they hold a [`SlotGuard`], and the slot is free again only
//! when that guard is dropped. Start is refused while a job is active and running, and while two
//! jobs physically exist, so repeated Cancel and Start can never pile up walks.

use serde::Serialize;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

/// A job id. Ids start at 1 and are never reused within a process.
pub type JobId = u64;

/// Most jobs that may exist at once (one active, one retiring).
pub const MAX_LIVE: usize = 2;

/// What a job is doing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum JobState {
    Idle,
    Walking,
    Analysing,
    Done,
    Failed,
    Cancelled,
}

impl JobState {
    fn running(self) -> bool {
        matches!(self, JobState::Walking | JobState::Analysing)
    }
}

/// Why Start was refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StartRefused {
    /// The current job is still walking or analysing.
    Busy,
    /// A cancelled job has not released its threads yet.
    StillStopping,
}

impl std::fmt::Display for StartRefused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            StartRefused::Busy => write!(f, "a scan is already running"),
            StartRefused::StillStopping => write!(f, "the previous scan is still stopping"),
        }
    }
}

impl std::error::Error for StartRefused {}

#[derive(Debug)]
struct State {
    next: JobId,
    current: Option<(JobId, JobState)>,
    /// The current job's cancel flag.
    flag: Option<Arc<AtomicBool>>,
    live: usize,
}

/// The job table. Clone it freely: clones share the state.
#[derive(Debug, Clone)]
pub struct Jobs {
    state: Arc<Mutex<State>>,
}

impl Default for Jobs {
    fn default() -> Self {
        Jobs::new()
    }
}

/// Held by the threads of one job; dropping it frees the job's slot.
#[derive(Debug)]
pub struct SlotGuard {
    state: Arc<Mutex<State>>,
}

impl Drop for SlotGuard {
    fn drop(&mut self) {
        let mut s = lock(&self.state);
        s.live = s.live.saturating_sub(1);
    }
}

/// What Start hands to the job's threads.
#[derive(Debug)]
pub struct Ticket {
    pub id: JobId,
    /// Set by Cancel; the walk checks it.
    pub cancel: Arc<AtomicBool>,
    pub guard: SlotGuard,
}

fn lock(m: &Mutex<State>) -> MutexGuard<'_, State> {
    // A panic while holding the lock leaves plain counters behind; keep using them.
    m.lock().unwrap_or_else(|p| p.into_inner())
}

impl Jobs {
    pub fn new() -> Jobs {
        Jobs {
            state: Arc::new(Mutex::new(State {
                next: 1,
                current: None,
                flag: None,
                live: 0,
            })),
        }
    }

    /// Start a job, or say why not.
    pub fn start(&self) -> Result<Ticket, StartRefused> {
        let mut s = lock(&self.state);
        if let Some((_, st)) = s.current {
            if st.running() {
                return Err(StartRefused::Busy);
            }
        }
        if s.live >= MAX_LIVE {
            return Err(StartRefused::StillStopping);
        }
        let id = s.next;
        s.next += 1;
        s.current = Some((id, JobState::Walking));
        s.live += 1;
        let cancel = Arc::new(AtomicBool::new(false));
        s.flag = Some(Arc::clone(&cancel));
        Ok(Ticket {
            id,
            cancel,
            guard: SlotGuard {
                state: Arc::clone(&self.state),
            },
        })
    }

    /// Cancel `id` if it is the current running job: its id stops being current at once.
    /// Returns false when `id` is not the current running job (already done, or superseded).
    pub fn cancel(&self, id: JobId) -> bool {
        let mut s = lock(&self.state);
        match s.current {
            Some((cur, st)) if cur == id && st.running() => {
                if let Some(f) = s.flag.take() {
                    f.store(true, Ordering::Relaxed);
                }
                s.current = Some((id, JobState::Cancelled));
                true
            }
            _ => false,
        }
    }

    /// Move the current job to another state. Ignored (returns false) when `id` is not the
    /// current job or the job was cancelled: a late report from an old job changes nothing.
    pub fn advance(&self, id: JobId, to: JobState) -> bool {
        let mut s = lock(&self.state);
        match s.current {
            Some((cur, st)) if cur == id && st.running() => {
                s.current = Some((id, to));
                true
            }
            _ => false,
        }
    }

    /// Is `id` the current job, still running? Results are accepted only then.
    pub fn is_current(&self, id: JobId) -> bool {
        matches!(lock(&self.state).current, Some((cur, st)) if cur == id && st.running())
    }

    /// The current job and its state.
    pub fn current(&self) -> (Option<JobId>, JobState) {
        match lock(&self.state).current {
            Some((id, st)) => (Some(id), st),
            None => (None, JobState::Idle),
        }
    }

    /// Jobs whose threads still exist (active plus retiring).
    pub fn live(&self) -> usize {
        lock(&self.state).live
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_second_start_is_refused_while_one_runs() {
        let jobs = Jobs::new();
        let t1 = jobs.start().unwrap();
        assert_eq!(jobs.start().unwrap_err(), StartRefused::Busy);
        assert!(jobs.advance(t1.id, JobState::Done));
        // Done: a new scan may start (the old result is replaced).
        let t2 = jobs.start().unwrap();
        assert_ne!(t1.id, t2.id);
    }

    #[test]
    fn cancel_retires_the_id_and_late_reports_are_dropped() {
        let jobs = Jobs::new();
        let t1 = jobs.start().unwrap();
        assert!(jobs.cancel(t1.id));
        assert!(t1.cancel.load(Ordering::Relaxed));
        assert!(!jobs.is_current(t1.id));
        assert!(!jobs.advance(t1.id, JobState::Done), "a cancelled job cannot finish");
        assert!(!jobs.cancel(t1.id), "cancelling twice is a no-op");
        assert_eq!(jobs.current(), (Some(t1.id), JobState::Cancelled));
    }

    #[test]
    fn at_most_two_jobs_exist_while_cancelled_ones_are_stopping() {
        let jobs = Jobs::new();
        // Neither guard is dropped: both walks are "blocked".
        let t1 = jobs.start().unwrap();
        assert!(jobs.cancel(t1.id));
        let t2 = jobs.start().unwrap();
        assert!(jobs.cancel(t2.id));
        assert_eq!(jobs.start().unwrap_err(), StartRefused::StillStopping);
        assert_eq!(jobs.live(), 2);
        drop(t1.guard);
        assert_eq!(jobs.live(), 1);
        let t3 = jobs.start().unwrap();
        assert!(jobs.is_current(t3.id));
        assert!(!jobs.is_current(t2.id));
        drop(t2);
        drop(t3);
        assert_eq!(jobs.live(), 0);
    }
}
