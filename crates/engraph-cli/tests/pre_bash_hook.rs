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
    assert!(
        reason.contains("engraph run"),
        "missing suggestion in {reason}"
    );
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
fn env_prefix_is_preserved_in_rewrite() {
    let (_t, db) = db_dir();
    // `FOO=bar git log` peels the env prefix for classification and re-emits
    // it ahead of `engraph run` so the assignment lands in the child's env.
    let out = run_hook(&db, "GIT_PAGER=cat git log -n 5");
    let v = parse(&out).expect("expected rewrite JSON");
    assert_eq!(
        v.pointer("/hookSpecificOutput/permissionDecision")
            .and_then(|s| s.as_str()),
        Some("allow")
    );
    let updated = v
        .pointer("/hookSpecificOutput/updatedInput/command")
        .and_then(|s| s.as_str())
        .expect("missing updatedInput.command");
    assert_eq!(updated, "GIT_PAGER=cat engraph run git log -n 5");
}

#[test]
fn sudo_prefix_is_passthrough() {
    let (_t, db) = db_dir();
    // sudo would run engraph as root with a different $HOME — passthrough.
    let out = run_hook(&db, "sudo git log -n 5");
    assert_eq!(out.trim(), "", "sudo should pass through: {out}");
}

#[test]
fn env_command_prefix_is_passthrough() {
    let (_t, db) = db_dir();
    // `env FOO=bar cmd` — non-trivial flag parsing; passthrough.
    let out = run_hook(&db, "env FOO=bar git log");
    assert_eq!(out.trim(), "", "env should pass through: {out}");
}

#[test]
fn absolute_path_argv0_is_normalized() {
    let (_t, db) = db_dir();
    // `/usr/bin/git log` should classify the same as `git log`.
    let out = run_hook(&db, "/usr/bin/git log -n 5");
    let v = parse(&out).expect("expected rewrite JSON");
    let updated = v
        .pointer("/hookSpecificOutput/updatedInput/command")
        .and_then(|s| s.as_str())
        .unwrap();
    assert_eq!(updated, "engraph run git log -n 5");
}

#[test]
fn git_dash_capital_c_is_preserved_in_rewrite() {
    let (_t, db) = db_dir();
    // `git -C <path> status` classifies as `git status`, but the `-C <path>`
    // must survive into the wrapped command — dropping it would silently run
    // against cwd instead of the target repo.
    let out = run_hook(&db, "git -C /tmp/x status");
    let v = parse(&out).expect("expected rewrite JSON");
    let updated = v
        .pointer("/hookSpecificOutput/updatedInput/command")
        .and_then(|s| s.as_str())
        .unwrap();
    assert_eq!(updated, "engraph run git -C /tmp/x status");
}

#[test]
fn git_lowercase_c_with_value_is_preserved_in_rewrite() {
    let (_t, db) = db_dir();
    // Same invariant for `-c <k=v>`: the global option must reach the wrapped
    // command. (`shell_words::quote` may wrap the `k=v` token in quotes; assert
    // the value is present and nothing is dropped rather than its exact quoting.)
    let out = run_hook(&db, "git -c color.ui=always log");
    let v = parse(&out).expect("expected rewrite JSON");
    let updated = v
        .pointer("/hookSpecificOutput/updatedInput/command")
        .and_then(|s| s.as_str())
        .unwrap();
    assert!(
        updated.starts_with("engraph run git -c ") && updated.ends_with(" log"),
        "-c global opt dropped from rewrite: {updated}"
    );
    assert!(
        updated.contains("color.ui=always"),
        "-c value dropped from rewrite: {updated}"
    );
}

#[test]
fn cat_is_rewritten_through_engraph_run() {
    let (_t, db) = db_dir();
    let out = run_hook(&db, "cat src/main.rs");
    let v = parse(&out).expect("expected rewrite JSON");
    let updated = v
        .pointer("/hookSpecificOutput/updatedInput/command")
        .and_then(|s| s.as_str())
        .unwrap();
    assert_eq!(updated, "engraph run cat src/main.rs");
}

#[test]
fn head_with_flags_is_rewritten() {
    let (_t, db) = db_dir();
    let out = run_hook(&db, "head -n 100 foo.py");
    let v = parse(&out).expect("expected rewrite JSON");
    let updated = v
        .pointer("/hookSpecificOutput/updatedInput/command")
        .and_then(|s| s.as_str())
        .unwrap();
    assert_eq!(updated, "engraph run head -n 100 foo.py");
}

#[test]
fn heredoc_command_is_passthrough() {
    let (_t, db) = db_dir();
    // `cat <<'EOF' ... EOF` would be corrupted by any rewrite — passthrough.
    let out = run_hook(&db, "cat <<'EOF'\nhello\nEOF");
    assert_eq!(out.trim(), "", "heredoc should pass through: {out}");
}

// --- Subgraph redirect for rg/grep ----------------------------------------

fn insert_entity(db: &std::path::Path, name: &str) {
    let conn = rusqlite::Connection::open(db).unwrap();
    let id = format!("scheme cargo testpkg 0.1.0 {name}().");
    conn.execute(
        "INSERT INTO entities (id, kind, name, project, file_path, line_range, signature)
         VALUES (?1, 'function', ?2, 'testproj', 'src/lib.rs', '1:10', 'fn ()')",
        rusqlite::params![id, name],
    )
    .unwrap();
}

#[test]
fn rg_on_indexed_symbol_redirects_to_subgraph() {
    let (_t, db) = db_dir();
    insert_entity(&db, "auth_handler");
    let out = run_hook(&db, "rg auth_handler");
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
        reason.contains("engraph subgraph auth_handler"),
        "missing subgraph hint: {reason}"
    );
}

#[test]
fn grep_on_indexed_symbol_redirects_to_subgraph() {
    let (_t, db) = db_dir();
    insert_entity(&db, "process_event");
    // `-r` flag before the pattern; first non-flag should still be picked.
    let out = run_hook(&db, "grep -r process_event .");
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
    assert!(reason.contains("engraph subgraph process_event"));
}

#[test]
fn rg_on_unindexed_pattern_still_rewrites_for_compression() {
    let (_t, db) = db_dir();
    // No entity inserted. Falls through to the compression rewrite path.
    let out = run_hook(&db, "rg unindexed_helper");
    let v = parse(&out).expect("expected rewrite JSON");
    assert_eq!(
        v.pointer("/hookSpecificOutput/permissionDecision")
            .and_then(|s| s.as_str()),
        Some("allow")
    );
    let updated = v
        .pointer("/hookSpecificOutput/updatedInput/command")
        .and_then(|s| s.as_str())
        .unwrap();
    assert_eq!(updated, "engraph run rg unindexed_helper");
}

#[test]
fn absolute_path_rg_on_indexed_symbol_redirects() {
    let (_t, db) = db_dir();
    insert_entity(&db, "validate_token");
    // /usr/bin/rg should normalize to rg and trigger the subgraph redirect.
    let out = run_hook(&db, "/usr/bin/rg validate_token");
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
    assert!(reason.contains("engraph subgraph validate_token"));
}

#[test]
fn rg_on_regex_pattern_still_rewrites_for_compression() {
    let (_t, db) = db_dir();
    insert_entity(&db, "process");
    // Regex metachar → not a single-symbol lookup. Don't redirect; compress.
    let out = run_hook(&db, "rg process.*");
    let v = parse(&out).expect("expected rewrite JSON");
    assert_eq!(
        v.pointer("/hookSpecificOutput/permissionDecision")
            .and_then(|s| s.as_str()),
        Some("allow")
    );
}
