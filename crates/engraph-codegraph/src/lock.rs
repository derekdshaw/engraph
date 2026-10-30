//! One index run per project at a time.
//!
//! An index run locks a file for its project under `<data dir>/locks/` (next
//! to the database, so `ENGRAPH_DB_PATH` moves it too) with an OS file lock
//! (`File::try_lock` / `File::lock`). The OS releases the lock when the
//! process ends — also when it is killed or crashes — so a dead run never
//! blocks the next one, and no stale-file timeout is needed.
//!
//! The file is never deleted. Deleting it while another process waits on it
//! would let a third process create and lock a new file at the same path
//! while the waiter goes on to lock the old, unlinked one: two holders.

use anyhow::{Context, Result};
use sha2::{Digest, Sha256};
use std::fs::{File, TryLockError};
use std::io::{Seek, Write};
use std::path::{Path, PathBuf};

/// Held for the duration of an index run; the OS lock is released when this
/// (its open file) is dropped.
#[derive(Debug)]
pub struct IndexLock {
    path: PathBuf,
    _file: File,
}

impl IndexLock {
    pub fn path(&self) -> &Path {
        &self.path
    }
}

/// Outcome of [`acquire`].
#[derive(Debug)]
pub enum Acquire {
    Acquired(IndexLock),
    /// Another run holds the lock.
    Held {
        path: PathBuf,
        /// What the holder wrote: project, pid, start time. Empty where the
        /// platform's lock also blocks reads (Windows).
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
        "an index run for {project} is already in progress (lock {}, started {started})",
        path.display()
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

/// Take the index lock for `project` without waiting. `Held` when another run
/// has it; `Err` only for I/O trouble.
pub fn acquire(project: &str) -> Result<Acquire> {
    let (path, file) = open(project)?;
    match file.try_lock() {
        Ok(()) => Ok(Acquire::Acquired(claim(project, path, file))),
        Err(TryLockError::WouldBlock) => {
            let info = std::fs::read_to_string(&path).unwrap_or_default();
            Ok(Acquire::Held { path, info })
        }
        Err(TryLockError::Error(e)) => {
            Err(e).with_context(|| format!("lock {}", path.display()))
        }
    }
}

/// Take the index lock for `project`, waiting for a run that holds it to
/// finish. For explicit index runs: the user asked for this index, so it
/// queues behind a background refresh instead of failing.
pub fn acquire_wait(project: &str) -> Result<IndexLock> {
    match acquire(project)? {
        Acquire::Acquired(lock) => Ok(lock),
        Acquire::Held { path, info } => {
            tracing::warn!(
                "{}; waiting for it to finish",
                held_message(project, &path, &info)
            );
            let (path, file) = open(project)?;
            file.lock()
                .with_context(|| format!("lock {}", path.display()))?;
            Ok(claim(project, path, file))
        }
    }
}

/// The lock file, created if missing. Not truncated: until we hold the lock,
/// its contents belong to whoever does.
fn open(project: &str) -> Result<(PathBuf, File)> {
    let path = lock_path(project);
    std::fs::create_dir_all(path.parent().expect("lock path has a parent"))
        .with_context(|| format!("create lock dir for {}", path.display()))?;
    let file = File::options()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&path)
        .with_context(|| format!("open index lock {}", path.display()))?;
    Ok((path, file))
}

/// Record who holds the lock, for a run that finds it held. Best-effort: the
/// lock is the OS lock, not these contents.
fn claim(project: &str, path: PathBuf, mut file: File) -> IndexLock {
    let _ = file.set_len(0).and_then(|()| file.rewind()).and_then(|()| {
        write!(
            file,
            "project={project}\npid={}\nstarted={}\n",
            std::process::id(),
            chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
        )
    });
    IndexLock { path, _file: file }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

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

    fn acquired(project: &str) -> IndexLock {
        match acquire(project).unwrap() {
            Acquire::Acquired(l) => l,
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn acquire_is_exclusive_and_released_on_drop() {
        with_temp_data_dir(|| {
            let lock = acquired("/p");
            assert!(lock.path().starts_with(lock_dir()));
            let contents = std::fs::read_to_string(lock.path()).unwrap();
            assert!(contents.contains("project=/p\n"), "{contents}");
            assert!(
                contents.contains(&format!("pid={}\n", std::process::id())),
                "{contents}"
            );

            match acquire("/p").unwrap() {
                Acquire::Held { path, info } => {
                    assert_eq!(path, lock.path());
                    #[cfg(unix)]
                    assert_eq!(info, contents);
                    let _ = info;
                }
                other => panic!("{other:?}"),
            }
            // A different project is independent.
            let _q = acquired("/q");

            let path = lock.path().to_path_buf();
            drop(lock);
            // Released, and the file stays (see the module doc).
            assert!(path.exists());
            let _again = acquired("/p");
        });
    }

    #[test]
    fn a_leftover_file_without_a_lock_does_not_block() {
        with_temp_data_dir(|| {
            // What a killed run leaves: the file, but no OS lock.
            let path = lock_path("/p");
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, "project=/p\npid=0\nstarted=2026-01-01T00:00:00Z\n").unwrap();
            let lock = acquired("/p");
            let contents = std::fs::read_to_string(lock.path()).unwrap();
            assert!(!contents.contains("pid=0\n"), "{contents}");
        });
    }

    #[test]
    fn acquire_wait_blocks_until_the_holder_releases() {
        with_temp_data_dir(|| {
            let lock = acquired("/p");
            let hold = Duration::from_millis(300);
            let holder = std::thread::spawn(move || {
                std::thread::sleep(hold);
                drop(lock);
            });
            let start = Instant::now();
            let lock = acquire_wait("/p").unwrap();
            assert!(start.elapsed() >= hold / 2, "{:?}", start.elapsed());
            holder.join().unwrap();
            let contents = std::fs::read_to_string(lock.path()).unwrap();
            assert!(contents.contains("project=/p\n"), "{contents}");
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
