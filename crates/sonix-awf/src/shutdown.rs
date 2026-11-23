//! The one time budget every wait of a shutdown shares.
//!
//! It is created once, before the first wait of the shutdown, so a slow backend cannot get a fresh
//! budget per storage, per attempt or per connection: everything is measured against one monotonic
//! deadline. A lock that blocks cannot extend it either, because locks are taken with the time
//! that is left. When the deadline passes, the expiry actions run once; an engine uses them to
//! fence every world, so that a queued commit commits nothing, and to cut off calls in flight.
//!
//! After expiry no new commit attempt starts, and nothing that missed the deadline is reported as
//! saved. What the deadline cuts is lock waits and new commit attempts. A FILE commit that is
//! already writing its files is not interrupted, so it can hold a stop past the deadline until it
//! finishes.

use std::any::Any;
use std::panic::{self, AssertUnwindSafe};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, LazyLock, Mutex, PoisonError};
use std::thread;
use std::time::{Duration, Instant};

use crate::error::{AwfError, Result};
use crate::lock;

type Clock = dyn Fn() -> u64 + Send + Sync;
type Action = Box<dyn FnOnce() + Send>;

/// The longest single sleep of the deadline timer; it sleeps again while time is left.
const LONGEST_SLEEP: Duration = Duration::from_secs(3600);

/// Nanoseconds on the process's monotonic clock, counted from its first use.
#[must_use]
pub fn monotonic_nanos() -> u64 {
    static ORIGIN: LazyLock<Instant> = LazyLock::new(Instant::now);
    u64::try_from(ORIGIN.elapsed().as_nanos()).unwrap_or(u64::MAX)
}

/// The time budget of one shutdown.
pub struct ShutdownBudget {
    clock: Box<Clock>,
    start_nanos: u64,
    total_nanos: u64,
    deadline_nanos: u64,
    /// Set once: by the expiry, or by [`Self::finish`].
    fired: AtomicBool,
    actions: Mutex<Vec<Action>>,
    timer: Mutex<Option<Arc<Timer>>>,
}

impl ShutdownBudget {
    /// A budget of `total`, at least one millisecond, on [`monotonic_nanos`].
    pub fn new(total: Duration) -> Result<Self> {
        Self::with_clock(total, monotonic_nanos)
    }

    /// As [`Self::new`], measured on `clock`, which returns monotonic nanoseconds.
    pub fn with_clock(
        total: Duration,
        clock: impl Fn() -> u64 + Send + Sync + 'static,
    ) -> Result<Self> {
        if total < Duration::from_millis(1) {
            return Err(AwfError::InvalidArgument(
                "shutdown deadline must be at least 1 ms".to_string(),
            ));
        }
        let start_nanos = clock();
        let total_nanos = u64::try_from(total.as_nanos()).unwrap_or(u64::MAX);
        Ok(Self {
            clock: Box::new(clock),
            start_nanos,
            total_nanos,
            deadline_nanos: start_nanos.saturating_add(total_nanos),
            fired: AtomicBool::new(false),
            actions: Mutex::default(),
            timer: Mutex::default(),
        })
    }

    /// Fires the expiry actions at the deadline from a thread of its own, even if nothing is
    /// waiting then. Without it they run when the first caller sees the deadline has passed.
    pub fn start_timer(self: &Arc<Self>) -> Result<()> {
        let mut timer = lock(&self.timer);
        if timer.is_some() {
            return Ok(());
        }
        let started = Arc::new(Timer::default());
        let sleeper = Arc::clone(&started);
        // Weak, so a budget nobody holds any more is not kept alive until its deadline.
        let budget = Arc::downgrade(self);
        let wait = self.remaining();
        thread::Builder::new()
            .name("sonix-awf-shutdown-deadline".to_string())
            .spawn(move || {
                if sleeper.sleep(wait)
                    && let Some(budget) = budget.upgrade()
                {
                    budget.fire_expiry();
                }
            })?;
        *timer = Some(started);
        Ok(())
    }

    /// The deadline on the clock's scale.
    #[must_use]
    pub const fn deadline_nanos(&self) -> u64 {
        self.deadline_nanos
    }

    /// The whole budget.
    #[must_use]
    pub const fn total(&self) -> Duration {
        Duration::from_nanos(self.total_nanos)
    }

    /// Time since the budget was created.
    #[must_use]
    pub fn elapsed(&self) -> Duration {
        Duration::from_nanos((self.clock)().saturating_sub(self.start_nanos))
    }

