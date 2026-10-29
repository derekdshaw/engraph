//! Indexing sub-projects of a monorepo one at a time:
//! - a relative sub-project path indexes that sub-project (not an empty index)

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

#[test]
fn relative_subproject_path_indexes_the_subproject() {
    let probe = Command::new("rust-analyzer").arg("--version").output();
    if !probe.map(|o| o.status.success()).unwrap_or(false) {
        eprintln!("skip: rust-analyzer not installed");
        return;
    }
    let (dir, mono) = mono();
    let db = dir.path().join("e.db");
    let out = engraph(&db, &mono, &["index", "svc-a"]);
    assert!(
        out.status.success(),
        "index: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(
        entity_count(&db, "alpha_handler"),
        1,
        "{}",
        String::from_utf8_lossy(&out.stdout)
    );
}
