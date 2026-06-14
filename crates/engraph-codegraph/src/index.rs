use crate::{bazel, bazel_symbols, driver, scip_loader};
use anyhow::{Context, Result};
use engraph_core::db::PooledConn;
use sha2::{Digest, Sha256};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::time::Instant;

#[derive(Debug, Default, Clone)]
pub struct IndexStats {
    pub entities_inserted: usize,
    pub relations_inserted: usize,
    pub scip_bytes: usize,
    pub elapsed_ms: i64,
    /// Whichever driver name actually ran, or "prebuilt" when --scip was used.
    pub driver_name: &'static str,
    /// Per-language outcomes of the Bazel symbol-level pass. Empty unless the
    /// `--bazel-symbols` pass ran; lets the CLI report why symbols were (or
    /// were not) produced instead of silently degrading to target-level.
    pub symbol_langs: Vec<SymbolLangSummary>,
    /// Orphan entities pruned by the pre-index GC pass (0 when GC is disabled).
    pub entities_pruned: usize,
}

/// One language's result from the Bazel symbol-level pass, flattened for
/// reporting. `status` is the `LangStatus` Display string.
#[derive(Debug, Clone)]
pub struct SymbolLangSummary {
    pub language: &'static str,
    pub status: String,
    pub scip_bytes: usize,
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
    pub fn pruned_total(&self) -> usize {
        self.repos
            .iter()
            .filter_map(|r| r.outcome.as_ref().ok())
            .map(|s| s.entities_pruned)
            .sum()
    }
    pub fn ok_count(&self) -> usize {
        self.repos.iter().filter(|r| r.outcome.is_ok()).count()
    }
    pub fn err_count(&self) -> usize {
        self.repos.iter().filter(|r| r.outcome.is_err()).count()
    }
}

/// Run the pre-index orphan GC for `project` when `gc` is set, logging and
/// returning the pruned count (0 when disabled). Called at the top of each
/// indexing entry point, before any load re-populates the project.
fn gc_pass(conn: &PooledConn, project: &str, gc: bool) -> Result<usize> {
    if !gc {
        return Ok(0);
    }
    let n = crate::gc::collect_orphans(conn, project)?;
    if n > 0 {
        tracing::info!(project, pruned = n, "GC: pruned orphan entities");
    }
    Ok(n)
}

/// Drive a SCIP indexer (or accept a prebuilt index.scip) and load its output
/// into the codegraph tables. `project` is the entity scope key (typically the
/// canonical repo path). When `gc` is set, orphan entities for `project` are
/// pruned before the load (see [`crate::gc`]).
pub fn index_repo(
    conn: &PooledConn,
    repo: &Path,
    scip_override: Option<&Path>,
    lang_override: Option<&str>,
    project: &str,
    bazel_symbols: bool,
    gc: bool,
) -> Result<IndexStats> {
    let start = Instant::now();
    let entities_pruned = gc_pass(conn, project, gc)?;

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
        let mut symbol_langs = Vec::new();
        if bazel_symbols {
            tracing::info!("--bazel-symbols set; running symbol-level pass");
            let sym = bazel_symbols::index_bazel_symbols(conn, repo, project)?;
            entities += sym.entities_inserted;
            relations += sym.relations_inserted;
            scip_bytes_total += sym.scip_bytes_total;
            driver_name = "bazel-query+symbols";
            symbol_langs = sym
                .results()
                .into_iter()
                .map(|r| SymbolLangSummary {
                    language: r.language,
                    status: r.status.to_string(),
                    scip_bytes: r.scip_bytes,
                })
                .collect();
        }
        return Ok(IndexStats {
            entities_inserted: entities,
            relations_inserted: relations,
            scip_bytes: scip_bytes_total,
            elapsed_ms: start.elapsed().as_millis() as i64,
            driver_name,
            symbol_langs,
            entities_pruned,
        });
    }

    // Prebuilt SCIP path: load the provided file as-is.
    if let Some(p) = scip_override {
        let bytes = std::fs::read(p).with_context(|| format!("reading SCIP at {}", p.display()))?;
        let load_stats = scip_loader::load(conn, project, &bytes)?;
        return Ok(IndexStats {
            entities_inserted: load_stats.entities_inserted,
            relations_inserted: load_stats.relations_inserted,
            scip_bytes: bytes.len(),
            elapsed_ms: start.elapsed().as_millis() as i64,
            driver_name: "prebuilt",
            symbol_langs: Vec::new(),
            entities_pruned,
        });
    }

    // Explicit --lang override: pin to one driver, single SCIP load.
    if let Some(name) = lang_override {
        let drv = driver::by_name(name).ok_or_else(|| anyhow::anyhow!("unknown driver: {name}"))?;
        let bytes = run_driver_to_scip(repo, &*drv)?;
        let load_stats = scip_loader::load(conn, project, &bytes)?;
        return Ok(IndexStats {
            entities_inserted: load_stats.entities_inserted,
            relations_inserted: load_stats.relations_inserted,
            scip_bytes: bytes.len(),
            elapsed_ms: start.elapsed().as_millis() as i64,
            driver_name: driver_static_name(drv.name()),
            symbol_langs: Vec::new(),
            entities_pruned,
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
        symbol_langs: Vec::new(),
        entities_pruned,
    })
}

