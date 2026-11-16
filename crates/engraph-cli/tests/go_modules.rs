//! Go repos with more than one module (plan 03). Tests skip when scip-go or go
//! isn't installed.

use std::path::Path;
use std::process::Command;
use tempfile::tempdir;

mod common;
use common::git;

/// `common::engraph`, with the SCIP scratch file kept out of the user's data
/// dir.
fn engraph(db: &Path, cwd: &Path, args: &[&str]) -> std::process::Output {
    common::command(db, cwd, args)
        .env(
            "ENGRAPH_SCIP_OUTPUT_DIR",
            std::env::temp_dir().join("engraph-test-scip"),
        )
        .output()
        .expect("spawn engraph")
}

fn go_tools_present() -> bool {
    let ok = ["scip-go", "go"].iter().all(|t| {
        Command::new(t)
            .arg(if *t == "go" { "version" } else { "--version" })
            .output()
            .is_ok_and(|o| o.status.success())
    });
    if !ok {
        eprintln!("skip: scip-go or go not installed");
    }
    ok
}

fn write(root: &Path, rel: &str, body: &str) {
    let p = root.join(rel);
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(p, body).unwrap();
}

/// `(project, file_path)` of the entity named `name`, if indexed.
fn entity(db: &Path, name: &str) -> Option<(String, String)> {
    rusqlite::Connection::open(db)
        .unwrap()
        .query_row(
            "SELECT project, file_path FROM entities WHERE name = ?1",
            [name],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .ok()
}

#[test]
fn recursive_index_covers_the_root_module_and_a_module_in_tools() {
    if !go_tools_present() {
        return;
    }
    let dir = tempdir().unwrap();
    let db = dir.path().join("e.db");
    let repo = dir.path().join("repo");
    write(&repo, "go.mod", "module example.com/root\n\ngo 1.21\n");
    write(
        &repo,
        "root.go",
        "package root\n\nfunc RootHandler() int { return 1 }\n",
    );
    write(
        &repo,
        "tools/go.mod",
        "module example.com/tools\n\ngo 1.21\n",
    );
    write(
        &repo,
        "tools/tool.go",
        "package tools\n\nfunc ToolHandler() int { return 2 }\n",
    );
    // A git repo, so `tools/` folds into the repo's project key.
    git(&repo, &["init", "-q"]);
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-q", "-m", "init"]);
    let repo = repo.canonicalize().unwrap();

    let out = engraph(
        &db,
        dir.path(),
        &["index", "--recursive", repo.to_str().unwrap()],
    );
    assert!(
        out.status.success(),
        "index: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let project = repo.to_string_lossy().into_owned();
    assert_eq!(
        entity(&db, "RootHandler"),
        Some((project.clone(), "root.go".into()))
    );
    assert_eq!(
        entity(&db, "ToolHandler"),
        Some((project, "tools/tool.go".into()))
    );
}

#[test]
fn go_work_root_indexes_every_listed_module() {
    if !go_tools_present() {
        return;
    }
    let dir = tempdir().unwrap();
    let db = dir.path().join("e.db");
    let repo = dir.path().join("repo");
    // No go.mod at the root: the go.work alone makes it a Go project.
    write(
        &repo,
        "go.work",
        "go 1.21\n\nuse (\n\t./tools\n\t./svc\n)\n",
    );
    write(
        &repo,
        "tools/go.mod",
        "module example.com/tools\n\ngo 1.21\n",
    );
    write(
        &repo,
        "tools/tool.go",
        "package tools\n\nfunc ToolHandler() int { return 2 }\n",
    );
    write(
        &repo,
        "svc/go.mod",
        "module example.com/svc\n\ngo 1.21\n\nrequire example.com/tools v0.0.0\n",
    );
    write(
        &repo,
        "svc/svc.go",
        "package svc\n\nimport \"example.com/tools\"\n\nfunc SvcHandler() int { return tools.ToolHandler() }\n",
    );
    git(&repo, &["init", "-q"]);
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-q", "-m", "init"]);
    let repo = repo.canonicalize().unwrap();

    let out = engraph(&db, dir.path(), &["index", repo.to_str().unwrap()]);
    assert!(
        out.status.success(),
        "index: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let project = repo.to_string_lossy().into_owned();
    assert_eq!(
        entity(&db, "ToolHandler"),
        Some((project.clone(), "tools/tool.go".into()))
    );
    assert_eq!(
        entity(&db, "SvcHandler"),
        Some((project, "svc/svc.go".into()))
    );
}
