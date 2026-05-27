//! Pure file-probe tests: write the trigger file into a tempdir and assert
//! exactly the expected driver matches. No external binary required.

use engraph_codegraph::driver;
use std::fs;
use tempfile::tempdir;

fn detecting(repo: &std::path::Path) -> Vec<&'static str> {
    driver::registry()
        .iter()
        .filter(|d| d.detect(repo))
        .map(|d| d.name())
        .collect()
}

#[test]
fn empty_dir_matches_no_driver() {
    let dir = tempdir().unwrap();
    assert_eq!(detecting(dir.path()), Vec::<&'static str>::new());
}

#[test]
fn cargo_toml_picks_rust_analyzer() {
    let dir = tempdir().unwrap();
    fs::write(dir.path().join("Cargo.toml"), "[package]\nname=\"x\"\n").unwrap();
    assert_eq!(detecting(dir.path()), vec!["rust-analyzer"]);
}

#[test]
fn pyproject_picks_scip_python() {
    let dir = tempdir().unwrap();
    fs::write(dir.path().join("pyproject.toml"), "[project]\nname=\"x\"\n").unwrap();
    assert_eq!(detecting(dir.path()), vec!["scip-python"]);
}

#[test]
fn setup_py_also_picks_scip_python() {
    let dir = tempdir().unwrap();
    fs::write(
        dir.path().join("setup.py"),
        "from setuptools import setup\n",
    )
    .unwrap();
    assert_eq!(detecting(dir.path()), vec!["scip-python"]);
}

#[test]
fn go_mod_picks_scip_go() {
    let dir = tempdir().unwrap();
    fs::write(dir.path().join("go.mod"), "module x\n").unwrap();
    assert_eq!(detecting(dir.path()), vec!["scip-go"]);
}

#[test]
fn package_json_alone_does_not_pick_typescript() {
    let dir = tempdir().unwrap();
    fs::write(dir.path().join("package.json"), "{}\n").unwrap();
    // Without tsconfig.json, scip-typescript must not match (per its detect
    // contract).
    assert!(detecting(dir.path()).is_empty());
}

#[test]
fn package_json_plus_tsconfig_picks_scip_typescript() {
    let dir = tempdir().unwrap();
    fs::write(dir.path().join("package.json"), "{}\n").unwrap();
    fs::write(dir.path().join("tsconfig.json"), "{}\n").unwrap();
    assert_eq!(detecting(dir.path()), vec!["scip-typescript"]);
}

#[test]
fn pom_xml_picks_scip_java() {
    let dir = tempdir().unwrap();
    fs::write(dir.path().join("pom.xml"), "<project/>\n").unwrap();
    assert_eq!(detecting(dir.path()), vec!["scip-java"]);
}

#[test]
fn build_gradle_picks_scip_java() {
    let dir = tempdir().unwrap();
    fs::write(dir.path().join("build.gradle"), "// gradle\n").unwrap();
    assert_eq!(detecting(dir.path()), vec!["scip-java"]);
}

#[test]
fn build_gradle_kts_picks_scip_java() {
    let dir = tempdir().unwrap();
    fs::write(dir.path().join("build.gradle.kts"), "// gradle.kts\n").unwrap();
    assert_eq!(detecting(dir.path()), vec!["scip-java"]);
}

#[test]
fn build_sbt_picks_scip_java() {
    let dir = tempdir().unwrap();
    fs::write(dir.path().join("build.sbt"), "name := \"tiny\"\n").unwrap();
    assert_eq!(detecting(dir.path()), vec!["scip-java"]);
}

#[test]
fn bazel_workspace_does_not_pick_scip_java() {
    // scip-java's index command does NOT support Bazel auto-detection; the
    // driver should NOT claim Bazel workspaces. (Phase 2.3 will add a
    // separate scip-bazel-driven path.)
    let dir = tempdir().unwrap();
    fs::write(dir.path().join("MODULE.bazel"), "module(name=\"x\")\n").unwrap();
    assert!(detecting(dir.path()).is_empty());
}

#[test]
fn cargo_and_pyproject_match_both_drivers() {
    let dir = tempdir().unwrap();
    fs::write(dir.path().join("Cargo.toml"), "[package]\nname=\"x\"\n").unwrap();
    fs::write(dir.path().join("pyproject.toml"), "[project]\nname=\"x\"\n").unwrap();
    let matched = detecting(dir.path());
    assert!(matched.contains(&"rust-analyzer"));
    assert!(matched.contains(&"scip-python"));
}
