//! Integration tests for the SessionStart hook (Phase 4):
//! - Empty project / empty budget → empty additionalContext
//! - Project with do-not-repeat / bugs / decisions → brief contains them
//! - Brief stays under MAX_BRIEF_BYTES

use std::io::Write;
use std::process::{Command, Stdio};
use tempfile::tempdir;

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_engraph")
}

fn run_hook_with_stdin(db_path: &std::path::Path, stdin: &str) -> String {
    let mut child = Command::new(bin())
        .env_clear()
        .env("PATH", std::env::var("PATH").unwrap_or_default())
        .env("ENGRAPH_DB_PATH", db_path)
        .args(["hook", "session-start"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn engraph");
    child
        .stdin
        .as_mut()
        .unwrap()
        .write_all(stdin.as_bytes())
        .unwrap();
    let out = child.wait_with_output().unwrap();
    String::from_utf8_lossy(&out.stdout).into_owned()
}

#[test]
fn empty_project_emits_empty_additional_context() {
    let dir = tempdir().unwrap();
    let db = dir.path().join("sh.db");
    let out = run_hook_with_stdin(&db, r#"{"cwd":"/some/unknown/path","session_id":"s1"}"#);
    let v: serde_json::Value = serde_json::from_str(out.trim()).unwrap();
    let body = v
        .pointer("/hookSpecificOutput/additionalContext")
        .and_then(|c| c.as_str())
        .unwrap();
    assert_eq!(body, "", "expected empty brief on unknown project, got: {body}");
}

#[test]
fn populated_project_includes_rules_and_bugs() {
    let dir = tempdir().unwrap();
    let db = dir.path().join("sh.db");
    // Touch the DB once via the budget command so the schema exists.
    Command::new(bin())
        .args(["budget", "status", "--session-id", "s_init"])
        .env("ENGRAPH_DB_PATH", &db)
        .output()
        .unwrap();

    // Seed do-not-repeat + bug rows directly.
    let conn = rusqlite::Connection::open(&db).unwrap();
    conn.execute(
        "INSERT INTO do_not_repeat (id, project, rule) VALUES ('dnr1','/proj','no force-push')",
        [],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO bugs (id, project, summary, resolved) VALUES ('b1','/proj','race in worker', 0)",
        [],
    )
    .unwrap();

    let out = run_hook_with_stdin(&db, r#"{"cwd":"/proj","session_id":"s2"}"#);
    let v: serde_json::Value = serde_json::from_str(out.trim()).unwrap();
    let body = v
        .pointer("/hookSpecificOutput/additionalContext")
        .and_then(|c| c.as_str())
        .unwrap();
    assert!(body.contains("# engraph brief — /proj"));
    assert!(body.contains("no force-push"));
    assert!(body.contains("race in worker"));
}

#[test]
fn brief_respects_max_bytes() {
    let dir = tempdir().unwrap();
    let db = dir.path().join("sh.db");
    Command::new(bin())
        .args(["budget", "status", "--session-id", "s_init"])
        .env("ENGRAPH_DB_PATH", &db)
        .output()
        .unwrap();
    let conn = rusqlite::Connection::open(&db).unwrap();
    for i in 0..100 {
        conn.execute(
            "INSERT INTO do_not_repeat (id, project, rule) VALUES (?1, '/big', ?2)",
            rusqlite::params![format!("d{i}"), format!("rule number {i} that takes a fair number of characters")],
        )
        .unwrap();
    }
    let out = run_hook_with_stdin(&db, r#"{"cwd":"/big"}"#);
    let v: serde_json::Value = serde_json::from_str(out.trim()).unwrap();
    let body = v
        .pointer("/hookSpecificOutput/additionalContext")
        .and_then(|c| c.as_str())
        .unwrap();
    assert!(body.len() <= 2048, "brief {} > 2048 bytes", body.len());
}
