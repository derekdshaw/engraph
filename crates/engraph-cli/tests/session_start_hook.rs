//! Integration tests for the SessionStart hook (Phase 4):
//! - Empty project / empty budget → empty additionalContext
//! - Project with do-not-repeat / bugs → brief contains them
//! - Brief stays under MAX_BRIEF_BYTES

use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};
use tempfile::tempdir;

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_engraph")
}

fn run_hook_with_stdin(db_path: &std::path::Path, stdin: &str) -> String {
    run_hook_with_args_and_env(db_path, &[], stdin, &[])
}

fn run_hook_with_args_and_env(
    db_path: &std::path::Path,
    extra_args: &[&str],
    stdin: &str,
    envs: &[(&str, &Path)],
) -> String {
    let mut child = Command::new(bin())
        .env_clear()
        .env("PATH", std::env::var("PATH").unwrap_or_default())
        .env("ENGRAPH_DB_PATH", db_path)
        .args(["hook", "session-start"])
        .args(extra_args)
        .envs(envs.iter().map(|(k, v)| (*k, v)))
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

fn init_db(db: &Path) {
    Command::new(bin())
        .args(["budget", "status", "--session-id", "s_init"])
        .env("ENGRAPH_DB_PATH", db)
        .output()
        .unwrap();
}

fn write_codex_rollout(path: &Path, session_id: &str, cwd: &Path, user: &str, assistant: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let cwd = cwd.to_string_lossy();
    let lines = [
        serde_json::json!({
            "timestamp": "2026-06-29T10:00:00.000Z",
            "type": "session_meta",
            "payload": {"id": session_id, "cwd": cwd, "git": {"branch": "main"}},
        })
        .to_string(),
        serde_json::json!({
            "timestamp": "2026-06-29T10:00:01.000Z",
            "type": "response_item",
            "payload": {
                "type": "message",
                "role": "user",
                "content": [{"type": "input_text", "text": user}],
            },
        })
        .to_string(),
        serde_json::json!({
            "timestamp": "2026-06-29T10:00:02.000Z",
            "type": "response_item",
            "payload": {
                "type": "message",
                "role": "assistant",
                "content": [{"type": "output_text", "text": assistant}],
            },
        })
        .to_string(),
    ];
    std::fs::write(path, format!("{}\n", lines.join("\n"))).unwrap();
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
    assert_eq!(
        body, "",
        "expected empty brief on unknown project, got: {body}"
    );
}

#[test]
fn populated_project_includes_rules_and_bugs() {
    let dir = tempdir().unwrap();
    let db = dir.path().join("sh.db");
    init_db(&db);

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
    init_db(&db);
    let conn = rusqlite::Connection::open(&db).unwrap();
    for i in 0..100 {
        conn.execute(
            "INSERT INTO do_not_repeat (id, project, rule) VALUES (?1, '/big', ?2)",
            rusqlite::params![
                format!("d{i}"),
                format!("rule number {i} that takes a fair number of characters")
            ],
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

#[test]
fn codex_empty_project_emits_empty_system_message() {
    let dir = tempdir().unwrap();
    let db = dir.path().join("sh.db");
    let out = run_hook_with_args_and_env(
        &db,
        &["--client", "codex"],
        r#"{"cwd":"/some/unknown/path","session_id":"cx1"}"#,
        &[],
    );
    let v: serde_json::Value = serde_json::from_str(out.trim()).unwrap();
    let body = v
        .pointer("/systemMessage")
        .and_then(|c| c.as_str())
        .unwrap();
    assert_eq!(body, "");
    assert!(v.pointer("/hookSpecificOutput").is_none());
}

#[test]
fn codex_populated_project_emits_system_message() {
    let dir = tempdir().unwrap();
    let db = dir.path().join("sh.db");
    let project = dir.path().join("project");
    std::fs::create_dir_all(&project).unwrap();
    init_db(&db);

    let project_key = project
        .canonicalize()
        .unwrap()
        .to_string_lossy()
        .into_owned();
    let conn = rusqlite::Connection::open(&db).unwrap();
    conn.execute(
        "INSERT INTO do_not_repeat (id, project, rule) VALUES ('dnr1', ?1, 'keep the Codex brief short')",
        rusqlite::params![project_key],
    )
    .unwrap();

    let stdin = serde_json::json!({"cwd": project, "session_id": "cx2"}).to_string();
    let out = run_hook_with_args_and_env(&db, &["--client", "codex"], &stdin, &[]);
    let v: serde_json::Value = serde_json::from_str(out.trim()).unwrap();
    let body = v
        .pointer("/systemMessage")
        .and_then(|c| c.as_str())
        .unwrap();
    assert!(body.contains("keep the Codex brief short"));
    assert!(v.pointer("/hookSpecificOutput").is_none());
}

#[test]
fn codex_catch_up_recurses_filters_project_and_is_idempotent() {
    let dir = tempdir().unwrap();
    let db = dir.path().join("sh.db");
    let codex_home = dir.path().join("codex-home");
    let project = dir.path().join("project");
    let other_project = dir.path().join("other");
    std::fs::create_dir_all(&project).unwrap();
    std::fs::create_dir_all(&other_project).unwrap();

    write_codex_rollout(
        &codex_home.join("sessions/2026/06/29/project.jsonl"),
        "cx-project",
        &project,
        "remember this project",
        "stored project answer",
    );
    write_codex_rollout(
        &codex_home.join("sessions/2026/06/28/other.jsonl"),
        "cx-other",
        &other_project,
        "wrong project",
        "wrong answer",
    );
    write_codex_rollout(
        &codex_home.join("sessions/2026/06/27/subagents/nested.jsonl"),
        "cx-subagent",
        &project,
        "nested project",
        "nested answer",
    );

    let stdin = serde_json::json!({"cwd": project, "session_id": "cx-current"}).to_string();
    let envs = [("CODEX_HOME", codex_home.as_path())];
    run_hook_with_args_and_env(&db, &["--client", "codex"], &stdin, &envs);

    let conn = rusqlite::Connection::open(&db).unwrap();
    let count: i64 = conn
        .query_row("SELECT COUNT(*) FROM messages", [], |row| row.get(0))
        .unwrap();
    assert_eq!(count, 2);

    run_hook_with_args_and_env(&db, &["--client", "codex"], &stdin, &envs);
    let count_after_rerun: i64 = conn
        .query_row("SELECT COUNT(*) FROM messages", [], |row| row.get(0))
        .unwrap();
    assert_eq!(count_after_rerun, 2);

    let body: String = conn
        .query_row(
            "SELECT group_concat(content, ' ') FROM messages",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert!(body.contains("remember this project"));
    assert!(!body.contains("wrong project"));
    assert!(!body.contains("nested project"));
}
