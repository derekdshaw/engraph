//! Integration tests for `engraph hook post-read`:
//! - file with indexed entities → emit additionalContext with symbol list
//! - file with no indexed entities → silent passthrough
//! - missing file_path → silent passthrough
//! - malformed JSON / empty stdin → silent passthrough (no crash)

use std::io::Write;
use std::process::{Command, Stdio};
use tempfile::tempdir;

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_engraph")
}

fn run_hook(db_path: &std::path::Path, file_path: &str) -> String {
    let payload = serde_json::json!({
        "tool_name": "Read",
        "tool_input": { "file_path": file_path }
    })
    .to_string();
    run_hook_raw(db_path, &payload)
}

fn run_hook_raw(db_path: &std::path::Path, payload: &str) -> String {
    let mut child = Command::new(bin())
        .env_clear()
        .env("PATH", std::env::var("PATH").unwrap_or_default())
        .env("ENGRAPH_DB_PATH", db_path)
        .args(["hook", "post-read"])
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

fn git(repo: &std::path::Path, args: &[&str]) {
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
