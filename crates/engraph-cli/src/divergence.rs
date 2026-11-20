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
    let out = engraph_core::utils::git_cmd(repo)
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

/// Record the commit `subtree` of `project` was just indexed from, and how
/// (`mode`, so `--if-changed` can repeat it). Indexers
/// read the working tree, so with uncommitted edits HEAD would flag the very
/// files just indexed; `git stash create` snapshots the tracked working tree as
/// a commit without touching the stash list or the files (empty output when the
/// tree is clean).
pub(crate) fn record_index_commit(
    conn: &db::PooledConn,
    project: &str,
    subtree: &str,
    repo: &Path,
    mode: &engraph_codegraph::IndexMode,
) -> anyhow::Result<()> {
    let commit = working_tree_commit(repo);
    engraph_codegraph::record_indexed_commit(conn, project, subtree, commit.as_deref(), mode)
}

/// The commit that contains what the working tree of `repo` holds now: a
/// `git stash create` snapshot when the tree is dirty, else HEAD. What
/// `record_index_commit` records, so uncommitted edits count.
pub(crate) fn working_tree_commit(repo: &Path) -> Option<String> {
    git_stdout(repo, &["stash", "create"]).or_else(|| git_stdout(repo, &["rev-parse", "HEAD"]))
}

/// The **tree hash** of the working tree of `repo`: the content identity the
/// overlay state stores. The tree and not the commit, because a stash commit
/// embeds the time it was made — two of them over one unchanged tree are two
/// commits, and a dirty worktree would always read as stale.
pub(crate) fn working_tree_id(repo: &Path) -> Option<String> {
    let commit = working_tree_commit(repo)?;
    git_stdout(repo, &["rev-parse", &format!("{commit}^{{tree}}")])
}

/// A digest of the `(subtree, commit)` rows of `index_state` for `project` —
/// the base the overlays diff against. A base refresh changes it, which is
/// how `overlay_stale` sees that every overlay must be computed again.
pub(crate) fn base_digest(conn: &db::PooledConn, project: &str) -> String {
    let rows = engraph_codegraph::indexed_commits(conn, project).unwrap_or_default();
    let text: String = rows
        .iter()
        .map(|(sub, sha)| format!("{sub}={sha}\n"))
        .collect();
    engraph_core::utils::sha256_hex(text.as_bytes())
}

/// Whether the overlay of the checkout containing `checkout` no longer
/// describes it (plan 10, part 3, step 12): no overlay run recorded a state,
/// the base graph was indexed again, the tracked working tree changed, or an
/// untracked file was written after the overlay. An untracked file *removed*
/// after the overlay is not seen — the same blind spot the `--if-changed`
/// untracked check has — and heals with the next change.
pub(crate) fn overlay_stale(conn: &db::PooledConn, project: &str, checkout: &Path) -> bool {
    let Some(top) = crate::git_current_toplevel(checkout).and_then(|t| t.canonicalize().ok())
    else {
        // Outside git there is no overlay to be stale.
        return false;
    };
    let worktree = top.to_string_lossy().into_owned();
    let Ok(Some(state)) = engraph_codegraph::overlay_state(conn, &worktree) else {
        return true;
    };
    if state.base_digest != base_digest(conn, project) {
        return true;
    }
    if state.checkout_tree.is_none() || state.checkout_tree != working_tree_id(&top) {
        return true;
    }
    untracked_files(&top, &["."])
        .iter()
        .any(|f| mtime_epoch(&top.join(f)).is_some_and(|m| m > state.indexed_at))
}

/// The most specific indexed subtree of `commits` covering the repo-relative
/// file `f` (`""` covers everything).
fn covering_subtree<'a>(commits: &'a [(String, String)], f: &str) -> Option<&'a (String, String)> {
    commits
        .iter()
        .filter(|(sub, _)| {
            sub.is_empty()
                || f.strip_prefix(sub.as_str())
                    .is_some_and(|r| r.starts_with('/'))
        })
        .max_by_key(|(sub, _)| sub.len())
}

