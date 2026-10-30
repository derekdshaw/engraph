//! Tie the codegraph to the checkout it was indexed from — its project key
//! and commit — so a lookup from a worktree on another branch can tell which
//! indexed files no longer match.

use engraph_codegraph::Neighborhood;
use engraph_core::db;
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

/// Codegraph `(project, subtree)` for an index of `repo`: the repo's canonical
/// key (its main worktree, shared by linked worktrees), and `repo`'s path below
/// its git root — empty at the root. A monorepo sub-project thus refreshes its
/// part of the one repo index, with paths rebased under its directory.
pub(crate) fn index_scope(repo: &Path) -> (String, String) {
    let rel = crate::git_current_toplevel(repo)
        .and_then(|top| top.canonicalize().ok())
        .zip(repo.canonicalize().ok())
        .and_then(|(top, repo)| repo.strip_prefix(&top).ok().map(slash_path));
    (crate::canonical_repo_key(repo), rel.unwrap_or_default())
}

/// Whether `dir` is inside a linked worktree rather than its repo's main
/// worktree. The codegraph is one index per repo under the main worktree's key,
/// so indexing from a linked worktree replaces the code every other worktree
/// sees with this branch's. Those worktrees rely on the per-file divergence
/// check instead. `false` outside git.
pub(crate) fn in_linked_worktree(dir: &Path) -> bool {
    let Some(top) = crate::git_current_toplevel(dir).and_then(|t| t.canonicalize().ok()) else {
        return false;
    };
    Path::new(&crate::canonical_repo_key(dir)) != top
}

/// Where the codegraph stores an absolute `file`: `(project, repo-relative
/// path)` of the git checkout it's in — a linked worktree maps to its repo's
/// key. `None` for relative paths or outside git.
pub(crate) fn indexed_location(file: &Path) -> Option<(String, String)> {
    if !file.is_absolute() {
        return None;
    }
    let dir = file.parent()?;
    let top = crate::git_current_toplevel(dir)?.canonicalize().ok()?;
    let rel = file.canonicalize().ok()?;
    let rel = slash_path(rel.strip_prefix(&top).ok()?);
    Some((crate::canonical_repo_key(dir), rel))
}

/// A relative path as the codegraph stores it: `/`-separated on every platform
/// (Windows `strip_prefix` yields `src\lib.rs`).
fn slash_path(rel: &Path) -> String {
    rel.components()
        .map(|c| c.as_os_str().to_string_lossy())
        .collect::<Vec<_>>()
        .join("/")
}

/// Trimmed stdout of `git -C repo <args>`; `None` on failure or empty output.
fn git_stdout(repo: &Path, args: &[&str]) -> Option<String> {
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8(out.stdout).ok()?;
    let s = s.trim();
    (!s.is_empty()).then(|| s.to_string())
}

/// Record the commit `subtree` of `project` was just indexed from. Indexers
/// read the working tree, so with uncommitted edits HEAD would flag the very
/// files just indexed; `git stash create` snapshots the tracked working tree as
/// a commit without touching the stash list or the files (empty output when the
/// tree is clean).
pub(crate) fn record_index_commit(
    conn: &db::PooledConn,
    project: &str,
    subtree: &str,
    repo: &Path,
) -> anyhow::Result<()> {
    let commit =
        git_stdout(repo, &["stash", "create"]).or_else(|| git_stdout(repo, &["rev-parse", "HEAD"]));
    engraph_codegraph::record_indexed_commit(conn, project, subtree, commit.as_deref())
}

/// The subset of `files` (repo-relative, indexed under `project`) whose copy in
/// the checkout containing `cwd` differs from the commit they were indexed at —
/// committed on this branch or not. Each file is compared with the commit of
/// the most specific indexed subtree covering it. Empty when no commit was
/// recorded or `cwd` is in another repo: the graph is trusted as before.
fn changed_since_index(
    conn: &db::PooledConn,
    project: &str,
    cwd: &Path,
    files: &BTreeSet<&str>,
) -> Vec<String> {
    if files.is_empty() {
        return Vec::new();
    }
    let commits = match engraph_codegraph::indexed_commits(conn, project) {
        Ok(c) if !c.is_empty() => c,
        _ => return Vec::new(),
    };
    if crate::canonical_repo_key(cwd) != project {
        return Vec::new();
    }
    let Some(top) = crate::git_current_toplevel(cwd) else {
        return Vec::new();
    };
    let mut by_commit: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
    for &f in files {
        let covering = commits
            .iter()
            .filter(|(sub, _)| {
                sub.is_empty()
                    || f.strip_prefix(sub.as_str())
                        .is_some_and(|r| r.starts_with('/'))
            })
            .max_by_key(|(sub, _)| sub.len());
        if let Some((_, sha)) = covering {
            by_commit.entry(sha).or_default().push(f);
        }
    }
    by_commit
        .into_iter()
        .flat_map(|(sha, files)| diff_names(&top, sha, &files))
        .collect()
}

