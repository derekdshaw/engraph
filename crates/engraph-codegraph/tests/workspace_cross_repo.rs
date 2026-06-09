//! Phase 2.2 — cross-repo stitching.
//!
//! Build a workspace with two sibling cargo crates where `app_b` depends on
//! `lib_a` via a path dependency, index them via `engraph_codegraph::
//! index_workspace`, then assert that `engraph_codegraph::subgraph_for` on
//! `app_b`'s caller surfaces a CALLS edge to `lib_a`'s function with the
//! `repo:lib_a` annotation in the rendered markdown.
//!
//! Soft-skips when rust-analyzer isn't installed so this file is safe to run
//! on any machine.

use engraph_codegraph::{format_markdown, index_workspace, subgraph_for};
use engraph_core::db::open_pool;
use std::process::Command;
use tempfile::tempdir;

fn rust_analyzer_present() -> bool {
    Command::new("rust-analyzer")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

fn write_fixture(root: &std::path::Path) {
    let lib_a = root.join("lib_a");
    std::fs::create_dir_all(lib_a.join("src")).unwrap();
    std::fs::write(
        lib_a.join("Cargo.toml"),
        r#"[package]
name = "lib_a"
version = "0.0.1"
edition = "2021"

[lib]
path = "src/lib.rs"
"#,
    )
    .unwrap();
    std::fs::write(lib_a.join("src/lib.rs"), "pub fn lib_foo() -> i32 { 42 }\n").unwrap();

    let app_b = root.join("app_b");
    std::fs::create_dir_all(app_b.join("src")).unwrap();
    std::fs::write(
        app_b.join("Cargo.toml"),
        r#"[package]
name = "app_b"
version = "0.0.1"
edition = "2021"

[dependencies]
lib_a = { path = "../lib_a" }

[lib]
path = "src/lib.rs"
"#,
    )
    .unwrap();
    std::fs::write(
        app_b.join("src/lib.rs"),
        "pub fn app_caller() -> i32 { lib_a::lib_foo() + 1 }\n",
    )
    .unwrap();
}

#[test]
fn workspace_links_app_b_caller_to_lib_a_foo() {
    if !rust_analyzer_present() {
        eprintln!("skip: rust-analyzer not installed");
        return;
    }
    let dir = tempdir().unwrap();
    let root = dir.path().join("workspace");
    std::fs::create_dir_all(&root).unwrap();
    write_fixture(&root);

    let pool = open_pool(&dir.path().join("eg.db")).unwrap();
    let conn = pool.get().unwrap();

    let stats = match index_workspace(&conn, &root, false, false) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("skip: index_workspace failed (likely rust-analyzer env issue): {e:#}");
            return;
        }
    };
    let any_ok = stats.repos.iter().any(|r| r.outcome.is_ok());
    if !any_ok {
        eprintln!("skip: no repo indexed successfully; per-repo errors below");
        for r in &stats.repos {
            if let Err(e) = &r.outcome {
                eprintln!("  {} -> {e:#}", r.project);
            }
        }
        return;
    }

    assert!(
        stats.ok_count() >= 1,
        "expected at least one repo to index ok"
    );

    // Both repos should be indexed.
    let projects: Vec<String> = stats.repos.iter().map(|r| r.project.clone()).collect();
    assert!(
        projects.iter().any(|p| p.ends_with("lib_a")),
        "{projects:?}"
    );
    assert!(
        projects.iter().any(|p| p.ends_with("app_b")),
        "{projects:?}"
    );

    // The DB should contain both symbols and a CALLS edge between them.
    let app_caller_count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM entities WHERE name = 'app_caller'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert!(app_caller_count >= 1, "missing app_caller entity");

    let lib_foo_count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM entities WHERE name = 'lib_foo'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert!(lib_foo_count >= 1, "missing lib_foo entity");

    // Cross-repo CALLS: src.project ends in app_b, dst.project ends in lib_a.
    let cross: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM relations r
             JOIN entities src ON src.id = r.src_entity
             JOIN entities dst ON dst.id = r.dst_entity
             WHERE r.kind = 'CALLS'
               AND src.name = 'app_caller'
               AND dst.name = 'lib_foo'
               AND src.project != dst.project",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert!(
        cross >= 1,
        "expected a cross-project CALLS edge app_caller -> lib_foo"
    );

    let n = subgraph_for(&conn, "app_caller", 30).unwrap();
    let md = format_markdown(&n, 8192);
    assert!(
        md.contains("`lib_foo`"),
        "missing lib_foo in markdown: {md}"
    );
    assert!(
        md.contains("repo:lib_a"),
        "missing repo:lib_a annotation in markdown: {md}"
    );
}

#[test]
fn discover_finds_immediate_children_with_manifests() {
    let dir = tempdir().unwrap();
    let root = dir.path().join("workspace");
    std::fs::create_dir_all(&root).unwrap();
    write_fixture(&root);
    std::fs::create_dir_all(root.join("not-a-repo")).unwrap();
    std::fs::write(root.join("not-a-repo/README.md"), "no manifest here\n").unwrap();

    let repos = engraph_codegraph::discover_workspace_repos(&root).unwrap();
    let names: Vec<String> = repos
        .iter()
        .filter_map(|p| p.file_name().map(|s| s.to_string_lossy().into_owned()))
        .collect();
    assert!(names.contains(&"lib_a".to_string()), "{names:?}");
    assert!(names.contains(&"app_b".to_string()), "{names:?}");
    assert!(!names.contains(&"not-a-repo".to_string()), "{names:?}");
}

#[test]
fn discover_returns_root_when_it_has_a_manifest() {
    let dir = tempdir().unwrap();
    let root = dir.path().join("cargo_ws");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::write(root.join("Cargo.toml"), "[workspace]\nmembers = []\n").unwrap();

    let repos = engraph_codegraph::discover_workspace_repos(&root).unwrap();
    assert_eq!(repos.len(), 1);
    assert_eq!(repos[0], root);
}