/// Repo-relative paths under `pathspecs` that exist untracked in `top`
/// (`git ls-files --others --exclude-standard`, so ignored files are pruned).
/// Empty when git fails.
fn untracked_files(top: &Path, pathspecs: &[&str]) -> Vec<String> {
    let out = engraph_core::utils::git_cmd(top)
        .args(["ls-files", "--others", "--exclude-standard", "-z", "--"])
        .args(pathspecs)
        .output();
    match out {
        Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout)
            .split('\0')
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .collect(),
        _ => Vec::new(),
    }
}

pub(crate) use engraph_codegraph::OverlayFile;

/// The files of the checkout containing `checkout` that differ from the base
/// commit of `project`: changed, untracked (and not ignored), and deleted, in
/// path order.
///
/// Each file is compared with the commit of the most specific indexed subtree
/// covering it (`covering_subtree`), the same rule the per-file divergence
/// check uses, so a monorepo whose sub-projects were indexed at different
/// commits compares each one with its own. The difference goes in the two
/// directions: a file this checkout changed differs, and so does a file the
/// base checkout changed after this one started.
///
/// Empty when the project has no recorded commit, or `checkout` is in another
/// repo — the same rule `changed_since_index` follows, and the caller must ask
/// for a base before it asks what differs from one. A subtree whose commit git
/// cannot resolve counts every tracked file under it as changed (plan 10,
/// decision 3): a full overlay, which the next refresh of the base heals.
pub(crate) fn overlay_files(
    conn: &db::PooledConn,
    project: &str,
    checkout: &Path,
) -> Vec<OverlayFile> {
    let commits = match engraph_codegraph::indexed_commits(conn, project) {
        Ok(c) if !c.is_empty() => c,
        _ => return Vec::new(),
    };
    if crate::canonical_repo_key(checkout) != project {
        return Vec::new();
    }
    let Some(top) = crate::git_current_toplevel(checkout) else {
        return Vec::new();
    };

    // What differs under each indexed subtree, against that subtree's commit.
    let mut by_subtree: BTreeMap<&str, BTreeMap<String, bool>> = BTreeMap::new();
    for (subtree, sha) in &commits {
        let pathspec = if subtree.is_empty() { "." } else { subtree };
        let found = diff_status(&top, sha, pathspec).unwrap_or_else(|| {
            tracked_files(&top, pathspec)
                .into_iter()
                .map(|p| (p, false))
                .collect()
        });
        by_subtree.insert(subtree, found);
    }
    // An untracked file is in no commit, so no diff reports it; it belongs to
    // the overlay of the subtree covering it.
    for path in untracked_files(&top, &["."]) {
        if let Some((subtree, _)) = covering_subtree(&commits, &path)
            && let Some(files) = by_subtree.get_mut(subtree.as_str())
        {
            files.insert(path, false);
        }
    }

    // Keep each file only from its covering subtree: nested subtrees overlap,
    // and the most specific one has the commit that describes the file.
    let mut out: BTreeMap<String, bool> = BTreeMap::new();
    for (subtree, files) in &by_subtree {
        for (path, deleted) in files {
            let covering = covering_subtree(&commits, path).map(|(s, _)| s.as_str());
            if covering == Some(subtree) {
                out.insert(path.clone(), *deleted);
            }
        }
    }
    out.into_iter()
        .map(|(path, deleted)| OverlayFile { path, deleted })
        .collect()
}

/// `files` without those under a subtree engraph cannot index itself.
///
/// A subtree indexed from prebuilt SCIP (`--scip`, `--scip-manifest`) has base
/// rows from whatever produced that file; an overlay for it would come from
/// engraph's own drivers, whose ids need not be the ids of that indexer, so
/// the overlay rows would not join the base rows. `--if-changed` refuses to
/// refresh such a subtree for the same reason.
pub(crate) fn driver_indexed_only(
    conn: &db::PooledConn,
    project: &str,
    files: Vec<OverlayFile>,
) -> Vec<OverlayFile> {
    let commits = match engraph_codegraph::indexed_commits(conn, project) {
        Ok(c) if !c.is_empty() => c,
        _ => return Vec::new(),
    };
    let modes = engraph_codegraph::index_modes(conn, project).unwrap_or_default();
    files
        .into_iter()
        .filter(|f| {
            let Some((subtree, _)) = covering_subtree(&commits, &f.path) else {
                return false;
            };
            modes
                .get(subtree)
                .map(|m| m.source == engraph_codegraph::IndexSource::Drivers)
                .unwrap_or(true)
        })
        .collect()
}

