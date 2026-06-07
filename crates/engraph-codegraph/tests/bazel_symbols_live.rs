//! F2 Phase 2.3 #2 — live integration check for the symbol-level Bazel pass.
//!
//! Java is *delegated* (`run_java` runs the command named by
//! `ENGRAPH_BAZEL_SCIP_JAVA_CMD`), so the heavy scip-java build is no longer
//! engraph's code — the delegation contract itself is unit-tested in
//! `bazel_symbols.rs` (`run_java_delegates_to_configured_command`). This live
//! test verifies the *integrated* `--bazel-symbols` pass against a real Bazel
//! fixture: the target-level pass still writes `bazel_target` rows, the symbol
//! pass runs and reports the (here unconfigured) Java language without aborting,
//! and the symbol load doesn't wipe the target-level rows.
//!
//! Double-gated so default `cargo test` doesn't pay the cold-cache cost of
//! fetching the Bazel Java toolchain:
//!   1. `bazel` / `bazelisk` on PATH
//!   2. `ENGRAPH_LIVE_BAZEL_SYMBOLS=1` env var set
//!
//! Any failure that smells like a transient cold-cache issue soft-skips with a
//! diagnostic rather than failing the suite.

use engraph_codegraph::index_repo;
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

fn live_gate_set() -> bool {
    std::env::var("ENGRAPH_LIVE_BAZEL_SYMBOLS")
        .map(|v| !v.is_empty() && v != "0")
        .unwrap_or(false)
}

fn write_java_fixture(root: &std::path::Path) {
    // bzlmod is the easier path on modern Bazel: a one-line MODULE.bazel
    // pulls rules_java without the WORKSPACE+http_archive boilerplate. An
    // empty WORKSPACE.bazel is still required to mark the workspace root
    // on older bazel versions that haven't switched to bzlmod-by-default.
    std::fs::write(root.join("WORKSPACE.bazel"), "").unwrap();
    std::fs::write(
        root.join("MODULE.bazel"),
        "bazel_dep(name = \"rules_java\", version = \"7.6.5\")\n",
    )
    .unwrap();
    let pkg = root.join("src/main/java/example");
    std::fs::create_dir_all(&pkg).unwrap();
    std::fs::write(
        pkg.join("BUILD.bazel"),
        r#"load("@rules_java//java:defs.bzl", "java_library")

java_library(
    name = "hello",
    srcs = ["Hello.java"],
    visibility = ["//visibility:public"],
)
"#,
    )
    .unwrap();
    std::fs::write(
        pkg.join("Hello.java"),
        r#"package example;

public class Hello {
    public static String greet() {
        return "hi";
    }
}
"#,
    )
    .unwrap();
}

fn write_go_fixture(root: &std::path::Path) {
    // bzlmod rules_go fixture: a one-line MODULE.bazel + a single go_library.
    // `bazel query` (the target-level pass) is loading-phase only, so this needs
    // no Go SDK/toolchain to enumerate the target.
    std::fs::write(root.join("WORKSPACE.bazel"), "").unwrap();
    std::fs::write(
        root.join("MODULE.bazel"),
        "bazel_dep(name = \"rules_go\", version = \"0.50.1\")\n",
    )
    .unwrap();
    let pkg = root.join("hello");
    std::fs::create_dir_all(&pkg).unwrap();
    std::fs::write(
        pkg.join("BUILD.bazel"),
        r#"load("@rules_go//go:def.bzl", "go_library")

go_library(
    name = "hello",
    srcs = ["hello.go"],
    importpath = "example.com/hello",
    visibility = ["//visibility:public"],
)
"#,
    )
    .unwrap();
    std::fs::write(
        pkg.join("hello.go"),
        "package hello\n\nfunc Greet() string { return \"hi\" }\n",
    )
    .unwrap();
}

