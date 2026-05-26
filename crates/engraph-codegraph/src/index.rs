use crate::{driver, scip_loader};
use anyhow::{Context, Result};
use engraph_core::db::PooledConn;
use std::path::{Path, PathBuf};
use std::time::Instant;

#[derive(Debug, Default, Clone, Copy)]
pub struct IndexStats {
    pub entities_inserted: usize,
    pub relations_inserted: usize,
    pub scip_bytes: usize,
    pub elapsed_ms: i64,
    /// Whichever driver name actually ran, or "prebuilt" when --scip was used.
    pub driver_name: &'static str,
}

/// One repo's outcome within a workspace run. Failed indexes are reported
/// rather than aborting — a single broken repo shouldn't take the whole
/// workspace down.
#[derive(Debug)]
pub struct WorkspaceRepoResult {
    pub repo: PathBuf,
    pub project: String,
    pub outcome: Result<IndexStats>,
}

#[derive(Debug, Default)]
pub struct WorkspaceStats {
    pub repos: Vec<WorkspaceRepoResult>,
}

impl WorkspaceStats {
    pub fn entities_total(&self) -> usize {
        self.repos
            .iter()
            .filter_map(|r| r.outcome.as_ref().ok())
            .map(|s| s.entities_inserted)
            .sum()
    }
    pub fn relations_total(&self) -> usize {
        self.repos
            .iter()
            .filter_map(|r| r.outcome.as_ref().ok())
            .map(|s| s.relations_inserted)
            .sum()
    }
    pub fn ok_count(&self) -> usize {
        self.repos.iter().filter(|r| r.outcome.is_ok()).count()
    }
    pub fn err_count(&self) -> usize {
        self.repos.iter().filter(|r| r.outcome.is_err()).count()
    }
}

/// Drive a SCIP indexer (or accept a prebuilt index.scip) and load its output
/// into the codegraph tables. `project` is the entity scope key (typically the
/// canonical repo path).
pub fn index_repo(
    conn: &PooledConn,
    repo: &Path,
    scip_override: Option<&Path>,
    lang_override: Option<&str>,
    project: &str,
) -> Result<IndexStats> {
    let start = Instant::now();

    let (scip_path, driver_name): (PathBuf, &'static str) = match scip_override {
        Some(p) => (p.to_path_buf(), "prebuilt"),
        None => {
            let drv = select_driver(repo, lang_override)?;
            let mut cmd = drv.command(repo);
            tracing::info!(driver = drv.name(), ?cmd, "running SCIP indexer");
            let status = cmd
                .status()
                .with_context(|| format!("spawning {}", drv.name()))?;
            if !status.success() {
                anyhow::bail!("{} exited with {}", drv.name(), status);
            }
            (drv.output_path(repo), driver_static_name(drv.name()))
        }
    };

    let bytes = std::fs::read(&scip_path)
        .with_context(|| format!("reading SCIP at {}", scip_path.display()))?;
    let load_stats = scip_loader::load(conn, project, &bytes)?;
    Ok(IndexStats {
        entities_inserted: load_stats.entities_inserted,
        relations_inserted: load_stats.relations_inserted,
        scip_bytes: bytes.len(),
        elapsed_ms: start.elapsed().as_millis() as i64,
        driver_name,
    })
}

/// Discover candidate repo roots under `workspace_root` (Phase 2.2). Rules:
///
/// - If `workspace_root` itself contains a build manifest, return `[workspace_root]`.
///   A Cargo workspace or any single-language root is handled in one pass; the
///   upstream indexer takes care of its internal structure.
/// - Otherwise, enumerate direct children of `workspace_root` and return those
///   whose `Driver::detect()` matches. Sub-recursion is intentionally left out
///   for the MVP; deeper layouts need explicit per-repo invocations until
///   Phase 2.3.
pub fn discover_workspace_repos(workspace_root: &Path) -> Result<Vec<PathBuf>> {
    if !workspace_root.is_dir() {
        anyhow::bail!("workspace root is not a directory: {}", workspace_root.display());
    }
    if driver::registry().iter().any(|d| d.detect(workspace_root)) {
        return Ok(vec![workspace_root.to_path_buf()]);
    }
    let mut out = Vec::new();
    let mut entries: Vec<_> = std::fs::read_dir(workspace_root)
        .with_context(|| format!("reading workspace root {}", workspace_root.display()))?
        .filter_map(|e| e.ok())
        .collect();
    entries.sort_by_key(|e| e.path()); // deterministic ordering for tests
    for entry in entries {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        if driver::registry().iter().any(|d| d.detect(&path)) {
            out.push(path);
        }
    }
    Ok(out)
}

/// Index every repo discovered under `workspace_root`. Each repo's
/// canonical path becomes its `project` key, so the loader's
/// per-project DELETE doesn't trample others. Failures are captured
/// per-repo rather than fatal — a broken indexer for one language
/// shouldn't block the rest of the workspace.
pub fn index_workspace(conn: &PooledConn, workspace_root: &Path) -> Result<WorkspaceStats> {
    let repos = discover_workspace_repos(workspace_root)?;
    if repos.is_empty() {
        anyhow::bail!(
            "no indexable repos found under {} (expected a build manifest at the root \
             or in an immediate child)",
            workspace_root.display()
        );
    }
    let mut stats = WorkspaceStats::default();
    for repo in repos {
        let canonical = repo.canonicalize().unwrap_or_else(|_| repo.clone());
        let project = canonical.to_string_lossy().into_owned();
        let outcome = index_repo(conn, &repo, None, None, &project);
        stats.repos.push(WorkspaceRepoResult {
            repo,
            project,
            outcome,
        });
    }
    Ok(stats)
}

fn select_driver(repo: &Path, lang: Option<&str>) -> Result<Box<dyn driver::Driver>> {
    if let Some(name) = lang {
        return driver::by_name(name).ok_or_else(|| anyhow::anyhow!("unknown driver: {name}"));
    }
    for d in driver::registry() {
        if d.detect(repo) {
            return Ok(d);
        }
    }
    anyhow::bail!(
        "no driver matched {} — pass --lang to force one or --scip <path> to skip detection",
        repo.display()
    )
}

/// Driver names are static strings declared in driver.rs; promote the runtime
/// `&str` back to one for telemetry.
fn driver_static_name(name: &str) -> &'static str {
    match name {
        "rust-analyzer" => "rust-analyzer",
        "scip-python" => "scip-python",
        "scip-go" => "scip-go",
        "scip-typescript" => "scip-typescript",
        "scip-java" => "scip-java",
        _ => "unknown",
    }
}