/// Load a manifest of externally-produced SCIP files, each rooted at a
/// repo-relative subdir. Every entry's document paths are rebased to repo-root
/// so per-project indexers don't collide, all entries are merged, and the
/// result is loaded once — the loader's per-project DELETE means a single
/// merged load is required.
///
/// Manifest format: one entry per line, `<repo-relative-root>\t<scip-file>`.
/// Blank lines and lines starting with `#` are ignored. A root of `.` or empty
/// means repo-root (no rebase). Relative `<scip-file>` paths resolve against the
/// manifest's own directory.
pub fn index_scip_manifest(
    conn: &PooledConn,
    manifest: &Path,
    project: &str,
    gc: bool,
) -> Result<IndexStats> {
    let start = Instant::now();
    let entities_pruned = gc_pass(conn, project, gc)?;
    let (merged, scip_bytes_in) = build_manifest_scip(manifest)?;
    let load_stats = scip_loader::load(conn, project, &merged)?;
    Ok(IndexStats {
        entities_inserted: load_stats.entities_inserted,
        relations_inserted: load_stats.relations_inserted,
        scip_bytes: scip_bytes_in,
        elapsed_ms: start.elapsed().as_millis() as i64,
        driver_name: "scip-manifest",
        symbol_langs: Vec::new(),
        entities_pruned,
    })
}

/// Read every SCIP named in `manifest`, rebase each by its entry's root prefix,
/// and merge into one SCIP blob. Returns the merged bytes and the total input
/// byte count. DB-free so it can be unit-tested directly.
fn build_manifest_scip(manifest: &Path) -> Result<(Vec<u8>, usize)> {
    let text = std::fs::read_to_string(manifest)
        .with_context(|| format!("reading manifest {}", manifest.display()))?;
    let base = manifest.parent().unwrap_or_else(|| Path::new("."));
    let mut parts: Vec<Vec<u8>> = Vec::new();
    let mut scip_bytes_in = 0usize;
    for (i, raw) in text.lines().enumerate() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let (root, scip_path) = parse_manifest_line(line, base, manifest, i + 1)?;
        let bytes = std::fs::read(&scip_path).with_context(|| {
            format!(
                "reading SCIP {} ({}:{})",
                scip_path.display(),
                manifest.display(),
                i + 1
            )
        })?;
        scip_bytes_in += bytes.len();
        let rebased = bazel_symbols::rebase_documents(&bytes, Path::new(&root))
            .with_context(|| format!("rebasing {} under root '{root}'", scip_path.display()))?;
        parts.push(rebased);
    }
    if parts.is_empty() {
        anyhow::bail!("manifest {} has no entries", manifest.display());
    }
    let merged = bazel_symbols::merge_scip_bytes(&parts)?;
    Ok((merged, scip_bytes_in))
}

