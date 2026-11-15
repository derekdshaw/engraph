//! Workspace discovery flags of `engraph index`, checked through `--dry-run`,
//! which discovers the projects without running an indexer.

use std::path::Path;
use std::process::Command;
use tempfile::tempdir;

fn engraph(db: &Path, args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_engraph"))
        .env_clear()
        .env("PATH", std::env::var("PATH").unwrap_or_default())
        .env("HOME", std::env::var("HOME").unwrap_or_default())
        .env("ENGRAPH_DB_PATH", db)
        .args(args)
        .output()
        .expect("spawn engraph")
}

fn manifest(root: &Path, rel: &str, name: &str) {
    let dir = root.join(rel);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join(name), "").unwrap();
}

#[test]
fn max_depth_limits_recursive_discovery() {
    let dir = tempdir().unwrap();
    let db = dir.path().join("e.db");
    let ws = dir.path().join("ws");
    manifest(&ws, "a/b", "Cargo.toml"); // depth 2
    manifest(&ws, "a/b/c", "go.mod"); // depth 3
    let ws_arg = ws.to_str().unwrap();

    let out = engraph(
        &db,
        &[
            "index",
            "--recursive",
            "--max-depth",
            "2",
            "--dry-run",
            ws_arg,
        ],
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "{out:?}");
    assert!(stdout.contains("discovered 1 repo(s)"), "{stdout}");
    assert!(
        stdout.contains(&ws.join("a/b").display().to_string()),
        "{stdout}"
    );

    // The default depth (16) reaches both.
    let out = engraph(&db, &["index", "--recursive", "--dry-run", ws_arg]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "{out:?}");
    assert!(stdout.contains("discovered 2 repo(s)"), "{stdout}");
}

#[test]
fn max_depth_requires_recursive() {
    let dir = tempdir().unwrap();
    let db = dir.path().join("e.db");
    let ws = dir.path().join("ws");
    manifest(&ws, "a", "Cargo.toml");
    let ws_arg = ws.to_str().unwrap();

    let out = engraph(
        &db,
        &[
            "index",
            "--workspace",
            ws_arg,
            "--max-depth",
            "2",
            "--dry-run",
        ],
    );
    assert!(
        !out.status.success(),
        "accepted --max-depth without --recursive"
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("--recursive"), "{stderr}");
}
