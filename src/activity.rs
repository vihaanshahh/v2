//! Shared local-activity signal for yield-to-local (H3 / invariant I1).
//!
//! The metering proxy `touch()`es this every time the machine's owner makes a
//! local request. The mesh serving loop reads `idle_secs()` and refuses / evicts
//! remote work whenever the owner has been active recently. One cheap atomic,
//! shared by clone.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use crate::usage::now_unix;

#[derive(Clone)]
pub struct Activity {
    last_local: Arc<AtomicU64>,
}

impl Default for Activity {
    fn default() -> Self {
        Self::new()
    }
}

impl Activity {
    pub fn new() -> Self {
        Self { last_local: Arc::new(AtomicU64::new(0)) }
    }

    /// Record that the owner just used the machine locally.
    pub fn touch(&self) {
        self.last_local.store(now_unix(), Ordering::Relaxed);
    }

    /// Seconds since the last local request (large number if never).
    pub fn idle_secs(&self) -> u64 {
        let last = self.last_local.load(Ordering::Relaxed);
        if last == 0 {
            u64::MAX
        } else {
            now_unix().saturating_sub(last)
        }
    }

    /// True if the owner has been active within `cooldown` seconds.
    pub fn owner_active(&self, cooldown: u64) -> bool {
        self.idle_secs() < cooldown
    }
}
