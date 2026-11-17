//! Integration tests for `engraph hook post-read`:
//! - file with indexed entities → emit additionalContext with symbol list
//! - file with no indexed entities → silent passthrough
//! - missing file_path → silent passthrough
//! - malformed JSON / empty stdin → silent passthrough (no crash)

use std::io::Write;
use std::process::{Command, Stdio};

mod common;
use common::{bin, git};
use tempfile::tempdir;

fn run_hook(db_path: &std::path::Path, file_path: &str) -> String {
    let payload = serde_json::json!({
        "tool_name": "Read",
        "tool_input": { "file_path": file_path }
    })
    .to_string();
    run_hook_raw(db_path, &payload)
}

/// Run the hook. A payload with a `file_path` makes the hook record its
/// telemetry in a detached child, so wait for that child's `read` event: the
/// tests then see it, and no child writes after the temp dir is gone.
fn run_hook_raw(db_path: &std::path::Path, payload: &str) -> String {
    let records = serde_json::from_str::<serde_json::Value>(payload)
        .ok()
        .and_then(|v| {
            v.pointer("/tool_input/file_path")
                .and_then(|s| s.as_str())
                .map(|s| !s.trim().is_empty())
        })
        .unwrap_or(false);
    let before = read_event_count(db_path);
    let out = run_engraph(db_path, &["hook", "post-read"], payload);
    if records {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
        while read_event_count(db_path) == before {
            assert!(
                std::time::Instant::now() < deadline,
                "the read event was not recorded"
            );
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
    }
    out
}

fn read_event_count(db: &std::path::Path) -> i64 {
    rusqlite::Connection::open(db)
        .unwrap()
        .query_row(
            "SELECT COUNT(*) FROM events WHERE feature = 'read'",
            [],
            |r| r.get(0),
        )
        .unwrap()
}

/// `engraph <args>` with `payload` on stdin; returns stdout.
fn run_engraph(db_path: &std::path::Path, args: &[&str], payload: &str) -> String {
    let mut child = Command::new(bin())
        .env_clear()
        .env("PATH", std::env::var("PATH").unwrap_or_default())
        .env("ENGRAPH_DB_PATH", db_path)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn engraph");
    child
        .stdin
        .as_mut()
        .unwrap()
        .write_all(payload.as_bytes())
        .unwrap();
    let out = child.wait_with_output().unwrap();
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn parse(out: &str) -> Option<serde_json::Value> {
    if out.trim().is_empty() {
        return None;
    }
    Some(serde_json::from_str(out.trim()).expect("hook emitted invalid JSON"))
}

fn db_dir() -> (tempfile::TempDir, std::path::PathBuf) {
    let dir = tempdir().unwrap();
    let db = dir.path().join("hook.db");
    Command::new(bin())
        .env_clear()
        .env("PATH", std::env::var("PATH").unwrap_or_default())
        .env("ENGRAPH_DB_PATH", &db)
        .args(["budget", "status", "--session-id", "warmup"])
        .output()
        .unwrap();
    (dir, db)
}

fn insert_entity(db: &std::path::Path, name: &str, file_path: &str, line_range: &str, sig: &str) {
    let conn = rusqlite::Connection::open(db).unwrap();
    let id = format!("scheme cargo testpkg 0.1.0 {file_path}/{name}().");
    conn.execute(
        "INSERT INTO entities (id, kind, name, project, file_path, line_range, signature)
         VALUES (?1, 'function', ?2, 'testproj', ?3, ?4, ?5)",
        rusqlite::params![id, name, file_path, line_range, sig],
    )
    .unwrap();
}

#[test]
fn indexed_file_emits_additional_context() {
    let (_t, db) = db_dir();
    insert_entity(
        &db,
        "process_order",
        "src/order.rs",
        "42-88",
        "fn process_order() -> Result<()>",
    );
    insert_entity(
        &db,
        "validate_order",
        "src/order.rs",
        "92-110",
        "fn validate_order()",
    );
    let out = run_hook(&db, "src/order.rs");
    let v = parse(&out).expect("expected JSON for indexed file");
    assert_eq!(
        v.pointer("/hookSpecificOutput/hookEventName")
            .and_then(|s| s.as_str()),
        Some("PostToolUse")
    );
    let ctx = v
        .pointer("/hookSpecificOutput/additionalContext")
        .and_then(|s| s.as_str())
        .expect("missing additionalContext");
    assert!(ctx.contains("Indexed symbols in src/order.rs"));
    assert!(ctx.contains("process_order"));
    assert!(ctx.contains("42-88"));
    assert!(ctx.contains("fn process_order"));
    assert!(ctx.contains("validate_order"));
    assert!(ctx.contains("engraph subgraph"));
}

#[test]
fn file_with_no_indexed_entities_passes_through() {
    let (_t, db) = db_dir();
    let out = run_hook(&db, "src/unindexed.rs");
    assert!(
        out.trim().is_empty(),
        "expected silent passthrough, got: {out}"
    );
}

#[test]
fn entity_without_signature_renders_without_dash_block() {
    let (_t, db) = db_dir();
    insert_entity(&db, "Foo", "src/types.rs", "10-15", "");
    let out = run_hook(&db, "src/types.rs");
    let v = parse(&out).expect("expected JSON");
    let ctx = v
        .pointer("/hookSpecificOutput/additionalContext")
        .and_then(|s| s.as_str())
        .unwrap();
    assert!(ctx.contains("`Foo` @ 10-15"));
    // No empty signature backticks.
    assert!(!ctx.contains("— ``"));
}

#[test]
fn missing_file_path_passes_through() {
    let (_t, db) = db_dir();
    let payload = serde_json::json!({"tool_name": "Read", "tool_input": {}}).to_string();
    let out = run_hook_raw(&db, &payload);
    assert!(out.trim().is_empty());
}

#[test]
fn malformed_json_passes_through() {
    let (_t, db) = db_dir();
    let out = run_hook_raw(&db, "{not valid");
    assert!(out.trim().is_empty());
}

#[test]
fn empty_stdin_passes_through() {
    let (_t, db) = db_dir();
    let out = run_hook_raw(&db, "");
    assert!(out.trim().is_empty());
}

/// A committed git repo at `<dir>/<name>` with `src/lib.rs`, canonicalized.
fn git_repo(dir: &std::path::Path, name: &str) -> std::path::PathBuf {
    let repo = dir.join(name);
    std::fs::create_dir_all(repo.join("src")).unwrap();
    std::fs::write(
        repo.join("src/lib.rs"),
        "fn widget_count() -> usize { 1 }\n",
    )
    .unwrap();
    git(&repo, &["init", "-q"]);
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-q", "-m", "init"]);
    repo.canonicalize().unwrap()
}

/// `widget_count` in `src/lib.rs`, indexed under `project`.
fn insert_in_project(db: &std::path::Path, project: &std::path::Path) {
    rusqlite::Connection::open(db)
        .unwrap()
        .execute(
            "INSERT INTO entities (id, kind, name, project, file_path, line_range, signature)
             VALUES ('scheme cargo t 0.1.0 widget_count().', 'function', 'widget_count', ?1,
                     'src/lib.rs', '1:1', 'fn widget_count() -> usize')",
            [project.to_str().unwrap()],
        )
        .unwrap();
}

fn context(out: &str) -> String {
    parse(out)
        .and_then(|v| {
            v.pointer("/hookSpecificOutput/additionalContext")
                .and_then(|s| s.as_str())
                .map(str::to_string)
        })
        .unwrap_or_default()
}

#[test]
fn absolute_path_in_an_indexed_repo_emits_context() {
    // Claude's Read tool always sends absolute paths.
    let (t, db) = db_dir();
    let repo = git_repo(t.path(), "repo");
    insert_in_project(&db, &repo);
    let out = run_hook(&db, repo.join("src/lib.rs").to_str().unwrap());
    assert!(context(&out).contains("widget_count"), "got: {out}");
}

#[test]
fn absolute_path_in_another_repo_with_the_same_relative_path_passes_through() {
    let (t, db) = db_dir();
    let indexed = git_repo(t.path(), "indexed");
    let other = git_repo(t.path(), "other");
    insert_in_project(&db, &indexed);
    let out = run_hook(&db, other.join("src/lib.rs").to_str().unwrap());
    assert!(out.trim().is_empty(), "expected passthrough, got: {out}");
}

#[test]
fn absolute_path_in_a_linked_worktree_emits_context() {
    let (t, db) = db_dir();
    let repo = git_repo(t.path(), "repo");
    let wt = t.path().join("wt");
    git(
        &repo,
        &["worktree", "add", "-q", "-b", "feat", wt.to_str().unwrap()],
    );
    insert_in_project(&db, &repo);
    let out = run_hook(&db, wt.join("src/lib.rs").to_str().unwrap());
    assert!(context(&out).contains("widget_count"), "got: {out}");
}

/// The `read` events in `db`: (session_id, filter_id, input_tokens, path_hash).
fn read_events(db: &std::path::Path) -> Vec<(Option<String>, String, i64, String)> {
    let conn = rusqlite::Connection::open(db).unwrap();
    let mut stmt = conn
        .prepare(
            "SELECT session_id, filter_id, input_tokens, path_hash FROM events
             WHERE kind = 'hook' AND feature = 'read' ORDER BY seq",
        )
        .unwrap();
    stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap()
}

/// A PostToolUse(Read) payload of a text read, in the shape Claude Code sends.
fn text_read_payload(session: &str, file_path: &str, content: &str) -> String {
    serde_json::json!({
        "session_id": session,
        "hook_event_name": "PostToolUse",
        "tool_name": "Read",
        "tool_input": { "file_path": file_path },
        "tool_response": {
            "type": "text",
            "file": {
                "filePath": file_path,
                "content": content,
                "numLines": 2,
                "startLine": 1,
                "totalLines": 2
            }
        }
    })
    .to_string()
}

#[test]
fn text_read_records_one_event_with_the_read_tokens() {
    let (_t, db) = db_dir();
    let content = "fn main() {\n    println!(\"hello\");\n}";
    run_hook_raw(&db, &text_read_payload("s1", "/repo/src/main.rs", content));
    let events = read_events(&db);
    assert_eq!(events.len(), 1, "got: {events:?}");
    let (session, kind, tokens, hash) = &events[0];
    assert_eq!(session.as_deref(), Some("s1"));
    assert_eq!(kind, "text");
    assert_eq!(*tokens, engraph_core::tokens::count(content) as i64);
    // A hash, not the path itself.
    assert_eq!(hash.len(), 64);
    assert!(hash.chars().all(|c| c.is_ascii_hexdigit()));
    assert!(!hash.contains("main.rs"));
}

#[test]
fn read_of_an_indexed_file_records_the_read_and_the_augment() {
    let (_t, db) = db_dir();
    insert_entity(
        &db,
        "process_order",
        "src/order.rs",
        "1-2",
        "fn process_order()",
    );
    let out = run_hook_raw(
        &db,
        &text_read_payload("s1", "src/order.rs", "fn process_order() {}"),
    );
    assert!(context(&out).contains("process_order"));
    assert_eq!(read_events(&db).len(), 1);
}

#[test]
fn reads_of_one_path_have_one_hash() {
    let (_t, db) = db_dir();
    run_hook_raw(&db, &text_read_payload("s1", "/repo/a.rs", "a"));
    run_hook_raw(&db, &text_read_payload("s1", "/repo/a.rs", "a"));
    run_hook_raw(&db, &text_read_payload("s1", "/repo/b.rs", "b"));
    let events = read_events(&db);
    assert_eq!(events.len(), 3);
    assert_eq!(events[0].3, events[1].3);
    assert_ne!(events[0].3, events[2].3);
}

#[test]
fn image_read_records_an_event_with_no_tokens() {
    let (_t, db) = db_dir();
    let payload = serde_json::json!({
        "session_id": "s1",
        "tool_name": "Read",
        "tool_input": { "file_path": "/repo/p.png" },
        "tool_response": {
            "type": "image",
            "file": { "base64": "iVBORw0KGgo=", "type": "image/png", "originalSize": 70 }
        }
    })
    .to_string();
    run_hook_raw(&db, &payload);
    let events = read_events(&db);
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].1, "image");
    assert_eq!(events[0].2, 0);
}

