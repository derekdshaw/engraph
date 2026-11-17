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

/// A git repo with module `a` (`Caller`), which calls `Helper` of module `b`
/// through a `replace` to `../b`. `go_work` adds a root `go.work` that lists
/// both; `vendor` adds a vendored copy of `b` below `a`.
fn caller_and_helper(dir: &Path, go_work: bool, vendor: bool) -> std::path::PathBuf {
    let repo = dir.join("repo");
    write(&repo, "b/go.mod", "module example.com/b\n\ngo 1.21\n");
    write(
        &repo,
        "b/b.go",
        "package b\n\nfunc Helper() int { return 2 }\n",
    );
    write(
        &repo,
        "a/go.mod",
        "module example.com/a\n\ngo 1.21\n\nrequire example.com/b v0.0.0\n\nreplace example.com/b => ../b\n",
    );
    write(
        &repo,
        "a/a.go",
        "package a\n\nimport \"example.com/b\"\n\nfunc Caller() int { return b.Helper() }\n",
    );
    if go_work {
        write(&repo, "go.work", "go 1.21\n\nuse (\n\t./a\n\t./b\n)\n");
    }
    if vendor {
        // What `go mod vendor` writes for the replaced module.
        write(
            &repo,
            "a/vendor/modules.txt",
            "# example.com/b v0.0.0 => ../b\n## explicit; go 1.21\nexample.com/b\n# example.com/b => ../b\n",
        );
        write(
            &repo,
            "a/vendor/example.com/b/b.go",
            "package b\n\nfunc Helper() int { return 2 }\n",
        );
    }
    git(&repo, &["init", "-q"]);
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-q", "-m", "init"]);
    repo.canonicalize().unwrap()
}

