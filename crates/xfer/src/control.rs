//! Goodput controller: the number of concurrent pack uploads K and the target pack size P.
//!
//! K starts at the memory-derived cap: object stores reward parallel requests, and the cap
//! already bounds what K can cost. Each epoch (5 s) the controller measures verified upload
//! goodput. It halves K on any throttling signal (multiplicative decrease), then hill-climbs back
//! one step at a time while a step gains at least 5% and reverses a step that does not. An epoch
//! counts only when uploads were the bottleneck (the packer waited for a slot), so a slow reader
//! never moves K. P follows per-stream goodput: about 4 s of upload
//! per request, clamped to [16 MiB, max_pack], so per-request latency stays a small fraction.

use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

pub const MIN_PACK: u64 = 16 << 20;

pub struct Shared {
    pub k: AtomicUsize,
    pub max_k: usize,
    pub fixed: bool,
    pub target_pack: AtomicU64,
    pub max_pack: u64,
    pub uploaded: AtomicU64,
    pub throttles: AtomicU64,
    /// Link goodput in bytes/s (EWMA), read by the compression level chooser.
    pub goodput: Arc<AtomicU64>,
    /// Nanoseconds the packer spent blocked handing a sealed pack to the uploaders: the signal
    /// that uploads, not reading or compression, are the bottleneck.
    pub blocked_ns: AtomicU64,
    pub stop: AtomicBool,
    in_flight: Mutex<usize>,
    cv: Condvar,
    pub k_trace: Mutex<Vec<(u64, usize, u64)>>,
}

impl Shared {
    pub fn new(start_k: usize, max_k: usize, fixed: bool, max_pack: u64) -> Arc<Shared> {
        let max_k = max_k.max(1);
        let start = start_k.clamp(1, max_k);
        let p0 = (32u64 << 20).min(max_pack).max(MIN_PACK.min(max_pack));
        Arc::new(Shared {
            k: AtomicUsize::new(start),
            max_k,
            fixed,
            target_pack: AtomicU64::new(p0),
            max_pack,
            uploaded: AtomicU64::new(0),
            throttles: AtomicU64::new(0),
            goodput: Arc::new(AtomicU64::new(0)),
            blocked_ns: AtomicU64::new(0),
            stop: AtomicBool::new(false),
            in_flight: Mutex::new(0),
            cv: Condvar::new(),
            k_trace: Mutex::new(Vec::new()),
        })
    }

    /// Block until fewer than K uploads are in flight, then take a slot.
    pub fn acquire(&self) {
        let mut n = self.in_flight.lock().expect("gate lock");
        while *n >= self.k.load(Ordering::Relaxed) {
            n = self
                .cv
                .wait_timeout(n, Duration::from_millis(200))
                .expect("gate lock")
                .0;
        }
        *n += 1;
    }

    pub fn release(&self) {
        let mut n = self.in_flight.lock().expect("gate lock");
        *n = n.saturating_sub(1);
        self.cv.notify_all();
    }

    pub fn throttled(&self) {
        self.throttles.fetch_add(1, Ordering::Relaxed);
    }
}

/// Pure decision state, separated so it can be unit tested without threads or clocks.
pub struct Climb {
    pub k: usize,
    pub max_k: usize,
    last_g: f64,
    dir: i32,
    hold: u32,
}

impl Climb {
    pub fn new(k: usize, max_k: usize) -> Self {
        Climb {
            k,
            max_k,
            last_g: 0.0,
            dir: 0,
            hold: 0,
        }
    }

