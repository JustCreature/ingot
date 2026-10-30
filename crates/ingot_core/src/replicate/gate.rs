//! The one gate every read of a source card goes through.
//!
//! A card serves a limited number of useful concurrent reads, and previews and
//! replication both want them. Previews are what the photographer is waiting for,
//! so a waiting preview read always goes first. Replication gets every slot the
//! previews aren't using and loses nothing overall: it only waits for the few MB
//! each preview reads.

use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};

use tokio::sync::Notify;

struct GateState {
    available: usize,
    priority_waiting: usize,
}

pub struct CardGate {
    state: Mutex<GateState>,
    priority_turn: Condvar,
    background_turn: Notify,
}

/// One card read slot. Hold it only for the read itself: holding it while blocked
/// on anything else starves preview reads.
pub struct CardPermit {
    gate: Arc<CardGate>,
}

impl Drop for CardPermit {
    fn drop(&mut self) {
        let mut state = self.gate.lock();
        state.available += 1;
        self.gate.wake(&state);
    }
}

impl CardGate {
    pub fn new(read_permits: usize) -> Arc<Self> {
        Arc::new(CardGate {
            state: Mutex::new(GateState {
                available: read_permits.max(1),
                priority_waiting: 0,
            }),
            priority_turn: Condvar::new(),
            background_turn: Notify::new(),
        })
    }

    fn lock(&self) -> MutexGuard<'_, GateState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn wake(&self, state: &GateState) {
        if state.available == 0 {
            return;
        }
        if state.priority_waiting > 0 {
            self.priority_turn.notify_one();
        } else {
            self.background_turn.notify_one();
        }
    }

    /// Blocks the calling thread until a slot is free, ahead of all background
    /// reads. For preview reads, which run on dedicated reader threads.
    pub fn acquire_priority(self: &Arc<Self>) -> CardPermit {
        let mut state = self.lock();
        state.priority_waiting += 1;
        while state.available == 0 {
            state = self
                .priority_turn
                .wait(state)
                .unwrap_or_else(PoisonError::into_inner);
        }
        state.priority_waiting -= 1;
        state.available -= 1;
        self.wake(&state);
        CardPermit {
            gate: Arc::clone(self),
        }
    }

    /// Waits until a slot is free and no preview read is waiting for one.
    pub async fn acquire_background(self: &Arc<Self>) -> CardPermit {
        loop {
            {
                let mut state = self.lock();
                if state.available > 0 && state.priority_waiting == 0 {
                    state.available -= 1;
                    self.wake(&state);
                    return CardPermit {
                        gate: Arc::clone(self),
                    };
                }
            }
            // `notify_one` stores a wakeup when nobody is waiting yet, so a release
            // between the check above and this await isn't lost.
            self.background_turn.notified().await;
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    #[tokio::test(flavor = "multi_thread")]
    async fn waiting_preview_read_goes_before_background_reads() {
        let gate = CardGate::new(1);
        let held = gate.acquire_priority();

        let preview = {
            let gate = Arc::clone(&gate);
            tokio::task::spawn_blocking(move || gate.acquire_priority())
        };
        while gate.lock().priority_waiting == 0 {
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
        let background = {
            let gate = Arc::clone(&gate);
            tokio::spawn(async move { gate.acquire_background().await })
        };
        tokio::time::sleep(Duration::from_millis(20)).await;

        drop(held);
        let preview_permit = preview.await.unwrap();
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert!(!background.is_finished());

        drop(preview_permit);
        tokio::time::timeout(Duration::from_secs(5), background)
            .await
            .expect("background read never got the slot")
            .unwrap();
    }

    #[tokio::test]
    async fn every_background_waiter_gets_a_free_slot() {
        let gate = CardGate::new(2);
        let (first, second) = (gate.acquire_priority(), gate.acquire_priority());
        let waiters: Vec<_> = (0..2)
            .map(|_| {
                let gate = Arc::clone(&gate);
                tokio::spawn(async move { gate.acquire_background().await })
            })
            .collect();
        tokio::task::yield_now().await;

        drop((first, second));
        for waiter in waiters {
            tokio::time::timeout(Duration::from_secs(5), waiter)
                .await
                .expect("waiter left asleep with a free slot")
                .unwrap();
        }
    }
}
