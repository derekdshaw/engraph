use crate::{
    bazel, bazel_symbols, driver,
    scip_loader::{self, LoadOptions},
};
use anyhow::{Context, Result};
use engraph_core::db::PooledConn;
use globset::{Glob, GlobBuilder, GlobSet, GlobSetBuilder};
use ignore::{IncrementalIgnore, WalkBuilder};
use sha2::{Digest, Sha256};
use std::collections::{BTreeSet, HashMap, HashSet};
use std::ffi::OsString;
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
    /// Entities pruned by GC/stale cleanup (0 when GC is disabled).
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
    pub subtree: String,
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
/// returning the pruned count (0 when disabled). Called at the top of the
/// Bazel path, before any load re-populates the project; SCIP loads run a
/// scoped orphan pass of their own.
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

/// How a subtree was indexed, recorded beside its commit so `engraph index
/// --if-changed` can repeat the same run.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct IndexMode {
    pub source: IndexSource,
    /// The `--lang` pin, if the run had one.
    pub lang: Option<String>,
    /// Whether the run included the symbol-level Bazel pass.
    pub bazel_symbols: bool,
    /// The `--go-tags` of the run, if it had them.
    pub go_tags: Option<String>,
}

/// Where an index run's SCIP came from.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum IndexSource {
    /// Indexers that engraph ran itself (detected, or pinned by `--lang`); a
    /// refresh can run them again.
    #[default]
    Drivers,
    /// A prebuilt `--scip` file, which only its producer can rebuild.
    Scip,
    /// A `--scip-manifest` of prebuilt files, likewise.
    Manifest,
}

impl IndexSource {
    pub fn as_str(self) -> &'static str {
        match self {
            IndexSource::Drivers => "drivers",
            IndexSource::Scip => "scip",
            IndexSource::Manifest => "manifest",
        }
    }

    fn from_db(s: &str) -> Self {
        match s {
            "scip" => IndexSource::Scip,
            "manifest" => IndexSource::Manifest,
            _ => IndexSource::Drivers,
        }
    }
}

/// Record the commit `subtree` of `project` was just indexed at (`""` for the
/// whole project), and how. It replaces the commits of subtrees nested under
/// it, which this run re-indexed. `None` (not a git repo, or HEAD unreadable)
/// leaves no commit rather than one that no longer describes the graph.
pub fn record_indexed_commit(
    conn: &PooledConn,
    project: &str,
    subtree: &str,
    commit: Option<&str>,
    mode: &IndexMode,
) -> Result<()> {
    conn.execute(
        "DELETE FROM index_state
         WHERE project = ?1
           AND (?2 = '' OR subtree = ?2 OR substr(subtree, 1, length(?2) + 1) = ?2 || '/')",
        rusqlite::params![project, subtree],
    )?;
    if let Some(sha) = commit {
        // Sub-second `indexed_at`: the untracked-file divergence check
        // compares file mtimes against it, and the column default is whole
        // seconds — too coarse for a file created moments after the index.
        conn.execute(
            "INSERT INTO index_state (project, subtree, commit_sha, source, lang, bazel_symbols, \
             go_tags, indexed_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, strftime('%Y-%m-%d %H:%M:%f','now'))",
            rusqlite::params![
                project,
                subtree,
                sha,
                mode.source.as_str(),
                mode.lang,
                mode.bazel_symbols,
                mode.go_tags
            ],
        )?;
    }
    Ok(())
}

