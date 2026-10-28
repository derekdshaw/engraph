//! The codegraph records the commit each project was indexed at:
//! - `engraph index` records the repo's HEAD commit

use std::path::{Path, PathBuf};
use std::process::Command;
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
