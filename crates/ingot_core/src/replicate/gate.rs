//! The one gate every read of a source card goes through, in strict priority order.
//!
//! A card is a slow serial device, and the photographer must not notice the copy. So:
//! 1. On-demand reads: the photo being looked at, full size.
//! 2. Culling reads: thumbnails and previews, in display order.
//! 3. Copy reads: only when no culling read is waiting, running or about to run.
//!
//! The copy reads in small chunks and comes back here between them, so a waiting
//! culling read sits behind one chunk at most, not a whole 30 MB file.

use std::{
    sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError},
    time::{Duration, Instant},
};

/// How long after a culling read the copy keeps waiting. The next culling read follows
/// in microseconds, and must not lose the card to a copy chunk in that gap. If nothing
/// comes, the culling reader is really idle and the copy may run.
const CULLING_GRACE: Duration = Duration::from_millis(10);

#[derive(Clone, Copy, PartialEq, Eq)]
enum Level {
    OnDemand,
    Culling,
    Copy,
}

struct GateState {
    available: usize,
    on_demand_waiting: usize,
    culling_waiting: usize,
    /// Culling and on-demand reads running now.
    culling_running: usize,
    /// At most one: two copy chunks in line would make a culling read wait for both.
    copy_running: bool,
    last_culling: Option<Instant>,
}

pub struct CardGate {
    /// For measuring: bytes per second of a simulated card, a single serial channel.
    simulated_speed: Option<u64>,
    simulated_device: Mutex<()>,
    state: Mutex<GateState>,
    turn: Condvar,
}

/// One card read slot. Hold it only for the read itself: holding it while blocked on
/// anything else keeps a culling read off the card.
pub struct CardPermit {
    gate: Arc<CardGate>,
    level: Level,
}

impl Drop for CardPermit {
    fn drop(&mut self) {
        let mut state = self.gate.lock();
        state.available += 1;
        if self.level == Level::Copy {
            state.copy_running = false;
        } else {
            state.culling_running -= 1;
            state.last_culling = Some(Instant::now());
        }
        drop(state);
        self.gate.turn.notify_all();
    }
}

impl CardGate {
    pub fn new(read_permits: usize) -> Arc<Self> {
        Self::with_simulated_speed(read_permits, None)
    }

    pub fn with_simulated_speed(read_permits: usize, simulated_speed: Option<u64>) -> Arc<Self> {
        Arc::new(CardGate {
            simulated_speed,
            simulated_device: Mutex::new(()),
            state: Mutex::new(GateState {
                available: read_permits.max(1),
                on_demand_waiting: 0,
                culling_waiting: 0,
                culling_running: 0,
                copy_running: false,
                last_culling: None,
            }),
            turn: Condvar::new(),
        })
    }

    /// Takes as long as reading `bytes` would on the simulated card, if there is one.
    pub(crate) fn simulate_read(&self, bytes: u64) {
        if let Some(speed) = self.simulated_speed {
            let _device = self
                .simulated_device
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            std::thread::sleep(Duration::from_secs_f64(bytes as f64 / speed.max(1) as f64));
        }
    }