/// The file of each entity named `dst` that an entity named `src` has a
/// relation to; `None` for a placeholder (a reference to no definition).
fn relation_targets(db: &Path, src: &str, dst: &str) -> Vec<Option<String>> {
    let conn = rusqlite::Connection::open(db).unwrap();
    let mut stmt = conn
        .prepare(
            "SELECT d.file_path FROM relations r
             JOIN entities s ON s.id = r.src_entity
             JOIN entities d ON d.id = r.dst_entity
             WHERE s.name = ?1 AND d.name = ?2",
        )
        .unwrap();
    stmt.query_map([src, dst], |r| r.get(0))
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

fn index_ok(db: &Path, cwd: &Path, args: &[&str]) {
    let out = engraph(db, cwd, args);
    assert!(
        out.status.success(),
        "index: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn a_call_into_another_go_work_module_points_at_its_definition() {
    if !go_tools_present() {
        return;
    }
    let dir = tempdir().unwrap();
    let db = dir.path().join("e.db");
    let repo = caller_and_helper(dir.path(), true, false);
    index_ok(&db, dir.path(), &["index", repo.to_str().unwrap()]);
    assert_eq!(
        relation_targets(&db, "Caller", "Helper"),
        [Some("b/b.go".into())]
    );
}

#[test]
fn a_call_into_a_separately_indexed_module_points_at_its_definition() {
    if !go_tools_present() {
        return;
    }
    let dir = tempdir().unwrap();
    let db = dir.path().join("e.db");
    // No go.work: --recursive runs scip-go once per module.
    let repo = caller_and_helper(dir.path(), false, false);
    index_ok(
        &db,
        dir.path(),
        &["index", "--recursive", repo.to_str().unwrap()],
    );
    assert_eq!(
        relation_targets(&db, "Caller", "Helper"),
        [Some("b/b.go".into())]
    );
}

#[test]
fn a_vendored_copy_of_a_module_adds_no_second_symbol() {
    if !go_tools_present() {
        return;
    }
    let dir = tempdir().unwrap();
    let db = dir.path().join("e.db");
    let repo = caller_and_helper(dir.path(), false, true);
    index_ok(
        &db,
        dir.path(),
        &["index", "--recursive", repo.to_str().unwrap()],
    );
    // Discovery skips `vendor/`, and scip-go writes no document for a
    // vendored package: the only `Helper` is the one in `b`.
    assert_eq!(entity_count(&db, "Helper"), 1);
    assert_eq!(
        relation_targets(&db, "Caller", "Helper"),
        [Some("b/b.go".into())]
    );
}

#[test]
fn a_module_its_go_work_does_not_list_still_gets_its_symbols() {
    if !go_tools_present() {
        return;
    }
    let dir = tempdir().unwrap();
    let db = dir.path().join("e.db");
    let repo = caller_and_helper(dir.path(), true, false);
    write(&repo, "c/go.mod", "module example.com/c\n\ngo 1.21\n");
    write(
        &repo,
        "c/c.go",
        "package c\n\nfunc Lone() int { return 3 }\n",
    );
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-q", "-m", "c"]);
    index_ok(
        &db,
        dir.path(),
        &["index", "--recursive", repo.to_str().unwrap()],
    );
    assert_eq!(
        entity(&db, "Lone"),
        Some((repo.to_string_lossy().into_owned(), "c/c.go".into()))
    );
    assert_eq!(
        relation_targets(&db, "Caller", "Helper"),
        [Some("b/b.go".into())]
    );
}

#[test]
fn a_go_work_member_indexed_alone_points_at_its_sibling() {
    if !go_tools_present() {
        return;
    }
    let dir = tempdir().unwrap();
    let db = dir.path().join("e.db");
    let repo = caller_and_helper(dir.path(), true, false);
    // Only the go.work ties `a` to `b`: scip-go, run in `a` alone, gives
    // `b`'s symbols the version `.`.
    write(
        &repo,
        "a/go.mod",
        "module example.com/a\n\ngo 1.21\n\nrequire example.com/b v0.0.0\n",
    );
    git(&repo, &["commit", "-q", "-am", "no replace"]);
    index_ok(&db, dir.path(), &["index", repo.to_str().unwrap()]);
    index_ok(&db, &repo, &["index", "a"]);
    assert_eq!(entity_count(&db, "Helper"), 1);
    assert_eq!(
        relation_targets(&db, "Caller", "Helper"),
        [Some("b/b.go".into())]
    );
}

/// A git repo with one module whose `Tagged` builds only with the
/// `integration` tag.
fn tagged_module(dir: &Path) -> std::path::PathBuf {
    let repo = dir.join("repo");
    write(&repo, "go.mod", "module example.com/t\n\ngo 1.21\n");
    write(
        &repo,
        "t.go",
        "package t\n\nfunc Plain() int { return 0 }\n",
    );
    write(
        &repo,
        "tag.go",
        "//go:build integration\n\npackage t\n\nfunc Tagged() int { return 1 }\n",
    );
    git(&repo, &["init", "-q"]);
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-q", "-m", "init"]);
    repo.canonicalize().unwrap()
}

#[test]
fn go_tags_select_the_files_scip_go_indexes() {
    if !go_tools_present() {
        return;
    }
    let dir = tempdir().unwrap();
    let repo = tagged_module(dir.path());
    let plain = dir.path().join("plain.db");
    index_ok(&plain, dir.path(), &["index", repo.to_str().unwrap()]);
    assert_eq!(entity_count(&plain, "Plain"), 1);
    assert_eq!(entity_count(&plain, "Tagged"), 0);

    let tagged = dir.path().join("tagged.db");
    index_ok(
        &tagged,
        dir.path(),
        &["index", "--go-tags", "integration", repo.to_str().unwrap()],
    );
    assert_eq!(entity_count(&tagged, "Tagged"), 1);

    // A refresh repeats the tags of the recorded run.
    write(
        &repo,
        "t.go",
        "package t\n\nfunc Plain() int { return 2 }\n",
    );
    git(&repo, &["commit", "-q", "-am", "change"]);
    index_ok(&tagged, &repo, &["index", "--if-changed"]);
    assert_eq!(entity_count(&tagged, "Tagged"), 1);
}

#[test]
fn go_tags_reject_whitespace() {
    let dir = tempdir().unwrap();
    let db = dir.path().join("e.db");
    let out = engraph(&db, dir.path(), &["index", "--go-tags", "a b", "--dry-run"]);
    assert!(!out.status.success());
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("comma"),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
}