/// `git diff --name-status <sha> -- <pathspec>` in `top` as `(path, deleted)`
/// pairs. `None` when git cannot compare, which is what the caller turns into
/// a full overlay. Renames are off, so a rename reads as a delete and an add —
/// which is what an overlay stores: the old path hides the base rows, the new
/// path gets rows of its own.
fn diff_status(top: &Path, sha: &str, pathspec: &str) -> Option<BTreeMap<String, bool>> {
    let out = engraph_core::utils::git_cmd(top)
        .args([
            "diff",
            "--name-status",
            "--no-renames",
            "-z",
            sha,
            "--",
            pathspec,
        ])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    // `-z` writes `<status>\0<path>\0` for each file.
    let text = String::from_utf8_lossy(&out.stdout).into_owned();
    let mut fields = text.split('\0').filter(|s| !s.is_empty());
    let mut found = BTreeMap::new();
    while let (Some(status), Some(path)) = (fields.next(), fields.next()) {
        found.insert(path.to_string(), status.starts_with('D'));
    }
    Some(found)
}

/// The tracked files under `pathspec` in `top` (`git ls-files`). Empty when
/// git fails.
fn tracked_files(top: &Path, pathspec: &str) -> Vec<String> {
    let out = engraph_core::utils::git_cmd(top)
        .args(["ls-files", "-z", "--", pathspec])
        .output();
    match out {
        Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout)
            .split('\0')
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .collect(),
        _ => Vec::new(),
    }
}

/// Modification time of `path` as fractional epoch seconds.
fn mtime_epoch(path: &Path) -> Option<f64> {
    path.metadata()
        .ok()?
        .modified()
        .ok()?
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .map(|d| d.as_secs_f64())
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
        if let Some((_, sha)) = covering_subtree(&commits, f) {
            by_commit.entry(sha.as_str()).or_default().push(f);
        }
    }
    let mut changed: Vec<String> = by_commit
        .into_iter()
        .flat_map(|(sha, files)| diff_names(&top, sha, &files))
        .collect();
    // `git diff` never reports a file that is untracked now and absent from
    // the recorded commit (indexed while untracked): compare its mtime with
    // the index time of the covering subtree. A file tracked at index time
    // but untracked now shows up in both lists, hence the dedup.
    let indexed_at = engraph_codegraph::indexed_at_by_subtree(conn, project).unwrap_or_default();
    let all: Vec<&str> = files.iter().copied().collect();
    for f in untracked_files(&top, &all) {
        let Some((sub, _)) = covering_subtree(&commits, &f) else {
            continue;
        };
        let Some(at) = indexed_at.get(sub) else {
            continue;
        };
        if mtime_epoch(&top.join(&f)).is_some_and(|m| m > *at) && !changed.contains(&f) {
            changed.push(f);
        }
    }
    changed
}

