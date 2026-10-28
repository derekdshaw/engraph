//! The codegraph records the commit each project was indexed at, and lookups
//! from a worktree whose copy of an indexed file has moved on stop trusting the
//! graph for that file:
//! - `engraph index` records the repo's HEAD commit
//! - pre-grep redirect: unchanged file → deny; changed file → passthrough
//! - subgraph: changed file → a "changed since the index" note

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

/// A git repo with `src/lib.rs` (holding `widget_count`) and `src/other.rs`
/// committed, plus an empty prebuilt SCIP to index it with.
fn repo() -> (tempfile::TempDir, PathBuf) {
    let dir = tempdir().unwrap();
    let repo = dir.path().join("repo");
    std::fs::create_dir_all(repo.join("src")).unwrap();
    std::fs::write(
        repo.join("src/lib.rs"),
        "fn widget_count() -> usize { 1 }\n",
    )
    .unwrap();
    std::fs::write(repo.join("src/other.rs"), "fn other() {}\n").unwrap();
    git(&repo, &["init", "-q"]);
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-q", "-m", "init"]);
    std::fs::write(dir.path().join("empty.scip"), b"").unwrap();
    let repo = repo.canonicalize().unwrap();
    (dir, repo)
}

/// Index `repo` (empty SCIP), then insert `widget_count` in `src/lib.rs` the way
/// a real index would, keyed by the repo path.
fn index_with_symbol(dir: &Path, repo: &Path, db: &Path) {
    let scip = dir.join("empty.scip");
    let out = engraph(db, repo, &["index", ".", "--scip", scip.to_str().unwrap()]);
    assert!(
        out.status.success(),
        "index: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let conn = rusqlite::Connection::open(db).unwrap();
    conn.execute(
        "INSERT INTO entities (id, kind, name, project, file_path, line_range, signature)
         VALUES ('scheme cargo t 0.1.0 widget_count().', 'function', 'widget_count', ?1,
                 'src/lib.rs', '1:1', 'fn widget_count() -> usize')",
        [repo.to_str().unwrap()],
    )
    .unwrap();
}

fn pre_grep(db: &Path, cwd: &Path, pattern: &str) -> String {
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
    String::from_utf8_lossy(&child.wait_with_output().unwrap().stdout).into_owned()
}

#[test]
fn index_records_head_commit() {
    let (dir, repo) = repo();
    let db = dir.path().join("e.db");
    index_with_symbol(dir.path(), &repo, &db);
    let conn = rusqlite::Connection::open(&db).unwrap();
    let sha: String = conn
        .query_row(
            "SELECT commit_sha FROM index_state WHERE project = ?1",
            [repo.to_str().unwrap()],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(sha, git(&repo, &["rev-parse", "HEAD"]));
}

fn decision(out: &str) -> Option<String> {
    if out.trim().is_empty() {
        return None;
    }
    let v: serde_json::Value = serde_json::from_str(out.trim()).expect("hook emitted invalid JSON");
    v.pointer("/hookSpecificOutput/permissionDecision")
        .and_then(|s| s.as_str())
        .map(str::to_string)
}

#[test]
fn unchanged_file_still_redirects() {
    let (dir, repo) = repo();
    let db = dir.path().join("e.db");
    index_with_symbol(dir.path(), &repo, &db);
    assert_eq!(
        decision(&pre_grep(&db, &repo, "widget_count")).as_deref(),
        Some("deny")
    );
}

#[test]
fn edit_to_another_file_still_redirects() {
    let (dir, repo) = repo();
    let db = dir.path().join("e.db");
    index_with_symbol(dir.path(), &repo, &db);
    std::fs::write(repo.join("src/other.rs"), "fn other() { todo!() }\n").unwrap();
    assert_eq!(
        decision(&pre_grep(&db, &repo, "widget_count")).as_deref(),
        Some("deny")
    );
}

#[test]
fn uncommitted_edit_to_the_symbols_file_passes_through() {
    let (dir, repo) = repo();
    let db = dir.path().join("e.db");
    index_with_symbol(dir.path(), &repo, &db);
    std::fs::write(
        repo.join("src/lib.rs"),
        "fn widget_count() -> usize { 2 }\n",
    )
    .unwrap();
    assert_eq!(decision(&pre_grep(&db, &repo, "widget_count")), None);
}

#[test]
fn linked_worktree_branch_that_changed_the_file_passes_through() {
    let (dir, repo) = repo();
    let db = dir.path().join("e.db");
    index_with_symbol(dir.path(), &repo, &db);
    let wt = dir.path().join("wt");
    git(
        &repo,
        &["worktree", "add", "-q", "-b", "feat", wt.to_str().unwrap()],
    );
    std::fs::write(
        wt.join("src/lib.rs"),
        "fn widget_count(n: usize) -> usize { n }\n",
    )
    .unwrap();
    git(&wt, &["commit", "-q", "-am", "change widget_count"]);
    // The agent in the worktree greps; the main checkout still matches the graph.
    assert_eq!(decision(&pre_grep(&db, &wt, "widget_count")), None);
    assert_eq!(
        decision(&pre_grep(&db, &repo, "widget_count")).as_deref(),
        Some("deny")
    );
}

#[test]
fn unknown_indexed_commit_passes_through() {
    let (dir, repo) = repo();
    let db = dir.path().join("e.db");
    index_with_symbol(dir.path(), &repo, &db);
    rusqlite::Connection::open(&db)
        .unwrap()
        .execute(
            "UPDATE index_state SET commit_sha = '0123456789abcdef0123456789abcdef01234567'",
            [],
        )
        .unwrap();
    assert_eq!(decision(&pre_grep(&db, &repo, "widget_count")), None);
}

#[test]
fn bash_rg_in_a_changed_worktree_passes_through() {
    let (dir, repo) = repo();
    let db = dir.path().join("e.db");
    index_with_symbol(dir.path(), &repo, &db);
    std::fs::write(
        repo.join("src/lib.rs"),
        "fn widget_count() -> usize { 2 }\n",
    )
    .unwrap();
    let payload = serde_json::json!({
        "tool_name": "Bash",
        "cwd": repo,
        "tool_input": { "command": "rg widget_count" }
    })
    .to_string();
    let mut child = Command::new(bin())
        .env_clear()
        .env("PATH", std::env::var("PATH").unwrap_or_default())
        .env("ENGRAPH_DB_PATH", &db)
        .args(["hook", "pre-bash"])
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
    assert_ne!(decision(&out).as_deref(), Some("deny"), "got: {out}");
}

#[test]
fn subgraph_flags_a_file_changed_since_the_index() {
    let (dir, repo) = repo();
    let db = dir.path().join("e.db");
    index_with_symbol(dir.path(), &repo, &db);
    let clean = engraph(&db, &repo, &["subgraph", "widget_count"]);
    let clean = String::from_utf8_lossy(&clean.stdout);
    assert!(clean.contains("widget_count"), "{clean}");
    assert!(!clean.contains("Changed since the index"), "{clean}");

    std::fs::write(
        repo.join("src/lib.rs"),
        "fn widget_count() -> usize { 2 }\n",
    )
    .unwrap();
    let out = engraph(&db, &repo, &["subgraph", "widget_count"]);
    let out = String::from_utf8_lossy(&out.stdout);
    assert!(out.contains("Changed since the index"), "{out}");
    assert!(out.contains("`src/lib.rs`"), "{out}");
}

#[test]
fn indexing_a_dirty_tree_does_not_flag_the_files_it_indexed() {
    let (dir, repo) = repo();
    let db = dir.path().join("e.db");
    // Edit, then index without committing: the graph reflects the edit.
    std::fs::write(
        repo.join("src/lib.rs"),
        "fn widget_count() -> usize { 2 }\n",
    )
    .unwrap();
    index_with_symbol(dir.path(), &repo, &db);
    assert_eq!(
        decision(&pre_grep(&db, &repo, "widget_count")).as_deref(),
        Some("deny")
    );
    let out = engraph(&db, &repo, &["subgraph", "widget_count"]);
    let out = String::from_utf8_lossy(&out.stdout);
    assert!(!out.contains("Changed since the index"), "{out}");
    // A later edit still counts.
    std::fs::write(
        repo.join("src/lib.rs"),
        "fn widget_count() -> usize { 3 }\n",
    )
    .unwrap();
    assert_eq!(decision(&pre_grep(&db, &repo, "widget_count")), None);
}
