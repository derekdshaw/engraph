//! Integration tests for the memory-writer subcommands (`remember`, `bug`,
//! `save`) and their surfacing in the SessionStart brief + `engraph recall`.
//! Writers are driven with an explicit `--project /proj` so the stored key is
//! deterministic (no dependence on the test process's cwd).

use std::io::Write;
use std::process::{Command, Stdio};
use tempfile::tempdir;

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_engraph")
}

/// Run `engraph <args>` against `db`, feeding `stdin` if provided. Returns
/// (stdout, success).
fn run(db: &std::path::Path, args: &[&str], stdin: Option<&str>) -> (String, bool) {
    let mut child = Command::new(bin())
        .env_clear()
        .env("PATH", std::env::var("PATH").unwrap_or_default())
        .env("ENGRAPH_DB_PATH", db)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn engraph");
    if let Some(s) = stdin {
        child
            .stdin
            .as_mut()
            .unwrap()
            .write_all(s.as_bytes())
            .unwrap();
    }
    // Close stdin so commands that read to EOF (hook session-start) don't block.
    drop(child.stdin.take());
    let out = child.wait_with_output().unwrap();
    (
        String::from_utf8_lossy(&out.stdout).into_owned(),
        out.status.success(),
    )
}

fn brief(db: &std::path::Path) -> String {
    let (out, ok) = run(
        db,
        &["hook", "session-start"],
        Some(r#"{"cwd":"/proj","session_id":"s1"}"#),
    );
    assert!(ok, "session-start hook failed");
    out
}

#[test]
fn remember_then_brief_surfaces_rule() {
    let dir = tempdir().unwrap();
    let db = dir.path().join("m.db");
    let (out, ok) = run(
        &db,
        &["remember", "never force-push main", "--project", "/proj"],
        None,
    );
    assert!(ok, "remember failed: {out}");
    let b = brief(&db);
    assert!(b.contains("## do-not-repeat"), "brief missing section: {b}");
    assert!(
        b.contains("never force-push main"),
        "brief missing rule: {b}"
    );
}

#[test]
fn bug_surfaces_then_resolves_out_of_brief() {
    let dir = tempdir().unwrap();
    let db = dir.path().join("m.db");
    let (out, ok) = run(
        &db,
        &[
            "bug",
            "race in worker pool",
            "--content",
            "missing mutex",
            "--project",
            "/proj",
        ],
        None,
    );
    assert!(ok, "bug failed: {out}");
    // out: "logged bug <id> for /proj"
    let id = out
        .trim()
        .strip_prefix("logged bug ")
        .and_then(|s| s.split(" for ").next())
        .expect("could not parse bug id")
        .to_string();

    let b = brief(&db);
    assert!(b.contains("## open bugs"), "brief missing section: {b}");
    assert!(b.contains("race in worker pool"), "brief missing bug: {b}");

    let (out2, ok2) = run(&db, &["bug", "--resolve", &id], None);
    assert!(ok2, "resolve failed: {out2}");
    let b2 = brief(&db);
    assert!(
        !b2.contains("race in worker pool"),
        "resolved bug still in brief: {b2}"
    );
}

#[test]
fn save_then_brief_surfaces_decision() {
    let dir = tempdir().unwrap();
    let db = dir.path().join("m.db");
    let (out, ok) = run(
        &db,
        &[
            "save",
            "use rusqlite r2d2 pool",
            "--kind",
            "architecture",
            "--project",
            "/proj",
        ],
        None,
    );
    assert!(ok, "save failed: {out}");
    let b = brief(&db);
    assert!(b.contains("## decisions"), "brief missing section: {b}");
    assert!(
        b.contains("use rusqlite r2d2 pool"),
        "brief missing decision: {b}"
    );
}

#[test]
fn save_is_recallable_by_project() {
    let dir = tempdir().unwrap();
    let db = dir.path().join("m.db");
    let (out, ok) = run(
        &db,
        &["save", "PINEAPPLE sentinel decision", "--project", "/proj"],
        None,
    );
    assert!(ok, "save failed: {out}");
    // The scope-member wiring in the Save handler lets `recall --project` find it.
    let (hits, ok) = run(
        &db,
        &["recall", "PINEAPPLE", "--project", "/proj", "--json"],
        None,
    );
    assert!(ok, "recall failed: {hits}");
    assert!(
        hits.contains("context_item"),
        "recall did not return the saved context_item: {hits}"
    );
}

#[test]
fn resolve_unknown_bug_errors() {
    let dir = tempdir().unwrap();
    let db = dir.path().join("m.db");
    let (_out, ok) = run(&db, &["bug", "--resolve", "no-such-id"], None);
    assert!(!ok, "resolving an unknown bug id should fail");
}
