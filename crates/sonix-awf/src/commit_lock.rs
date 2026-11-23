//! The lock the commits of one world take in turn.
//!
//! A plain mutex cannot be taken with a time limit and cannot say whether it is held. The shutdown
//! path needs both: it takes the lock with the time its budget has left, and it reports a commit
//! that is still running.

use std::sync::{Condvar, Mutex, PoisonError};
use std::time::{Duration, Instant};

use crate::lock;
use crate::shutdown::ShutdownBudget;

/// A lock held from a commit's snapshot until the store has the commit.
#[derive(Default)]
pub struct CommitLock {
    state: Mutex<State>,
    released: Condvar,
}

#[derive(Default)]
struct State {
    held: bool,
}

/// Holds a [`CommitLock`] until it is dropped.
#[must_use = "the lock is released as soon as the guard is dropped"]
pub struct CommitGuard<'a> {
    lock: &'a CommitLock,
}

impl CommitLock {
    /// Takes the lock, waiting for whoever holds it.
    pub fn lock(&self) -> CommitGuard<'_> {
        let mut state = lock(&self.state);
        while state.held {
            state = self
                .released
                .wait(state)
                .unwrap_or_else(PoisonError::into_inner);
        }
        state.held = true;
        CommitGuard { lock: self }
    }

    /// Takes the lock if it is free, or becomes free within `timeout`.
    pub fn try_lock_for(&self, timeout: Duration) -> Option<CommitGuard<'_>> {
        let started = Instant::now();
        let mut state = lock(&self.state);
        while state.held {
            let left = timeout.saturating_sub(started.elapsed());
            if left.is_zero() {
                return None;
            }
            state = self
                .released
                .wait_timeout(state, left)
                .unwrap_or_else(PoisonError::into_inner)
                .0;
        }
        state.held = true;
        Some(CommitGuard { lock: self })
    }

    /// Takes the lock if it is free, or becomes free within the time `budget` has left. Never
    /// waits past the deadline.
    pub fn try_lock_within(&self, budget: &ShutdownBudget) -> Option<CommitGuard<'_>> {
        self.try_lock_for(Duration::ZERO)
            .or_else(|| self.try_lock_for(budget.remaining()))
    }

    /// Whether the lock is held right now.
    pub fn is_locked(&self) -> bool {
        lock(&self.state).held
    }
}

impl Drop for CommitGuard<'_> {
    fn drop(&mut self) {
        lock(&self.lock.state).held = false;
        self.lock.released.notify_one();
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::mpsc;
    use std::thread;

    use super::*;

    #[test]
    fn the_lock_is_free_again_once_its_guard_is_dropped() {
        let lock = CommitLock::default();
        assert!(!lock.is_locked());
        let guard = lock.lock();
        assert!(lock.is_locked());
        assert!(lock.try_lock_for(Duration::ZERO).is_none());
        drop(guard);
        assert!(!lock.is_locked());
        assert!(lock.try_lock_for(Duration::ZERO).is_some());
    }

    #[test]
    fn a_waiter_gets_the_lock_when_the_holder_lets_go() {
        let lock = Arc::new(CommitLock::default());
        let guard = lock.lock();
        let (got_it, waiting) = mpsc::channel();
        let waiter = {
            let lock = Arc::clone(&lock);
            thread::spawn(move || {
                let guard = lock.try_lock_for(Duration::from_secs(30));
                got_it.send(guard.is_some()).unwrap();
            })
        };
        assert!(
            waiting.recv_timeout(Duration::from_millis(200)).is_err(),
            "held: the waiter is still waiting"
        );
        drop(guard);
        assert!(waiting.recv_timeout(Duration::from_secs(30)).unwrap());
        waiter.join().unwrap();
        assert!(!lock.is_locked(), "the waiter's guard was dropped with it");
    }

    #[test]
    fn a_lock_wait_never_outlasts_the_budget() {
        let lock = CommitLock::default();
        let guard = lock.lock();
        // Taken before the budget starts, so the wait measured below covers the whole budget.
        let started = Instant::now();
        let budget = ShutdownBudget::new(Duration::from_millis(50)).unwrap();
        assert!(lock.try_lock_within(&budget).is_none());
        let waited = started.elapsed();
        assert!(
            waited >= Duration::from_millis(45),
            "it waited for the budget: {waited:?}"
        );
        assert!(
            waited < Duration::from_secs(20),
            "and not for the holder: {waited:?}"
        );
        assert!(budget.remaining().is_zero());
        assert!(
            lock.try_lock_within(&budget).is_none(),
            "no wait at all after the deadline"
        );
        drop(guard);
        assert!(
            lock.try_lock_within(&budget).is_some(),
            "a free lock is taken whatever the budget says"
        );
    }
}
