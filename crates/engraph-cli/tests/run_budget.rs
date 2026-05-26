//! Integration tests for `engraph run`:
//! - Budget tracking: post-filter token count is charged to session_budget when
//!   CLAUDE_SESSION_ID is set; no row exists when it isn't.
//! - tokio::process migration: stdin is inherited (cat round-trips piped input),
//!   and large concurrent stdout+stderr drains without deadlock.

use std::io::Write;
use std::process::{Command, Stdio};
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
    assert!(
        used > 0,
        "expected non-zero used_tokens after `engraph run`, got {used}"
    );

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
fn wrapped_run_inherits_stdin() {
    // Positive: tokio::process should wire stdin through to the child. Pipe
    // a known phrase into `cat` and assert it shows up in the wrapped output.
    let dir = tempdir().unwrap();
    let db = dir.path().join("stdin.db");
    let phrase = "tokio-stdin-inheritance-marker-7f3a";

    let mut child = Command::new(bin())
        .env_clear()
        .env("PATH", std::env::var("PATH").unwrap_or_default())
        .env("ENGRAPH_DB_PATH", &db)
        .args(["run", "cat"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn engraph run cat");
    child
        .stdin
        .as_mut()
        .unwrap()
        .write_all(phrase.as_bytes())
        .unwrap();
    // Close stdin so `cat` sees EOF and exits.
    drop(child.stdin.take());
    let out = child.wait_with_output().expect("wait");
    assert!(out.status.success(), "engraph run cat failed: {out:?}");
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(
        text.contains(phrase),
        "stdin marker missing from wrapped output: {text}"
    );
}

#[test]
fn wrapped_run_drains_large_concurrent_output_without_deadlock() {
    // Negative regression: with std::process::Command::output() this pattern
    // is safe (the runtime drains both pipes), but the migration to tokio
    // needs to preserve concurrent draining. Use a shell command that writes
    // a lot to BOTH stdout and stderr so a single-pipe drain would fill a
    // pipe buffer (typically 64KB on Linux) and deadlock.
    let dir = tempdir().unwrap();
    let db = dir.path().join("drain.db");

    let out = Command::new(bin())
        .env_clear()
        .env("PATH", std::env::var("PATH").unwrap_or_default())
        .env("ENGRAPH_DB_PATH", &db)
        .args([
            "run",
            "sh",
            "-c",
            // 200000 chars each on stdout and stderr (~200KB > 64KB pipe buffer).
            "yes a | head -c 200000; yes b | head -c 200000 1>&2",
        ])
        .output()
        .expect("spawn engraph run sh");
    assert!(
        out.status.success(),
        "engraph run sh exited non-zero (likely deadlock or stream drop): {:?}",
        out.status
    );
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
