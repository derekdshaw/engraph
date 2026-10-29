//! Indexing sub-projects of a monorepo one at a time:
//! - a relative sub-project path indexes that sub-project (not an empty index)
//! - each sub-project gets its own key, so indexing one doesn't prune another
//! - a linked worktree's sub-project shares the main checkout's key
//! - branch divergence works for sub-project keys (paths prefixed by the
//!   sub-project's path in the repo)

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use tempfile::tempdir;

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_engraph")
}

fn engraph(db: &Path, cwd: &Path, args: &[&str]) -> std::process::Output {
    Command::new(bin())
        .env_clear()
        .env("PATH", std::env::var("PATH").unwrap_or_default())
        .env("HOME", std::env::var("HOME").unwrap_or_default())
        .env("ENGRAPH_DB_PATH", db)
        .env(
            "ENGRAPH_SCIP_OUTPUT_DIR",
            std::env::temp_dir().join("engraph-test-scip"),
        )
        .current_dir(cwd)
        .args(args)
        .output()
        .expect("spawn engraph")
}

fn git(repo: &Path, args: &[&str]) -> String {
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
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

/// A git monorepo with two Rust crates whose files share a relative path:
/// `svc-a/src/lib.rs` (`alpha_handler`) and `svc-b/src/lib.rs` (`beta_handler`).
fn mono() -> (tempfile::TempDir, PathBuf) {
    let dir = tempdir().unwrap();
    let mono = dir.path().join("mono");
    for (svc, f) in [("svc-a", "alpha_handler"), ("svc-b", "beta_handler")] {
        std::fs::create_dir_all(mono.join(svc).join("src")).unwrap();
        std::fs::write(
            mono.join(svc).join("Cargo.toml"),
            format!("[package]\nname = \"{svc}\"\nversion = \"0.1.0\"\nedition = \"2021\"\n"),
        )
        .unwrap();
        std::fs::write(
            mono.join(svc).join("src/lib.rs"),
            format!("pub fn {f}() -> u32 {{ 1 }}\n"),
        )
        .unwrap();
    }
    std::fs::write(mono.join(".gitignore"), "target/\n").unwrap();
    git(&mono, &["init", "-q"]);
    git(&mono, &["add", "."]);
    git(&mono, &["commit", "-q", "-m", "init"]);
    std::fs::write(dir.path().join("empty.scip"), b"").unwrap();
    let mono = mono.canonicalize().unwrap();
    (dir, mono)
}

fn key(p: &Path) -> String {
    p.to_string_lossy().into_owned()
}

/// `engraph index <sub> --scip empty.scip` from `cwd`, then insert `name` in
/// `src/lib.rs` under `project` the way a real index would.
fn index_sub(dir: &Path, db: &Path, cwd: &Path, sub: &str, project: &str, name: &str) {
    let scip = dir.join("empty.scip");
    let out = engraph(db, cwd, &["index", sub, "--scip", scip.to_str().unwrap()]);
    assert!(
        out.status.success(),
        "index: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    rusqlite::Connection::open(db)
        .unwrap()
        .execute(
            "INSERT INTO entities (id, kind, name, project, file_path, line_range, signature)
             VALUES (?1, 'symbol', ?2, ?3, 'src/lib.rs', '1:1', 'fn ()')",
            rusqlite::params![format!("scheme cargo t 0.1.0 {name}()."), name, project],
        )
        .unwrap();
}

fn index_state_keys(db: &Path) -> Vec<String> {
    let conn = rusqlite::Connection::open(db).unwrap();
    let mut stmt = conn
        .prepare("SELECT project FROM index_state ORDER BY project")
        .unwrap();
    stmt.query_map([], |r| r.get(0))
        .unwrap()
        .map(Result::unwrap)
        .collect()
}

fn entity_count(db: &Path, name: &str) -> i64 {
    rusqlite::Connection::open(db)
        .unwrap()
        .query_row(
            "SELECT COUNT(*) FROM entities WHERE name = ?1",
            [name],
            |r| r.get(0),
        )
        .unwrap()
}

fn pre_grep(db: &Path, cwd: &Path, pattern: &str) -> Option<String> {
    let payload = serde_json::json!({
        "tool_name": "Grep",
        "cwd": cwd,
        "tool_input": { "pattern": pattern }
    })
    .to_string();
    let mut child = Command::new(bin())
        .env_clear()
        .env("PATH", std::env::var("PATH").unwrap_or_default())
        .env("ENGRAPH_DB_PATH", db)
        .args(["hook", "pre-grep"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    child
        .stdin
        .as_mut()
        .unwrap()
        .write_all(payload.as_bytes())
        .unwrap();
    let out = String::from_utf8_lossy(&child.wait_with_output().unwrap().stdout).into_owned();
    if out.trim().is_empty() {
        return None;
    }
    let v: serde_json::Value = serde_json::from_str(out.trim()).unwrap();
    v.pointer("/hookSpecificOutput/permissionDecision")
        .and_then(|s| s.as_str())
        .map(str::to_string)
}

#[test]
fn relative_subproject_path_indexes_the_subproject() {
    let probe = Command::new("rust-analyzer").arg("--version").output();
    if !probe.map(|o| o.status.success()).unwrap_or(false) {
        eprintln!("skip: rust-analyzer not installed");
        return;
    }
    let (dir, mono) = mono();
    let db = dir.path().join("e.db");
    let out = engraph(&db, &mono, &["index", "svc-a"]);
    assert!(
        out.status.success(),
        "index: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(
        entity_count(&db, "alpha_handler"),
        1,
        "{}",
        String::from_utf8_lossy(&out.stdout)
    );
}

#[test]
fn subprojects_get_their_own_keys_and_survive_each_other() {
    let (dir, mono) = mono();
    let db = dir.path().join("e.db");
    let (a, b) = (key(&mono.join("svc-a")), key(&mono.join("svc-b")));
    index_sub(dir.path(), &db, &mono, "svc-a", &a, "alpha_handler");
    index_sub(dir.path(), &db, &mono, "svc-b", &b, "beta_handler");
    assert_eq!(index_state_keys(&db), vec![a, b]);
    // Indexing svc-b must not prune svc-a's symbols.
    assert_eq!(entity_count(&db, "alpha_handler"), 1);
}

#[test]
fn repo_root_index_keeps_the_repo_key() {
    let (dir, mono) = mono();
    let db = dir.path().join("e.db");
    index_sub(dir.path(), &db, &mono, ".", &key(&mono), "alpha_handler");
    assert_eq!(index_state_keys(&db), vec![key(&mono)]);
}

#[test]
fn linked_worktree_subproject_shares_the_main_checkouts_key() {
    let (dir, mono) = mono();
    let db = dir.path().join("e.db");
    let wt = dir.path().join("wt");
    git(
        &mono,
        &["worktree", "add", "-q", "-b", "feat", wt.to_str().unwrap()],
    );
    let a = key(&mono.join("svc-a"));
    index_sub(dir.path(), &db, &wt, "svc-a", &a, "alpha_handler");
    assert_eq!(index_state_keys(&db), vec![a]);
}

#[test]
fn divergence_checks_the_subprojects_own_file() {
    let (dir, mono) = mono();
    let db = dir.path().join("e.db");
    let a = key(&mono.join("svc-a"));
    index_sub(dir.path(), &db, &mono, "svc-a", &a, "alpha_handler");
    assert_eq!(
        pre_grep(&db, &mono, "alpha_handler").as_deref(),
        Some("deny")
    );
    // Same relative path, other sub-project: not alpha_handler's file.
    std::fs::write(
        mono.join("svc-b/src/lib.rs"),
        "pub fn beta_handler() -> u32 { 2 }\n",
    )
    .unwrap();
    assert_eq!(
        pre_grep(&db, &mono, "alpha_handler").as_deref(),
        Some("deny")
    );
    std::fs::write(
        mono.join("svc-a/src/lib.rs"),
        "pub fn alpha_handler() -> u32 { 2 }\n",
    )
    .unwrap();
    assert_eq!(pre_grep(&db, &mono, "alpha_handler"), None);
    assert_eq!(pre_grep(&db, &mono.join("svc-a"), "alpha_handler"), None);
}

#[test]
fn divergence_for_a_subproject_in_a_linked_worktree() {
    let (dir, mono) = mono();
    let db = dir.path().join("e.db");
    let a = key(&mono.join("svc-a"));
    index_sub(dir.path(), &db, &mono, "svc-a", &a, "alpha_handler");
    let wt = dir.path().join("wt");
    git(
        &mono,
        &["worktree", "add", "-q", "-b", "feat", wt.to_str().unwrap()],
    );
    std::fs::write(
        wt.join("svc-a/src/lib.rs"),
        "pub fn alpha_handler(n: u32) -> u32 { n }\n",
    )
    .unwrap();
    git(&wt, &["commit", "-q", "-am", "change alpha_handler"]);
    assert_eq!(pre_grep(&db, &wt.join("svc-a"), "alpha_handler"), None);
    assert_eq!(
        pre_grep(&db, &mono, "alpha_handler").as_deref(),
        Some("deny")
    );
}

#[test]
fn subgraph_note_names_the_repo_relative_path() {
    let (dir, mono) = mono();
    let db = dir.path().join("e.db");
    let a = key(&mono.join("svc-a"));
    index_sub(dir.path(), &db, &mono, "svc-a", &a, "alpha_handler");
    std::fs::write(
        mono.join("svc-a/src/lib.rs"),
        "pub fn alpha_handler() -> u32 { 2 }\n",
    )
    .unwrap();
    let out = engraph(&db, &mono, &["subgraph", "alpha_handler"]);
    let out = String::from_utf8_lossy(&out.stdout);
    assert!(out.contains("Changed since the index"), "{out}");
    assert!(out.contains("`svc-a/src/lib.rs`"), "{out}");
}

#[test]
fn divergence_covers_workspace_indexed_subprojects() {
    let probe = Command::new("rust-analyzer").arg("--version").output();
    if !probe.map(|o| o.status.success()).unwrap_or(false) {
        eprintln!("skip: rust-analyzer not installed");
        return;
    }
    let (dir, mono) = mono();
    let db = dir.path().join("e.db");
    let out = engraph(&db, &mono, &["index", "--workspace", "."]);
    assert!(
        out.status.success(),
        "index: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(
        pre_grep(&db, &mono, "alpha_handler").as_deref(),
        Some("deny")
    );
    std::fs::write(
        mono.join("svc-a/src/lib.rs"),
        "pub fn alpha_handler() -> u32 { 2 }\n",
    )
    .unwrap();
    assert_eq!(pre_grep(&db, &mono, "alpha_handler"), None);
}
