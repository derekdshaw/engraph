//! Integration test for `engraph run` budget tracking. After a wrapped command
//! runs inside a session, `session_budget.used_tokens` for that session must
//! reflect the post-filter output size (what actually lands in Claude's context).

use std::process::Command;
use tempfile::tempdir;

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_engraph")
}

#[test]
fn wrapped_run_charges_session_budget() {
    let dir = tempdir().unwrap();
    let db = dir.path().join("budget.db");
    let session = "sess-run-1";

    // Warm up schema.
    Command::new(bin())
        .env_clear()
        .env("PATH", std::env::var("PATH").unwrap_or_default())
        .env("ENGRAPH_DB_PATH", &db)
        .args(["budget", "status", "--session-id", session])
        .output()
        .unwrap();

    // Wrap a small command. `echo` is portable; output is ASCII.
    let out = Command::new(bin())
        .env_clear()
        .env("PATH", std::env::var("PATH").unwrap_or_default())
        .env("ENGRAPH_DB_PATH", &db)
        .env("CLAUDE_SESSION_ID", session)
        .args(["run", "echo", "hello from engraph budget test"])
        .output()
        .expect("spawn engraph run");
    assert!(out.status.success(), "engraph run failed: {:?}", out);

    // Budget must have moved.
    let conn = rusqlite::Connection::open(&db).unwrap();
    let used: i64 = conn
        .query_row(
            "SELECT used_tokens FROM session_budget WHERE session_id = ?1",
            [session],
            |r| r.get(0),
        )
        .unwrap();
    assert!(used > 0, "expected non-zero used_tokens after `engraph run`, got {used}");

    // events table should also have the wrapped_cmd row.
    let count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM events WHERE kind = 'wrapped_cmd' AND session_id = ?1",
            [session],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(count, 1);
}

#[test]
fn wrapped_run_without_session_id_does_not_panic() {
    // Negative: no CLAUDE_SESSION_ID → budget path is skipped, but the command
    // and telemetry still complete successfully.
    let dir = tempdir().unwrap();
    let db = dir.path().join("no_sid.db");

    let out = Command::new(bin())
        .env_clear()
        .env("PATH", std::env::var("PATH").unwrap_or_default())
        .env("ENGRAPH_DB_PATH", &db)
        .args(["run", "echo", "no session"])
        .output()
        .expect("spawn engraph run");
    assert!(out.status.success());

    let conn = rusqlite::Connection::open(&db).unwrap();
    // No row should exist in session_budget because we never opened one.
    let n: i64 = conn
        .query_row("SELECT COUNT(*) FROM session_budget", [], |r| r.get(0))
        .unwrap();
    assert_eq!(n, 0, "no session id means no budget row");
}