#[test]
fn notebook_read_counts_the_cell_sources() {
    let (_t, db) = db_dir();
    let payload = serde_json::json!({
        "session_id": "s1",
        "tool_name": "Read",
        "tool_input": { "file_path": "/repo/n.ipynb" },
        "tool_response": {
            "type": "notebook",
            "file": {
                "filePath": "/repo/n.ipynb",
                "cells": [
                    { "cellType": "code", "source": "print(1)", "cell_id": "cell-0" },
                    { "cellType": "markdown", "source": "# Title", "cell_id": "cell-1" }
                ]
            }
        }
    })
    .to_string();
    run_hook_raw(&db, &payload);
    let events = read_events(&db);
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].1, "notebook");
    let expected = engraph_core::tokens::count("print(1)") + engraph_core::tokens::count("# Title");
    assert_eq!(events[0].2, expected as i64);
}

#[test]
fn record_read_records_the_read_and_the_augment() {
    let (_t, db) = db_dir();
    let payload = serde_json::json!({
        "read": serde_json::from_str::<serde_json::Value>(
            &text_read_payload("s1", "/repo/a.rs", "fn a() {}")
        ).unwrap(),
        "augment": "Indexed symbols in /repo/a.rs:\n- `a` @ 1:1\n"
    })
    .to_string();
    let out = run_engraph(&db, &["hook", "record-read"], &payload);
    assert!(
        out.trim().is_empty(),
        "record-read prints nothing, got: {out}"
    );
    let events = read_events(&db);
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].2, engraph_core::tokens::count("fn a() {}") as i64);
    let augment: i64 = rusqlite::Connection::open(&db)
        .unwrap()
        .query_row(
            "SELECT output_tokens FROM events WHERE feature = 'read_augment'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        augment,
        engraph_core::tokens::count("Indexed symbols in /repo/a.rs:\n- `a` @ 1:1\n") as i64
    );
}

#[test]
fn read_of_an_indexed_file_records_the_augment_tokens() {
    let (_t, db) = db_dir();
    insert_entity(
        &db,
        "process_order",
        "src/order.rs",
        "1-2",
        "fn process_order()",
    );
    let out = run_hook_raw(
        &db,
        &text_read_payload("s1", "src/order.rs", "fn process_order() {}"),
    );
    let ctx = context(&out);
    // The augment event is written in the same child, before the read event.
    let augment: i64 = rusqlite::Connection::open(&db)
        .unwrap()
        .query_row(
            "SELECT output_tokens FROM events WHERE feature = 'read_augment'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(augment, engraph_core::tokens::count(&ctx) as i64);
}
