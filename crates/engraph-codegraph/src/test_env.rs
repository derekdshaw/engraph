//! Test-only: the lock and restore guard for unit tests that write process
//! env vars (`ENGRAPH_BAZEL_SCIP_*_CMD`, `NODE_OPTIONS`,
//! `ENGRAPH_NODE_HEAP_MB`, `PATH`). One lock for the crate, because a test in
//! one module reads what a test in another sets.

use std::ffi::{OsStr, OsString};
use std::sync::{Mutex, MutexGuard};

static ENV: Mutex<()> = Mutex::new(());

/// Held by every test that writes one of these variables, and by every test
/// whose assertion depends on their value.
pub(crate) fn lock() -> MutexGuard<'static, ()> {
    // A failed test poisons the mutex; the next test still gets the lock.
    ENV.lock().unwrap_or_else(|e| e.into_inner())
}

/// Env vars a test sets, put back as they were on drop, so a failed assert
/// does not leak them into the next test. Declare it after the `lock()`
/// guard, so it drops (restores) while the lock is still held.
#[derive(Default)]
pub(crate) struct EnvGuard(Vec<(&'static str, Option<OsString>)>);

impl EnvGuard {
    /// Set `key` to `value`, or unset it with `None`.
    pub(crate) fn set(&mut self, key: &'static str, value: Option<&OsStr>) {
        self.0.push((key, std::env::var_os(key)));
        write(key, value);
    }

    /// Put `dir` first on `PATH`, ahead of the real binaries.
    pub(crate) fn prepend_path(&mut self, dir: &std::path::Path) {
        let old = std::env::var_os("PATH").unwrap_or_default();
        let path = std::env::join_paths(
            std::iter::once(dir.to_path_buf()).chain(std::env::split_paths(&old)),
        )
        .unwrap();
        self.set("PATH", Some(&path));
    }
}

impl Drop for EnvGuard {
    fn drop(&mut self) {
        // Newest first, so a key set twice ends at its first saved value.
        for (key, old) in self.0.drain(..).rev() {
            write(key, old.as_deref());
        }
    }
}

fn write(key: &str, value: Option<&OsStr>) {
    // SAFETY: the caller holds `lock()`, so no other test writes these
    // variables meanwhile. Rust's own reads (`std::env::var`, and the PATH
    // search of `Command` spawn) take std's env lock, the one `set_var`
    // takes, so the tests that spawn processes without `lock()` cannot race
    // this write. Not covered: C code in a parallel test that calls `getenv`
    // directly. That is a test-only risk, accepted here.
    unsafe {
        match value {
            Some(v) => std::env::set_var(key, v),
            None => std::env::remove_var(key),
        }
    }
}
