//! Indexing sub-projects of a monorepo: each run refreshes its directory of the
//! one repo index (repo key, repo-relative paths), so runs don't prune each
//! other, a root index and a member index converge, linked worktrees share the
//! key, and branch divergence checks the sub-project's own files. Tests that
//! need a real indexer skip when rust-analyzer isn't installed.

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
/// `<sub>/src/lib.rs` under the repo's project the way a real index would.
fn index_sub(dir: &Path, db: &Path, cwd: &Path, mono: &Path, sub: &str, name: &str) {
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
             VALUES (?1, 'symbol', ?2, ?3, ?4, '1:1', 'fn ()')",
            rusqlite::params![
                format!("scheme cargo t 0.1.0 {name}()."),
                name,
                key(mono),
                format!("{sub}/src/lib.rs")
            ],
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

/// `(project, file_path)` of the entity named `name`.
fn entity(db: &Path, name: &str) -> (String, String) {
    rusqlite::Connection::open(db)
        .unwrap()
        .query_row(
            "SELECT project, file_path FROM entities WHERE name = ?1",
            [name],
            |r| Ok((r.get(0)?, r.get(1)?)),
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

fn rust_analyzer_present() -> bool {
    let probe = Command::new("rust-analyzer").arg("--version").output();
    let ok = probe.map(|o| o.status.success()).unwrap_or(false);
    if !ok {
        eprintln!("skip: rust-analyzer not installed");
    }
    ok
}

#[test]
fn subprojects_indexed_one_at_a_time_merge_into_the_repo_index() {
    if !rust_analyzer_present() {
        return;
    }
    let (dir, mono) = mono();
    let db = dir.path().join("e.db");
    // Relative paths, from the repo root: each run indexes only its crate.
    for sub in ["svc-a", "svc-b"] {
        let out = engraph(&db, &mono, &["index", sub]);
        assert!(
            out.status.success(),
            "index {sub}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
    // svc-b's run (GC and prune on) leaves svc-a's symbols in place.
    assert_eq!(
        entity(&db, "alpha_handler"),
        (key(&mono), "svc-a/src/lib.rs".into())
    );
    assert_eq!(
        entity(&db, "beta_handler"),
        (key(&mono), "svc-b/src/lib.rs".into())
    );
}

#[test]
fn subproject_index_from_a_linked_worktree_uses_the_repo_key() {
    let (dir, mono) = mono();
    let db = dir.path().join("e.db");
    let wt = dir.path().join("wt");
    git(
        &mono,
        &["worktree", "add", "-q", "-b", "feat", wt.to_str().unwrap()],
    );
    index_sub(dir.path(), &db, &wt, &mono, "svc-a", "alpha_handler");
    assert_eq!(index_state_keys(&db), vec![key(&mono)]);
}

#[test]
fn divergence_checks_the_subprojects_own_file() {
    let (dir, mono) = mono();
    let db = dir.path().join("e.db");
    index_sub(dir.path(), &db, &mono, &mono, "svc-a", "alpha_handler");
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
    index_sub(dir.path(), &db, &mono, &mono, "svc-a", "alpha_handler");
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
    index_sub(dir.path(), &db, &mono, &mono, "svc-a", "alpha_handler");
    std::fs::write(
        mono.join("svc-a/src/lib.rs"),
        "pub fn alpha_handler() -> u32 { 2 }\n",
    )
    .unwrap();
    let out = engraph(&db, &mono, &["subgraph", "alpha_handler"]);
    let out = String::from_utf8_lossy(&out.stdout);
    assert!(out.contains("Changed since the index"), "{out}");
    assert!(out.contains("`svc-a/src/lib.rs`"), "{out}");
    assert!(out.contains("defined in svc-a/src/lib.rs"), "{out}");
}

#[test]
fn workspace_run_merges_subprojects_and_tracks_divergence() {
    if !rust_analyzer_present() {
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
        entity(&db, "alpha_handler"),
        (key(&mono), "svc-a/src/lib.rs".into())
    );
    assert_eq!(
        entity(&db, "beta_handler"),
        (key(&mono), "svc-b/src/lib.rs".into())
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

#[test]
fn root_workspace_and_member_indexes_converge() {
    if !rust_analyzer_present() {
        return;
    }
    let (dir, mono) = mono();
    std::fs::write(
        mono.join("Cargo.toml"),
        "[workspace]\nmembers = [\"svc-a\", \"svc-b\"]\nresolver = \"2\"\n",
    )
    .unwrap();
    let db = dir.path().join("e.db");
    let counts = || -> (i64, i64) {
        let conn = rusqlite::Connection::open(&db).unwrap();
        let n = |sql: &str| conn.query_row(sql, [], |r| r.get::<_, i64>(0)).unwrap();
        (
            n("SELECT COUNT(*) FROM entities"),
            n("SELECT COUNT(*) FROM relations"),
        )
    };
    let run = |sub: &str| {
        let out = engraph(&db, &mono, &["index", sub]);
        assert!(
            out.status.success(),
            "index {sub}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    };
    run(".");
    let first = counts();
    run("svc-a");
    assert_eq!(counts(), first);
    run(".");
    assert_eq!(counts(), first);
    assert_eq!(
        entity(&db, "alpha_handler"),
        (key(&mono), "svc-a/src/lib.rs".into())
    );
}

#[test]
fn each_subproject_is_checked_against_the_commit_it_was_indexed_at() {
    let (dir, mono) = mono();
    let db = dir.path().join("e.db");
    index_sub(dir.path(), &db, &mono, &mono, "svc-a", "alpha_handler");
    // svc-a changes and is committed, but only svc-b is re-indexed.
    std::fs::write(
        mono.join("svc-a/src/lib.rs"),
        "pub fn alpha_handler() -> u32 { 2 }\n",
    )
    .unwrap();
    git(&mono, &["commit", "-q", "-am", "change alpha_handler"]);
    index_sub(dir.path(), &db, &mono, &mono, "svc-b", "beta_handler");
    // The graph still has svc-a as first indexed: don't trust it.
    assert_eq!(pre_grep(&db, &mono, "alpha_handler"), None);
    assert_eq!(
        pre_grep(&db, &mono, "beta_handler").as_deref(),
        Some("deny")
    );
    // A root run re-indexes everything, svc-a included.
    let scip = dir.path().join("empty.scip");
    let out = engraph(
        &db,
        &mono,
        &["index", ".", "--scip", scip.to_str().unwrap()],
    );
    assert!(out.status.success());
    assert_eq!(
        pre_grep(&db, &mono, "alpha_handler").as_deref(),
        Some("deny")
    );
}

#[test]
fn nested_subproject_subtree_uses_forward_slashes() {
    // Windows `strip_prefix` yields `group\svc-c`; the codegraph stores `/`.
    let (dir, mono) = mono();
    std::fs::create_dir_all(mono.join("group/svc-c/src")).unwrap();
    std::fs::write(
        mono.join("group/svc-c/src/lib.rs"),
        "pub fn gamma_handler() -> u32 { 1 }\n",
    )
    .unwrap();
    git(&mono, &["add", "."]);
    git(&mono, &["commit", "-q", "-m", "add svc-c"]);
    let db = dir.path().join("e.db");
    index_sub(
        dir.path(),
        &db,
        &mono,
        &mono,
        "group/svc-c",
        "gamma_handler",
    );
    let subtree: String = rusqlite::Connection::open(&db)
        .unwrap()
        .query_row("SELECT subtree FROM index_state", [], |r| r.get(0))
        .unwrap();
    assert_eq!(subtree, "group/svc-c");
    std::fs::write(
        mono.join("group/svc-c/src/lib.rs"),
        "pub fn gamma_handler() -> u32 { 2 }\n",
    )
    .unwrap();
    assert_eq!(pre_grep(&db, &mono, "gamma_handler"), None);
}
