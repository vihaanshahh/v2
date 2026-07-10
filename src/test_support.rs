//! Shared test-only infrastructure for anything that must point the
//! process-global `HOME` at a temp dir (mesh identity/policy/usage all resolve
//! paths through `paths::home()`, which reads `$HOME`). All such tests share
//! this one lock — `std::env::set_var` mutates process-global state, so two
//! tests racing to set different temp homes would corrupt each other's runs.

use std::sync::{Mutex, MutexGuard};

static HOME_LOCK: Mutex<()> = Mutex::new(());

/// Hold this for the duration of any test that calls `set_temp_home`.
pub(crate) fn lock() -> MutexGuard<'static, ()> {
    HOME_LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

/// Point `$HOME` at a fresh temp dir keyed by test name, so `~/.v2` reads/writes
/// in this test never touch the real user's home directory.
pub(crate) fn set_temp_home(label: &str) {
    let dir = std::env::temp_dir().join(format!("v2-test-{}-{}", std::process::id(), label));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::env::set_var("HOME", &dir);
}