    /// One epoch. `busy` is false when uploads were starved (nothing queued), in which case the
    /// measurement says nothing about K.
    pub fn step(&mut self, goodput: f64, throttled: bool, busy: bool) -> usize {
        if throttled {
            self.k = (self.k / 2).max(1);
            self.dir = 0;
            self.hold = 2;
        } else if !busy {
            // producer-limited: hold K
        } else if self.hold > 0 {
            self.hold -= 1;
        } else if self.dir == 0 {
            if self.k < self.max_k {
                self.k += 1;
                self.dir = 1;
            }
        } else if goodput >= self.last_g * 1.05 {
            let next = self.k as i64 + self.dir as i64;
            if next >= 1 && next as usize <= self.max_k {
                self.k = next as usize;
            } else {
                self.dir = 0;
                self.hold = 3;
            }
        } else {
            let back = self.k as i64 - self.dir as i64;
            self.k = back.clamp(1, self.max_k as i64) as usize;
            self.dir = 0;
            self.hold = 3;
        }
        self.last_g = goodput;
        self.k
    }
}

/// Run the controller loop until `shared.stop` is set.
pub fn run(shared: Arc<Shared>, epoch: Duration) {
    let t0 = Instant::now();
    let mut climb = Climb::new(shared.k.load(Ordering::Relaxed), shared.max_k);
    let mut last_bytes = 0u64;
    let mut last_throttles = 0u64;
    let mut last_blocked = 0u64;
    let mut last_t = Instant::now();
    let mut ewma = 0f64;
    while !shared.stop.load(Ordering::Relaxed) {
        // sleep in short steps so stop is prompt
        let until = Instant::now() + epoch;
        while Instant::now() < until && !shared.stop.load(Ordering::Relaxed) {
            std::thread::sleep(Duration::from_millis(100));
        }
        let now = Instant::now();
        let dt = now.duration_since(last_t).as_secs_f64().max(1e-3);
        let bytes = shared.uploaded.load(Ordering::Relaxed);
        let g = (bytes - last_bytes) as f64 / dt;
        last_bytes = bytes;
        last_t = now;
        let th = shared.throttles.load(Ordering::Relaxed);
        let throttled = th > last_throttles;
        last_throttles = th;
        if g > 0.0 {
            ewma = if ewma == 0.0 { g } else { 0.5 * g + 0.5 * ewma };
            shared.goodput.store(ewma as u64, Ordering::Relaxed);
        }
        // upload-bound when the packer waited for an upload slot for a fifth of the epoch or more
        let blocked = shared.blocked_ns.load(Ordering::Relaxed);
        let busy = (blocked - last_blocked) as f64 >= 0.2 * dt * 1e9;
        last_blocked = blocked;
        let k = if shared.fixed {
            shared.k.load(Ordering::Relaxed)
        } else {
            climb.step(g, throttled, busy)
        };
        shared.k.store(k, Ordering::Relaxed);
        if ewma > 0.0 {
            let per = ewma / k as f64;
            let p = ((per * 4.0) as u64).clamp(MIN_PACK.min(shared.max_pack), shared.max_pack);
            shared.target_pack.store(p, Ordering::Relaxed);
        }
        shared.k_trace.lock().expect("trace lock").push((
            t0.elapsed().as_millis() as u64,
            k,
            ewma as u64,
        ));
        shared.cv.notify_all();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn climbs_while_it_pays_and_backs_off_when_it_does_not() {
        let mut c = Climb::new(2, 8);
        // goodput scales with k up to 4 streams then flattens
        let g = |k: usize| (k.min(4) as f64) * 10.0;
        let mut k = 2;
        for _ in 0..20 {
            k = c.step(g(k), false, true);
        }
        assert!((4..=5).contains(&k), "settled at {k}");
    }

    #[test]
    fn throttling_halves() {
        let mut c = Climb::new(8, 16);
        assert_eq!(c.step(100.0, true, true), 4);
        assert_eq!(c.step(100.0, true, true), 2);
        assert_eq!(c.step(100.0, true, true), 1);
        assert_eq!(c.step(100.0, true, true), 1);
    }

    #[test]
    fn starved_epochs_hold_k() {
        let mut c = Climb::new(3, 8);
        for _ in 0..5 {
            assert_eq!(c.step(0.0, false, false), 3);
        }
    }
}
