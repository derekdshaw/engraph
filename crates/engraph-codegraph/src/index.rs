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
