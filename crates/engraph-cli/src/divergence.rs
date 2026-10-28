//! Tie the codegraph to the commit it was indexed at, so a lookup from a
//! worktree on another branch can tell which indexed files no longer match.

use engraph_core::db;
use std::path::Path;

/// `git rev-parse HEAD` of the checkout at `repo`; `None` outside a git repo.
fn git_head(repo: &Path) -> Option<String> {
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["rev-parse", "HEAD"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let sha = String::from_utf8(out.stdout).ok()?;
    let sha = sha.trim();
    (!sha.is_empty()).then(|| sha.to_string())
}

/// Record `repo`'s HEAD as the commit `project` was just indexed at.
pub(crate) fn record_index_commit(
    conn: &db::PooledConn,
    project: &str,
    repo: &Path,
) -> anyhow::Result<()> {
    engraph_codegraph::record_indexed_commit(conn, project, git_head(repo).as_deref())
}
