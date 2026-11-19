//! One lock for tests that mutate process-wide environment variables.
//!
//! `std::env` is shared by every test in a binary, so a test that sets `HOME`, `XDG_*`,
//! `APPDATA`, or `TSK_*` holds this lock until it stops reading paths resolved from them.
//! It is crate-wide on purpose: a per-module lock lets two modules rewrite the same
//! variable at once, and a test then resolves another module's temporary home.

static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Hold the process-wide environment lock for as long as the guard lives.
pub(crate) fn env_lock() -> std::sync::MutexGuard<'static, ()> {
    ENV_LOCK.lock().unwrap_or_else(|error| error.into_inner())
}