/// `git diff --name-only <sha> -- <files>` in `top`. When git can't compare
/// (the commit was rebased or gc'd away) every file counts as changed, since an
/// unneeded grep costs less than a stale graph.
fn diff_names(top: &Path, sha: &str, files: &[&str]) -> Vec<String> {
    let out = engraph_core::utils::git_cmd(top)
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
    let indexed_at = engraph_codegraph::indexed_at_by_subtree(conn, project).unwrap_or_default();
    let modes = engraph_codegraph::index_modes(conn, project).unwrap_or_default();
    commits
        .into_iter()
        .filter(|(subtree, sha)| {
            let exts = engraph_codegraph::source_extensions(
                &top.join(subtree),
                &modes.get(subtree).cloned().unwrap_or_default(),
            );
            subtree_changed(
                &top,
                sha,
                subtree,
                indexed_at.get(subtree).copied().unwrap_or(f64::MAX),
                exts.as_deref(),
            )
        })
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
/// `git diff` never sees untracked files, and the recorded `stash create`
/// commit does not include them either, so a clean diff still counts the
/// subtree as changed when an untracked file under it has an mtime later
/// than `indexed_at` (fractional epoch seconds). With `exts`, only an
/// untracked file with one of those extensions counts (the subtree's
/// indexers read nothing else); `None` counts every untracked file.
fn subtree_changed(
    top: &Path,
    sha: &str,
    subtree: &str,
    indexed_at: f64,
    exts: Option<&[&str]>,
) -> bool {
    let pathspec = if subtree.is_empty() { "." } else { subtree };
    let status = engraph_core::utils::git_cmd(top)
        .args(["diff", "--quiet", sha, "--", pathspec])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
    if !matches!(status, Ok(s) if s.success()) {
        return true;
    }
    let indexable = |f: &str| {
        exts.is_none_or(|e| {
            Path::new(f)
                .extension()
                .and_then(|x| x.to_str())
                .is_some_and(|x| e.contains(&x))
        })
    };
    untracked_files(top, &[pathspec])
        .iter()
        .filter(|f| indexable(f))
        .any(|f| mtime_epoch(&top.join(f)).is_some_and(|m| m > indexed_at))
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

    fn git(repo: &Path, args: &[&str]) -> String {
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
    fn a_nested_git_repo_has_its_own_project_key() {
        let (_dir, repo, _head) = repo();
        let inner = repo.join("inner");
        std::fs::create_dir_all(&inner).unwrap();
        git(&inner, &["init", "-q"]);
        git(&inner, &["commit", "-q", "--allow-empty", "-m", "init"]);

        // The nested repo scopes to itself, not to the repo around it.
        let (key, subtree) = index_scope(&inner);
        assert_eq!(PathBuf::from(&key), inner.canonicalize().unwrap());
        assert_eq!(subtree, "");

        // A directory inside it scopes to the nested repo too.
        std::fs::create_dir_all(inner.join("src")).unwrap();
        let (sub_key, sub_subtree) = index_scope(&inner.join("src"));
        assert_eq!(sub_key, key);
        assert_eq!(sub_subtree, "src");

        // The outer repo keeps its own key.
        assert_eq!(PathBuf::from(index_scope(&repo).0), repo);
    }

    #[test]
    fn a_submodule_scopes_to_its_checkout() {
        let (dir, repo, _head) = repo();
        let upstream = dir.path().join("upstream");
        std::fs::create_dir_all(&upstream).unwrap();
        std::fs::write(upstream.join("lib.rs"), "fn u() {}\n").unwrap();
        git(&upstream, &["init", "-q"]);
        git(&upstream, &["add", "."]);
        git(&upstream, &["commit", "-q", "-m", "init"]);
        git(
            &repo,
            &[
                "-c",
                "protocol.file.allow=always",
                "submodule",
                "add",
                upstream.to_str().unwrap(),
                "sm",
            ],
        );
        let sm = repo.join("sm").canonicalize().unwrap();

        // The key is the submodule checkout, not its gitdir under
        // <parent>/.git/modules.
        let (key, subtree) = index_scope(&sm);
        assert_eq!(PathBuf::from(&key), sm);
        assert_eq!(subtree, "");

        // A directory inside the submodule scopes to the submodule.
        std::fs::create_dir_all(sm.join("src")).unwrap();
        assert_eq!(index_scope(&sm.join("src")), (key, "src".to_string()));
    }

    #[test]
    fn subtree_changed_tracks_edits_under_the_subtree_only() {
        let (_dir, repo, head) = repo();
        // Far-future index time: only tracked changes can flag the subtree.
        let at = f64::MAX;
        assert!(!subtree_changed(&repo, &head, "", at, None));
        assert!(!subtree_changed(&repo, &head, "svc-a", at, None));
        assert!(!subtree_changed(&repo, &head, "svc-b", at, None));

        std::fs::write(repo.join("svc-a/a.rs"), "fn a() { 1 }\n").unwrap();
        assert!(subtree_changed(&repo, &head, "", at, None));
        assert!(subtree_changed(&repo, &head, "svc-a", at, None));
        assert!(!subtree_changed(&repo, &head, "svc-b", at, None));

        // A commit git can't resolve counts as changed.
        assert!(subtree_changed(
            &repo,
            "0000000000000000000000000000000000000000",
            "svc-b",
            at,
            None
        ));
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

    /// Back-date every `indexed_at` so a file written after the index gets an
    /// mtime later than it, whatever the clock granularity.
    fn backdate_index(conn: &db::PooledConn, secs: i64) {
        conn.execute(
            "UPDATE index_state SET indexed_at = datetime('now', ?1 || ' seconds')",
            rusqlite::params![-secs],
        )
        .unwrap();
    }

    /// `overlay_files` as `(path, deleted)` pairs, for readable assertions.
    fn overlay(conn: &db::PooledConn, project: &str, checkout: &Path) -> Vec<(String, bool)> {
        overlay_files(conn, project, checkout)
            .into_iter()
            .map(|f| (f.path, f.deleted))
            .collect()
    }

    /// Plan 10, part 3, step 9: the files of a checkout that differ from the
    /// base commit — changed, untracked, and deleted.
    #[test]
    fn overlay_files_lists_changed_untracked_and_deleted_files() {
        let (dir, repo, head) = repo();
        let pool = db::open_pool(&dir.path().join("t.db")).unwrap();
        let conn = pool.get().unwrap();
        let project = crate::canonical_repo_key(&repo);
        engraph_codegraph::record_indexed_commit(
            &conn,
            &project,
            "",
            Some(&head),
            &Default::default(),
        )
        .unwrap();

        // The checkout is the indexed commit: nothing differs.
        assert!(overlay(&conn, &project, &repo).is_empty());

        // A changed file, a deleted file, an untracked file, and an ignored
        // file, which does not count.
        std::fs::write(repo.join("svc-a/a.rs"), "fn a() { 1 }\n").unwrap();
        std::fs::remove_file(repo.join("svc-b/b.rs")).unwrap();
        std::fs::write(repo.join("svc-a/gen.rs"), "fn gen() {}\n").unwrap();
        std::fs::write(repo.join(".gitignore"), "*.tmp\n").unwrap();
        std::fs::write(repo.join("svc-a/x.tmp"), "tmp\n").unwrap();
        assert_eq!(
            overlay(&conn, &project, &repo),
            vec![
                (".gitignore".to_string(), false),
                ("svc-a/a.rs".to_string(), false),
                ("svc-a/gen.rs".to_string(), false),
                ("svc-b/b.rs".to_string(), true),
            ]
        );

        // Another repo's checkout says nothing about this project.
        let other = tempfile::tempdir().unwrap();
        git(other.path(), &["init", "-q"]);
        assert!(overlay(&conn, &project, other.path()).is_empty());
    }

    /// Each file is compared with the commit of the most specific subtree
    /// covering it, and it is listed once.
    #[test]
    fn overlay_files_uses_the_commit_of_the_covering_subtree() {
        let (dir, repo, head) = repo();
        let pool = db::open_pool(&dir.path().join("t.db")).unwrap();
        let conn = pool.get().unwrap();
        let project = crate::canonical_repo_key(&repo);

        // svc-b is indexed at a later commit than the whole project.
        std::fs::write(repo.join("svc-b/b.rs"), "fn b() { 2 }\n").unwrap();
        git(&repo, &["commit", "-q", "-am", "b"]);
        let head2 = git(&repo, &["rev-parse", "HEAD"]);
        for (subtree, sha) in [("", &head), ("svc-b", &head2)] {
            engraph_codegraph::record_indexed_commit(
                &conn,
                &project,
                subtree,
                Some(sha),
                &Default::default(),
            )
            .unwrap();
        }

        // svc-b matches its own commit, so only the file that the first
        // commit does not have differs — and svc-b/b.rs is not listed twice
        // although the "" row is at the older commit.
        assert!(overlay(&conn, &project, &repo).is_empty());

        std::fs::write(repo.join("svc-b/b.rs"), "fn b() { 3 }\n").unwrap();
        assert_eq!(
            overlay(&conn, &project, &repo),
            vec![("svc-b/b.rs".to_string(), false)]
        );
    }

    /// A worktree on another branch differs in the two directions: its own
    /// changes, and the files the base checkout changed after it started.
    #[test]
    fn overlay_files_covers_both_directions_of_the_difference() {
        let (dir, repo, head) = repo();
        let pool = db::open_pool(&dir.path().join("t.db")).unwrap();
        let conn = pool.get().unwrap();
        let project = crate::canonical_repo_key(&repo);
        let wt = dir.path().join("wt");
        git(
            &repo,
            &[
                "worktree",
                "add",
                "-q",
                "-b",
                "feature",
                wt.to_str().unwrap(),
            ],
        );

        // The base moves on: svc-a/a.rs changes after the worktree started.
        std::fs::write(repo.join("svc-a/a.rs"), "fn a() { base }\n").unwrap();
        git(&repo, &["commit", "-q", "-am", "base work"]);
        let base_head = git(&repo, &["rev-parse", "HEAD"]);
        assert_ne!(base_head, head);
        engraph_codegraph::record_indexed_commit(
            &conn,
            &project,
            "",
            Some(&base_head),
            &Default::default(),
        )
        .unwrap();

        // The worktree changed svc-b/b.rs itself.
        std::fs::write(wt.join("svc-b/b.rs"), "fn b() { feature }\n").unwrap();
        assert_eq!(
            overlay(&conn, &project, &wt),
            vec![
                ("svc-a/a.rs".to_string(), false),
                ("svc-b/b.rs".to_string(), false),
            ]
        );
    }

    /// A base commit git cannot resolve (`git gc` of a `git stash create`
    /// commit) makes every tracked file of the subtree differ — the full
    /// overlay that decision 3 of the plan accepts.
    #[test]
    fn overlay_files_falls_back_to_every_file_when_the_base_commit_is_gone() {
        let (dir, repo, _head) = repo();
        let pool = db::open_pool(&dir.path().join("t.db")).unwrap();
        let conn = pool.get().unwrap();
        let project = crate::canonical_repo_key(&repo);
        engraph_codegraph::record_indexed_commit(
            &conn,
            &project,
            "",
            Some("0000000000000000000000000000000000000000"),
            &Default::default(),
        )
        .unwrap();
        assert_eq!(
            overlay(&conn, &project, &repo),
            vec![
                ("svc-a/a.rs".to_string(), false),
                ("svc-b/b.rs".to_string(), false),
            ]
        );
    }

    /// Plan 10, part 3, steps 11 and 12: the overlay of a worktree is stale
    /// when the checkout changed after it, or when the base graph did.
    #[test]
    fn overlay_stale_follows_the_checkout_and_the_base() {
        let (dir, repo, head) = repo();
        let pool = db::open_pool(&dir.path().join("t.db")).unwrap();
        let conn = pool.get().unwrap();
        let project = crate::canonical_repo_key(&repo);
        engraph_codegraph::record_indexed_commit(
            &conn,
            &project,
            "",
            Some(&head),
            &Default::default(),
        )
        .unwrap();
        let wt = dir.path().join("wt");
        git(
            &repo,
            &[
                "worktree",
                "add",
                "-q",
                "-b",
                "feature",
                wt.to_str().unwrap(),
            ],
        );
        let wt = wt.canonicalize().unwrap();
        let worktree = wt.to_string_lossy().into_owned();
        let record = |conn: &db::PooledConn| {
            engraph_codegraph::record_overlay_state(
                conn,
                &worktree,
                &project,
                working_tree_id(&wt).as_deref(),
                &base_digest(conn, &project),
            )
            .unwrap();
        };

        // No overlay run yet: stale.
        assert!(overlay_stale(&conn, &project, &wt));
        record(&conn);
        assert!(!overlay_stale(&conn, &project, &wt));

        // An edit to a tracked file: stale, although nothing is committed —
        // and recording again makes it fresh, although the stash commit of an
        // unchanged dirty tree is a new commit each time (the tree is not).
        std::fs::write(wt.join("svc-a/a.rs"), "fn a() { 1 }\n").unwrap();
        assert!(overlay_stale(&conn, &project, &wt));
        record(&conn);
        assert!(!overlay_stale(&conn, &project, &wt));

        // A new untracked file, written after the overlay: stale. The
        // backdate makes the mtime comparison decisive, whatever the clock
        // granularity.
        conn.execute(
            "UPDATE overlay_state SET indexed_at = datetime('now', '-10 seconds')",
            [],
        )
        .unwrap();
        assert!(!overlay_stale(&conn, &project, &wt));
        std::fs::write(wt.join("svc-a/gen.rs"), "fn gen() {}\n").unwrap();
        assert!(overlay_stale(&conn, &project, &wt));
        record(&conn);
        std::fs::File::options()
            .write(true)
            .open(wt.join("svc-a/gen.rs"))
            .unwrap()
            .set_modified(std::time::SystemTime::now() - std::time::Duration::from_secs(60))
            .unwrap();
        assert!(!overlay_stale(&conn, &project, &wt));

        // The base was indexed again at another commit: every overlay is
        // stale, this worktree's included, although the worktree is unchanged.
        git(
            &repo,
            &["commit", "-q", "--allow-empty", "-m", "base moves"],
        );
        let head2 = git(&repo, &["rev-parse", "HEAD"]);
        engraph_codegraph::record_indexed_commit(
            &conn,
            &project,
            "",
            Some(&head2),
            &Default::default(),
        )
        .unwrap();
        assert!(overlay_stale(&conn, &project, &wt));
        record(&conn);
        assert!(!overlay_stale(&conn, &project, &wt));
    }

    #[test]
    fn changed_subtrees_flags_a_new_untracked_file() {
        let (dir, repo, head) = repo();
        let pool = db::open_pool(&dir.path().join("t.db")).unwrap();
        let conn = pool.get().unwrap();
        let project = crate::canonical_repo_key(&repo);
        for sub in ["svc-a", "svc-b"] {
            engraph_codegraph::record_indexed_commit(
                &conn,
                &project,
                sub,
                Some(&head),
                &Default::default(),
            )
            .unwrap();
        }
        backdate_index(&conn, 10);
        assert!(changed_subtrees(&conn, &project, &repo).is_empty());

        // Ignored files do not count as changes.
        std::fs::write(repo.join(".gitignore"), "*.tmp\n").unwrap();
        std::fs::write(repo.join("svc-b/x.tmp"), "tmp\n").unwrap();
        assert!(changed_subtrees(&conn, &project, &repo).is_empty());

        // A new untracked file is invisible to `git diff`, but it must start
        // a refresh of its subtree.
        std::fs::write(repo.join("svc-b/gen.rs"), "fn gen() {}\n").unwrap();
        assert_eq!(changed_subtrees(&conn, &project, &repo), vec!["svc-b"]);

        // Re-recorded: the file is now older than the index.
        engraph_codegraph::record_indexed_commit(
            &conn,
            &project,
            "svc-b",
            Some(&head),
            &Default::default(),
        )
        .unwrap();
        assert!(changed_subtrees(&conn, &project, &repo).is_empty());
    }

    #[test]
    fn changed_subtrees_counts_only_untracked_files_its_indexer_reads() {
        let (dir, repo, _) = repo();
        // svc-b is a Rust crate: rust-analyzer reads `.rs` files only.
        std::fs::write(repo.join("svc-b/Cargo.toml"), "[package]\n").unwrap();
        git(&repo, &["add", "."]);
        git(&repo, &["commit", "-q", "-m", "crate"]);
        let head = git(&repo, &["rev-parse", "HEAD"]);
        let pool = db::open_pool(&dir.path().join("t.db")).unwrap();
        let conn = pool.get().unwrap();
        let project = crate::canonical_repo_key(&repo);
        engraph_codegraph::record_indexed_commit(
            &conn,
            &project,
            "svc-b",
            Some(&head),
            &Default::default(),
        )
        .unwrap();
        backdate_index(&conn, 10);

        // A note or a log is not source: no refresh.
        std::fs::write(repo.join("svc-b/notes.md"), "todo\n").unwrap();
        std::fs::write(repo.join("svc-b/run.log"), "x\n").unwrap();
        assert!(changed_subtrees(&conn, &project, &repo).is_empty());

        // A new `.rs` file is.
        std::fs::write(repo.join("svc-b/gen.rs"), "fn gen() {}\n").unwrap();
        assert_eq!(changed_subtrees(&conn, &project, &repo), vec!["svc-b"]);
    }

    #[test]
    fn changed_since_index_flags_a_modified_untracked_indexed_file() {
        let (dir, repo, head) = repo();
        let pool = db::open_pool(&dir.path().join("t.db")).unwrap();
        let conn = pool.get().unwrap();
        let project = crate::canonical_repo_key(&repo);
        // `svc-a/gen.rs` was untracked when it was indexed: it is in the
        // graph, but neither HEAD nor the recorded commit contains it.
        // Its mtime is set a minute back: `indexed_at` has millisecond
        // precision and APFS/NTFS mtimes are finer, so a file written in the
        // same millisecond as the record would read as newer than the index.
        let gen_rs = repo.join("svc-a/gen.rs");
        std::fs::write(&gen_rs, "fn gen() {}\n").unwrap();
        std::fs::File::options()
            .write(true)
            .open(&gen_rs)
            .unwrap()
            .set_modified(std::time::SystemTime::now() - std::time::Duration::from_secs(60))
            .unwrap();
        engraph_codegraph::record_indexed_commit(
            &conn,
            &project,
            "",
            Some(&head),
            &Default::default(),
        )
        .unwrap();
        let files: BTreeSet<&str> = ["svc-a/gen.rs", "svc-a/a.rs"].into_iter().collect();
        // Indexed after the file was written: nothing diverged.
        assert!(changed_since_index(&conn, &project, &repo, &files).is_empty());
        // Modified after the index: the untracked file counts as diverged;
        // the tracked unmodified one does not.
        backdate_index(&conn, 10);
        std::fs::write(&gen_rs, "fn gen() { 1 }\n").unwrap();
        assert_eq!(
            changed_since_index(&conn, &project, &repo, &files),
            vec!["svc-a/gen.rs"]
        );
    }

    #[test]
    fn changed_subtrees_lists_subtrees_with_changes_since_their_commit() {
        let (dir, repo, head) = repo();
        let pool = db::open_pool(&dir.path().join("t.db")).unwrap();
        let conn = pool.get().unwrap();
        let project = crate::canonical_repo_key(&repo);
        assert!(changed_subtrees(&conn, &project, &repo).is_empty());

        engraph_codegraph::record_indexed_commit(
            &conn,
            &project,
            "svc-a",
            Some(&head),
            &Default::default(),
        )
        .unwrap();
        engraph_codegraph::record_indexed_commit(
            &conn,
            &project,
            "svc-b",
            Some(&head),
            &Default::default(),
        )
        .unwrap();
        assert!(changed_subtrees(&conn, &project, &repo).is_empty());

        std::fs::write(repo.join("svc-b/b.rs"), "fn b() { 2 }\n").unwrap();
        assert_eq!(changed_subtrees(&conn, &project, &repo), vec!["svc-b"]);

        // svc-b re-indexed at the edited state: nothing is stale any more.
        git(&repo, &["commit", "-q", "-am", "b"]);
        let head2 = git(&repo, &["rev-parse", "HEAD"]);
        engraph_codegraph::record_indexed_commit(
            &conn,
            &project,
            "svc-b",
            Some(&head2),
            &Default::default(),
        )
        .unwrap();
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