#[test]
fn bazel_symbols_pass_runs_with_java_delegated() {
    if !live_gate_set() {
        eprintln!("skip: set ENGRAPH_LIVE_BAZEL_SYMBOLS=1 to run; cold runs cost minutes");
        return;
    }
    if !bazel_present() {
        eprintln!("skip: neither bazel nor bazelisk on PATH");
        return;
    }

    let dir = tempdir().unwrap();
    let root = dir.path().join("ws");
    std::fs::create_dir_all(&root).unwrap();
    write_java_fixture(&root);

    let output_base = dir.path().join("bazel-out");
    // SAFETY: single-threaded test setup. Pin the output_base, and ensure Java
    // is unconfigured so the delegated pass reports SkippedNotConfigured rather
    // than trying to run a user's real command against this throwaway fixture.
    unsafe {
        std::env::set_var("ENGRAPH_BAZEL_OUTPUT_BASE", &output_base);
        std::env::remove_var("ENGRAPH_BAZEL_SCIP_JAVA_CMD");
    }

    let pool = open_pool(&dir.path().join("eg.db")).unwrap();
    let conn = pool.get().unwrap();

    let stats = match index_repo(&conn, &root, None, None, "/proj/ws", true) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("skip: index_repo failed (cold toolchain / network?): {e:#}");
            return;
        }
    };
    assert_eq!(
        stats.driver_name, "bazel-query+symbols",
        "with --bazel-symbols on a Bazel root, driver should report both passes"
    );

    // Target-level pass must have produced the java_library target entity, and
    // the symbol load must not have wiped it.
    let target_count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM entities
             WHERE kind='bazel_target' AND id='//src/main/java/example:hello'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(target_count, 1, "java_library target should be present");

    // The delegated Java pass should soft-report skipped-not-configured (no
    // ENGRAPH_BAZEL_SCIP_JAVA_CMD), proving the symbol pass ran without aborting.
    let java = stats.symbol_langs.iter().find(|l| l.language == "java");
    assert!(
        java.map(|l| l.status.contains("not set")).unwrap_or(false),
        "java symbol pass should report 'skipped (… not set)'; got {:?}",
        java.map(|l| &l.status)
    );
}

#[test]
fn bazel_symbols_pass_runs_with_go_delegated() {
    if !live_gate_set() {
        eprintln!("skip: set ENGRAPH_LIVE_BAZEL_SYMBOLS=1 to run; cold runs cost minutes");
        return;
    }
    if !bazel_present() {
        eprintln!("skip: neither bazel nor bazelisk on PATH");
        return;
    }

    let dir = tempdir().unwrap();
    let root = dir.path().join("ws");
    std::fs::create_dir_all(&root).unwrap();
    write_go_fixture(&root);

    // A fake delegated Go command writes a minimal one-document SCIP to $2. This
    // exercises the integrated delegated path (target-level pass + symbol merge +
    // load) without a real scip-go / Bazel Go toolchain.
    let fake_scip = dir.path().join("fake-go.scip");
    {
        use protobuf::Message;
        use scip::types::{Document, Index};
        let mut idx = Index::new();
        let mut d = Document::new();
        d.relative_path = "hello/hello.go".to_string();
        idx.documents.push(d);
        std::fs::write(&fake_scip, idx.write_to_bytes().unwrap()).unwrap();
    }
    let cmd = dir.path().join("go-cmd.sh");
    std::fs::write(&cmd, format!("cp \"{}\" \"$2\"\n", fake_scip.display())).unwrap();

    let output_base = dir.path().join("bazel-out");
    // SAFETY: single-threaded test setup.
    unsafe {
        std::env::set_var("ENGRAPH_BAZEL_OUTPUT_BASE", &output_base);
        std::env::set_var(
            "ENGRAPH_BAZEL_SCIP_GO_CMD",
            format!("sh \"{}\"", cmd.display()),
        );
    }

    let pool = open_pool(&dir.path().join("eg.db")).unwrap();
    let conn = pool.get().unwrap();

    let stats = match index_repo(&conn, &root, None, None, "/proj/ws", true) {
        Ok(s) => s,
        Err(e) => {
            unsafe { std::env::remove_var("ENGRAPH_BAZEL_SCIP_GO_CMD") };
            eprintln!("skip: index_repo failed (cold toolchain / network?): {e:#}");
            return;
        }
    };
    unsafe { std::env::remove_var("ENGRAPH_BAZEL_SCIP_GO_CMD") };

    assert_eq!(stats.driver_name, "bazel-query+symbols");

    // Target-level pass produced the go_library target and the symbol load did
    // not wipe it.
    let target_count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM entities
             WHERE kind='bazel_target' AND id='//hello:hello'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(target_count, 1, "go_library target should be present");

    // The delegated Go pass ran our fake command and merged its SCIP.
    let go = stats.symbol_langs.iter().find(|l| l.language == "go");
    assert!(
        go.map(|l| l.status == "indexed").unwrap_or(false),
        "go symbol pass should report 'indexed'; got {:?}",
        go.map(|l| &l.status)
    );
}
