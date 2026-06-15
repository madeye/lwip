use parking_lot::{Mutex, MutexGuard};

/// Global serialization lock for every lwIP entry point.
///
/// This was a hand-rolled `AtomicBool` spin lock whose contention path called
/// `std::thread::yield_now()`. That yields the OS thread but NOT the tokio
/// scheduler, so a worker spinning here never returns to poll other tasks:
/// under a connection burst on a small (2–4 worker) runtime, a worker spinning
/// for the lock could starve the holder and wedge the entire runtime — the iOS
/// packet-tunnel total-freeze (packet count flat, control API dead).
///
/// It is now a real parking mutex (`parking_lot::Mutex`). A contender does a
/// brief adaptive spin and then BLOCKS at the OS level, so the scheduler
/// immediately runs the lock holder and the lock always drains — livelock is
/// impossible. The type/method names are kept (`AtomicMutex`, `AtomicMutexErr`,
/// `try_lock`, `lock`) so existing call sites and the `LWIPMutexGuard` alias
/// are untouched.
///
/// The lock is non-reentrant: lwIP callbacks run with it already held and must
/// never re-acquire it (they don't — they take `assume_locked` references to
/// the already-locked state). The guard is `!Send`, which additionally turns
/// "held across an `.await`" into a compile error instead of a latent deadlock.
#[derive(Debug, Default)]
pub struct AtomicMutex {
    inner: Mutex<()>,
}

/// Error returned by [`AtomicMutex::try_lock`] when the lock is already held.
#[derive(Debug, Clone, Copy)]
pub struct AtomicMutexErr;

impl std::fmt::Display for AtomicMutexErr {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Mutex is already locked")
    }
}

impl std::error::Error for AtomicMutexErr {}

/// RAII guard; dropping it unlocks the mutex.
pub struct AtomicMutexGuard<'a> {
    _guard: MutexGuard<'a, ()>,
}

impl AtomicMutex {
    pub const fn new() -> Self {
        Self {
            inner: Mutex::new(()),
        }
    }

    // Kept for API parity with the previous hand-rolled mutex (and so the
    // `AtomicMutexErr` / `Error::AtomicMutexErr` variant stays live); no
    // current caller now that `lock()` delegates straight to parking_lot.
    #[allow(dead_code)]
    pub fn try_lock(&self) -> Result<AtomicMutexGuard<'_>, AtomicMutexErr> {
        match self.inner.try_lock() {
            Some(guard) => Ok(AtomicMutexGuard { _guard: guard }),
            None => Err(AtomicMutexErr),
        }
    }

    pub fn lock(&self) -> AtomicMutexGuard<'_> {
        AtomicMutexGuard {
            _guard: self.inner.lock(),
        }
    }
}
