//! One index run per project at a time.
//!
//! An index run takes a lock file for its project under `<data dir>/locks/`
//! (next to the database, so `ENGRAPH_DB_PATH` moves it too) and removes it
//! when it finishes, by `Drop`. A second run that finds the file does not
//! start. A run that dies before the end — killed, crashed, machine off —
//! leaves its file behind, so a file older than [`STALE_AFTER`] is taken for
//! such a remnant and removed by the next run to come along.
//!
//! Creation is `create_new` (O_EXCL), which is atomic on every platform we
//! build for, so two runs racing for a free lock get exactly one winner. A
//! stale file is not deleted in place: it is renamed to a per-process name
//! first, so two runs that both judge it stale can't delete each other's
//! fresh replacement — only the one whose rename succeeded took it away, and
//! both then go back to `create_new`, where again one wins.

use anyhow::{Context, Result};
use sha2::{Digest, Sha256};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

/// A lock file this old belongs to a run that never finished.
pub const STALE_AFTER: Duration = Duration::from_secs(30 * 60);

/// Held for the duration of an index run; removing the file on drop.
#[derive(Debug)]
pub struct IndexLock {
    path: PathBuf,
}

impl IndexLock {
    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for IndexLock {
    fn drop(&mut self) {
        // A vanished file (someone judged us stale and removed it) is fine;
        // anything else is logged, since the remnant blocks the next run for
        // up to STALE_AFTER.
        if let Err(e) = std::fs::remove_file(&self.path)
            && e.kind() != std::io::ErrorKind::NotFound
        {
            tracing::warn!(path = %self.path.display(), error = %e, "index lock not removed");
        }
    }
}

/// Outcome of [`acquire`].
#[derive(Debug)]
pub enum Acquire {
    Acquired(IndexLock),
    /// Another run holds the lock (its file is younger than [`STALE_AFTER`]).
    Held {
        path: PathBuf,
        /// What the holder wrote: project, pid, start time.
        info: String,
    },
}

/// The one-line explanation for a run that found `project`'s lock held.
pub fn held_message(project: &str, path: &Path, info: &str) -> String {
    let started = info
        .lines()
        .find_map(|l| l.strip_prefix("started="))
        .unwrap_or("unknown time");
    format!(
        "an index run for {project} is already in progress (lock {}, started {started}); \
         locks older than {} min are cleared automatically",
        path.display(),
        STALE_AFTER.as_secs() / 60
    )
}

/// `<data dir>/locks`, beside the database.
pub fn lock_dir() -> PathBuf {
    engraph_core::db::default_db_path()
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."))
        .join("locks")
}

/// The lock file for `project`: `index-<sha256(project)[..16]>.lock` in
/// [`lock_dir`], so any project key — a path with separators and spaces —
/// maps to one flat name.
pub fn lock_path(project: &str) -> PathBuf {
    lock_path_in(&lock_dir(), project)
}

/// [`lock_path`] under an explicit directory.
pub fn lock_path_in(dir: &Path, project: &str) -> PathBuf {
    let mut hasher = Sha256::new();
    hasher.update(project.as_bytes());
    let hex: String = hasher
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    dir.join(format!("index-{}.lock", &hex[..16]))
}

/// Take the index lock for `project`, removing a stale file first. `Held`
/// when a live run has it; `Err` only for I/O trouble other than contention.
pub fn acquire(project: &str) -> Result<Acquire> {
    let path = lock_path(project);
    std::fs::create_dir_all(path.parent().expect("lock path has a parent"))
        .with_context(|| format!("create lock dir for {}", path.display()))?;
    // Two attempts: the first may find a stale file and clear it; the second
    // then competes for the free slot. Anything still there after that is
    // live, or was just taken by a run that beat us to it — either way, held.
    for attempt in 0..2 {
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
        {
            Ok(mut f) => {
                // Best-effort contents for whoever reads the file by hand;
                // staleness goes by mtime, not by this.
                let _ = write!(
                    f,
                    "project={project}\npid={}\nstarted={}\n",
                    std::process::id(),
                    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
                );
                return Ok(Acquire::Acquired(IndexLock { path }));
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                if attempt == 0 && remove_if_stale(&path) {
                    continue;
                }
                let info = std::fs::read_to_string(&path).unwrap_or_default();
                return Ok(Acquire::Held { path, info });
            }
            Err(e) => {
                return Err(e).with_context(|| format!("create index lock {}", path.display()));
            }
        }
    }
    unreachable!("both acquire attempts fell through")
}

/// Remove `path` if its mtime is older than [`STALE_AFTER`]. `true` when the
/// file is gone afterwards — removed here, or by someone else meanwhile — so
/// the caller should try to create it again; `false` when it is live.
fn remove_if_stale(path: &Path) -> bool {
    let modified = match std::fs::metadata(path).and_then(|m| m.modified()) {
        Ok(t) => t,
        // Vanished between the failed create and now: free to retry.
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return true,
        Err(_) => return false,
    };
    let age = SystemTime::now()
        .duration_since(modified)
        .unwrap_or(Duration::ZERO);
    if age < STALE_AFTER {
        return false;
    }
    // Claim it by rename before deleting: if another run already renamed it
    // away, our rename fails and we must not touch the fresh lock it may
    // have created since.
    let claimed = path.with_extension(format!("stale-{}", std::process::id()));
    match std::fs::rename(path, &claimed) {
        Ok(()) => {
            tracing::warn!(
                path = %path.display(),
                age_secs = age.as_secs(),
                "removing stale index lock"
            );
            let _ = std::fs::remove_file(&claimed);
            true
        }
        Err(e) => e.kind() == std::io::ErrorKind::NotFound,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Serialize tests that set `ENGRAPH_DB_PATH` (process-global).
    static ENV: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn with_temp_data_dir<T>(f: impl FnOnce() -> T) -> T {
        let _g = ENV.lock().unwrap_or_else(|e| e.into_inner());
        let dir = tempfile::tempdir().unwrap();
        // SAFETY: the mutex above keeps this test-only env mutation from
        // racing the other tests in this module that read it.
        unsafe { std::env::set_var("ENGRAPH_DB_PATH", dir.path().join("e.db")) };
        let out = f();
        unsafe { std::env::remove_var("ENGRAPH_DB_PATH") };
        out
    }

    #[test]
    fn acquire_is_exclusive_and_released_on_drop() {
        with_temp_data_dir(|| {
            let lock = match acquire("/p").unwrap() {
                Acquire::Acquired(l) => l,
                other => panic!("{other:?}"),
            };
            assert!(lock.path().exists());
            assert!(lock.path().starts_with(lock_dir()));
            let contents = std::fs::read_to_string(lock.path()).unwrap();
            assert!(contents.contains("project=/p\n"), "{contents}");
            assert!(contents.contains("pid="), "{contents}");

            match acquire("/p").unwrap() {
                Acquire::Held { path, info } => {
                    assert_eq!(path, lock.path());
                    assert_eq!(info, contents);
                }
                other => panic!("{other:?}"),
            }
            // A different project is independent.
            assert!(matches!(acquire("/q").unwrap(), Acquire::Acquired(_)));

            let path = lock.path().to_path_buf();
            drop(lock);
            assert!(!path.exists());
            assert!(matches!(acquire("/p").unwrap(), Acquire::Acquired(_)));
        });
    }

    #[test]
    fn stale_lock_is_replaced_and_fresh_lock_is_not() {
        with_temp_data_dir(|| {
            let path = lock_path("/p");
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, "project=/p\npid=0\n").unwrap();
            let old = SystemTime::now() - STALE_AFTER - Duration::from_secs(60);
            std::fs::File::options()
                .write(true)
                .open(&path)
                .unwrap()
                .set_modified(old)
                .unwrap();

            let lock = match acquire("/p").unwrap() {
                Acquire::Acquired(l) => l,
                other => panic!("stale lock should be replaced: {other:?}"),
            };
            let contents = std::fs::read_to_string(lock.path()).unwrap();
            assert!(contents.contains(&format!("pid={}\n", std::process::id())));
            // No `.stale-<pid>` leftovers.
            let leftovers: Vec<_> = std::fs::read_dir(lock_dir())
                .unwrap()
                .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
                .filter(|n| n.contains("stale"))
                .collect();
            assert!(leftovers.is_empty(), "{leftovers:?}");

            // Just under the threshold is still live.
            let nearly = SystemTime::now() - STALE_AFTER + Duration::from_secs(60);
            std::fs::File::options()
                .write(true)
                .open(lock.path())
                .unwrap()
                .set_modified(nearly)
                .unwrap();
            assert!(matches!(acquire("/p").unwrap(), Acquire::Held { .. }));
        });
    }

    #[test]
    fn lock_path_is_flat_and_stable() {
        with_temp_data_dir(|| {
            let a = lock_path("/home/me/my repo");
            assert_eq!(a, lock_path("/home/me/my repo"));
            assert_ne!(a, lock_path("/home/me/my-repo"));
            assert_eq!(a.parent().unwrap(), lock_dir());
            let name = a.file_name().unwrap().to_string_lossy();
            assert!(
                name.starts_with("index-") && name.ends_with(".lock"),
                "{name}"
            );
            assert_eq!(name.len(), "index-".len() + 16 + ".lock".len());
        });
    }
}