/// Parse one `<repo-relative-root>\t<scip-file>` line. `.`/empty root normalizes
/// to empty (no rebase). Relative scip paths resolve against `base`.
fn parse_manifest_line(
    line: &str,
    base: &Path,
    manifest: &Path,
    lineno: usize,
) -> Result<(String, PathBuf)> {
    let (root, path) = line.split_once('\t').ok_or_else(|| {
        anyhow::anyhow!(
            "{}:{}: expected '<root>\\t<scip-file>', got: {line}",
            manifest.display(),
            lineno
        )
    })?;
    let path = path.trim();
    if path.is_empty() {
        anyhow::bail!("{}:{}: empty SCIP path", manifest.display(), lineno);
    }
    let root = root.trim();
    let root = if root == "." {
        String::new()
    } else {
        root.to_string()
    };
    let p = Path::new(path);
    let scip_path = if p.is_absolute() {
        p.to_path_buf()
    } else {
        base.join(p)
    };
    Ok((root, scip_path))
}

/// Directory the external indexer writes its `index.scip` into. Deliberately
/// OUTSIDE the repo so indexing never pollutes the working tree. Default base is
/// `~/.local/share/engraph/scip/` (alongside the DB); `ENGRAPH_SCIP_OUTPUT_DIR`
/// overrides the base. Either way a per-repo `<hash>` subdir is appended so
/// concurrent indexes of different repos don't clobber each other's file.
fn scip_output_dir(repo: &Path) -> PathBuf {
    let base = std::env::var("ENGRAPH_SCIP_OUTPUT_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            dirs::data_local_dir()
                .unwrap_or_else(|| dirs::home_dir().unwrap_or_default().join(".local/share"))
                .join("engraph")
                .join("scip")
        });
    let canonical = repo.canonicalize().unwrap_or_else(|_| repo.to_path_buf());
    let mut hasher = Sha256::new();
    hasher.update(canonical.to_string_lossy().as_bytes());
    let hex: String = hasher
        .finalize()
        .iter()
        .map(|b| format!("{:02x}", b))
        .collect();
    base.join(&hex[..16])
}

fn run_driver_to_scip(repo: &Path, drv: &dyn driver::Driver) -> Result<Vec<u8>> {
    let out_dir = scip_output_dir(repo);
    std::fs::create_dir_all(&out_dir)
        .with_context(|| format!("creating SCIP output dir {}", out_dir.display()))?;
    let scip_path = out_dir.join("index.scip");
    let mut cmd = drv.command(repo, &scip_path);
    tracing::info!(driver = drv.name(), ?cmd, "running SCIP indexer");
    // Capture rather than inherit: rust-analyzer is noisy on stderr (load
    // progress, known SCIP-emitter warnings, duplicate-symbol reports) yet
    // writes the index to --output, not stdout. Keep its chatter out of
    // engraph's output on success; surface a tail only on failure.
    let output = cmd
        .output()
        .with_context(|| format!("spawning {}", drv.name()))?;
    if !output.status.success() {
        anyhow::bail!(
            "{} exited with {}\nstderr (tail):\n{}",
            drv.name(),
            output.status,
            bazel::tail_lines(&String::from_utf8_lossy(&output.stderr), 25)
        );
    }
    if !output.stderr.is_empty() {
        tracing::debug!(
            driver = drv.name(),
            stderr = %String::from_utf8_lossy(&output.stderr),
            "indexer stderr"
        );
    }
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

/// Languages detected at `dir`: "bazel" if it's a Bazel root, plus each driver
/// (rust-analyzer / scip-go / …) whose probe matches. Empty ⇒ not a project root.
fn detected_langs(dir: &Path) -> HashSet<&'static str> {
    let mut langs = HashSet::new();
    if bazel::detect_bazel(dir) {
        langs.insert("bazel");
    }
    for d in driver::registry() {
        if d.detect(dir) {
            langs.insert(d.name());
        }
    }
    langs
}

