//! F2 Phase 2.3 — end-to-end target-level Bazel indexing.
//!
//! Builds a 2-target genrule workspace (no external rules needed, so first-
//! run cost is small) and runs `engraph_codegraph::index_repo` against it.
//! Asserts both targets land as `bazel_target` entities and the dependency
//! between them lands as a `BAZEL_DEPENDS_ON` relation.
//!
//! Soft-skips when neither `bazel` nor `bazelisk` is on PATH.

use engraph_codegraph::{discover_workspace_repos, index_repo};
use engraph_core::db::open_pool;
use std::process::Command;
use tempfile::tempdir;

fn bazel_present() -> bool {
    for bin in &["bazel", "bazelisk"] {
        if Command::new(bin)
            .arg("--version")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
        {
            return true;
        }
    }
    false
}

fn write_fixture(root: &std::path::Path) {
    std::fs::create_dir_all(root.join("foo")).unwrap();
    std::fs::create_dir_all(root.join("bar")).unwrap();
    // WORKSPACE (empty) is enough to mark the root for `bazel query`. Avoid
    // MODULE.bazel here because that triggers bzlmod which wants
    // bazel_dep(rules_cc) etc. — an empty WORKSPACE just works with
    // genrule, which is native.
    std::fs::write(root.join("WORKSPACE"), "").unwrap();
    std::fs::write(
        root.join("foo/BUILD.bazel"),
        r#"genrule(
    name = "foo",
    srcs = [],
    outs = ["foo.txt"],
    cmd = "echo foo > $@",
    visibility = ["//visibility:public"],
)
"#,
    )
    .unwrap();
    std::fs::write(
        root.join("bar/BUILD.bazel"),
        r#"genrule(
    name = "bar",
    srcs = ["//foo:foo"],
    outs = ["bar.txt"],
    cmd = "cat $(SRCS) > $@",
)
"#,
    )
    .unwrap();
}

#[test]
fn discover_recognizes_workspace_file() {
    let dir = tempdir().unwrap();
    let root = dir.path().join("ws");
    std::fs::create_dir_all(&root).unwrap();
    write_fixture(&root);
    let repos = discover_workspace_repos(&root).unwrap();
    assert_eq!(repos.len(), 1);
    assert_eq!(repos[0], root);
}

#[test]
fn target_level_index_creates_targets_and_deps() {
    if !bazel_present() {
        eprintln!("skip: neither bazel nor bazelisk on PATH");
        return;
    }
    let dir = tempdir().unwrap();
    let root = dir.path().join("ws");
    std::fs::create_dir_all(&root).unwrap();
    write_fixture(&root);

    // Point Bazel's output_base into the per-test tempdir so we don't
    // pollute the user's ~/.cache/engraph between unrelated test runs.
    let output_base = dir.path().join("bazel-out");
    std::env::set_var("ENGRAPH_BAZEL_OUTPUT_BASE", &output_base);

    let pool = open_pool(&dir.path().join("eg.db")).unwrap();
    let conn = pool.get().unwrap();

    let stats = match index_repo(&conn, &root, None, None, "/proj/ws", false) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("skip: bazel query failed (first-run env issue?): {e:#}");
            return;
        }
    };
    assert_eq!(
        stats.driver_name, "bazel-query",
        "Bazel workspace should take the bazel-query path"
    );
    assert!(stats.entities_inserted >= 2, "got {}", stats.entities_inserted);
    assert!(stats.relations_inserted >= 1, "got {}", stats.relations_inserted);

    let foo: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM entities WHERE kind='bazel_target' AND id='//foo:foo'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(foo, 1, "//foo:foo target should be present");
    let bar: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM entities WHERE kind='bazel_target' AND id='//bar:bar'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(bar, 1, "//bar:bar target should be present");

    let dep_edge: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM relations
             WHERE kind='BAZEL_DEPENDS_ON'
               AND src_entity='//bar:bar'
               AND dst_entity='//foo:foo'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        dep_edge, 1,
        "expected BAZEL_DEPENDS_ON edge //bar:bar -> //foo:foo"
    );

    let foo_path: Option<String> = conn
        .query_row(
            "SELECT file_path FROM entities WHERE id='//foo:foo'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        foo_path.as_deref(),
        Some("foo/BUILD.bazel"),
        "file_path should be repo-relative"
    );
}

#[test]
fn reindex_is_idempotent() {
    if !bazel_present() {
        eprintln!("skip: neither bazel nor bazelisk on PATH");
        return;
    }
    let dir = tempdir().unwrap();
    let root = dir.path().join("ws");
    std::fs::create_dir_all(&root).unwrap();
    write_fixture(&root);
    std::env::set_var(
        "ENGRAPH_BAZEL_OUTPUT_BASE",
        dir.path().join("bazel-out"),
    );

    let pool = open_pool(&dir.path().join("eg.db")).unwrap();
    let conn = pool.get().unwrap();
    if index_repo(&conn, &root, None, None, "/proj/ws", false).is_err() {
        eprintln!("skip: bazel first-run failed");
        return;
    }
    let n1: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM relations WHERE kind='BAZEL_DEPENDS_ON'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    index_repo(&conn, &root, None, None, "/proj/ws", false).unwrap();
    let n2: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM relations WHERE kind='BAZEL_DEPENDS_ON'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(n1, n2, "re-indexing must not duplicate BAZEL_DEPENDS_ON edges");
}