/// `git diff --name-only <sha> -- <files>` in `top`. When git can't compare
/// (the commit was rebased or gc'd away) every file counts as changed, since an
/// unneeded grep costs less than a stale graph.
fn diff_names(top: &Path, sha: &str, files: &[&str]) -> Vec<String> {
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(top)
        .args(["diff", "--name-only", sha, "--"])
        .args(files)
        .output();
    match out {
        Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout)
            .lines()
            .map(str::to_string)
            .collect(),
        _ => files.iter().map(|f| f.to_string()).collect(),
    }
}

/// The indexed subtrees of `project` (`""` for a whole-project index) whose
/// contents in the checkout containing `cwd` differ from the commit they were
/// indexed at, in `index_state` order. One `git diff --quiet <commit> --
/// <subtree>` per row: exit 0 is unchanged, exit 1 is changed. A subtree whose
/// commit git can't compare (rebased or gc'd away) counts as changed, as in
/// `diff_names`. Empty when no commit was recorded or `cwd` is in another repo.
pub(crate) fn changed_subtrees(conn: &db::PooledConn, project: &str, cwd: &Path) -> Vec<String> {
    let commits = match engraph_codegraph::indexed_commits(conn, project) {
        Ok(c) if !c.is_empty() => c,
        _ => return Vec::new(),
    };
    if crate::canonical_repo_key(cwd) != project {
        return Vec::new();
    }
    let Some(top) = crate::git_current_toplevel(cwd) else {
        return Vec::new();
    };
    commits
        .into_iter()
        .filter(|(subtree, sha)| subtree_changed(&top, sha, subtree))
        .map(|(subtree, _)| subtree)
        .collect()
}

/// `subtrees` without those nested under another entry (`""` covers all):
/// re-indexing a parent re-indexes its children, so running both is wasted
/// work. Order is preserved.
pub(crate) fn outermost_subtrees(subtrees: Vec<String>) -> Vec<String> {
    let covered = |s: &str| {
        subtrees.iter().any(|t| {
            t != s
                && (t.is_empty()
                    || s.strip_prefix(t.as_str())
                        .is_some_and(|r| r.starts_with('/')))
        })
    };
    let keep: Vec<bool> = subtrees.iter().map(|s| !covered(s)).collect();
    subtrees
        .into_iter()
        .zip(keep)
        .filter_map(|(s, k)| k.then_some(s))
        .collect()
}

/// `git diff --quiet <sha> -- <subtree>` in `top`; the whole-project subtree
/// `""` diffs `.` (git rejects an empty pathspec). Only exit 0 means unchanged.
fn subtree_changed(top: &Path, sha: &str, subtree: &str) -> bool {
    let pathspec = if subtree.is_empty() { "." } else { subtree };
    let status = std::process::Command::new("git")
        .arg("-C")
        .arg(top)
        .args(["diff", "--quiet", sha, "--", pathspec])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
    !matches!(status, Ok(s) if s.success())
}

/// Indexed files grouped by project, from `(project, file_path)` pairs.
fn files_by_project<'a>(
    rows: impl IntoIterator<Item = (Option<&'a str>, Option<&'a str>)>,
) -> BTreeMap<&'a str, BTreeSet<&'a str>> {
    let mut by_project: BTreeMap<&str, BTreeSet<&str>> = BTreeMap::new();
    for (project, file) in rows {
        if let (Some(p), Some(f)) = (project, file) {
            by_project.entry(p).or_default().insert(f);
        }
    }
    by_project
}

/// Whether any of the `(project, file_path)` rows names a file `cwd`'s
/// checkout has changed since the index.
pub(crate) fn any_changed_since_index<'a>(
    conn: &db::PooledConn,
    cwd: &Path,
    rows: impl IntoIterator<Item = (Option<&'a str>, Option<&'a str>)>,
) -> bool {
    files_by_project(rows)
        .iter()
        .any(|(project, files)| !changed_since_index(conn, project, cwd, files).is_empty())
}

