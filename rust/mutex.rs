use std::sync::atomic::{AtomicBool, Ordering::*};

#[derive(Debug)]
pub struct AtomicMutex {
    locked: AtomicBool,
}

#[derive(Debug, Clone, Copy)]
pub struct AtomicMutexErr;

impl std::fmt::Display for AtomicMutexErr {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Mutex is already locked")
    }
}

impl std::error::Error for AtomicMutexErr {}

pub struct AtomicMutexGuard<'a> {
    mutex: &'a AtomicMutex,
}

impl AtomicMutex {
    pub const fn new() -> Self {
        Self {
            locked: AtomicBool::new(false),
        }
    }

    pub fn try_lock(&self) -> Result<AtomicMutexGuard<'_>, AtomicMutexErr> {
        if self.locked.swap(true, Acquire) {
            Err(AtomicMutexErr)
        } else {
            Ok(AtomicMutexGuard { mutex: self })
        }
    }

    pub fn lock(&self) -> AtomicMutexGuard<'_> {
        // Bounded spin, then yield. The previous pure `loop { try_lock }`
        // burned the whole OS thread while waiting: on a small tokio
        // runtime (worker_threads(2) in the iOS packet tunnel), one worker
        // holding the lock in sys_check_timeouts while another spun here
        // meant NO other task could be polled — with enough contenders the
        // runtime live-locked permanently. Yielding lets the OS reschedule
        // the holder (and lets other runtime threads make progress) at the
        // cost of a syscall on the slow path.
        let mut spins = 0u32;
        loop {
            if let Ok(m) = self.try_lock() {
                break m;
            }
            spins += 1;
            if spins < 64 {
                std::hint::spin_loop();
            } else {
                std::thread::yield_now();
            }
        }
    }
}

impl Default for AtomicMutex {
    fn default() -> Self {
        Self::new()
    }
}

impl<'a> Drop for AtomicMutexGuard<'a> {
    fn drop(&mut self) {
        let _prev = self.mutex.locked.swap(false, Release);
        debug_assert!(_prev);
    }
}
