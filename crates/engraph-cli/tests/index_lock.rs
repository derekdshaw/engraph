//! One index run per project: `engraph index` takes `<data dir>/locks/
//! index-<hash>.lock` for the project and removes it at the end, also after a
//! failure. A second run finding the file does not start — an explicit run
//! fails, `--if-changed` (the background refresh) skips — and a file older
//! than 30 minutes is a remnant of a run that never finished, so it is
//! cleared and the run proceeds.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, SystemTime};
use tempfile::tempdir;

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_engraph")
}

fn engraph(db: &Path, cwd: &Path, args: &[&str]) -> std::process::Output {
    Command::new(bin())
        .env_clear()
        .env("PATH", std::env::var("PATH").unwrap_or_default())
        .env("HOME", std::env::var("HOME").unwrap_or_default())
        .env("ENGRAPH_DB_PATH", db)
        .current_dir(cwd)
        .args(args)
        .output()
        .expect("spawn engraph")
}

fn text(out: &std::process::Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    )
}

fn git(repo: &Path, args: &[&str]) {
    let out = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args([
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@t",
            "-c",
            "commit.gpgsign=false",
        ])
        .args(args)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// A committed monorepo with cargo crates `svc-a` and `svc-b`, an empty
/// prebuilt SCIP, and the DB path (its directory is the lock directory's
/// parent).
fn mono() -> (tempfile::TempDir, PathBuf, PathBuf) {
    let dir = tempdir().unwrap();
    let mono = dir.path().join("mono");
    for svc in ["svc-a", "svc-b"] {
        std::fs::create_dir_all(mono.join(svc).join("src")).unwrap();
        std::fs::write(
            mono.join(svc).join("Cargo.toml"),
            format!("[package]\nname = \"{svc}\"\nversion = \"0.1.0\"\nedition = \"2021\"\n"),
        )
        .unwrap();
        std::fs::write(mono.join(svc).join("src/lib.rs"), "pub fn f() {}\n").unwrap();
    }
    git(&mono, &["init", "-q"]);
    git(&mono, &["add", "."]);
    git(&mono, &["commit", "-q", "-m", "init"]);
    std::fs::write(dir.path().join("empty.scip"), b"").unwrap();
    let mono = mono.canonicalize().unwrap();
    let db = dir.path().join("e.db");
    (dir, mono, db)
}

/// The lock file `engraph index` would take for `repo` with this DB.
fn lock_file(db: &Path, repo: &Path) -> PathBuf {
    let dir = db.parent().unwrap().join("locks");
    engraph_codegraph::lock::lock_path_in(&dir, &repo.to_string_lossy())
}

fn hold_lock(path: &Path, age: Duration) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, "project=test\npid=0\nstarted=2026-01-01T00:00:00Z\n").unwrap();
    std::fs::File::options()
        .write(true)
        .open(path)
        .unwrap()
        .set_modified(SystemTime::now() - age)
        .unwrap();
}

fn scip_args(dir: &Path) -> [String; 4] {
    let scip = dir.join("empty.scip").to_string_lossy().into_owned();
    ["index".into(), ".".into(), "--scip".into(), scip]
}

fn args(a: &[String]) -> Vec<&str> {
    a.iter().map(String::as_str).collect()
}

#[test]
fn a_run_takes_the_lock_and_releases_it_at_the_end() {
    let (dir, mono, db) = mono();
    let lock = lock_file(&db, &mono);
    let out = engraph(&db, &mono, &args(&scip_args(dir.path())));
    assert!(out.status.success(), "{}", text(&out));
    assert!(!lock.exists(), "lock left behind after a finished run");
    // Also after a run that fails: no manifest in an empty repo → no driver.
    let other = dir.path().join("other");
    std::fs::create_dir_all(&other).unwrap();
    git(&other, &["init", "-q"]);
    let other = other.canonicalize().unwrap();
    let out = engraph(&db, &other, &["index", "."]);
    assert!(!out.status.success());
    assert!(
        !lock_file(&db, &other).exists(),
        "lock left behind after a failed run"
    );
}

#[test]
fn a_held_lock_stops_a_second_run() {
    let (dir, mono, db) = mono();
    let scip = scip_args(dir.path());
    let out = engraph(&db, &mono, &args(&scip));
    assert!(out.status.success(), "{}", text(&out));
    let lock = lock_file(&db, &mono);
    hold_lock(&lock, Duration::from_secs(60));

    // An explicit run fails and says why.
    let out = engraph(&db, &mono, &args(&scip));
    assert!(!out.status.success());
    let t = text(&out);
    assert!(t.contains("already in progress"), "{t}");
    assert!(t.contains(&lock.to_string_lossy().into_owned()), "{t}");
    assert!(t.contains("2026-01-01T00:00:00Z"), "{t}");

    // The background refresh yields quietly: exit 0, nothing re-indexed.
    std::fs::write(mono.join("svc-a/src/lib.rs"), "pub fn f() { 1 }\n").unwrap();
    let out = engraph(&db, &mono, &["index", "--if-changed", "."]);
    assert!(out.status.success(), "{}", text(&out));
    let t = text(&out);
    assert!(
        t.starts_with("skipped: ") && t.contains("already in progress"),
        "{t}"
    );
    assert!(!t.contains("indexed "), "{t}");
    // A dry run needs no lock.
    let out = engraph(&db, &mono, &["index", "--if-changed", "--dry-run", "."]);
    assert!(text(&out).contains("would re-index"), "{}", text(&out));

    // A workspace run reports it per repo and indexes nothing.
    let out = engraph(&db, &mono, &["index", "--workspace", "."]);
    let t = text(&out);
    assert_eq!(t.matches("already in progress").count(), 2, "{t}");
    assert!(t.contains("0 repo(s) ok, 2 failed"), "{t}");

    // None of that touched the foreign lock.
    assert!(lock.exists());
    assert_eq!(
        std::fs::read_to_string(&lock).unwrap(),
        "project=test\npid=0\nstarted=2026-01-01T00:00:00Z\n"
    );
}

#[test]
fn a_lock_older_than_thirty_minutes_is_cleared() {
    let (dir, mono, db) = mono();
    let lock = lock_file(&db, &mono);
    hold_lock(
        &lock,
        engraph_codegraph::STALE_AFTER + Duration::from_secs(60),
    );
    let out = engraph(&db, &mono, &args(&scip_args(dir.path())));
    assert!(out.status.success(), "{}", text(&out));
    assert!(text(&out).contains("indexed "), "{}", text(&out));
    assert!(!lock.exists(), "the run's own lock is released at the end");
    let leftovers: Vec<_> = std::fs::read_dir(lock.parent().unwrap())
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert!(leftovers.is_empty(), "{leftovers:?}");

    // Younger than that, it is honoured.
    hold_lock(
        &lock,
        engraph_codegraph::STALE_AFTER - Duration::from_secs(60),
    );
    let out = engraph(&db, &mono, &args(&scip_args(dir.path())));
    assert!(!out.status.success());
    assert!(text(&out).contains("already in progress"), "{}", text(&out));
}