/// Directories `--recursive` never descends into: hidden dirs and the usual
/// build / dependency trees (they hold foreign manifests and would explode the
/// walk — `node_modules` alone has thousands of `package.json`).
fn is_pruned_dir(name: &str) -> bool {
    name.starts_with('.')
        || matches!(
            name,
            "node_modules" | "target" | "vendor" | "__pycache__" | ".venv" | "venv" | "testdata"
        )
}

const RECURSIVE_MAX_DEPTH: usize = 16;

/// Recursive project discovery for `index --recursive`. Walks `root`, recording
/// each project root found at any depth (sorted, deduped). See `walk_recursive`
/// for the per-directory rules (prune build dirs, stop at Bazel, suppress
/// same-language descendants so a workspace's members aren't each indexed).
fn discover_repos_recursive(root: &Path) -> Result<Vec<PathBuf>> {
    if !root.is_dir() {
        anyhow::bail!("workspace root is not a directory: {}", root.display());
    }
    let mut out = Vec::new();
    walk_recursive(root, 0, &HashSet::new(), &mut out);
    out.sort();
    out.dedup();
    Ok(out)
}

fn walk_recursive(
    dir: &Path,
    depth: usize,
    claimed: &HashSet<&'static str>,
    out: &mut Vec<PathBuf>,
) {
    if depth > RECURSIVE_MAX_DEPTH {
        return;
    }
    let detected = detected_langs(dir);
    let new_langs: HashSet<&'static str> = detected.difference(claimed).copied().collect();

    // A Bazel root is one indexable unit: record it and don't descend — the
    // target-level `bazel query` pass already covers the whole subtree.
    if new_langs.contains("bazel") {
        out.push(dir.to_path_buf());
        return;
    }

    // Record this dir only if it introduces a language no ancestor already
    // covers (else it's a workspace member / sub-package of an ancestor). Either
    // way keep descending to catch nested modules of *other* languages.
    let child_claimed = if new_langs.is_empty() {
        claimed.clone()
    } else {
        out.push(dir.to_path_buf());
        claimed.union(&detected).copied().collect()
    };

    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut children: Vec<PathBuf> = entries
        .filter_map(|e| e.ok())
        .filter_map(|e| {
            let ft = e.file_type().ok()?;
            // Skip symlinks (e.g. Bazel's bazel-* convenience links), files, and
            // pruned build/dependency dirs.
            if ft.is_symlink() || !ft.is_dir() || is_pruned_dir(&e.file_name().to_string_lossy()) {
                return None;
            }
            Some(e.path())
        })
        .collect();
    children.sort();
    for child in children {
        walk_recursive(&child, depth + 1, &child_claimed, out);
    }
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
    recursive: bool,
    gc: bool,
) -> Result<WorkspaceStats> {
    let repos = if recursive {
        discover_repos_recursive(workspace_root)?
    } else {
        discover_workspace_repos(workspace_root)?
    };
    if repos.is_empty() {
        anyhow::bail!(
            "no indexable repos found under {} (expected a build manifest at the root{})",
            workspace_root.display(),
            if recursive {
                " or any descendant"
            } else {
                " or an immediate child"
            }
        );
    }
    let mut stats = WorkspaceStats::default();
    for repo in repos {
        let canonical = repo.canonicalize().unwrap_or_else(|_| repo.clone());
        let project = canonical.to_string_lossy().into_owned();
        let outcome = index_repo(conn, &repo, None, None, &project, bazel_symbols, gc);
        stats.repos.push(WorkspaceRepoResult {
            repo,
            project,
            outcome,
        });
    }
    Ok(stats)
}

