use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use std::process::Command;

/// The SHA-256 of `bytes` as 64 lowercase hex digits.
pub fn sha256_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// `git -C dir`, ready for the subcommand and its arguments.
pub fn git_cmd(dir: &Path) -> Command {
    let mut c = Command::new("git");
    c.arg("-C").arg(dir);
    c
}

/// The repo's main worktree path via `git worktree list --porcelain`, whose
/// first entry is always the main worktree. `None` when `start` isn't a git
/// working tree or git can't be run.
///
/// Every worktree of a repo answers the same path, which is what makes it an
/// identity for the repo: the `project` key of the codegraph
/// (`cli::canonical_repo_key`) and the scip-python fallback project name
/// (`codegraph::driver::python_project_name`) both rest on that.
pub fn main_worktree(start: &Path) -> Option<String> {
    let out = git_cmd(start)
        .args(["worktree", "list", "--porcelain"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let stdout = String::from_utf8(out.stdout).ok()?;
    let mut first = stdout.split("\n\n").next()?.lines();
    let path = first.next()?.strip_prefix("worktree ")?.trim();
    if path.is_empty() {
        return None;
    }
    let p = Path::new(path);
    // In a submodule, the first entry is the gitdir (<parent>/.git/modules/
    // <name>), not a checkout. Unlike a bare repo's directory — also listed
    // first, with a `bare` attribute, and kept as the shared key of its
    // worktrees — the gitdir is no key: fall back to this checkout's own
    // root, so the submodule scopes to itself and `in_linked_worktree` stays
    // false for it.
    if !first.any(|l| l.trim() == "bare") && !p.join(".git").exists() {
        return current_toplevel(start).map(|top| {
            top.canonicalize()
                .unwrap_or(top)
                .to_string_lossy()
                .into_owned()
        });
    }
    Some(
        p.canonicalize()
            .unwrap_or_else(|_| p.to_path_buf())
            .to_string_lossy()
            .into_owned(),
    )
}

/// The *current* worktree's root (`git rev-parse --show-toplevel` of `start`) —
/// the checkout you're actually working in, on its branch. Used to resolve a
/// stored relative `file_path` to live source, so subgraph reads the worktree
/// copy, not the (possibly stale or absent) indexed checkout. `None` outside a
/// git working tree.
pub fn current_toplevel(start: &Path) -> Option<PathBuf> {
    let out = git_cmd(start)
        .args(["rev-parse", "--show-toplevel"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let top = String::from_utf8(out.stdout).ok()?;
    let top = top.trim();
    if top.is_empty() {
        return None;
    }
    Some(PathBuf::from(top))
}