/// `(subtree, commit)` for each part of `project` with a recorded index commit.
pub fn indexed_commits(conn: &PooledConn, project: &str) -> Result<Vec<(String, String)>> {
    let mut stmt = conn.prepare(
        "SELECT subtree, commit_sha FROM index_state WHERE project = ?1 ORDER BY subtree",
    )?;
    let rows = stmt
        .query_map([project], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

/// When each recorded subtree of `project` was indexed, by subtree, as
/// fractional epoch seconds — what the untracked-file divergence check
/// compares file mtimes against.
pub fn indexed_at_by_subtree(
    conn: &PooledConn,
    project: &str,
) -> Result<std::collections::HashMap<String, f64>> {
    let mut stmt = conn.prepare(
        "SELECT subtree, (julianday(indexed_at) - 2440587.5) * 86400.0
         FROM index_state WHERE project = ?1",
    )?;
    let rows = stmt
        .query_map([project], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<rusqlite::Result<_>>()?;
    Ok(rows)
}

/// How each recorded subtree of `project` was indexed, by subtree.
pub fn index_modes(
    conn: &PooledConn,
    project: &str,
) -> Result<std::collections::HashMap<String, IndexMode>> {
    let mut stmt = conn.prepare(
        "SELECT subtree, source, lang, bazel_symbols, go_tags FROM index_state WHERE project = ?1",
    )?;
    let rows = stmt
        .query_map([project], |r| {
            Ok((
                r.get::<_, String>(0)?,
                IndexMode {
                    source: IndexSource::from_db(&r.get::<_, String>(1)?),
                    lang: r.get(2)?,
                    bazel_symbols: r.get(3)?,
                    go_tags: r.get(4)?,
                },
            ))
        })?
        .collect::<rusqlite::Result<_>>()?;
    Ok(rows)
}

/// Where an index run's output lands: the `project` key, and the repo-relative
/// directory (`subtree`) the run covers — empty for the whole project. A
/// sub-project of a monorepo indexes into its repo's project with its
/// directory as the subtree, so its paths are rebased under it and the load
/// replaces only that part of the project.
#[derive(Debug, Clone, Copy)]
pub struct IndexScope<'a> {
    pub project: &'a str,
    pub subtree: &'a str,
}

/// `index_repo_in` for the whole of `project`.
pub fn index_repo(
    conn: &PooledConn,
    repo: &Path,
    scip_override: Option<&Path>,
    lang_override: Option<&str>,
    project: &str,
    bazel_symbols: bool,
    gc: bool,
) -> Result<IndexStats> {
    let scope = IndexScope {
        project,
        subtree: "",
    };
    index_repo_in(
        conn,
        repo,
        scip_override,
        lang_override,
        scope,
        bazel_symbols,
        None,
        gc,
    )
}

/// Drive a SCIP indexer (or accept a prebuilt index.scip) and load its output
/// into the codegraph tables under `scope`. When `gc` is set, orphan entities
/// in scope are pruned before the load and stale SCIP symbols after it. The
/// Bazel path ignores `scope.subtree` and refreshes the whole project.
#[allow(clippy::too_many_arguments)]
pub fn index_repo_in(
    conn: &PooledConn,
    repo: &Path,
    scip_override: Option<&Path>,
    lang_override: Option<&str>,
    scope: IndexScope,
    bazel_symbols: bool,
    go_tags: Option<&str>,
    gc: bool,
) -> Result<IndexStats> {
    let IndexScope { project, subtree } = scope;
    let load_opts = LoadOptions {
        prune_stale: gc,
        subtree,
    };
    let start = Instant::now();
    // Drivers run inside `repo` and also pass it as an argument; a relative path
    // would resolve against itself (`svc-a/svc-a`), yield an empty index, and
    // the prune would then empty the project.
    let repo = &repo
        .canonicalize()
        .with_context(|| format!("resolving repo path {}", repo.display()))?;

    // Phase 2.3: a Bazel workspace takes the bazel-query path unless the
    // caller explicitly passed --scip <path> (which says "load this SCIP
    // file no matter what") or --lang <name> (which forces a specific SCIP
    // driver). Target-level always runs; symbol-level (Phase 2.3 #2) layers
    // on top when --bazel-symbols is set.
    if scip_override.is_none() && lang_override.is_none() && bazel::detect_bazel(repo) {
        tracing::info!("Bazel workspace detected; running target-level index");
        let mut entities_pruned = gc_pass(conn, project, gc)?;
        let target_stats = bazel::index_bazel_workspace(conn, repo, project)?;
        let mut entities = target_stats.targets_inserted;
        let mut relations = target_stats.deps_inserted;
        let mut scip_bytes_total = 0usize;
        let mut driver_name: &'static str = "bazel-query";
        let mut symbol_langs = Vec::new();
        if bazel_symbols {
            tracing::info!("--bazel-symbols set; running symbol-level pass");
            let sym = bazel_symbols::index_bazel_symbols(conn, repo, project, gc, go_tags)?;
            entities += sym.entities_inserted;
            relations += sym.relations_inserted;
            scip_bytes_total += sym.scip_bytes_total;
            entities_pruned += sym.entities_pruned;
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
        let load_stats = scip_loader::load_with_options(
            conn,
            project,
            &bazel_symbols::rebase_subtree_documents(&bytes, Path::new(subtree), repo)?,
            load_opts,
        )?;
        return Ok(IndexStats {
            entities_inserted: load_stats.entities_inserted,
            relations_inserted: load_stats.relations_inserted,
            scip_bytes: bytes.len(),
            elapsed_ms: start.elapsed().as_millis() as i64,
            driver_name: "prebuilt",
            symbol_langs: Vec::new(),
            entities_pruned: load_stats.entities_pruned,
        });
    }

    // Explicit --lang override: pin to one driver, single SCIP load.
    if let Some(name) = lang_override {
        let drv = driver::by_name(name).ok_or_else(|| anyhow::anyhow!("unknown driver: {name}"))?;
        let bytes = run_driver_to_scip(repo, &*drv, go_tags)?;
        let load_stats = scip_loader::load_with_options(
            conn,
            project,
            &bazel_symbols::rebase_subtree_documents(&bytes, Path::new(subtree), repo)?,
            load_opts,
        )?;
        return Ok(IndexStats {
            entities_inserted: load_stats.entities_inserted,
            relations_inserted: load_stats.relations_inserted,
            scip_bytes: bytes.len(),
            elapsed_ms: start.elapsed().as_millis() as i64,
            driver_name: driver_static_name(drv.name()),
            symbol_langs: Vec::new(),
            entities_pruned: load_stats.entities_pruned,
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
        match run_driver_to_scip(repo, &**drv, go_tags) {
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
    let load_stats = scip_loader::load_with_options(
        conn,
        project,
        &bazel_symbols::rebase_subtree_documents(&merged, Path::new(subtree), repo)?,
        load_opts,
    )?;
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
        entities_pruned: load_stats.entities_pruned,
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
/// manifest's own directory. Under a `scope.subtree`, the manifest's repo is
/// that directory of the project, and paths are rebased under it too.
pub fn index_scip_manifest(
    conn: &PooledConn,
    manifest: &Path,
    scope: IndexScope,
    gc: bool,
) -> Result<IndexStats> {
    let start = Instant::now();
    let (merged, scip_bytes_in) = build_manifest_scip(manifest)?;
    let load_stats = scip_loader::load_with_options(
        conn,
        scope.project,
        &bazel_symbols::rebase_documents(&merged, Path::new(scope.subtree))?,
        LoadOptions {
            prune_stale: gc,
            subtree: scope.subtree,
        },
    )?;
    Ok(IndexStats {
        entities_inserted: load_stats.entities_inserted,
        relations_inserted: load_stats.relations_inserted,
        scip_bytes: scip_bytes_in,
        elapsed_ms: start.elapsed().as_millis() as i64,
        driver_name: "scip-manifest",
        symbol_langs: Vec::new(),
        entities_pruned: load_stats.entities_pruned,
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

fn run_driver_to_scip(
    repo: &Path,
    drv: &dyn driver::Driver,
    go_tags: Option<&str>,
) -> Result<Vec<u8>> {
    run_driver_into(&scip_output_dir(repo), repo, drv, go_tags)
}

/// `run_driver_to_scip` with the output directory given, so a test does not
/// write under the user's data directory.
fn run_driver_into(
    out_dir: &Path,
    repo: &Path,
    drv: &dyn driver::Driver,
    go_tags: Option<&str>,
) -> Result<Vec<u8>> {
    std::fs::create_dir_all(out_dir)
        .with_context(|| format!("creating SCIP output dir {}", out_dir.display()))?;
    let scip_path = out_dir.join("index.scip");
    let mut cmd = drv.command(repo, &scip_path);
    if drv.name() == "scip-go" {
        driver::apply_go_tags(&mut cmd, go_tags);
    }
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
            "{} exited with {}\n{}",
            drv.name(),
            output.status,
            bazel::failure_tail(&output.stderr, &output.stdout, bazel::FAILURE_TAIL_LINES)
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

/// The projects of `--workspace` discovery under `workspace_root`, without the
/// `--include` / `--exclude` filter:
///
/// - If `workspace_root` itself is a project (a build manifest or a Bazel
///   root), only `[workspace_root]`: its indexer takes care of its internal
///   structure.
/// - Otherwise, each direct child that is a project. Hidden and build/dep
///   children (`is_pruned_dir`) are skipped, and so is a child that the ignore
///   rules name, unless it is its own git repo. `--recursive` goes deeper
///   (`examine_recursive`).
pub fn discover_workspace_repos(workspace_root: &Path) -> Result<Vec<PathBuf>> {
    Ok(projects(examine_children(workspace_root)?))
}

/// The paths of the projects among `dirs`.
fn projects(dirs: Vec<DiscoveredDir>) -> Vec<PathBuf> {
    dirs.into_iter()
        .filter(|d| d.outcome.is_project())
        .map(|d| d.path)
        .collect()
}

/// `--workspace` discovery, as a report: the root alone if it is a project,
/// else every direct child.
fn examine_children(workspace_root: &Path) -> Result<Vec<DiscoveredDir>> {
    if !workspace_root.is_dir() {
        anyhow::bail!(
            "workspace root is not a directory: {}",
            workspace_root.display()
        );
    }
    let root = examine_alone(workspace_root, false);
    if root.outcome.is_project() {
        return Ok(vec![root]);
    }
    let mut ig = discovery_ignores(workspace_root);
    let mut out = Vec::new();
    let mut entries: Vec<_> = std::fs::read_dir(workspace_root)
        .with_context(|| format!("reading workspace root {}", workspace_root.display()))?
        .filter_map(|e| e.ok())
        .collect();
    entries.sort_by_key(|e| e.path()); // deterministic ordering for tests
    for entry in entries {
        let path = entry.path();
        // Skip files and, as `--recursive` does, hidden and build/dep dirs.
        if !path.is_dir() || is_pruned_dir(&entry.file_name().to_string_lossy()) {
            continue;
        }
        // An ignored child is skipped unless it is its own git repo.
        let ignored = ig.matched(entry.file_name(), true).is_ignore();
        let own_repo = ignored && path.join(".git").exists();
        out.push(if ignored && !own_repo {
            DiscoveredDir {
                path,
                outcome: DirOutcome::Ignored,
                own_repo: false,
            }
        } else {
            examine_alone(&path, own_repo)
        });
    }
    Ok(out)
}

/// The record of `dir` examined with no ancestor project above it.
fn examine_alone(dir: &Path, own_repo: bool) -> DiscoveredDir {
    DiscoveredDir {
        path: dir.to_path_buf(),
        // With nothing claimed, no member rule (nor its tool check) runs.
        outcome: classify(dir, &HashMap::new(), &|_| true),
        own_repo,
    }
}

/// What `dir` is: a Bazel root, a project, a member, or none of these.
/// `claimed` maps each language that an ancestor project covers to that
/// project, and `installed` is the tool check of `is_member`. Each detected
/// language is new here, or covered by its parent, the nearest recorded
/// project of that language, when its member rule says `dir` belongs to it.
fn classify(
    dir: &Path,
    claimed: &HashMap<&'static str, PathBuf>,
    installed: &dyn Fn(&str) -> bool,
) -> DirOutcome {
    let mut langs: Vec<&'static str> = Vec::new();
    let mut covered: Vec<(&'static str, PathBuf)> = Vec::new();
    for lang in detected_langs(dir) {
        match claimed.get(lang) {
            Some(parent) if is_member(lang, parent, dir, installed) => {
                covered.push((lang, parent.clone()));
            }
            _ => langs.push(lang),
        }
    }
    langs.sort();
    covered.sort();
    // Bazel takes precedence: a Bazel monorepo often *also* has a Cargo.toml
    // / pyproject.toml in some subdirectory for IDE integration, but the
    // single-language SCIP driver would just thrash on the polyglot layout.
    // The target-level Bazel path is the right one for these.
    if langs.contains(&"bazel") {
        DirOutcome::BazelRoot
    } else if !langs.is_empty() {
        DirOutcome::Project { langs, covered }
    } else if !covered.is_empty() {
        DirOutcome::Member { covered }
    } else {
        DirOutcome::NoManifest
    }
}

/// The ignore rules discovery obeys below `root`: the `ignore` crate's
/// defaults (`.gitignore` inside a git repo, `.ignore`, `.git/info/exclude`,
/// the global git excludes), the same rules as `git ls-files
/// --exclude-standard`. Hidden dirs are not filtered here; each caller
/// decides about them.
fn discovery_ignores(root: &Path) -> IncrementalIgnore {
    WalkBuilder::new(root)
        .hidden(false)
        .build_matchers()
        .pop()
        .expect("the builder has one root, so one matcher")
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

/// The file extensions a refresh of `dir`, indexed as `mode`, would read:
/// those of the drivers `index_repo_in` picks for it — the `--lang` pin, or
/// else every driver whose probe matches, in the same order of precedence.
/// `None` when no driver list describes what the run reads (a prebuilt
/// `--scip` or manifest, a Bazel workspace, or no driver matching); the
/// caller then counts every file.
pub fn source_extensions(dir: &Path, mode: &IndexMode) -> Option<Vec<&'static str>> {
    if mode.source != IndexSource::Drivers {
        return None;
    }
    let drivers: Vec<Box<dyn driver::Driver>> = match &mode.lang {
        Some(name) => driver::by_name(name).into_iter().collect(),
        None if bazel::detect_bazel(dir) => return None,
        None => driver::registry()
            .into_iter()
            .filter(|d| d.detect(dir))
            .collect(),
    };
    let exts: Vec<&'static str> = drivers
        .iter()
        .flat_map(|d| d.extensions().iter().copied())
        .collect();
    (!exts.is_empty()).then_some(exts)
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

/// Default for `index --recursive --max-depth`.
pub const RECURSIVE_MAX_DEPTH: usize = 16;

/// How a workspace run finds its projects.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Discovery {
    /// The root if it is a project, else its direct children
    /// (`--workspace`).
    Children,
    /// Every project down to `max_depth` directories below the root; the root
    /// is depth 0 (`--recursive`).
    Recursive { max_depth: usize },
}

/// One directory in the report of a workspace discovery.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiscoveredDir {
    pub path: PathBuf,
    pub outcome: DirOutcome,
    /// The ignore rules of its parent name it, but discovery examined it
    /// because it is its own git repo.
    pub own_repo: bool,
}

/// Why discovery lists a directory. The report lists every directory with a
/// manifest and each place where discovery stopped. It does not list a plain
/// directory below the root, or one pruned by name (`is_pruned_dir`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DirOutcome {
    /// A project. `langs`: the indexers it adds. `covered`: languages it also
    /// has that an ancestor project covers, each with that project.
    Project {
        langs: Vec<&'static str>,
        covered: Vec<(&'static str, PathBuf)>,
    },
    /// A Bazel root: a project whose subtree discovery does not examine.
    BazelRoot,
    /// Has manifests, but ancestor projects cover all of their languages.
    Member {
        covered: Vec<(&'static str, PathBuf)>,
    },
    /// A project that an `--exclude` glob drops.
    Excluded,
    /// A project that no `--include` glob matches.
    NotIncluded,
    /// No manifest: a `--workspace` child, or a directory examined as its own
    /// repo.
    NoManifest,
    /// The ignore rules name it, so discovery does not examine it.
    Ignored,
    /// Deeper than `--max-depth`, so discovery does not examine it.
    BeyondMaxDepth { max_depth: usize },
}

impl DirOutcome {
    /// Whether the run indexes the directory.
    pub fn is_project(&self) -> bool {
        matches!(self, Self::Project { .. } | Self::BazelRoot)
    }
}

/// What a workspace run would do (`plan_workspace`).
#[derive(Debug)]
pub struct WorkspacePlan {
    /// The discovery report, sorted by path.
    pub dirs: Vec<DiscoveredDir>,
    /// The projects the run would index, with the plan of each.
    pub repos: Vec<(PathBuf, IndexPlan)>,
}

/// `path` below the workspace `root`, `/`-separated, with `.` for the root
/// itself: the form the `--include` / `--exclude` globs match.
pub fn workspace_rel(root: &Path, path: &Path) -> String {
    let parts: Vec<_> = path
        .strip_prefix(root)
        .unwrap_or(path)
        .components()
        .map(|c| c.as_os_str().to_string_lossy())
        .collect();
    if parts.is_empty() {
        ".".to_string()
    } else {
        parts.join("/")
    }
}

/// The `--include` and `--exclude` globs of a workspace run. A glob matches
/// the path of a discovered project below the workspace root, `/`-separated,
/// with `.` for the root itself: `*` stays within one level and `**` crosses
/// levels. A glob matches only the path it names, not the paths below it.
/// With include globs, only the projects they match are kept; a project an
/// exclude glob matches is dropped, even if an include glob matches it too.
#[derive(Debug, Default)]
pub struct DiscoveryFilter {
    include: Option<GlobSet>,
    exclude: Option<GlobSet>,
}

impl DiscoveryFilter {
    pub fn new(include: &[String], exclude: &[String]) -> Result<Self> {
        Ok(Self {
            include: glob_set("--include", include)?,
            exclude: glob_set("--exclude", exclude)?,
        })
    }

    /// Mark each project in `dirs` (discovered under `root`) that the globs
    /// drop.
    fn apply(&self, root: &Path, dirs: &mut [DiscoveredDir]) {
        if self.include.is_none() && self.exclude.is_none() {
            return;
        }
        for d in dirs.iter_mut().filter(|d| d.outcome.is_project()) {
            let rel = workspace_rel(root, &d.path);
            let included = self.include.as_ref().is_none_or(|g| g.is_match(&rel));
            let excluded = self.exclude.as_ref().is_some_and(|g| g.is_match(&rel));
            if excluded && included && self.include.is_some() {
                tracing::warn!("{rel} matches both --include and --exclude; --exclude wins");
            }
            if excluded {
                d.outcome = DirOutcome::Excluded;
            } else if !included {
                d.outcome = DirOutcome::NotIncluded;
            }
        }
    }
}

/// A glob over a `/`-separated path below a directory, as `workspace_rel`
/// gives it: `*` stays within one level. A leading `./` and a trailing `/`
/// (shell completion of a directory adds one) are dropped, since the path has
/// neither.
fn path_glob(pat: &str) -> Result<Glob, globset::Error> {
    let pat = pat.strip_prefix("./").unwrap_or(pat).trim_end_matches('/');
    GlobBuilder::new(pat).literal_separator(true).build()
}

/// The globs of one filter flag (`flag` names it in errors), or `None` when
/// none were given.
fn glob_set(flag: &str, pats: &[String]) -> Result<Option<GlobSet>> {
    if pats.is_empty() {
        return Ok(None);
    }
    let mut set = GlobSetBuilder::new();
    for pat in pats {
        set.add(path_glob(pat).with_context(|| format!("invalid {flag} glob {pat}"))?);
    }
    Ok(Some(set.build()?))
}

/// The discovery report of a workspace run under `root`, with the filter
/// applied, sorted by path.
fn discover(
    root: &Path,
    discovery: Discovery,
    filter: &DiscoveryFilter,
) -> Result<Vec<DiscoveredDir>> {
    let mut dirs = match discovery {
        Discovery::Children => examine_children(root)?,
        Discovery::Recursive { max_depth } => {
            examine_recursive(root, max_depth, &crate::which::on_path)?
        }
    };
    filter.apply(root, &mut dirs);
    Ok(dirs)
}

/// Recursive project discovery for `index --recursive`, as a report. See
/// `Walker::walk` for the per-directory rules (prune build dirs, stop at
/// Bazel, skip the members of a project above so they aren't each indexed;
/// `is_member` gives the rule of each language). `installed` says whether a
/// tool is on `PATH` (`which::on_path`).
fn examine_recursive(
    root: &Path,
    max_depth: usize,
    installed: &dyn Fn(&str) -> bool,
) -> Result<Vec<DiscoveredDir>> {
    if !root.is_dir() {
        anyhow::bail!("workspace root is not a directory: {}", root.display());
    }
    let mut walker = Walker {
        max_depth,
        installed,
        out: Vec::new(),
    };
    walker.walk(
        root,
        Path::new(""),
        0,
        &HashMap::new(),
        &mut discovery_ignores(root),
        false,
    );
    let mut out = walker.out;
    out.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(out)
}

/// Whether `dir`, which has a manifest of `lang`, is a member of `parent`, the
/// nearest recorded project of `lang` above it: whether the parent's index run
/// covers `dir`, so `dir` is not indexed on its own.
fn is_member(lang: &str, parent: &Path, dir: &Path, installed: &dyn Fn(&str) -> bool) -> bool {
    match lang {
        // rust-analyzer indexes a workspace's members from its root, and only
        // its own crate graph from a plain package. A crate that declares its
        // own workspace (e.g. a `cargo fuzz` crate) is in no other.
        "rust-analyzer" => cargo_has_workspace(parent) && !cargo_has_workspace(dir),
        "scip-typescript" => typescript_workspace_lists(parent, dir, installed),
        // Go tools leave a nested module out of the module above it, unless
        // the parent's go.work lists it.
        "scip-go" => go_work_lists(parent, dir),
        // scip-python indexes every file below its directory, so a nested
        // project is in the parent's run; scip-java builds Maven and Gradle
        // multi-module projects from the root.
        _ => true,
    }
}

/// Whether the `Cargo.toml` in `dir` defines a workspace: a `[workspace]` or
/// `[workspace.*]` table header. A line scan, not a TOML parse.
fn cargo_has_workspace(dir: &Path) -> bool {
    std::fs::read_to_string(dir.join("Cargo.toml")).is_ok_and(|toml| {
        toml.lines().any(|l| {
            let l = l.trim_start();
            l.starts_with("[workspace]") || l.starts_with("[workspace.")
        })
    })
}

/// Whether the `go.work` in `parent` lists `dir` as a `use` directory: then
/// scip-go indexes `dir` from `parent` (`driver::go_work_uses`).
fn go_work_lists(parent: &Path, dir: &Path) -> bool {
    let rel = workspace_rel(parent, dir);
    driver::go_work_uses(parent)
        .is_some_and(|uses| uses.iter().any(|u| driver::go_work_rel(u) == rel))
}

/// Whether `parent`'s workspace lists `dir`: a workspace whose packages
/// scip-typescript indexes from the parent (`driver::TsWorkspace`). npm has no
/// flag for that, so in an npm workspace no package is a member. Without the
/// package manager installed, the driver runs without the flag, so no package
/// is a member either.
fn typescript_workspace_lists(parent: &Path, dir: &Path, installed: &dyn Fn(&str) -> bool) -> bool {
    driver::TsWorkspace::of(parent)
        .filter(|ws| installed(ws.tool()))
        .is_some_and(|ws| workspace_globs_match(ws.globs(), &workspace_rel(parent, dir)))
}

/// Whether workspace `globs` (relative to the workspace's directory; a `!`
/// glob excludes) list `rel`. Same glob rules as `--include`. A glob that does
/// not compile matches nothing.
fn workspace_globs_match(globs: &[String], rel: &str) -> bool {
    let mut includes = GlobSetBuilder::new();
    let mut excludes = GlobSetBuilder::new();
    for g in globs {
        let (set, pat) = match g.strip_prefix('!') {
            Some(pat) => (&mut excludes, pat),
            None => (&mut includes, g.as_str()),
        };
        if let Ok(glob) = path_glob(pat) {
            set.add(glob);
        }
    }
    let lists = |set: GlobSetBuilder| set.build().is_ok_and(|s| s.is_match(rel));
    lists(includes) && !lists(excludes)
}

/// A recursive discovery: the depth limit, the tool check of `is_member`, and
/// the report so far.
struct Walker<'a> {
    max_depth: usize,
    installed: &'a dyn Fn(&str) -> bool,
    out: Vec<DiscoveredDir>,
}

impl Walker<'_> {
    fn record(&mut self, dir: &Path, outcome: DirOutcome, own_repo: bool) {
        self.out.push(DiscoveredDir {
            path: dir.to_path_buf(),
            outcome,
            own_repo,
        });
    }

    /// Examine `dir` and its subtree. `rel` is `dir`'s path below the root of
    /// `ig`, the ignore rules that govern `dir`. `claimed` maps each language
    /// that an ancestor project covers to that project.
    fn walk(
        &mut self,
        dir: &Path,
        rel: &Path,
        depth: usize,
        claimed: &HashMap<&'static str, PathBuf>,
        ig: &mut IncrementalIgnore,
        own_repo: bool,
    ) {
        if depth > self.max_depth {
            let max_depth = self.max_depth;
            self.record(dir, DirOutcome::BeyondMaxDepth { max_depth }, own_repo);
            return;
        }
        let outcome = classify(dir, claimed, self.installed);

        // A Bazel root is one indexable unit: record it and don't descend — the
        // target-level `bazel query` pass already covers the whole subtree.
        if outcome == DirOutcome::BazelRoot {
            self.record(dir, outcome, own_repo);
            return;
        }

        // A project covers its new languages below it. A member or a plain
        // dir covers nothing new. Either way keep descending to catch nested
        // modules of *other* languages.
        let extended: HashMap<&'static str, PathBuf>;
        let child_claimed = if let DirOutcome::Project { langs, .. } = &outcome {
            let mut map = claimed.clone();
            map.extend(langs.iter().map(|l| (*l, dir.to_path_buf())));
            extended = map;
            &extended
        } else {
            claimed
        };
        // A plain dir is listed only when examined as its own repo.
        if outcome != DirOutcome::NoManifest || own_repo {
            self.record(dir, outcome, own_repo);
        }

        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        let mut children: Vec<OsString> = entries
            .filter_map(|e| e.ok())
            .filter_map(|e| {
                let ft = e.file_type().ok()?;
                // Skip symlinks (e.g. Bazel's bazel-* convenience links),
                // files, and pruned build/dependency dirs.
                if ft.is_symlink()
                    || !ft.is_dir()
                    || is_pruned_dir(&e.file_name().to_string_lossy())
                {
                    return None;
                }
                Some(e.file_name())
            })
            .collect();
        children.sort();
        let no_claims = HashMap::new();
        for name in children {
            let child = dir.join(&name);
            let child_rel = rel.join(&name);
            // A nested git checkout (repo or submodule) has its own project
            // key, so no project above it covers it: lookups from inside it
            // would miss code indexed under the outer key.
            let checkout = child.join(".git").exists();
            let claims = if checkout { &no_claims } else { child_claimed };
            if !ig.matched(&child_rel, true).is_ignore() {
                self.walk(&child, &child_rel, depth + 1, claims, ig, false);
            } else if checkout {
                // An ignored dir that is its own git repo: its parent's rules
                // don't govern it, so walk it under its own.
                let mut child_ig = discovery_ignores(&child);
                self.walk(
                    &child,
                    Path::new(""),
                    depth + 1,
                    claims,
                    &mut child_ig,
                    true,
                );
            } else {
                self.record(&child, DirOutcome::Ignored, false);
            }
        }
    }
}

/// Index every repo discovered under `workspace_root`. `scope_for` maps each
/// discovered root to its `(project, subtree)` — separate repos get their own
/// project, sub-projects of one repo its subtrees — and the scoped load keeps
/// runs from trampling each other. Failures are captured per-repo rather than
/// fatal — a broken indexer for one language shouldn't block the rest of the
/// workspace.
#[allow(clippy::too_many_arguments)]
pub fn index_workspace(
    conn: &PooledConn,
    workspace_root: &Path,
    bazel_symbols: bool,
    go_tags: Option<&str>,
    discovery: Discovery,
    filter: &DiscoveryFilter,
    gc: bool,
    scope_for: &dyn Fn(&Path) -> (String, String),
) -> Result<WorkspaceStats> {
    let dirs = discover(workspace_root, discovery, filter)?;
    let found_any = dirs.iter().any(|d| {
        d.outcome.is_project()
            || matches!(d.outcome, DirOutcome::Excluded | DirOutcome::NotIncluded)
    });
    if !found_any {
        let below = match discovery {
            Discovery::Children => " or an immediate child".to_string(),
            Discovery::Recursive { max_depth } => {
                format!(" or any descendant to depth {max_depth}")
            }
        };
        anyhow::bail!(
            "no indexable repos found under {} (expected a build manifest at the root{below})",
            workspace_root.display(),
        );
    }
    let repos = projects(dirs);
    if repos.is_empty() {
        anyhow::bail!(
            "no project discovered under {} passes the --include and --exclude globs",
            workspace_root.display()
        );
    }
    let mut stats = WorkspaceStats::default();
    let scoped: Vec<(PathBuf, String, String)> = repos
        .into_iter()
        .map(|repo| {
            let (project, subtree) = scope_for(&repo);
            (repo, project, subtree)
        })
        .collect();
    // Take every project's lock before indexing anything, in sorted key
    // order, and hold them all until the run is done. Sub-projects of a
    // monorepo share a project, so another run mustn't slip in between them;
    // and two workspace runs that share projects take their locks in the same
    // order, so neither can hold one the other waits for while waiting on
    // the other. A workspace run is explicit, so it waits for a held lock.
    let projects: BTreeSet<&str> = scoped.iter().map(|(_, p, _)| p.as_str()).collect();
    let locks: HashMap<String, std::result::Result<crate::lock::IndexLock, String>> = projects
        .into_iter()
        .map(|p| {
            let lock = crate::lock::acquire_wait(p).map_err(|e| format!("{e:#}"));
            (p.to_string(), lock)
        })
        .collect();
    for (repo, project, subtree) in scoped {
        let scope = IndexScope {
            project: &project,
            subtree: &subtree,
        };
        let outcome = match &locks[&project] {
            Ok(_) => index_repo_in(conn, &repo, None, None, scope, bazel_symbols, go_tags, gc),
            Err(e) => Err(anyhow::anyhow!("{e}")),
        };
        stats.repos.push(WorkspaceRepoResult {
            repo,
            project,
            subtree,
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
/// each one's plan is. Uses the same `discover` as the real run, so the
/// discovered set matches exactly.
pub fn plan_workspace(
    workspace_root: &Path,
    bazel_symbols: bool,
    discovery: Discovery,
    filter: &DiscoveryFilter,
) -> Result<WorkspacePlan> {
    let dirs = discover(workspace_root, discovery, filter)?;
    let repos = dirs
        .iter()
        .filter(|d| d.outcome.is_project())
        .map(|d| {
            (
                d.path.clone(),
                plan_repo(&d.path, None, None, bazel_symbols),
            )
        })
        .collect();
    Ok(WorkspacePlan { dirs, repos })
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
        let plans = plan_workspace(
            root.path(),
            false,
            Discovery::Children,
            &DiscoveryFilter::default(),
        )
        .unwrap()
        .repos;
        assert_eq!(plans.len(), 1);
        assert_eq!(plans[0].0, child);
        assert!(matches!(plans[0].1, IndexPlan::AutoDrivers(_)));
    }

    #[test]
    fn plan_workspace_bazel_root_collapses_to_root() {
        let root = tempfile::tempdir().unwrap();
        touch(root.path(), "MODULE.bazel");
        let plans = plan_workspace(
            root.path(),
            true,
            Discovery::Children,
            &DiscoveryFilter::default(),
        )
        .unwrap()
        .repos;
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

    /// `dir_with`, but the manifest gets `content`.
    fn write(root: &Path, rel: &str, manifest: &str, content: &str) -> PathBuf {
        let d = dir_with(root, rel, manifest);
        std::fs::write(d.join(manifest), content).unwrap();
        d
    }

    fn recursive_roots(root: &Path) -> Vec<PathBuf> {
        plan_workspace(
            root,
            false,
            Discovery::Recursive {
                max_depth: RECURSIVE_MAX_DEPTH,
            },
            &DiscoveryFilter::default(),
        )
        .unwrap()
        .repos
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
        let nonrec = plan_workspace(
            root.path(),
            false,
            Discovery::Children,
            &DiscoveryFilter::default(),
        )
        .unwrap()
        .repos;
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
        let r = write(root.path(), "", "Cargo.toml", "[workspace]\n");
        dir_with(root.path(), "crates/a", "Cargo.toml");
        dir_with(root.path(), "crates/b", "Cargo.toml");
        // Members of the root workspace → not re-recorded.
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

    /// `.gitignore` applies only inside a git repo (the `ignore` crate's
    /// `require_git` default), so the fixtures that test it need one.
    fn git_init(dir: &Path) {
        let out = engraph_core::utils::git_cmd(dir)
            .args(["init", "-q"])
            .output()
            .unwrap();
        assert!(out.status.success(), "git init failed: {out:?}");
    }

    #[test]
    fn recursive_obeys_gitignore() {
        let root = tempfile::tempdir().unwrap();
        git_init(root.path());
        std::fs::write(root.path().join(".gitignore"), "gen-out/\n").unwrap();
        let r = dir_with(root.path(), "", "Cargo.toml");
        dir_with(root.path(), "gen-out/x", "go.mod");
        let go = dir_with(root.path(), "svc", "go.mod");
        assert_eq!(recursive_roots(root.path()), vec![r, go]);
    }

    #[test]
    fn recursive_obeys_ignore_file() {
        // `.ignore` applies with or without git.
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join(".ignore"), "scratch-dir/\n").unwrap();
        let r = dir_with(root.path(), "", "Cargo.toml");
        dir_with(root.path(), "scratch-dir", "go.mod");
        assert_eq!(recursive_roots(root.path()), vec![r]);
    }

    #[test]
    fn workspace_obeys_gitignore() {
        let root = tempfile::tempdir().unwrap();
        git_init(root.path());
        std::fs::write(root.path().join(".gitignore"), "gen-out/\n").unwrap();
        let a = dir_with(root.path(), "a", "Cargo.toml");
        dir_with(root.path(), "gen-out", "Cargo.toml");
        assert_eq!(discover_workspace_repos(root.path()).unwrap(), vec![a]);
    }

    #[test]
    fn recursive_rescues_ignored_nested_repo() {
        // A repo the parent ignores is still found, and the parent's rules
        // don't govern inside it: `gen-out/` matches at any depth in the
        // parent's repo, but not in `app`'s.
        let root = tempfile::tempdir().unwrap();
        git_init(root.path());
        std::fs::write(root.path().join(".gitignore"), "app/\ngen-out/\n").unwrap();
        let r = dir_with(root.path(), "", "Cargo.toml");
        let app = dir_with(root.path(), "app", "go.mod");
        git_init(&app);
        let py = dir_with(root.path(), "app/gen-out", "pyproject.toml");
        assert_eq!(recursive_roots(root.path()), vec![r, app, py]);
    }

    #[test]
    fn workspace_rescues_ignored_nested_repo() {
        let root = tempfile::tempdir().unwrap();
        git_init(root.path());
        std::fs::write(root.path().join(".gitignore"), "app/\ngen-out/\n").unwrap();
        let app = dir_with(root.path(), "app", "Cargo.toml");
        git_init(&app);
        dir_with(root.path(), "gen-out", "Cargo.toml");
        assert_eq!(discover_workspace_repos(root.path()).unwrap(), vec![app]);
    }

    /// The dry-run report of `plan_workspace`: each listed directory's path
    /// below `root`, outcome, and whether it was examined as its own repo.
    fn report(
        root: &Path,
        discovery: Discovery,
        filter: &DiscoveryFilter,
    ) -> Vec<(String, DirOutcome, bool)> {
        plan_workspace(root, false, discovery, filter)
            .unwrap()
            .dirs
            .into_iter()
            .map(|d| (workspace_rel(root, &d.path), d.outcome, d.own_repo))
            .collect()
    }

    fn project(langs: &[&'static str], covered: &[(&'static str, &Path)]) -> DirOutcome {
        DirOutcome::Project {
            langs: langs.to_vec(),
            covered: covered.iter().map(|(l, p)| (*l, p.to_path_buf())).collect(),
        }
    }

    #[test]
    fn recursive_reports_why_each_dir_is_listed() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        git_init(root);
        std::fs::write(root.join(".gitignore"), "build/\nvendor-repo/\n").unwrap();
        write(root, "", "Cargo.toml", "[workspace]\n");
        dir_with(root, "crates/b", "Cargo.toml");
        let svc = dir_with(root, "svc", "go.mod");
        touch(&svc, "Cargo.toml");
        dir_with(root, "svc/inner", "go.mod");
        dir_with(root, "build", "Cargo.toml");
        let vendor = dir_with(root, "vendor-repo", "pyproject.toml");
        git_init(&vendor);
        dir_with(root, "infra", "MODULE.bazel");
        dir_with(root, "infra/sub", "go.mod"); // under Bazel: not listed
        std::fs::create_dir_all(root.join("docs")).unwrap(); // no manifest: not listed
        dir_with(root, "target/x", "Cargo.toml"); // pruned by name: not listed
        dir_with(root, "a/b/c/d", "go.mod"); // depth 4

        let got = report(
            root,
            Discovery::Recursive { max_depth: 3 },
            &DiscoveryFilter::default(),
        );
        let rust = "rust-analyzer";
        assert_eq!(
            got,
            vec![
                (".".into(), project(&[rust], &[]), false),
                (
                    "a/b/c/d".into(),
                    DirOutcome::BeyondMaxDepth { max_depth: 3 },
                    false
                ),
                ("build".into(), DirOutcome::Ignored, false),
                (
                    "crates/b".into(),
                    DirOutcome::Member {
                        covered: vec![(rust, root.to_path_buf())]
                    },
                    false
                ),
                ("infra".into(), DirOutcome::BazelRoot, false),
                ("svc".into(), project(&["scip-go"], &[(rust, root)]), false),
                // A Go module is never a member.
                ("svc/inner".into(), project(&["scip-go"], &[]), false),
                ("vendor-repo".into(), project(&["scip-python"], &[]), true),
            ]
        );
    }

    #[test]
    fn filter_globs_ignore_a_trailing_slash() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        dir_with(root, "", "Cargo.toml");
        dir_with(root, "svc/x", "go.mod");
        let recursive = Discovery::Recursive {
            max_depth: RECURSIVE_MAX_DEPTH,
        };
        let roots = |filter: DiscoveryFilter| -> Vec<String> {
            plan_workspace(root, false, recursive, &filter)
                .unwrap()
                .repos
                .iter()
                .map(|(p, _)| workspace_rel(root, p))
                .collect()
        };
        let slash = vec!["svc/x/".to_string()];
        assert_eq!(roots(DiscoveryFilter::new(&slash, &[]).unwrap()), ["svc/x"]);
        assert_eq!(roots(DiscoveryFilter::new(&[], &slash).unwrap()), ["."]);
    }

    #[test]
    fn report_marks_filtered_projects() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        dir_with(root, "", "Cargo.toml");
        dir_with(root, "svc", "go.mod");
        dir_with(root, "infra", "MODULE.bazel");
        let filter =
            DiscoveryFilter::new(&["svc".into(), "infra".into()], &["infra".into()]).unwrap();
        let got = report(
            root,
            Discovery::Recursive {
                max_depth: RECURSIVE_MAX_DEPTH,
            },
            &filter,
        );
        let outcomes: Vec<(&str, &DirOutcome)> =
            got.iter().map(|(p, o, _)| (p.as_str(), o)).collect();
        assert_eq!(
            outcomes,
            vec![
                (".", &DirOutcome::NotIncluded),
                ("infra", &DirOutcome::Excluded),
                ("svc", &project(&["scip-go"], &[])),
            ]
        );
    }

    #[test]
    fn workspace_reports_every_child() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        git_init(root);
        std::fs::write(root.join(".gitignore"), "gen-out/\napp/\n").unwrap();
        dir_with(root, "a", "Cargo.toml");
        std::fs::create_dir_all(root.join("b")).unwrap();
        dir_with(root, "gen-out", "Cargo.toml");
        let app = dir_with(root, "app", "go.mod");
        git_init(&app);
        dir_with(root, "z", "MODULE.bazel");
        let got = report(root, Discovery::Children, &DiscoveryFilter::default());
        assert_eq!(
            got,
            vec![
                ("a".into(), project(&["rust-analyzer"], &[]), false),
                ("app".into(), project(&["scip-go"], &[]), true),
                ("b".into(), DirOutcome::NoManifest, false),
                ("gen-out".into(), DirOutcome::Ignored, false),
                ("z".into(), DirOutcome::BazelRoot, false),
            ]
        );
    }

    #[test]
    fn recursive_cargo_members_need_a_workspace_above() {
        // No `[workspace]` in the parent: the nested crate is its own project.
        let tmp = tempfile::tempdir().unwrap();
        let r = write(
            tmp.path(),
            "",
            "Cargo.toml",
            "[package]\nname = \"r\"\n# [workspace]\n",
        );
        let sub = dir_with(tmp.path(), "tools/x", "Cargo.toml");
        assert_eq!(recursive_roots(tmp.path()), vec![r, sub]);

        // `[workspace.dependencies]` alone defines a workspace too.
        let tmp = tempfile::tempdir().unwrap();
        let r = write(
            tmp.path(),
            "",
            "Cargo.toml",
            "[workspace.dependencies]\nanyhow = \"1\"\n",
        );
        dir_with(tmp.path(), "crates/a", "Cargo.toml");
        assert_eq!(recursive_roots(tmp.path()), vec![r]);

        // The parent is the nearest *recorded* Rust project: a member is not
        // recorded, so a crate below a member checks the workspace root.
        let tmp = tempfile::tempdir().unwrap();
        let r = write(tmp.path(), "", "Cargo.toml", "[workspace]\n");
        dir_with(tmp.path(), "crates/a", "Cargo.toml"); // member of r
        dir_with(tmp.path(), "crates/a/nested", "Cargo.toml"); // member of r too
        assert_eq!(recursive_roots(tmp.path()), vec![r]);

        // A crate that declares its own workspace (the `cargo fuzz` layout)
        // is in no other one.
        let tmp = tempfile::tempdir().unwrap();
        let r = write(tmp.path(), "", "Cargo.toml", "[workspace]\n");
        let fuzz = write(
            tmp.path(),
            "fuzz",
            "Cargo.toml",
            "[package]\nname = \"f\"\n\n[workspace]\nmembers = [\".\"]\n",
        );
        assert_eq!(recursive_roots(tmp.path()), vec![r, fuzz]);
    }

    #[test]
    fn recursive_nested_git_repos_are_not_members() {
        // A nested checkout has its own project key, so the projects of the
        // repo around it don't cover it, ignored or not, nor what's below it.
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        git_init(root);
        std::fs::write(root.join(".gitignore"), "vendored/\n").unwrap();
        let r = write(root, "", "Cargo.toml", "[workspace]\n");
        touch(&r, "pyproject.toml");
        let vendored = dir_with(root, "vendored", "Cargo.toml");
        git_init(&vendored);
        let lib = dir_with(root, "lib", "pyproject.toml");
        git_init(&lib);
        let inner = dir_with(root, "lib/inner", "Cargo.toml");
        assert_eq!(recursive_roots(root), vec![r, lib, inner, vendored]);
    }

    #[test]
    fn recursive_go_work_members_are_covered_by_the_root() {
        // A go.work root without its own go.mod is a project; the modules
        // it lists are its members, and a module it does not list is not.
        let tmp = tempfile::tempdir().unwrap();
        let r = dir_with(tmp.path(), "", "go.work");
        std::fs::write(
            r.join("go.work"),
            "go 1.21\n\nuse (\n\t./tools\n\t./svc/api\n)\n",
        )
        .unwrap();
        dir_with(tmp.path(), "tools", "go.mod");
        dir_with(tmp.path(), "svc/api", "go.mod");
        let other = dir_with(tmp.path(), "svc/other", "go.mod");
        assert_eq!(recursive_roots(tmp.path()), vec![r, other]);
    }

    #[test]
    fn recursive_go_modules_are_never_members() {
        let tmp = tempfile::tempdir().unwrap();
        let r = dir_with(tmp.path(), "", "go.mod");
        let a = dir_with(tmp.path(), "svc/a", "go.mod");
        let b = dir_with(tmp.path(), "svc/a/b", "go.mod");
        assert_eq!(recursive_roots(tmp.path()), vec![r, a, b]);
    }

    #[test]
    fn recursive_python_and_java_keep_the_member_rule() {
        let tmp = tempfile::tempdir().unwrap();
        let r = dir_with(tmp.path(), "", "pyproject.toml");
        dir_with(tmp.path(), "pkg/sub", "pyproject.toml");
        assert_eq!(recursive_roots(tmp.path()), vec![r]);

        let tmp = tempfile::tempdir().unwrap();
        let r = dir_with(tmp.path(), "", "pom.xml");
        dir_with(tmp.path(), "module-a", "pom.xml");
        assert_eq!(recursive_roots(tmp.path()), vec![r]);
    }

    /// A TypeScript project: `package.json` (with `content`) and `tsconfig.json`.
    fn ts(root: &Path, rel: &str, content: &str) -> PathBuf {
        let d = write(root, rel, "package.json", content);
        touch(&d, "tsconfig.json");
        d
    }

    /// `recursive_roots`, with `installed` in place of the `PATH` check of
    /// the package managers.
    fn ts_roots(root: &Path, installed: &dyn Fn(&str) -> bool) -> Vec<PathBuf> {
        projects(examine_recursive(root, RECURSIVE_MAX_DEPTH, installed).unwrap())
    }

    #[test]
    fn recursive_typescript_members_follow_pnpm_workspace() {
        let tmp = tempfile::tempdir().unwrap();
        let root = ts(tmp.path(), "", "{}");
        std::fs::write(
            root.join("pnpm-workspace.yaml"),
            "# comment\npackages:\n  - 'packages/*'\n  - \"!packages/skip\" # not this one\n  - libs/**\ncatalog:\n  - 'apps/*'\n",
        )
        .unwrap();
        ts(tmp.path(), "packages/a", "{}"); // member
        let skip = ts(tmp.path(), "packages/skip", "{}"); // negated
        ts(tmp.path(), "libs/deep/x", "{}"); // member, `**`
        let web = ts(tmp.path(), "apps/web", "{}"); // under another key
        assert_eq!(
            ts_roots(tmp.path(), &|_| true),
            vec![root.clone(), web.clone(), skip.clone()]
        );

        // Without pnpm, the root's run indexes no package, so each is its own
        // project.
        let all = ts_roots(tmp.path(), &|tool| tool != "pnpm");
        assert_eq!(all.len(), 5, "{all:?}");
    }

    #[test]
    fn recursive_typescript_members_follow_yarn_workspaces() {
        // Array form.
        let tmp = tempfile::tempdir().unwrap();
        let root = ts(tmp.path(), "", r#"{"workspaces": ["packages/*"]}"#);
        touch(&root, "yarn.lock");
        ts(tmp.path(), "packages/a", "{}");
        let tool = ts(tmp.path(), "tools/x", "{}");
        assert_eq!(
            ts_roots(tmp.path(), &|_| true),
            vec![root.clone(), tool.clone()]
        );
        let a = tmp.path().join("packages/a");
        assert_eq!(
            ts_roots(tmp.path(), &|tool| tool != "yarn"),
            vec![root, a, tool]
        );

        // Object form.
        let tmp = tempfile::tempdir().unwrap();
        let root = ts(
            tmp.path(),
            "",
            r#"{"workspaces": {"packages": ["./libs/**"]}}"#,
        );
        touch(&root, "yarn.lock");
        ts(tmp.path(), "libs/deep/x", "{}");
        assert_eq!(ts_roots(tmp.path(), &|_| true), vec![root]);
    }

    #[test]
    fn recursive_typescript_npm_workspace_packages_are_not_members() {
        // No yarn.lock and no pnpm-workspace.yaml: scip-typescript has no flag
        // that indexes the packages from the root, so each is its own project.
        let tmp = tempfile::tempdir().unwrap();
        let root = ts(tmp.path(), "", r#"{"workspaces": ["packages/*"]}"#);
        let a = ts(tmp.path(), "packages/a", "{}");
        assert_eq!(recursive_roots(tmp.path()), vec![root, a]);
    }

    #[test]
    fn workspace_skips_hidden_and_pruned_children() {
        // Step 12: plain `--workspace` prunes by name as `--recursive` does,
        // and the report does not list the pruned children.
        let root = tempfile::tempdir().unwrap();
        dir_with(root.path(), ".hidden", "Cargo.toml");
        dir_with(root.path(), "node_modules/x", "Cargo.toml");
        dir_with(root.path(), "node_modules", "package.json");
        let a = dir_with(root.path(), "a", "Cargo.toml");
        assert_eq!(
            discover_workspace_repos(root.path()).unwrap(),
            vec![a.clone()]
        );
        let got = report(
            root.path(),
            Discovery::Children,
            &DiscoveryFilter::default(),
        );
        assert_eq!(
            got,
            vec![("a".into(), project(&["rust-analyzer"], &[]), false)]
        );
    }

    #[test]
    fn indexed_commits_are_per_subtree() {
        let dir = tempfile::tempdir().unwrap();
        let pool = engraph_core::db::open_pool(&dir.path().join("t.db")).unwrap();
        let conn = pool.get().unwrap();
        let commits = |c: &PooledConn| indexed_commits(c, "/p").unwrap();
        assert!(commits(&conn).is_empty());
        record_indexed_commit(&conn, "/p", "svc-a", Some("aaa"), &IndexMode::default()).unwrap();
        record_indexed_commit(&conn, "/p", "svc-b", Some("bbb"), &IndexMode::default()).unwrap();
        record_indexed_commit(&conn, "/p", "svc-b", Some("ccc"), &IndexMode::default()).unwrap();
        assert_eq!(
            commits(&conn),
            [
                ("svc-a".into(), "aaa".into()),
                ("svc-b".into(), "ccc".into())
            ]
        );
        // An index run with no known commit must not leave the old one trusted.
        record_indexed_commit(&conn, "/p", "svc-b", None, &IndexMode::default()).unwrap();
        assert_eq!(commits(&conn), [("svc-a".into(), "aaa".into())]);
        // A root run covers every subtree, so it replaces their commits.
        record_indexed_commit(&conn, "/p", "", Some("ddd"), &IndexMode::default()).unwrap();
        assert_eq!(commits(&conn), [(String::new(), "ddd".into())]);
    }

    #[test]
    fn index_modes_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let pool = engraph_core::db::open_pool(&dir.path().join("t.db")).unwrap();
        let conn = pool.get().unwrap();
        let pinned = IndexMode {
            source: IndexSource::Drivers,
            lang: Some("scip-go".into()),
            bazel_symbols: true,
            go_tags: Some("integration,e2e".into()),
        };
        let prebuilt = IndexMode {
            source: IndexSource::Manifest,
            ..IndexMode::default()
        };
        record_indexed_commit(&conn, "/p", "svc-a", Some("a"), &pinned).unwrap();
        record_indexed_commit(&conn, "/p", "svc-b", Some("b"), &prebuilt).unwrap();
        let modes = index_modes(&conn, "/p").unwrap();
        assert_eq!(modes.len(), 2);
        assert_eq!(modes["svc-a"], pinned);
        assert_eq!(modes["svc-b"], prebuilt);
        assert!(index_modes(&conn, "/other").unwrap().is_empty());
    }

    #[test]
    fn source_extensions_follow_the_drivers_a_refresh_would_run() {
        let root = tempfile::tempdir().unwrap();
        let auto = IndexMode::default();
        // Detected drivers: every matching one contributes.
        let rs = dir_with(root.path(), "rs", "Cargo.toml");
        assert_eq!(source_extensions(&rs, &auto), Some(vec!["rs"]));
        let ts = dir_with(root.path(), "ts", "package.json");
        std::fs::write(ts.join("tsconfig.json"), "{}").unwrap();
        std::fs::write(ts.join("pyproject.toml"), "").unwrap();
        let exts = source_extensions(&ts, &auto).unwrap();
        assert!(exts.contains(&"js") && exts.contains(&"tsx") && exts.contains(&"py"));
        // A `--lang` pin wins over detection.
        let pinned = IndexMode {
            lang: Some("scip-go".into()),
            ..IndexMode::default()
        };
        assert_eq!(source_extensions(&rs, &pinned), Some(vec!["go"]));
        // No list describes these: every file counts.
        let none = dir_with(root.path(), "none", "README.md");
        assert_eq!(source_extensions(&none, &auto), None);
        let bazel = dir_with(root.path(), "bazel", "MODULE.bazel");
        std::fs::write(bazel.join("Cargo.toml"), "").unwrap();
        assert_eq!(source_extensions(&bazel, &auto), None);
        let scip = IndexMode {
            source: IndexSource::Scip,
            ..IndexMode::default()
        };
        assert_eq!(source_extensions(&rs, &scip), None);
    }

    /// An indexer that fails the way scip-typescript does: the error on
    /// stdout, nothing on stderr.
    struct StdoutOnlyIndexer;
    impl driver::Driver for StdoutOnlyIndexer {
        fn name(&self) -> &'static str {
            "fake-indexer"
        }
        fn detect(&self, _: &Path) -> bool {
            true
        }
        fn extensions(&self) -> &'static [&'static str] {
            &[]
        }
        fn command(&self, _: &Path, _: &Path) -> std::process::Command {
            let mut c = std::process::Command::new("sh");
            c.args([
                "-c",
                "echo \"error TS6053: File 'x/tsconfig' not found.\"; exit 1",
            ]);
            c
        }
    }

    #[test]
    fn a_failed_indexer_that_writes_only_to_stdout_shows_the_stdout_tail() {
        let out = tempfile::tempdir().unwrap();
        let repo = tempfile::tempdir().unwrap();
        let err = run_driver_into(out.path(), repo.path(), &StdoutOnlyIndexer, None).unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("stdout (tail):\nerror TS6053: File 'x/tsconfig' not found."),
            "{msg}"
        );
    }
}
