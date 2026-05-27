use crate::{bazel, bazel_symbols, driver, scip_loader};
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
    bazel_symbols: bool,
) -> Result<IndexStats> {
    let start = Instant::now();

    // Phase 2.3: a Bazel workspace takes the bazel-query path unless the
    // caller explicitly passed --scip <path> (which says "load this SCIP
    // file no matter what") or --lang <name> (which forces a specific SCIP
    // driver). Target-level always runs; symbol-level (Phase 2.3 #2) layers
    // on top when --bazel-symbols is set.
    if scip_override.is_none() && lang_override.is_none() && bazel::detect_bazel(repo) {
        tracing::info!("Bazel workspace detected; running target-level index");
        let target_stats = bazel::index_bazel_workspace(conn, repo, project)?;
        let mut entities = target_stats.targets_inserted;
        let mut relations = target_stats.deps_inserted;
        let mut scip_bytes_total = 0usize;
        let mut driver_name: &'static str = "bazel-query";
        if bazel_symbols {
            tracing::info!("--bazel-symbols set; running symbol-level pass");
            let sym = bazel_symbols::index_bazel_symbols(conn, repo, project)?;
            entities += sym.entities_inserted;
            relations += sym.relations_inserted;
            scip_bytes_total += sym.scip_bytes_total;
            driver_name = "bazel-query+symbols";
        }
        return Ok(IndexStats {
            entities_inserted: entities,
            relations_inserted: relations,
            scip_bytes: scip_bytes_total,
            elapsed_ms: start.elapsed().as_millis() as i64,
            driver_name,
        });
    }

    // Prebuilt SCIP path: load the provided file as-is.
    if let Some(p) = scip_override {
        let bytes = std::fs::read(p)
            .with_context(|| format!("reading SCIP at {}", p.display()))?;
        let load_stats = scip_loader::load(conn, project, &bytes)?;
        return Ok(IndexStats {
            entities_inserted: load_stats.entities_inserted,
            relations_inserted: load_stats.relations_inserted,
            scip_bytes: bytes.len(),
            elapsed_ms: start.elapsed().as_millis() as i64,
            driver_name: "prebuilt",
        });
    }

    // Explicit --lang override: pin to one driver, single SCIP load.
    if let Some(name) = lang_override {
        let drv = driver::by_name(name)
            .ok_or_else(|| anyhow::anyhow!("unknown driver: {name}"))?;
        let bytes = run_driver_to_scip(repo, &*drv)?;
        let load_stats = scip_loader::load(conn, project, &bytes)?;
        return Ok(IndexStats {
            entities_inserted: load_stats.entities_inserted,
            relations_inserted: load_stats.relations_inserted,
            scip_bytes: bytes.len(),
            elapsed_ms: start.elapsed().as_millis() as i64,
            driver_name: driver_static_name(drv.name()),
        });
    }

    // Auto-detect: run EVERY matching driver and merge their SCIP outputs into
    // a single Index protobuf before loading. Multi-language repos (e.g.
    // Python + TypeScript, Java + Python) get full coverage instead of just
    // the first detected language. Per-driver failures are warnings; we
    // proceed with whatever succeeded. If nothing succeeds we bail.
    let matching: Vec<Box<dyn driver::Driver>> = driver::registry()
        .into_iter()
        .filter(|d| d.detect(repo))
        .collect();
    if matching.is_empty() {
        anyhow::bail!(
            "no driver matched {} — pass --lang to force one or --scip <path> to skip detection",
            repo.display()
        );
    }

    let mut all_bytes: Vec<Vec<u8>> = Vec::with_capacity(matching.len());
    let mut total_scip_bytes = 0usize;
    let mut succeeded_names: Vec<&'static str> = Vec::with_capacity(matching.len());
    for drv in &matching {
        match run_driver_to_scip(repo, &**drv) {
            Ok(bytes) => {
                total_scip_bytes += bytes.len();
                all_bytes.push(bytes);
                succeeded_names.push(driver_static_name(drv.name()));
            }
            Err(e) => {
                tracing::warn!(
                    driver = drv.name(),
                    error = %e,
                    "driver failed; continuing with remaining drivers for this repo",
                );
            }
        }
    }
    if all_bytes.is_empty() {
        anyhow::bail!(
            "all {} driver(s) failed for {}",
            matching.len(),
            repo.display()
        );
    }

    let merged = if all_bytes.len() == 1 {
        all_bytes.into_iter().next().unwrap()
    } else {
        bazel_symbols::merge_scip_bytes(&all_bytes)?
    };
    let load_stats = scip_loader::load(conn, project, &merged)?;
    let driver_name: &'static str = if succeeded_names.len() == 1 {
        succeeded_names[0]
    } else {
        "multi"
    };
    Ok(IndexStats {
        entities_inserted: load_stats.entities_inserted,
        relations_inserted: load_stats.relations_inserted,
        scip_bytes: total_scip_bytes,
        elapsed_ms: start.elapsed().as_millis() as i64,
        driver_name,
    })
}

fn run_driver_to_scip(repo: &Path, drv: &dyn driver::Driver) -> Result<Vec<u8>> {
    let mut cmd = drv.command(repo);
    tracing::info!(driver = drv.name(), ?cmd, "running SCIP indexer");
    let status = cmd
        .status()
        .with_context(|| format!("spawning {}", drv.name()))?;
    if !status.success() {
        anyhow::bail!("{} exited with {}", drv.name(), status);
    }
    let scip_path = drv.output_path(repo);
    let bytes = std::fs::read(&scip_path)
        .with_context(|| format!("reading SCIP at {}", scip_path.display()))?;
    Ok(bytes)
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
        anyhow::bail!(
            "workspace root is not a directory: {}",
            workspace_root.display()
        );
    }
    if is_indexable_root(workspace_root) {
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
        if is_indexable_root(&path) {
            out.push(path);
        }
    }
    Ok(out)
}

fn is_indexable_root(path: &Path) -> bool {
    // Bazel takes precedence: a Bazel monorepo often *also* has a Cargo.toml
    // / pyproject.toml in some subdirectory for IDE integration, but the
    // single-language SCIP driver would just thrash on the polyglot layout.
    // The target-level Bazel path is the right one for these.
    if bazel::detect_bazel(path) {
        return true;
    }
    driver::registry().iter().any(|d| d.detect(path))
}

/// Index every repo discovered under `workspace_root`. Each repo's
/// canonical path becomes its `project` key, so the loader's
/// per-project DELETE doesn't trample others. Failures are captured
/// per-repo rather than fatal — a broken indexer for one language
/// shouldn't block the rest of the workspace.
pub fn index_workspace(
    conn: &PooledConn,
    workspace_root: &Path,
    bazel_symbols: bool,
) -> Result<WorkspaceStats> {
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
        let outcome = index_repo(conn, &repo, None, None, &project, bazel_symbols);
        stats.repos.push(WorkspaceRepoResult {
            repo,
            project,
            outcome,
        });
    }
    Ok(stats)
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