    /// Time left; zero once the deadline has passed.
    #[must_use]
    pub fn remaining(&self) -> Duration {
        Duration::from_nanos(self.deadline_nanos.saturating_sub((self.clock)()))
    }

    /// Whether the deadline has passed. The first caller that sees it runs the expiry actions.
    ///
    /// A budget that was [finished](Self::finish) answers `true` as well: nothing new starts
    /// under it.
    pub fn expired(&self) -> bool {
        if self.fired.load(Ordering::SeqCst) {
            return true;
        }
        if !self.remaining().is_zero() {
            return false;
        }
        self.fire_expiry();
        true
    }

    /// Registers an action for the moment the deadline passes, and runs it now if it already
    /// has. Actions must not block: they run on the timer thread or on whichever thread first
    /// sees the expiry.
    pub fn on_expiry(&self, action: impl FnOnce() + Send + 'static) {
        let action: Action = Box::new(action);
        let late = {
            let mut actions = lock(&self.actions);
            if self.fired.load(Ordering::SeqCst) {
                Some(action)
            } else {
                actions.push(action);
                None
            }
        };
        if let Some(action) = late {
            run_quietly(action);
        }
    }

    /// The shutdown is over: the deadline no longer applies, the expiry actions never run and the
    /// timer stops. It keeps a slow step after the storage shutdown from producing a deadline
    /// warning.
    pub fn finish(&self) {
        if self.claim() {
            let dropped = std::mem::take(&mut *lock(&self.actions));
            drop(dropped);
            self.stop_timer();
        }
    }

    /// Runs the expiry actions, once.
    pub fn fire_expiry(&self) {
        if self.claim() {
            let actions = std::mem::take(&mut *lock(&self.actions));
            for action in actions {
                run_quietly(action);
            }
        }
    }

    /// Whether this call is the one that ends the budget.
    fn claim(&self) -> bool {
        self.fired
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_ok()
    }

    fn stop_timer(&self) {
        let timer = lock(&self.timer).clone();
        if let Some(timer) = timer {
            timer.cancel();
        }
    }
}

impl Drop for ShutdownBudget {
    fn drop(&mut self) {
        self.stop_timer();
    }
}

/// Runs one expiry action. A failing action is logged and does not stop the ones after it.
fn run_quietly(action: Action) {
    if let Err(failure) = panic::catch_unwind(AssertUnwindSafe(action)) {
        tracing::warn!(
            "AWF shutdown deadline action failed: {}",
            panic_text(failure.as_ref())
        );
    }
}

fn panic_text(failure: &(dyn Any + Send)) -> &str {
    failure
        .downcast_ref::<&str>()
        .copied()
        .or_else(|| failure.downcast_ref::<String>().map(String::as_str))
        .unwrap_or("no message")
}

/// What the deadline thread sleeps on, so that it can be told to stop early.
#[derive(Default)]
struct Timer {
    state: Mutex<TimerState>,
    wake: Condvar,
}

#[derive(Default)]
struct TimerState {
    cancelled: bool,
}

impl Timer {
    /// Sleeps for `wait`. Whether the time passed without the timer being cancelled.
    fn sleep(&self, wait: Duration) -> bool {
        let started = Instant::now();
        let mut state = lock(&self.state);
        loop {
            if state.cancelled {
                return false;
            }
            let left = wait.saturating_sub(started.elapsed());
            if left.is_zero() {
                return true;
            }
            state = self
                .wake
                .wait_timeout(state, left.min(LONGEST_SLEEP))
                .unwrap_or_else(PoisonError::into_inner)
                .0;
        }
    }

    fn cancel(&self) {
        lock(&self.state).cancelled = true;
        self.wake.notify_all();
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU64, AtomicUsize};
    use std::sync::mpsc;

    use super::*;

    /// A budget on a clock the test moves by hand, starting at an arbitrary reading.
    fn manual(total_millis: u64) -> (ShutdownBudget, Arc<AtomicU64>) {
        let now = Arc::new(AtomicU64::new(7_000));
        let clock = Arc::clone(&now);
        let budget = ShutdownBudget::with_clock(Duration::from_millis(total_millis), move || {
            clock.load(Ordering::SeqCst)
        })
        .unwrap();
        (budget, now)
    }

    fn advance(now: &AtomicU64, millis: u64) {
        now.fetch_add(millis * 1_000_000, Ordering::SeqCst);
    }