    fn lock(&self) -> MutexGuard<'_, GateState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn permit(self: &Arc<Self>, level: Level) -> CardPermit {
        CardPermit {
            gate: Arc::clone(self),
            level,
        }
    }

    /// Ahead of everything: the photo being looked at. Blocks the calling thread.
    pub fn acquire_on_demand(self: &Arc<Self>) -> CardPermit {
        let mut state = self.lock();
        state.on_demand_waiting += 1;
        while state.available == 0 {
            state = self
                .turn
                .wait(state)
                .unwrap_or_else(PoisonError::into_inner);
        }
        state.on_demand_waiting -= 1;
        state.available -= 1;
        state.culling_running += 1;
        self.permit(Level::OnDemand)
    }

    /// Thumbnails and previews, ahead of the copy. Blocks the calling thread.
    pub fn acquire_priority(self: &Arc<Self>) -> CardPermit {
        let mut state = self.lock();
        state.culling_waiting += 1;
        while state.available == 0 || state.on_demand_waiting > 0 {
            state = self
                .turn
                .wait(state)
                .unwrap_or_else(PoisonError::into_inner);
        }
        state.culling_waiting -= 1;
        state.available -= 1;
        state.culling_running += 1;
        self.permit(Level::Culling)
    }

    /// For the copy: waits until no culling read is waiting, running or just finished.
    /// Blocks the calling thread.
    pub fn acquire_background(self: &Arc<Self>) -> CardPermit {
        let mut state = self.lock();
        loop {
            let busy = state.available == 0
                || state.copy_running
                || state.on_demand_waiting > 0
                || state.culling_waiting > 0
                || state.culling_running > 0;
            let grace_left = state.last_culling.map_or(Duration::ZERO, |at| {
                CULLING_GRACE.saturating_sub(at.elapsed())
            });
            if !busy && grace_left.is_zero() {
                state.available -= 1;
                state.copy_running = true;
                return self.permit(Level::Copy);
            }
            // A wakeup is sent on every release; the timeout covers the grace ending.
            let wait = if busy {
                Duration::from_millis(50)
            } else {
                grace_left
            };
            state = self
                .turn
                .wait_timeout(state, wait)
                .unwrap_or_else(PoisonError::into_inner)
                .0;
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;

    fn wait_for(what: &str, done: impl Fn() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while !done() {
            assert!(Instant::now() < deadline, "timed out: {what}");
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    #[test]
    fn the_order_is_on_demand_then_culling_then_copy() {
        let gate = CardGate::new(1);
        let held = gate.acquire_priority();
        let order = Arc::new(Mutex::new(vec![]));
        let spawn = |name: &'static str, level: Level| {
            let (gate, order) = (Arc::clone(&gate), Arc::clone(&order));
            std::thread::spawn(move || {
                let _permit = match level {
                    Level::OnDemand => gate.acquire_on_demand(),
                    Level::Culling => gate.acquire_priority(),
                    Level::Copy => gate.acquire_background(),
                };
                order.lock().unwrap().push(name);
                std::thread::sleep(Duration::from_millis(5));
            })
        };
        let copy = spawn("copy", Level::Copy);
        std::thread::sleep(Duration::from_millis(20));
        let culling = spawn("culling", Level::Culling);
        wait_for("culling waiting", || gate.lock().culling_waiting == 1);
        let on_demand = spawn("on demand", Level::OnDemand);
        wait_for("on demand waiting", || gate.lock().on_demand_waiting == 1);

        drop(held);
        for thread in [copy, culling, on_demand] {
            thread.join().unwrap();
        }
        assert_eq!(*order.lock().unwrap(), ["on demand", "culling", "copy"]);
    }

    #[test]
    fn a_steady_stream_of_culling_reads_keeps_the_copy_off_the_card() {
        let gate = CardGate::new(2);
        let copied = Arc::new(AtomicUsize::new(0));
        let copy = {
            let (gate, copied) = (Arc::clone(&gate), Arc::clone(&copied));
            std::thread::spawn(move || {
                let _permit = gate.acquire_background();
                copied.fetch_add(1, Ordering::SeqCst);
            })
        };
        // Reads back to back, as the ordered loader does.
        for _ in 0..200 {
            let _permit = gate.acquire_priority();
            std::thread::sleep(Duration::from_millis(1));
        }
        assert_eq!(copied.load(Ordering::SeqCst), 0);

        // Once the culling reads are done, the copy gets the card.
        copy.join().unwrap();
        assert_eq!(copied.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn the_copy_does_not_share_the_card_with_a_running_culling_read() {
        let gate = CardGate::new(4);
        let held = gate.acquire_priority();
        let copy = {
            let gate = Arc::clone(&gate);
            std::thread::spawn(move || drop(gate.acquire_background()))
        };
        std::thread::sleep(Duration::from_millis(50));
        assert!(!copy.is_finished());
        drop(held);
        copy.join().unwrap();
    }

    #[test]
    fn an_idle_card_goes_straight_to_the_copy() {
        let gate = CardGate::new(1);
        let started = Instant::now();
        drop(gate.acquire_background());
        drop(gate.acquire_background());
        assert!(started.elapsed() < Duration::from_millis(50));
    }

    #[test]
    fn a_waiting_culling_read_waits_for_only_one_chunk_of_a_copy() {
        let gate = CardGate::new(1);
        let copy = {
            let gate = Arc::clone(&gate);
            std::thread::spawn(move || {
                // A long file, read in chunks with the gate between them.
                for _ in 0..50 {
                    let _permit = gate.acquire_background();
                    std::thread::sleep(Duration::from_millis(5));
                }
            })
        };
        std::thread::sleep(Duration::from_millis(30));
        let asked = Instant::now();
        drop(gate.acquire_priority());
        assert!(
            asked.elapsed() < Duration::from_millis(100),
            "{:?}",
            asked.elapsed()
        );
        copy.join().unwrap();
    }
}
