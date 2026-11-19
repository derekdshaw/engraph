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

/// The local ref of the repo's default branch, as `refs/heads/<name>`, or
/// `None` outside git and when no candidate branch is there.
///
/// The base graph of plan 10 follows this ref, not the branch of a checkout:
/// a feature branch in the main worktree then does not become the base. The
/// name comes from `refs/remotes/origin/HEAD` (what the remote calls its
/// default branch), else `init.defaultBranch`, else `main` or `master`. The
/// ref is the **local** one, because engraph does not fetch: the base changes
/// when the local ref moves, which a merge or a pull does.
pub fn default_branch_ref(repo: &Path) -> Option<String> {
    let from_origin = git_stdout(repo, &["symbolic-ref", "--quiet", "--short", ORIGIN_HEAD])
        .and_then(|head| head.strip_prefix("origin/").map(str::to_string));
    let configured = || git_stdout(repo, &["config", "--get", "init.defaultBranch"]);
    from_origin
        .or_else(configured)
        .into_iter()
        .chain(["main".to_string(), "master".to_string()])
        .map(|name| format!("refs/heads/{name}"))
        .find(|r| branch_exists(repo, r))
}

const ORIGIN_HEAD: &str = "refs/remotes/origin/HEAD";

/// Whether `git_ref` names a commit in `repo`.
fn branch_exists(repo: &Path, git_ref: &str) -> bool {
    git_stdout(repo, &["rev-parse", "--verify", "--quiet", git_ref]).is_some()
}

/// Trimmed stdout of `git -C repo <args>`; `None` on failure or empty output.
fn git_stdout(repo: &Path, args: &[&str]) -> Option<String> {
    let out = git_cmd(repo).args(args).output().ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8(out.stdout).ok()?;
    let s = s.trim();
    (!s.is_empty()).then(|| s.to_string())
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

#[cfg(test)]
mod tests {
    use super::*;

    fn git(repo: &Path, args: &[&str]) {
        let out = git_cmd(repo)
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
    }

    /// A repo with one commit on `main`.
    fn repo(dir: &Path) -> PathBuf {
        let repo = dir.join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        git(&repo, &["init", "-q", "-b", "main"]);
        git(&repo, &["commit", "-q", "--allow-empty", "-m", "init"]);
        repo
    }

    /// The base follows the default branch, not the branch of the checkout:
    /// this is what keeps a feature branch in the main worktree out of the
    /// base graph (plan 10, decision 1).
    #[test]
    fn default_branch_ref_is_main_on_a_feature_branch() {
        let dir = tempfile::tempdir().unwrap();
        let repo = repo(dir.path());
        git(&repo, &["checkout", "-q", "-b", "feature"]);
        assert_eq!(
            default_branch_ref(&repo).as_deref(),
            Some("refs/heads/main")
        );
    }

    /// `refs/remotes/origin/HEAD` names the remote's default branch, and it
    /// wins over `main` — but the ref recorded is the local one.
    #[test]
    fn default_branch_ref_follows_origin_head() {
        let dir = tempfile::tempdir().unwrap();
        let repo = repo(dir.path());
        git(&repo, &["branch", "trunk"]);
        git(
            &repo,
            &[
                "symbolic-ref",
                "refs/remotes/origin/HEAD",
                "refs/remotes/origin/trunk",
            ],
        );
        assert_eq!(
            default_branch_ref(&repo).as_deref(),
            Some("refs/heads/trunk")
        );

        // origin names a branch this checkout does not have: fall back to the
        // candidates that are there.
        git(&repo, &["branch", "-D", "-q", "trunk"]);
        assert_eq!(
            default_branch_ref(&repo).as_deref(),
            Some("refs/heads/main")
        );
    }

    #[test]
    fn default_branch_ref_is_none_without_a_candidate_branch() {
        let dir = tempfile::tempdir().unwrap();
        let repo = repo(dir.path());
        git(&repo, &["checkout", "-q", "-b", "feature"]);
        git(&repo, &["branch", "-D", "-q", "main"]);
        assert_eq!(default_branch_ref(&repo), None);
        // Outside git there is no default branch either.
        assert_eq!(default_branch_ref(dir.path()), None);
    }
}