    fn counter() -> (Arc<AtomicUsize>, impl FnOnce() + Send + 'static) {
        let count = Arc::new(AtomicUsize::new(0));
        let counted = Arc::clone(&count);
        (count, move || {
            counted.fetch_add(1, Ordering::SeqCst);
        })
    }

    #[test]
    fn a_budget_shorter_than_a_millisecond_is_refused() {
        for too_short in [Duration::ZERO, Duration::from_micros(999)] {
            assert!(matches!(
                ShutdownBudget::new(too_short),
                Err(AwfError::InvalidArgument(_))
            ));
        }
        assert!(ShutdownBudget::new(Duration::from_millis(1)).is_ok());
    }

    #[test]
    fn the_deadline_is_measured_on_the_clock_it_was_given() {
        let (budget, now) = manual(100);
        assert_eq!(budget.deadline_nanos(), 7_000 + 100_000_000);
        assert_eq!(budget.total(), Duration::from_millis(100));
        assert_eq!(budget.remaining(), Duration::from_millis(100));
        assert_eq!(budget.elapsed(), Duration::ZERO);
        assert!(!budget.expired());

        advance(&now, 40);
        assert_eq!(budget.elapsed(), Duration::from_millis(40));
        assert_eq!(budget.remaining(), Duration::from_millis(60));
        assert!(!budget.expired());

        advance(&now, 60);
        assert_eq!(budget.remaining(), Duration::ZERO);
        assert!(budget.expired(), "at the deadline, not only after it");
        advance(&now, 500);
        assert_eq!(budget.remaining(), Duration::ZERO, "never negative");
        assert_eq!(budget.elapsed(), Duration::from_millis(600));
    }

    #[test]
    fn expiry_actions_run_once_in_the_order_they_were_registered() {
        let (budget, now) = manual(100);
        let order = Arc::new(Mutex::new(Vec::new()));
        for name in ["first", "second", "third"] {
            let order = Arc::clone(&order);
            budget.on_expiry(move || order.lock().unwrap().push(name));
        }
        assert!(!budget.expired());
        assert!(
            order.lock().unwrap().is_empty(),
            "nothing before the deadline"
        );

        advance(&now, 100);
        assert!(budget.expired());
        assert!(budget.expired());
        budget.fire_expiry();
        assert_eq!(*order.lock().unwrap(), ["first", "second", "third"]);
    }

    #[test]
    fn an_action_registered_after_expiry_runs_at_once() {
        let (budget, now) = manual(100);
        advance(&now, 100);
        assert!(budget.expired());
        let (count, action) = counter();
        budget.on_expiry(action);
        assert_eq!(count.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn a_finished_budget_never_runs_the_actions_it_was_given() {
        let (budget, now) = manual(100);
        let (count, action) = counter();
        budget.on_expiry(action);
        budget.finish();
        advance(&now, 500);
        assert!(
            budget.expired(),
            "nothing new starts under a finished budget"
        );
        budget.fire_expiry();
        assert_eq!(count.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn a_failing_action_does_not_stop_the_ones_after_it() {
        let (budget, now) = manual(100);
        budget.on_expiry(|| panic!("failing on purpose, for the test"));
        let (count, action) = counter();
        budget.on_expiry(action);
        advance(&now, 100);
        assert!(budget.expired());
        assert_eq!(count.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn the_timer_fires_the_actions_without_anyone_asking() {
        let budget = Arc::new(ShutdownBudget::new(Duration::from_millis(50)).unwrap());
        let (fired, waiting) = mpsc::channel();
        budget.on_expiry(move || {
            // The receiver is gone only if the test already failed.
            let _ = fired.send(());
        });
        budget.start_timer().unwrap();
        budget.start_timer().unwrap();
        assert!(
            waiting.recv_timeout(Duration::from_secs(30)).is_ok(),
            "the deadline thread ran the action"
        );
        assert!(budget.expired());
    }

    #[test]
    fn a_finished_budget_stops_its_timer() {
        let budget = Arc::new(ShutdownBudget::new(Duration::from_millis(100)).unwrap());
        let (fired, waiting) = mpsc::channel();
        budget.on_expiry(move || {
            let _ = fired.send(());
        });
        budget.start_timer().unwrap();
        budget.finish();
        assert!(
            waiting.recv_timeout(Duration::from_millis(400)).is_err(),
            "the action was dropped, not run"
        );
    }
}