/// What `index_repo` *would* do, without doing it. Used by `engraph index
/// --dry-run` to preview the chosen path with no side effects.
#[derive(Debug)]
pub enum IndexPlan {
    /// Bazel target-level pass (`bazel query //...`, covers the whole tree in
    /// one pass). `symbol_langs` is empty unless `--bazel-symbols` is in effect.
    Bazel {
        symbol_langs: Vec<crate::bazel_symbols::SymbolLangPlan>,
    },
    /// Load a prebuilt SCIP file as-is (`--scip <path>`).
    PrebuiltScip(PathBuf),
    /// Run exactly one driver (`--lang <name>`).
    ForcedDriver(String),
    /// Auto-detect: run every driver whose `detect()` matched, by name.
    AutoDrivers(Vec<&'static str>),
    /// No driver matched and no override given — `index_repo` would bail.
    NoDriverMatch,
}

/// Pure preview of `index_repo`'s decision. MUST mirror the precedence in
/// `index_repo` (bazel > `--scip` > `--lang` > auto-detect); keep the two in
/// sync. No process spawned, no Bazel server, no DB writes.
pub fn plan_repo(
    repo: &Path,
    scip_override: Option<&Path>,
    lang_override: Option<&str>,
    bazel_symbols: bool,
) -> IndexPlan {
    if scip_override.is_none() && lang_override.is_none() && bazel::detect_bazel(repo) {
        let symbol_langs = if bazel_symbols {
            crate::bazel_symbols::plan_symbol_langs()
        } else {
            Vec::new()
        };
        return IndexPlan::Bazel { symbol_langs };
    }
    if let Some(p) = scip_override {
        return IndexPlan::PrebuiltScip(p.to_path_buf());
    }
    if let Some(name) = lang_override {
        return IndexPlan::ForcedDriver(name.to_string());
    }
    let detected: Vec<&'static str> = driver::registry()
        .iter()
        .filter(|d| d.detect(repo))
        .map(|d| d.name())
        .collect();
    if detected.is_empty() {
        IndexPlan::NoDriverMatch
    } else {
        IndexPlan::AutoDrivers(detected)
    }
}

/// Pure preview of `index_workspace`: which repos would be discovered and what
/// each one's plan is. Reuses the same `discover_workspace_repos` the real run
/// uses, so the discovered set matches exactly.
pub fn plan_workspace(
    workspace_root: &Path,
    bazel_symbols: bool,
    recursive: bool,
) -> Result<Vec<(PathBuf, IndexPlan)>> {
    let repos = if recursive {
        discover_repos_recursive(workspace_root)?
    } else {
        discover_workspace_repos(workspace_root)?
    };
    Ok(repos
        .into_iter()
        .map(|repo| {
            let plan = plan_repo(&repo, None, None, bazel_symbols);
            (repo, plan)
        })
        .collect())
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

#[cfg(test)]
mod tests {
    use super::*;

    fn touch(dir: &Path, name: &str) {
        std::fs::write(dir.join(name), "").unwrap();
    }

    #[test]
    fn plan_repo_bazel_wins_over_lang_markers() {
        let dir = tempfile::tempdir().unwrap();
        touch(dir.path(), "MODULE.bazel");
        touch(dir.path(), "Cargo.toml"); // Bazel still wins.
        match plan_repo(dir.path(), None, None, false) {
            IndexPlan::Bazel { symbol_langs } => assert!(symbol_langs.is_empty()),
            other => panic!("expected Bazel, got {other:?}"),
        }
    }

    #[test]
    fn plan_repo_bazel_symbols_lists_python() {
        let dir = tempfile::tempdir().unwrap();
        touch(dir.path(), "WORKSPACE.bazel");
        match plan_repo(dir.path(), None, None, true) {
            IndexPlan::Bazel { symbol_langs } => {
                let langs: Vec<&str> = symbol_langs.iter().map(|l| l.language).collect();
                assert!(langs.contains(&"python"));
                assert!(langs.contains(&"java"));
            }
            other => panic!("expected Bazel, got {other:?}"),
        }
    }

    #[test]
    fn plan_repo_auto_detects_single_driver() {
        let dir = tempfile::tempdir().unwrap();
        touch(dir.path(), "Cargo.toml");
        match plan_repo(dir.path(), None, None, false) {
            IndexPlan::AutoDrivers(names) => assert_eq!(names, vec!["rust-analyzer"]),
            other => panic!("expected AutoDrivers, got {other:?}"),
        }
    }

    #[test]
    fn plan_repo_auto_detects_multiple_drivers() {
        let dir = tempfile::tempdir().unwrap();
        touch(dir.path(), "pyproject.toml");
        touch(dir.path(), "package.json");
        touch(dir.path(), "tsconfig.json");
        match plan_repo(dir.path(), None, None, false) {
            IndexPlan::AutoDrivers(names) => {
                assert!(names.contains(&"scip-python"));
                assert!(names.contains(&"scip-typescript"));
            }
            other => panic!("expected AutoDrivers, got {other:?}"),
        }
    }

    #[test]
    fn plan_repo_scip_override_beats_drivers() {
        let dir = tempfile::tempdir().unwrap();
        touch(dir.path(), "Cargo.toml");
        let scip = dir.path().join("prebuilt.scip");
        match plan_repo(dir.path(), Some(&scip), None, false) {
            IndexPlan::PrebuiltScip(p) => assert_eq!(p, scip),
            other => panic!("expected PrebuiltScip, got {other:?}"),
        }
    }

    #[test]
    fn plan_repo_lang_override_forces_driver() {
        let dir = tempfile::tempdir().unwrap();
        touch(dir.path(), "Cargo.toml");
        match plan_repo(dir.path(), None, Some("scip-go"), false) {
            IndexPlan::ForcedDriver(name) => assert_eq!(name, "scip-go"),
            other => panic!("expected ForcedDriver, got {other:?}"),
        }
    }

    #[test]
    fn plan_repo_no_match_on_empty_dir() {
        let dir = tempfile::tempdir().unwrap();
        assert!(matches!(
            plan_repo(dir.path(), None, None, false),
            IndexPlan::NoDriverMatch
        ));
    }

    #[test]
    fn plan_workspace_discovers_child_repos() {
        let root = tempfile::tempdir().unwrap();
        let child = root.path().join("crate-a");
        std::fs::create_dir(&child).unwrap();
        touch(&child, "Cargo.toml");
        let plans = plan_workspace(root.path(), false, false).unwrap();
        assert_eq!(plans.len(), 1);
        assert_eq!(plans[0].0, child);
        assert!(matches!(plans[0].1, IndexPlan::AutoDrivers(_)));
    }

    #[test]
    fn plan_workspace_bazel_root_collapses_to_root() {
        let root = tempfile::tempdir().unwrap();
        touch(root.path(), "MODULE.bazel");
        let plans = plan_workspace(root.path(), true, false).unwrap();
        assert_eq!(plans.len(), 1);
        assert_eq!(plans[0].0, root.path());
        assert!(matches!(plans[0].1, IndexPlan::Bazel { .. }));
    }

    fn write_scip(path: &Path, doc_paths: &[&str]) {
        use protobuf::Message;
        use scip::types::{Document, Index};
        let mut idx = Index::new();
        for p in doc_paths {
            let mut d = Document::new();
            d.relative_path = (*p).to_string();
            idx.documents.push(d);
        }
        std::fs::write(path, idx.write_to_bytes().unwrap()).unwrap();
    }

    fn merged_doc_paths(bytes: &[u8]) -> Vec<String> {
        use protobuf::Message;
        use scip::types::Index;
        Index::parse_from_bytes(bytes)
            .unwrap()
            .documents
            .iter()
            .map(|d| d.relative_path.clone())
            .collect()
    }

    #[test]
    fn manifest_rebases_per_root_and_merges() {
        let dir = tempfile::tempdir().unwrap();
        let go = dir.path().join("go.scip");
        let py = dir.path().join("py.scip");
        write_scip(&go, &["pkg/widget.go"]); // root "." -> unchanged
        write_scip(&py, &["bar.py"]); // root "py/foo" -> prefixed
        let manifest = dir.path().join("m.tsv");
        std::fs::write(
            &manifest,
            format!(
                "# comment\n\n.\t{}\npy/foo\t{}\n",
                go.display(),
                py.display()
            ),
        )
        .unwrap();

        let (merged, n) = build_manifest_scip(&manifest).unwrap();
        assert!(n > 0);
        let paths = merged_doc_paths(&merged);
        assert_eq!(paths.len(), 2, "{paths:?}");
        assert!(paths.contains(&"pkg/widget.go".to_string()), "{paths:?}");
        assert!(paths.contains(&"py/foo/bar.py".to_string()), "{paths:?}");
    }

    #[test]
    fn manifest_relative_scip_paths_resolve_against_manifest_dir() {
        let dir = tempfile::tempdir().unwrap();
        write_scip(&dir.path().join("a.scip"), &["x.go"]);
        let manifest = dir.path().join("m.tsv");
        std::fs::write(&manifest, ".\ta.scip\n").unwrap(); // relative path
        let (merged, _) = build_manifest_scip(&manifest).unwrap();
        assert_eq!(merged_doc_paths(&merged), vec!["x.go".to_string()]);
    }

    #[test]
    fn manifest_empty_or_missing_errors() {
        let dir = tempfile::tempdir().unwrap();
        let empty = dir.path().join("empty.tsv");
        std::fs::write(&empty, "# only a comment\n\n").unwrap();
        assert!(build_manifest_scip(&empty).is_err());

        let bad = dir.path().join("bad.tsv");
        std::fs::write(&bad, ".\t/no/such/file.scip\n").unwrap();
        assert!(build_manifest_scip(&bad).is_err());
    }

    #[test]
    fn manifest_line_requires_tab_and_normalizes_dot() {
        let base = Path::new("/tmp");
        let manifest = Path::new("/tmp/m.tsv");
        assert!(parse_manifest_line("no-tab-here", base, manifest, 1).is_err());
        let (root, path) = parse_manifest_line(".\tx.scip", base, manifest, 1).unwrap();
        assert_eq!(root, ""); // "." normalized to empty (no rebase)
        assert_eq!(path, Path::new("/tmp/x.scip"));
    }

    fn dir_with(root: &Path, rel: &str, manifest: &str) -> PathBuf {
        let d = if rel.is_empty() {
            root.to_path_buf()
        } else {
            root.join(rel)
        };
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(d.join(manifest), "").unwrap();
        d
    }

    fn recursive_roots(root: &Path) -> Vec<PathBuf> {
        plan_workspace(root, false, true)
            .unwrap()
            .into_iter()
            .map(|(p, _)| p)
            .collect()
    }

    #[test]
    fn recursive_finds_nested_cross_language() {
        let root = tempfile::tempdir().unwrap();
        let r = dir_with(root.path(), "", "Cargo.toml");
        let go = dir_with(root.path(), "go", "go.mod");

        // Non-recursive: the root is itself a manifest root → just [root].
        let nonrec = plan_workspace(root.path(), false, false).unwrap();
        assert_eq!(nonrec.len(), 1);
        assert_eq!(nonrec[0].0, r);

        // Recursive: the Rust root AND the nested Go module.
        let rec = recursive_roots(root.path());
        assert_eq!(rec.len(), 2, "{rec:?}");
        assert!(rec.contains(&r) && rec.contains(&go), "{rec:?}");
    }

    #[test]
    fn recursive_suppresses_same_language_members() {
        let root = tempfile::tempdir().unwrap();
        let r = dir_with(root.path(), "", "Cargo.toml");
        dir_with(root.path(), "crates/a", "Cargo.toml");
        dir_with(root.path(), "crates/b", "Cargo.toml");
        // Members are same-language under the recorded root → not re-recorded.
        assert_eq!(recursive_roots(root.path()), vec![r]);
    }

    #[test]
    fn recursive_prunes_build_dirs() {
        let root = tempfile::tempdir().unwrap();
        let r = dir_with(root.path(), "", "Cargo.toml");
        dir_with(root.path(), "target/dep", "Cargo.toml");
        dir_with(root.path(), "node_modules/x", "package.json");
        assert_eq!(recursive_roots(root.path()), vec![r]);
    }

    #[test]
    fn recursive_stops_at_bazel_root() {
        let root = tempfile::tempdir().unwrap();
        let r = dir_with(root.path(), "", "MODULE.bazel");
        dir_with(root.path(), "sub", "go.mod");
        // Bazel covers the subtree → the nested go.mod is not a separate project.
        assert_eq!(recursive_roots(root.path()), vec![r]);
    }
}
