//! Helpers shared by the integration tests that run the `engraph` binary.

// Each test binary compiles this module and uses only some of it.
#![allow(dead_code)]

use std::io::Write;
use std::path::Path;
use std::process::{Command, Output, Stdio};

pub fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_engraph")
}

/// `engraph <args>` in `cwd`, with a clean environment: only `PATH`, `HOME`,
/// and `db` as the database.
pub fn command(db: &Path, cwd: &Path, args: &[&str]) -> Command {
    let mut c = Command::new(bin());
    c.env_clear()
        .env("PATH", std::env::var("PATH").unwrap_or_default())
        .env("HOME", std::env::var("HOME").unwrap_or_default())
        .env("ENGRAPH_DB_PATH", db)
        .current_dir(cwd)
        .args(args);
    c
}

/// Run `command` to completion.
pub fn engraph(db: &Path, cwd: &Path, args: &[&str]) -> Output {
    command(db, cwd, args).output().expect("spawn engraph")
}

/// Run `command` to completion with `stdin` as its input.
pub fn engraph_stdin(db: &Path, cwd: &Path, args: &[&str], stdin: &str) -> Output {
    let mut child = command(db, cwd, args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn engraph");
    child
        .stdin
        .take()
        .unwrap()
        .write_all(stdin.as_bytes())
        .unwrap();
    child.wait_with_output().unwrap()
}

/// The whitespace-separated fields of the line of `text` whose first field
/// is `key`, for a row of a text table.
pub fn row(text: &str, key: &str) -> Vec<String> {
    text.lines()
        .find(|l| l.split_whitespace().next() == Some(key))
        .unwrap_or_else(|| panic!("no `{key}` row in:\n{text}"))
        .split_whitespace()
        .map(str::to_string)
        .collect()
}

/// `engraph`, with the SCIP scratch file kept out of the user's data dir.
pub fn engraph_temp_scip(db: &Path, cwd: &Path, args: &[&str]) -> Output {
    command(db, cwd, args)
        .env(
            "ENGRAPH_SCIP_OUTPUT_DIR",
            std::env::temp_dir().join("engraph-test-scip"),
        )
        .output()
        .expect("spawn engraph")
}

/// `git -C repo <args>` with a throwaway identity and no signing, so commits
/// work on any machine. Panics on failure; returns trimmed stdout.
pub fn git(repo: &Path, args: &[&str]) -> String {
    let out = engraph_core::utils::git_cmd(repo)
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
        .expect("spawn git");
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}
