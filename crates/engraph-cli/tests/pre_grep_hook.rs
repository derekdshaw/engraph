//! Integration tests for `engraph hook pre-grep`:
//! - bareword + 1-8 matches in entities → deny + suggest
//! - bareword + 0 matches → passthrough
//! - bareword + 9+ matches (ambiguous) → passthrough
//! - regex pattern → passthrough
//! - too-short pattern → passthrough
//! - malformed/empty stdin → passthrough (no crash)

use std::io::Write;
use std::process::{Command, Stdio};
use tempfile::tempdir;

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_engraph")
}

fn run_hook(db_path: &std::path::Path, pattern: &str) -> String {
    let payload = serde_json::json!({
        "tool_name": "Grep",
        "tool_input": { "pattern": pattern }
    })
    .to_string();
    let mut child = Command::new(bin())
        .env_clear()
        .env("PATH", std::env::var("PATH").unwrap_or_default())
        .env("ENGRAPH_DB_PATH", db_path)
        .args(["hook", "pre-grep"])
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

fn run_hook_raw(db_path: &std::path::Path, payload: &str) -> String {
    let mut child = Command::new(bin())
        .env_clear()
        .env("PATH", std::env::var("PATH").unwrap_or_default())
        .env("ENGRAPH_DB_PATH", db_path)
        .args(["hook", "pre-grep"])
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
    // Warm up: any subcommand opens the pool, which runs migrations and
    // creates the `entities` table.
    Command::new(bin())
        .env_clear()
        .env("PATH", std::env::var("PATH").unwrap_or_default())
        .env("ENGRAPH_DB_PATH", &db)
        .args(["budget", "status", "--session-id", "warmup"])
        .output()
        .unwrap();
    (dir, db)
}

fn insert_entities(db: &std::path::Path, name: &str, count: usize) {
    let conn = rusqlite::Connection::open(db).unwrap();
    for i in 0..count {
        let id = format!("scheme cargo testpkg 0.1.0 mod{i}/{name}().");
        conn.execute(
            "INSERT INTO entities (id, kind, name, project, file_path, line_range, signature)
             VALUES (?1, 'function', ?2, 'testproj', 'src/lib.rs', '1:10', 'fn ()')",
            rusqlite::params![id, name],
        )
        .unwrap();
    }
}

#[test]
fn bareword_with_single_match_is_denied() {
    let (_t, db) = db_dir();
    insert_entities(&db, "run_migrations", 1);
    let out = run_hook(&db, "run_migrations");
    let v = parse(&out).expect("expected deny JSON");
    assert_eq!(
        v.pointer("/hookSpecificOutput/permissionDecision")
            .and_then(|s| s.as_str()),
        Some("deny")
    );
    let reason = v
        .pointer("/hookSpecificOutput/permissionDecisionReason")
        .and_then(|s| s.as_str())
        .unwrap();
    assert!(
        reason.contains("engraph subgraph run_migrations"),
        "missing subgraph suggestion in {reason}"
    );
}

#[test]
fn bareword_with_three_matches_is_denied() {
    let (_t, db) = db_dir();
    insert_entities(&db, "process_order", 3);
    let out = run_hook(&db, "process_order");
    let v = parse(&out).expect("expected deny JSON");
    assert_eq!(
        v.pointer("/hookSpecificOutput/permissionDecision")
            .and_then(|s| s.as_str()),
        Some("deny")
    );
}

#[test]
fn bareword_with_no_matches_passes_through() {
    let (_t, db) = db_dir();
    let out = run_hook(&db, "unindexed_symbol");
    assert!(
        out.trim().is_empty(),
        "expected silent passthrough, got: {out}"
    );
}

#[test]
fn bareword_within_widened_window_is_denied() {
    // The redirect window is 1..=8; a symbol with 6 matches now redirects
    // (it was passthrough under the old 1..=3 window).
    let (_t, db) = db_dir();
    insert_entities(&db, "mid_pop_symbol", 6);
    let out = run_hook(&db, "mid_pop_symbol");
    let v = parse(&out).expect("expected deny JSON");
    assert_eq!(
        v.pointer("/hookSpecificOutput/permissionDecision")
            .and_then(|s| s.as_str()),
        Some("deny")
    );
}

#[test]
fn ambiguous_bareword_passes_through() {
    // 9+ matches is too ambiguous to be a useful neighborhood — keep grepping.
    let (_t, db) = db_dir();
    insert_entities(&db, "foo_bar", 9);
    let out = run_hook(&db, "foo_bar");
    assert!(
        out.trim().is_empty(),
        "ambiguous should passthrough, got: {out}"
    );
}

#[test]
fn regex_pattern_passes_through() {
    let (_t, db) = db_dir();
    insert_entities(&db, "process", 1);
    // Has a regex metachar — not a single-symbol lookup. Don't deny.
    let out = run_hook(&db, "process.*");
    assert!(
        out.trim().is_empty(),
        "regex should passthrough, got: {out}"
    );
}

#[test]
fn short_pattern_passes_through() {
    let (_t, db) = db_dir();
    insert_entities(&db, "id", 1);
    let out = run_hook(&db, "id");
    assert!(
        out.trim().is_empty(),
        "short pattern should passthrough, got: {out}"
    );
}

#[test]
fn multiword_pattern_passes_through() {
    let (_t, db) = db_dir();
    insert_entities(&db, "foo", 1);
    let out = run_hook(&db, "foo bar");
    assert!(
        out.trim().is_empty(),
        "multi-word should passthrough, got: {out}"
    );
}

#[test]
fn malformed_json_passes_through() {
    let (_t, db) = db_dir();
    let out = run_hook_raw(&db, "{not valid json");
    assert!(
        out.trim().is_empty(),
        "malformed should passthrough, got: {out}"
    );
}

#[test]
fn empty_stdin_passes_through() {
    let (_t, db) = db_dir();
    let out = run_hook_raw(&db, "");
    assert!(
        out.trim().is_empty(),
        "empty should passthrough, got: {out}"
    );
}

/// A `Config` type with a `new` method, plus many other `new` methods, so the
/// bare name is ambiguous (> 8) while the qualified name is unique.
fn insert_qualified_entities(db: &std::path::Path) {
    let conn = rusqlite::Connection::open(db).unwrap();
    conn.execute(
        "INSERT INTO entities (id, kind, name, qualified_name, project, file_path, line_range)
         VALUES ('rust-analyzer cargo fd 0.1.0 config/Config#', 'symbol', 'Config', 'Config',
                 'testproj', 'src/config.rs', '14:40'),
                ('rust-analyzer cargo fd 0.1.0 config/Config#new().', 'symbol', 'new', 'Config.new',
                 'testproj', 'src/config.rs', '20:25')",
        [],
    )
    .unwrap();
    for i in 0..10 {
        conn.execute(
            "INSERT INTO entities (id, kind, name, qualified_name, project, file_path, line_range)
             VALUES (?1, 'symbol', 'new', ?2, 'testproj', 'src/other.rs', '1:1')",
            rusqlite::params![
                format!("rust-analyzer cargo fd 0.1.0 other/Type{i}#new()."),
                format!("Type{i}.new")
            ],
        )
        .unwrap();
    }
}

fn decision(out: &str) -> Option<String> {
    parse(out)?
        .pointer("/hookSpecificOutput/permissionDecision")
        .and_then(|s| s.as_str())
        .map(str::to_string)
}

#[test]
fn qualified_path_shapes_resolve_by_qualified_name() {
    let (_t, db) = db_dir();
    insert_qualified_entities(&db);
    // The bare name is too ambiguous: the search continues.
    assert_eq!(decision(&run_hook(&db, "new")), None);
    // Every qualified shape of the same symbol finds exactly it.
    for pattern in ["Config::new", "Config#new", "Config.new"] {
        let out = run_hook(&db, pattern);
        assert_eq!(decision(&out).as_deref(), Some("deny"), "{pattern}: {out}");
        let reason = parse(&out)
            .unwrap()
            .pointer("/hookSpecificOutput/permissionDecisionReason")
            .and_then(|s| s.as_str())
            .unwrap()
            .to_string();
        assert!(reason.contains("(1 match)"), "{pattern}: {reason}");
        assert!(
            reason.contains(&format!("engraph subgraph {pattern}")),
            "{pattern}: {reason}"
        );
    }
    // A qualified path with no qualified match falls back to the last
    // segment's name: `Other::Config` → `Config`, one match.
    assert_eq!(
        decision(&run_hook(&db, "Other::Config")).as_deref(),
        Some("deny")
    );
    // A type name with no `#`.
    assert_eq!(decision(&run_hook(&db, "Config")).as_deref(), Some("deny"));
}

#[test]
fn global_symbols_shadow_locals_in_the_match_count() {
    let (_t, db) = db_dir();
    let conn = rusqlite::Connection::open(&db).unwrap();
    // One field plus ten local variables of the same name: 1 match, not 11.
    conn.execute(
        "INSERT INTO entities (id, kind, name, qualified_name, project, file_path, line_range)
         VALUES ('ra config/Config#full_path_base.', 'symbol', 'full_path_base',
                 'Config.full_path_base', 'testproj', 'src/config.rs', '17:17')",
        [],
    )
    .unwrap();
    for i in 0..10 {
        conn.execute(
            "INSERT INTO entities (id, kind, name, project, file_path, line_range)
             VALUES (?1, 'symbol', 'full_path_base', 'testproj', 'src/walk.rs', '1:1')",
            [format!("local {i}")],
        )
        .unwrap();
    }
    drop(conn);
    let out = run_hook(&db, "full_path_base");
    assert_eq!(decision(&out).as_deref(), Some("deny"), "{out}");
    assert!(out.contains("(1 match)"), "{out}");
}
