//! One index run per project: `engraph index` holds an OS lock on `<data
//! dir>/locks/index-<hash>.lock` for the project and releases it at the end,
//! also after a failure. A second run that finds it held waits if it is
//! explicit and skips if it is `--if-changed` (the background refresh). A
//! leftover file with no lock on it — what a killed run leaves — blocks
//! nothing.

use std::fs::File;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::Duration;
use tempfile::tempdir;

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_engraph")
}

fn command(db: &Path, cwd: &Path, args: &[&str]) -> Command {
    let mut c = Command::new(bin());
    c.env_clear()
        .env("PATH", std::env::var("PATH").unwrap_or_default())
        .env("HOME", std::env::var("HOME").unwrap_or_default())
        .env("ENGRAPH_DB_PATH", db)
        .current_dir(cwd)
        .args(args);
    c
}

fn engraph(db: &Path, cwd: &Path, args: &[&str]) -> std::process::Output {
    command(db, cwd, args).output().expect("spawn engraph")
}

/// `engraph` started in the background, for a run expected to wait.
fn spawn(db: &Path, cwd: &Path, args: &[&str]) -> Child {
    command(db, cwd, args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
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

const HOLDER: &str = "project=test\npid=0\nstarted=2026-01-01T00:00:00Z\n";

/// Lock `path` the way another index run would; held until dropped.
fn hold(path: &Path) -> File {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, HOLDER).unwrap();
    let f = File::options().read(true).write(true).open(path).unwrap();
    f.lock().unwrap();
    f
}

/// Whether nobody holds the OS lock on `path`.
fn is_free(path: &Path) -> bool {
    let f = File::options().read(true).write(true).open(path).unwrap();
    f.try_lock().is_ok()
}

fn scip_args(dir: &Path) -> [String; 4] {
    let scip = dir.join("empty.scip").to_string_lossy().into_owned();
    ["index".into(), ".".into(), "--scip".into(), scip]
}

fn args(a: &[String]) -> Vec<&str> {
    a.iter().map(String::as_str).collect()
}

#[test]
fn a_run_releases_the_lock_at_the_end() {
    let (dir, mono, db) = mono();
    let lock = lock_file(&db, &mono);
    let out = engraph(&db, &mono, &args(&scip_args(dir.path())));
    assert!(out.status.success(), "{}", text(&out));
    assert!(is_free(&lock), "lock still held after a finished run");
    // Also after a run that fails: no manifest in an empty repo → no driver.
    let other = dir.path().join("other");
    std::fs::create_dir_all(&other).unwrap();
    git(&other, &["init", "-q"]);
    let other = other.canonicalize().unwrap();
    let out = engraph(&db, &other, &["index", "."]);
    assert!(!out.status.success());
    let other_lock = lock_file(&db, &other);
    assert!(
        !other_lock.exists() || is_free(&other_lock),
        "lock still held after a failed run"
    );
}

#[test]
fn the_background_refresh_skips_a_held_lock() {
    let (dir, mono, db) = mono();
    let out = engraph(&db, &mono, &args(&scip_args(dir.path())));
    assert!(out.status.success(), "{}", text(&out));
    let lock = lock_file(&db, &mono);
    let _held = hold(&lock);

    std::fs::write(mono.join("svc-a/src/lib.rs"), "pub fn f() { 1 }\n").unwrap();
    let out = engraph(&db, &mono, &["index", "--if-changed", "."]);
    assert!(out.status.success(), "{}", text(&out));
    let t = text(&out);
    assert!(
        t.starts_with("skipped: ") && t.contains("already in progress"),
        "{t}"
    );
    assert!(t.contains(&lock.to_string_lossy().into_owned()), "{t}");
    // Windows locks block reads too, so only Unix can show the holder's start.
    #[cfg(unix)]
    assert!(t.contains("2026-01-01T00:00:00Z"), "{t}");
    assert!(!t.contains("indexed "), "{t}");
    // A dry run needs no lock.
    let out = engraph(&db, &mono, &["index", "--if-changed", "--dry-run", "."]);
    assert!(text(&out).contains("would re-index"), "{}", text(&out));
    // The holder's record is untouched.
    #[cfg(unix)]
    assert_eq!(std::fs::read_to_string(&lock).unwrap(), HOLDER);
}

#[test]
fn an_explicit_run_waits_for_a_held_lock() {
    let (dir, mono, db) = mono();
    let lock = lock_file(&db, &mono);
    let held = hold(&lock);

    let scip = scip_args(dir.path());
    let mut child = spawn(&db, &mono, &args(&scip));
    std::thread::sleep(Duration::from_millis(700));
    assert!(
        child.try_wait().unwrap().is_none(),
        "explicit run did not wait for the held lock"
    );
    drop(held);
    let out = child.wait_with_output().unwrap();
    assert!(out.status.success(), "{}", text(&out));
    let t = text(&out);
    assert!(t.contains("waiting for it to finish"), "{t}");
    assert!(t.contains("indexed "), "{t}");
    assert!(is_free(&lock));
}

#[test]
fn a_workspace_run_waits_once_per_project() {
    let (_dir, mono, db) = mono();
    let lock = lock_file(&db, &mono);
    let held = hold(&lock);
    // Both crates are sub-projects of one repo: one project, one lock.
    let child = spawn(&db, &mono, &["index", "--workspace", "."]);
    std::thread::sleep(Duration::from_millis(700));
    drop(held);
    let out = child.wait_with_output().unwrap();
    let t = text(&out);
    assert_eq!(t.matches("waiting for it to finish").count(), 1, "{t}");
    assert!(is_free(&lock));
}

#[test]
fn a_leftover_lock_file_does_not_block() {
    let (dir, mono, db) = mono();
    let lock = lock_file(&db, &mono);
    // What a killed run leaves behind: the file, with no lock on it.
    std::fs::create_dir_all(lock.parent().unwrap()).unwrap();
    std::fs::write(&lock, HOLDER).unwrap();
    let out = engraph(&db, &mono, &args(&scip_args(dir.path())));
    assert!(out.status.success(), "{}", text(&out));
    let t = text(&out);
    assert!(t.contains("indexed "), "{t}");
    assert!(!t.contains("waiting"), "{t}");
}
