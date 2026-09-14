//! Mailbox arrival notification for `MailWait` (dispatch lands in).
//!
//! One condvar for the whole daemon. The predicate is evaluated under
//! the watch lock and [`MailboxWatch::ring`] takes the same lock before
//! notifying, so a ring can never slip between a waiter's predicate
//! check and its park — the lost-wakeup window is closed by
//! construction. Rings are agent-agnostic; the predicate (mailbox
//! depth) is the filter, so a wakeup reveals nothing about other
//! agents' mail.
//!
//! Lock ordering: a waiter holds the watch lock while briefly taking
//! the store lock inside its predicate. Senders must therefore never
//! ring while holding the store lock — commit, release, then ring.

use std::sync::{Condvar, Mutex, MutexGuard};
use std::time::{Duration, Instant};

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Shared mail-arrival signal. Cheap to clone into every connection.
#[derive(Default)]
pub struct MailboxWatch {
    state: Mutex<()>,
    cond: Condvar,
    /// Test seam: runs inside `wait_until` after a false predicate,
    /// before the park, still under the watch lock. Lets a test place
    /// a ring attempt exactly in the check-to-park window.
    #[cfg(test)]
    post_check_hook: Mutex<Option<std::sync::Arc<dyn Fn() + Send + Sync>>>,
}

impl MailboxWatch {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Wake every watcher. Takes the watch lock first, which orders the
    /// notify against predicate checks: a ring lands either before a
    /// waiter's check (the predicate sees the new state) or after its
    /// park (the waiter wakes). Never between.
    pub fn ring(&self) {
        let _guard = lock(&self.state);
        self.cond.notify_all();
    }

    /// Evaluate `check` under the watch lock; while it yields `None`,
    /// park until the next ring or `timeout`. Returns the check value,
    /// or `None` on timeout.
    ///
    /// `check` runs under the watch lock, so it must be fast and must
    /// never call [`MailboxWatch::ring`] (same-thread relock).
    ///
    /// # Errors
    ///
    /// Propagates `check`'s error; the wait ends immediately.
    pub fn wait_until<T, E>(
        &self,
        timeout: Duration,
        mut check: impl FnMut() -> Result<Option<T>, E>,
    ) -> Result<Option<T>, E> {
        let mut guard = lock(&self.state);
        let deadline = Instant::now() + timeout;
        loop {
            if let Some(value) = check()? {
                return Ok(Some(value));
            }
            #[cfg(test)]
            {
                let hook = lock(&self.post_check_hook).clone();
                if let Some(hook) = hook {
                    hook();
                }
            }
            let now = Instant::now();
            if now >= deadline {
                return Ok(None);
            }
            guard = self
                .cond
                .wait_timeout(guard, deadline - now)
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .0;
        }
    }

    #[cfg(test)]
    pub fn set_post_check_hook(&self, hook: Option<std::sync::Arc<dyn Fn() + Send + Sync>>) {
        *lock(&self.post_check_hook) = hook;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::convert::Infallible;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::{Arc, Barrier};

    #[test]
    fn wait_until_returns_on_ring_before_timeout() {
        let watch = Arc::new(MailboxWatch::new());
        let other = Arc::clone(&watch);
        let start = Instant::now();
        let waker = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(50));
            other.ring();
        });
        let mut checks = 0;
        let result = watch
            .wait_until(Duration::from_secs(30), || {
                checks += 1;
                Ok::<_, Infallible>((checks > 1).then_some(()))
            })
            .unwrap();
        assert!(result.is_some(), "ring must wake the waiter");
        assert!(start.elapsed() < prismattyc_core::test_time_budget(Duration::from_secs(5)));
        waker.join().unwrap();
    }

    #[test]
    fn wait_until_times_out_without_ring() {
        let watch = MailboxWatch::new();
        let start = Instant::now();
        let result = watch
            .wait_until(Duration::from_millis(50), || {
                Ok::<Option<()>, Infallible>(None)
            })
            .unwrap();
        assert!(result.is_none());
        assert!(start.elapsed() >= Duration::from_millis(50));
    }

    #[test]
    fn wait_cannot_lose_a_ring_between_check_and_park() {
        // Place the ring attempt exactly in the check-to-park window:
        // the sender stores its mail, signals barrier B poised at
        // ring(), and the hook (running under the watch lock) then
        // sleeps 50ms — an uncoupled notify fires long before the park
        // and is lost; the lock-coupled ring blocks until the park and
        // wakes the waiter.
        let watch = Arc::new(MailboxWatch::new());
        let depth = Arc::new(AtomicUsize::new(0));
        let barrier = Arc::new(Barrier::new(2));
        let fired = Arc::new(AtomicBool::new(false));
        {
            let barrier = Arc::clone(&barrier);
            let fired = Arc::clone(&fired);
            watch.set_post_check_hook(Some(Arc::new(move || {
                if !fired.swap(true, Ordering::SeqCst) {
                    barrier.wait(); // A: sender may store now
                    barrier.wait(); // B: sender is poised at ring()
                    std::thread::sleep(Duration::from_millis(50));
                }
            })));
        }

        let ringer = {
            let depth = Arc::clone(&depth);
            let watch = Arc::clone(&watch);
            let barrier = Arc::clone(&barrier);
            std::thread::spawn(move || {
                barrier.wait(); // A: waiter's first check is done
                depth.fetch_add(1, Ordering::SeqCst);
                barrier.wait(); // B: stored, poised at ring
                watch.ring();
            })
        };

        let check_depth = Arc::clone(&depth);
        let result = watch
            .wait_until(Duration::from_secs(5), || {
                Ok::<_, Infallible>((check_depth.load(Ordering::SeqCst) > 0).then_some(()))
            })
            .unwrap();
        assert!(
            result.is_some(),
            "ring in the check-to-park window must not be lost"
        );
        ringer.join().unwrap();
    }
}
