//! Integration tests for `engraph hook pre-bash` (v2 auto-rewrite):
//! - rewrite-eligible commands → allow + updatedInput
//! - compound commands → deny + suggestion
//! - unknown commands → passthrough (empty stdout)
//! - already-wrapped → passthrough (recursion guard)
//! - quoted args preserved through the rewrite

use std::io::Write;
use std::process::{Command, Stdio};
use tempfile::tempdir;

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_engraph")
}

fn run_hook(db_path: &std::path::Path, command: &str) -> String {
    let payload = serde_json::json!({
        "tool_name": "Bash",
        "tool_input": { "command": command }
    })
    .to_string();
    let mut child = Command::new(bin())
        .env_clear()
        .env("PATH", std::env::var("PATH").unwrap_or_default())
        .env("ENGRAPH_DB_PATH", db_path)
        .args(["hook", "pre-bash"])
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
    // Pre-init schema via any other subcommand.
    Command::new(bin())
        .env_clear()
        .env("PATH", std::env::var("PATH").unwrap_or_default())
        .env("ENGRAPH_DB_PATH", &db)
        .args(["budget", "status", "--session-id", "warmup"])
        .output()
        .unwrap();
    (dir, db)
}

#[test]
fn simple_command_is_auto_rewritten() {
    let (_t, db) = db_dir();
    let out = run_hook(&db, "git log -n 5");
    let v = parse(&out).expect("expected JSON for rewrite");
    assert_eq!(
        v.pointer("/hookSpecificOutput/permissionDecision")
            .and_then(|s| s.as_str()),
        Some("allow")
    );
    let updated = v
        .pointer("/hookSpecificOutput/updatedInput/command")
        .and_then(|s| s.as_str())
        .expect("missing updatedInput.command");
    assert_eq!(updated, "engraph run git log -n 5");
}

#[test]
fn quoted_args_are_preserved() {
    let (_t, db) = db_dir();
    // Single-quoted arg with a space inside.
    let out = run_hook(&db, "git log --grep='fix the bug'");
    let v = parse(&out).expect("expected rewrite JSON");
    let updated = v
        .pointer("/hookSpecificOutput/updatedInput/command")
        .and_then(|s| s.as_str())
        .unwrap();
    // shell-words re-quotes the arg; the inner string is preserved.
    // shell-words re-quotes the whole arg since it contains spaces; the
    // resulting single-quoted form preserves the inner string verbatim.
    assert!(
        updated.contains("'--grep=fix the bug'"),
        "quoted arg lost: {updated}"
    );
    assert!(updated.starts_with("engraph run git log "));
}

#[test]
fn compound_command_falls_back_to_deny() {
    let (_t, db) = db_dir();
    let out = run_hook(&db, "cd /tmp && git log -n 5");
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
    assert!(reason.contains("engraph run"), "missing suggestion in {reason}");
}

#[test]
fn piped_command_falls_back_to_deny() {
    let (_t, db) = db_dir();
    let out = run_hook(&db, "git log -n 5 | head");
    let v = parse(&out).expect("expected deny JSON");
    assert_eq!(
        v.pointer("/hookSpecificOutput/permissionDecision")
            .and_then(|s| s.as_str()),
        Some("deny")
    );
}

#[test]
fn meta_inside_single_quotes_does_not_trigger_deny() {
    let (_t, db) = db_dir();
    // `&&` lives inside a single-quoted arg — must NOT be treated as compound.
    let out = run_hook(&db, "git log --grep='foo && bar'");
    let v = parse(&out).expect("expected rewrite JSON");
    assert_eq!(
        v.pointer("/hookSpecificOutput/permissionDecision")
            .and_then(|s| s.as_str()),
        Some("allow")
    );
}

#[test]
fn unknown_command_is_passthrough() {
    let (_t, db) = db_dir();
    let out = run_hook(&db, "some-totally-unknown-tool arg1 arg2");
    assert_eq!(out.trim(), "", "expected empty stdout, got: {out}");
}

#[test]
fn already_wrapped_is_passthrough() {
    let (_t, db) = db_dir();
    let out = run_hook(&db, "engraph run git log -n 5");
    assert_eq!(out.trim(), "", "recursion guard failed: {out}");
}

#[test]
fn env_prefix_is_passthrough() {
    let (_t, db) = db_dir();
    // `FOO=bar git log` would lose the env if we wrapped, so we don't.
    let out = run_hook(&db, "GIT_PAGER=cat git log -n 5");
    assert_eq!(out.trim(), "", "env-prefix should pass through: {out}");
}