/// A warning to put above a subgraph when files it draws on have changed in
/// `cwd`'s checkout since the index, naming them; `None` when none have.
pub(crate) fn stale_note(conn: &db::PooledConn, cwd: &Path, n: &Neighborhood) -> Option<String> {
    let rows = n
        .matches
        .iter()
        .chain(n.outgoing.iter().map(|e| &e.other))
        .chain(n.incoming.iter().map(|e| &e.other))
        .chain(n.siblings.iter())
        .map(|r| (r.project.as_deref(), r.file_path.as_deref()));
    let changed: Vec<String> = files_by_project(rows)
        .iter()
        .flat_map(|(project, files)| changed_since_index(conn, project, cwd, files))
        .collect();
    if changed.is_empty() {
        return None;
    }
    let list = changed
        .iter()
        .map(|f| format!("`{f}`"))
        .collect::<Vec<_>>()
        .join(", ");
    Some(format!(
        "> **Changed since the index:** {list} differ in this checkout from the commit the \
         graph was built at. Signatures, line numbers and edges from them may be stale; read \
         the file for current code.\n\n"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::process::Command;

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

    /// A repo with `svc-a/a.rs` and `svc-b/b.rs` committed; returns its
    /// canonical path and HEAD.
    fn repo() -> (tempfile::TempDir, PathBuf, String) {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().join("repo");
        std::fs::create_dir_all(repo.join("svc-a")).unwrap();
        std::fs::create_dir_all(repo.join("svc-b")).unwrap();
        std::fs::write(repo.join("svc-a/a.rs"), "fn a() {}\n").unwrap();
        std::fs::write(repo.join("svc-b/b.rs"), "fn b() {}\n").unwrap();
        git(&repo, &["init", "-q"]);
        git(&repo, &["add", "."]);
        git(&repo, &["commit", "-q", "-m", "init"]);
        let head = git(&repo, &["rev-parse", "HEAD"]);
        let repo = repo.canonicalize().unwrap();
        (dir, repo, head)
    }

    #[test]
    fn subtree_changed_tracks_edits_under_the_subtree_only() {
        let (_dir, repo, head) = repo();
        assert!(!subtree_changed(&repo, &head, ""));
        assert!(!subtree_changed(&repo, &head, "svc-a"));
        assert!(!subtree_changed(&repo, &head, "svc-b"));

        std::fs::write(repo.join("svc-a/a.rs"), "fn a() { 1 }\n").unwrap();
        assert!(subtree_changed(&repo, &head, ""));
        assert!(subtree_changed(&repo, &head, "svc-a"));
        assert!(!subtree_changed(&repo, &head, "svc-b"));

        // A commit git can't resolve counts as changed.
        assert!(subtree_changed(
            &repo,
            "0000000000000000000000000000000000000000",
            "svc-b"
        ));
    }

    #[test]
    fn in_linked_worktree_is_false_in_the_main_worktree_only() {
        let (dir, repo, _) = repo();
        assert!(!in_linked_worktree(&repo));
        assert!(!in_linked_worktree(&repo.join("svc-a")));
        let wt = dir.path().join("wt");
        git(&repo, &["worktree", "add", "-q", "-b", "feature", wt.to_str().unwrap()]);
        assert!(in_linked_worktree(&wt));
        assert!(in_linked_worktree(&wt.join("svc-a")));
        // Outside git there is no worktree to be linked.
        let plain = tempfile::tempdir().unwrap();
        assert!(!in_linked_worktree(plain.path()));
    }

    #[test]
    fn outermost_subtrees_drops_nested_entries() {
        let v = |a: &[&str]| a.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(
            outermost_subtrees(v(&["svc-a", "svc-b"])),
            v(&["svc-a", "svc-b"])
        );
        assert_eq!(
            outermost_subtrees(v(&["svc-a", "svc-a/sub"])),
            v(&["svc-a"])
        );
        assert_eq!(
            outermost_subtrees(v(&["group/svc-c", "group"])),
            v(&["group"])
        );
        // A name prefix is not a directory prefix.
        assert_eq!(
            outermost_subtrees(v(&["svc", "svc-a"])),
            v(&["svc", "svc-a"])
        );
        assert_eq!(outermost_subtrees(v(&["svc-a", "", "svc-b"])), v(&[""]));
        assert!(outermost_subtrees(Vec::new()).is_empty());
    }

    #[test]
    fn changed_subtrees_lists_subtrees_with_changes_since_their_commit() {
        let (dir, repo, head) = repo();
        let pool = db::open_pool(&dir.path().join("t.db")).unwrap();
        let conn = pool.get().unwrap();
        let project = crate::canonical_repo_key(&repo);
        assert!(changed_subtrees(&conn, &project, &repo).is_empty());

        engraph_codegraph::record_indexed_commit(&conn, &project, "svc-a", Some(&head)).unwrap();
        engraph_codegraph::record_indexed_commit(&conn, &project, "svc-b", Some(&head)).unwrap();
        assert!(changed_subtrees(&conn, &project, &repo).is_empty());

        std::fs::write(repo.join("svc-b/b.rs"), "fn b() { 2 }\n").unwrap();
        assert_eq!(changed_subtrees(&conn, &project, &repo), vec!["svc-b"]);

        // svc-b re-indexed at the edited state: nothing is stale any more.
        git(&repo, &["commit", "-q", "-am", "b"]);
        let head2 = git(&repo, &["rev-parse", "HEAD"]);
        engraph_codegraph::record_indexed_commit(&conn, &project, "svc-b", Some(&head2)).unwrap();
        assert!(changed_subtrees(&conn, &project, &repo).is_empty());
        // ...while svc-a, still indexed at the first commit, is unaffected by
        // a commit that only touched svc-b.
        std::fs::write(repo.join("svc-a/a.rs"), "fn a() { 1 }\n").unwrap();
        assert_eq!(changed_subtrees(&conn, &project, &repo), vec!["svc-a"]);

        // Another repo's checkout says nothing about this project.
        let other = tempfile::tempdir().unwrap();
        git(other.path(), &["init", "-q"]);
        assert!(changed_subtrees(&conn, &project, other.path()).is_empty());
    }
}
