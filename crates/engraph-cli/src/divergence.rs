//! Tie the codegraph to the checkout it was indexed from — its project key
//! and commit — so a lookup from a worktree on another branch can tell which
//! indexed files no longer match.

use engraph_codegraph::Neighborhood;
use engraph_core::db;
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

/// Codegraph project key for an index of `repo`: the repo's canonical key (its
/// main worktree, shared by linked worktrees), plus `repo`'s path below its git
/// root when it's a sub-project of a monorepo. Indexers emit paths relative to
/// `repo`, so each sub-project needs its own key or their files collide and
/// indexing one prunes the others.
pub(crate) fn index_project_key(repo: &Path) -> String {
    let key = crate::canonical_repo_key(repo);
    let rel = crate::git_current_toplevel(repo)
        .and_then(|top| top.canonicalize().ok())
        .zip(repo.canonicalize().ok())
        .and_then(|(top, repo)| repo.strip_prefix(&top).ok().map(Path::to_path_buf));
    match rel {
        Some(rel) if !rel.as_os_str().is_empty() => {
            Path::new(&key).join(rel).to_string_lossy().into_owned()
        }
        _ => key,
    }
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

/// Record the commit `project` was just indexed from. Indexers read the working
/// tree, so with uncommitted edits HEAD would flag the very files just indexed;
/// `git stash create` snapshots the tracked working tree as a commit without
/// touching the stash list or the files (empty output when the tree is clean).
pub(crate) fn record_index_commit(
    conn: &db::PooledConn,
    project: &str,
    repo: &Path,
) -> anyhow::Result<()> {
    let commit =
        git_stdout(repo, &["stash", "create"]).or_else(|| git_stdout(repo, &["rev-parse", "HEAD"]));
    engraph_codegraph::record_indexed_commit(conn, project, commit.as_deref())
}

/// The subset of `files` (repo-relative, indexed under `project`) whose copy in
/// the checkout containing `cwd` differs from the commit `project` was indexed
/// at — committed on this branch or not. Empty when no commit was recorded or
/// `cwd` is in another repo: the graph is trusted as before. When git can't
/// compare (the commit was rebased or gc'd away) every file counts as changed,
/// since an unneeded grep costs less than a stale graph.
fn changed_since_index(
    conn: &db::PooledConn,
    project: &str,
    cwd: &Path,
    files: &BTreeSet<&str>,
) -> Vec<String> {
    if files.is_empty() {
        return Vec::new();
    }
    let Ok(Some(sha)) = engraph_codegraph::indexed_commit(conn, project) else {
        return Vec::new();
    };
    if crate::canonical_repo_key(cwd) != project {
        return Vec::new();
    }
    let Some(top) = crate::git_current_toplevel(cwd) else {
        return Vec::new();
    };
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(&top)
        .args(["diff", "--name-only", &sha, "--"])
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
