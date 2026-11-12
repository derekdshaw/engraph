//! F2 Phase 2.3 #2 — symbol-level Bazel indexing.
//!
//! Sits on top of the target-level pass in `bazel.rs`. For each of
//! {Java, Go, TypeScript, Python} we (a) probe `bazel query` for any target of that
//! language in the workspace; (b) probe PATH for the per-language SCIP
//! indexer binary; (c) on both-present, run the indexer, capture its SCIP
//! bytes. We MERGE the per-language SCIP byte streams in memory and call
//! `scip_loader::load` exactly once — calling it per language would have
//! each language wipe the previous one's edges (the loader's DELETE is
//! per-project, kind-blind except for BAZEL_DEPENDS_ON).
//!
//! Per-language strategy:
//! - **Java**: delegated (`run_java`). Java's SCIP build is too repo-specific to
//!   bake in — a Bazel SemanticDB aspect, Maven, Gradle, and custom
//!   annotation-processor toolchains all differ — so engraph runs the command named by
//!   `ENGRAPH_BAZEL_SCIP_JAVA_CMD` (`<cmd> <repo> <out.scip>`) and merges the SCIP
//!   it writes. The command owns all build-system knowledge; engraph stays
//!   agnostic. Unset → reported not-configured. A ready-made Bazel-aspect driver
//!   ships in `docs/examples/scip-java-bazel-index.sh`.
//! - **Go**: if `ENGRAPH_BAZEL_SCIP_GO_CMD` is set, delegated exactly like Java
//!   (it owns the Bazel→scip-go glue and reaches gazelle-managed `go_library`
//!   targets that have no `go.mod`). Unset → the native multi-module pass:
//!   enumerate every `go.mod` (`discover_go_modules`, skipping symlinked /
//!   `vendor` / `testdata` / hidden trees), run `scip-go index --module-root
//!   <dir>` per module, and rebase each module's document paths back to
//!   repo-root before merging. The native pass can't see `go_library` targets
//!   without a `go.mod`; that gap is reported via the `go targets` count.
//! - **TypeScript**: `scip-typescript index` at the workspace root.
//!   `rules_ts`-based repos may need a prior `bazel build //...` to
//!   populate `bazel-bin/<pkg>/node_modules` symlinks; documented
//!   limitation, not addressed here.
//! - **Python**: `scip-python index --cwd .` at the workspace root. In a
//!   Bazel-`rules_python` monorepo the sub-projects share no single venv, so
//!   import resolution from the root is best-effort — unresolved cross-package
//!   imports surface as external symbols rather than edges (analogous to the
//!   `rules_ts` and multi-`go.mod` caveats above).
//!
//! Off by default (`engraph index --bazel-symbols`); toolchain downloads
//! and full builds make it heavy. The target-level pass remains the fast
//! deterministic default.
//!
//! **Output-base / cache note.** The target-level pass pins Bazel's
//! `--output_base` into `~/.cache/engraph/bazel-out/<hash>`; the Go pass reuses
//! that isolated base for its `bazel query` target probe. Java is delegated, so
//! any Bazel `--output_base` / cache behavior is owned by the configured command,
//! not engraph. Go and TS read sources directly (no Bazel build subprocess).

use crate::bazel::{bazel_binary, failure_tail, output_base_for, tail_lines};
use crate::scip_loader::{self, LoadOptions};
use anyhow::{Context, Result};
use engraph_core::db::PooledConn;
use sha2::{Digest, Sha256};
use std::fmt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Instant;

#[derive(Debug, Default)]
pub struct BazelSymbolStats {
    pub java: Option<LangIndexResult>,
    pub go: Option<LangIndexResult>,
    pub ts: Option<LangIndexResult>,
    pub python: Option<LangIndexResult>,
    pub entities_inserted: usize,
    pub relations_inserted: usize,
    pub entities_pruned: usize,
    pub scip_bytes_total: usize,
    pub elapsed_ms: i64,
}

impl BazelSymbolStats {
    /// Per-language results in stable order (java, go, ts, python), skipping
    /// languages that were never attempted (None). After a real run every
    /// `LANGS` entry is present, including skipped/failed ones.
    pub fn results(&self) -> Vec<&LangIndexResult> {
        [&self.java, &self.go, &self.ts, &self.python]
            .into_iter()
            .filter_map(|o| o.as_ref())
            .collect()
    }
}

#[derive(Debug)]
pub struct LangIndexResult {
    pub language: &'static str,
    pub scip_bytes: usize,
    pub elapsed_ms: i64,
    pub status: LangStatus,
}

/// The command that indexes Go under Bazel when the native per-module pass
/// cannot (`run_go`). Also the `env` of the `SkippedNotConfigured` the native
/// pass reports when it finds no `go.mod`, so both stay in step.
const GO_CMD_ENV: &str = "ENGRAPH_BAZEL_SCIP_GO_CMD";

#[derive(Debug, PartialEq, Eq)]
pub enum LangStatus {
    Indexed,
    /// Aggregate outcome of the per-module Go pass: `indexed` modules produced
    /// SCIP, `failed` modules errored (isolated), out of `targets` total go
    /// rule targets in the workspace. `targets` makes the gazelle coverage gap
    /// visible (go.mod-rooted modules « go_library targets on gazelle repos).
    IndexedModules {
        indexed: usize,
        failed: usize,
        targets: usize,
    },
    SkippedNoTargets,
    SkippedNoIndexer {
        binary: &'static str,
    },
    /// Targets of this language exist, but engraph cannot index them without a
    /// command the user supplies, and `env` does not name one. One variant for
    /// every language that reaches this state, by either route: a language
    /// whose SCIP build is always delegated because it is repo-specific (Java:
    /// Bazel aspect vs Maven vs Gradle vs custom toolchains), or one whose
    /// native pass found no root to work from (Go with no `go.mod`, a pure
    /// gazelle repo). `targets` sizes the gap — `SkippedNoTargets` would claim
    /// the workspace has no code of this language at all.
    SkippedNotConfigured {
        targets: usize,
        env: &'static str,
    },
    Failed(String),
}

impl fmt::Display for LangStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            LangStatus::Indexed => write!(f, "indexed"),
            LangStatus::IndexedModules {
                indexed,
                failed,
                targets,
            } => {
                write!(
                    f,
                    "indexed {indexed} go.mod modules of {targets} go targets"
                )?;
                if *failed > 0 {
                    write!(f, ", {failed} failed")?;
                }
                Ok(())
            }
            LangStatus::SkippedNoTargets => write!(f, "skipped (no targets in this workspace)"),
            LangStatus::SkippedNoIndexer { binary } => {
                write!(f, "skipped ({} not on PATH)", binary)
            }
            LangStatus::SkippedNotConfigured { targets, env } => {
                write!(f, "skipped ({targets} targets, set {env} to index them)")
            }
            LangStatus::Failed(msg) => write!(f, "failed: {}", msg),
        }
    }
}

struct LangSpec {
    language: &'static str,
    binary: &'static str,
    /// Bazel rule classes that count as "this language has targets here".
    rule_kinds: &'static [&'static str],
}

const LANGS: &[LangSpec] = &[
    LangSpec {
        language: "java",
        binary: "scip-java",
        rule_kinds: &["java_library", "java_binary", "java_test"],
    },
    LangSpec {
        language: "go",
        binary: "scip-go",
        rule_kinds: &["go_library", "go_binary", "go_test"],
    },
    LangSpec {
        language: "ts",
        binary: "scip-typescript",
        rule_kinds: &["ts_project", "ts_library"],
    },
    LangSpec {
        language: "python",
        binary: "scip-python",
        rule_kinds: &["py_library", "py_binary", "py_test"],
    },
];

/// What a `--dry-run` reports for one symbol-level language: the language, the
/// indexer binary it needs, and whether that binary is on PATH right now.
/// Target presence is NOT probed here (that needs a Bazel server) — it is
/// checked at real run time by `bazel_has_targets`.
#[derive(Debug)]
pub struct SymbolLangPlan {
    pub language: &'static str,
    pub binary: &'static str,
    pub indexer_on_path: bool,
}

/// Static preview of the symbol-level pass: one entry per `LANGS` language with
/// a cheap PATH probe for its indexer. No Bazel invocation, no side effects.
pub fn plan_symbol_langs() -> Vec<SymbolLangPlan> {
    LANGS
        .iter()
        .map(|spec| SymbolLangPlan {
            language: spec.language,
            binary: spec.binary,
            indexer_on_path: indexer_present(spec.binary),
        })
        .collect()
}

pub fn index_bazel_symbols(
    conn: &PooledConn,
    repo: &Path,
    project: &str,
    prune_stale: bool,
) -> Result<BazelSymbolStats> {
    let start = Instant::now();
    let mut stats = BazelSymbolStats::default();
    let scip_dir = scip_output_dir(repo)?;
    let bazel = bazel_binary()?;

    let mut parts: Vec<Vec<u8>> = Vec::new();
    for spec in LANGS {
        // Go: delegate if ENGRAPH_BAZEL_SCIP_GO_CMD is set, else the native
        // multi-module pass. Java: always delegated (its SCIP build is
        // repo-specific). TS / Python: single-shot standalone indexer at the
        // workspace root.
        let outcome = match spec.language {
            "go" => run_go(spec, repo, &bazel, &scip_dir, &mut parts),
            "java" => run_java(spec, repo, &bazel, &scip_dir, &mut parts),
            _ => run_language(spec, repo, &bazel, &scip_dir, &mut parts),
        };
        let result = match outcome {
            Ok(r) => r,
            Err(e) => LangIndexResult {
                language: spec.language,
                scip_bytes: 0,
                elapsed_ms: 0,
                status: LangStatus::Failed(format!("{e:#}")),
            },
        };
        stats.scip_bytes_total += result.scip_bytes;
        match spec.language {
            "java" => stats.java = Some(result),
            "go" => stats.go = Some(result),
            "ts" => stats.ts = Some(result),
            "python" => stats.python = Some(result),
            _ => unreachable!(),
        }
    }

    if !parts.is_empty() {
        let merged = merge_scip_bytes(&parts)?;
        let load_stats = scip_loader::load_with_options(
            conn,
            project,
            &merged,
            LoadOptions {
                prune_stale,
                ..Default::default()
            },
        )?;
        stats.entities_inserted = load_stats.entities_inserted;
        stats.relations_inserted = load_stats.relations_inserted;
        stats.entities_pruned = load_stats.entities_pruned;
    }
    stats.elapsed_ms = start.elapsed().as_millis() as i64;
    Ok(stats)
}

fn run_language(
    spec: &LangSpec,
    repo: &Path,
    bazel: &Path,
    scip_dir: &Path,
    parts: &mut Vec<Vec<u8>>,
) -> Result<LangIndexResult> {
    let start = Instant::now();

    if !bazel_has_targets(bazel, repo, spec.rule_kinds)? {
        return Ok(LangIndexResult {
            language: spec.language,
            scip_bytes: 0,
            elapsed_ms: start.elapsed().as_millis() as i64,
            status: LangStatus::SkippedNoTargets,
        });
    }

    if !indexer_present(spec.binary) {
        return Ok(LangIndexResult {
            language: spec.language,
            scip_bytes: 0,
            elapsed_ms: start.elapsed().as_millis() as i64,
            status: LangStatus::SkippedNoIndexer {
                binary: spec.binary,
            },
        });
    }

    let out_path = scip_dir.join(format!("index-{}.scip", spec.language));
    let _ = std::fs::remove_file(&out_path); // ignore-if-absent

    let mut cmd = build_indexer_command(spec, repo, &out_path);
    tracing::info!(driver = spec.binary, ?cmd, "running symbol-level indexer");
    let output = cmd
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .with_context(|| format!("spawning {}", spec.binary))?;
    if !output.status.success() {
        return Ok(LangIndexResult {
            language: spec.language,
            scip_bytes: 0,
            elapsed_ms: start.elapsed().as_millis() as i64,
            status: LangStatus::Failed(format!(
                "{} exited with {}\n{}",
                spec.binary,
                output.status,
                failure_tail(&output.stderr, &output.stdout, 20)
            )),
        });
    }

    let bytes = read_scip_output(spec, &out_path)?;
    let n = bytes.len();
    parts.push(bytes);
    Ok(LangIndexResult {
        language: spec.language,
        scip_bytes: n,
        elapsed_ms: start.elapsed().as_millis() as i64,
        status: LangStatus::Indexed,
    })
}

/// Delegated symbol pass, shared by Java and Go. The language's SCIP build is too
/// repo-specific to bake into engraph (a Bazel aspect vs Maven/Gradle, hermetic Go
/// deps vs GOPATH, custom toolchains all differ), so engraph runs the command named
/// by `cmd_env` as `<cmd> <repo> <out.scip>` and merges whatever SCIP that command
/// writes; the command owns all build-system knowledge. The command must emit
/// repo-root-relative document paths (no rebasing happens here). Ready-made Bazel
/// drivers live under `docs/examples/`.
///
/// Targets are counted before `cmd_env` is read, so the two skips say different
/// things: no targets of this language at all, versus N targets this command
/// would have indexed. Reading the env var first would tell a Go-only workspace
/// to configure a Java indexer.
fn run_delegated(
    spec: &LangSpec,
    repo: &Path,
    bazel: &Path,
    scip_dir: &Path,
    parts: &mut Vec<Vec<u8>>,
    cmd_env: &'static str,
    out_name: &str,
) -> Result<LangIndexResult> {
    let start = Instant::now();
    let skip = |status: LangStatus| LangIndexResult {
        language: spec.language,
        scip_bytes: 0,
        elapsed_ms: start.elapsed().as_millis() as i64,
        status,
    };

    let targets = bazel_count_targets(bazel, repo, spec.rule_kinds)?;
    if targets == 0 {
        return Ok(skip(LangStatus::SkippedNoTargets));
    }

    let cmd = match std::env::var(cmd_env) {
        Ok(c) if !c.trim().is_empty() => c,
        _ => {
            return Ok(skip(LangStatus::SkippedNotConfigured {
                targets,
                env: cmd_env,
            }));
        }
    };

    let out_path = scip_dir.join(out_name);
    let _ = std::fs::remove_file(&out_path); // ignore-if-absent

    // Run `<cmd> <repo> <out>` through a shell so CMD may carry its own flags.
    // Contract: the command writes a SCIP index to its 2nd arg and exits 0.
    let mut c = Command::new("sh");
    c.arg("-c")
        .arg(format!("{cmd} \"$1\" \"$2\""))
        .arg("sh") // $0
        .arg(repo)
        .arg(&out_path)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    tracing::info!(%cmd, env = cmd_env, "running configured SCIP command");
    let output = c
        .output()
        .with_context(|| format!("spawning {cmd_env} command"))?;
    if !output.status.success() {
        return Ok(skip(LangStatus::Failed(format!(
            "{cmd_env} command exited with {}\n{}",
            output.status,
            failure_tail(&output.stderr, &output.stdout, 25)
        ))));
    }

    match std::fs::read(&out_path) {
        Ok(bytes) if !bytes.is_empty() => {
            let n = bytes.len();
            parts.push(bytes);
            Ok(LangIndexResult {
                language: spec.language,
                scip_bytes: n,
                elapsed_ms: start.elapsed().as_millis() as i64,
                status: LangStatus::Indexed,
            })
        }
        _ => Ok(skip(LangStatus::Failed(format!(
            "{cmd_env} command succeeded but wrote no SCIP to {}",
            out_path.display()
        )))),
    }
}

/// Java symbol pass — always delegated (Java has no native fallback). See
/// `run_delegated`. A ready-made Bazel-aspect driver lives in
/// `docs/examples/scip-java-bazel-index.sh`.
fn run_java(
    spec: &LangSpec,
    repo: &Path,
    bazel: &Path,
    scip_dir: &Path,
    parts: &mut Vec<Vec<u8>>,
) -> Result<LangIndexResult> {
    run_delegated(
        spec,
        repo,
        bazel,
        scip_dir,
        parts,
        "ENGRAPH_BAZEL_SCIP_JAVA_CMD",
        "index-java.scip",
    )
}

/// Go symbol pass. If `ENGRAPH_BAZEL_SCIP_GO_CMD` is set, delegate the whole pass
/// to it — the only way to reach gazelle-managed `go_library` targets that have no
/// `go.mod` (scip-go needs a module root *and* Bazel-resolved deps, both
/// repo-specific). The command owns the Bazel→scip-go glue and emits
/// repo-root-relative SCIP. Unset → the native multi-module pass
/// (`run_go_modules`). A ready-made Bazel driver lives in
/// `docs/examples/scip-go-bazel-index.sh`.
fn run_go(
    spec: &LangSpec,
    repo: &Path,
    bazel: &Path,
    scip_dir: &Path,
    parts: &mut Vec<Vec<u8>>,
) -> Result<LangIndexResult> {
    const CMD_ENV: &str = GO_CMD_ENV;
    let configured = std::env::var(CMD_ENV)
        .map(|c| !c.trim().is_empty())
        .unwrap_or(false);
    if configured {
        run_delegated(spec, repo, bazel, scip_dir, parts, CMD_ENV, "index-go.scip")
    } else {
        run_go_modules(spec, repo, bazel, scip_dir, parts)
    }
}

fn build_indexer_command(spec: &LangSpec, repo: &Path, out_path: &Path) -> Command {
    match spec.language {
        "ts" => {
            let mut c = Command::new(spec.binary);
            c.arg("index")
                .arg("--output")
                .arg(out_path)
                .current_dir(repo);
            c
        }
        "python" => {
            // Mirror the standalone scip-python driver (driver.rs): --output is
            // resolved relative to --cwd, so pin --cwd to the repo and pass an
            // absolute out_path. --project-name defaults to the repo dir name,
            // and --project-version is pinned (see driver::SCIP_PYTHON_VERSION)
            // to avoid the undefined-version crash and keep entity IDs stable.
            let project_name = repo
                .file_name()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_else(|| "project".to_string());
            let mut c = Command::new(spec.binary);
            c.arg("index")
                .arg("--cwd")
                .arg(repo)
                .arg("--output")
                .arg(out_path)
                .arg("--project-name")
                .arg(project_name)
                .arg("--project-version")
                .arg(crate::driver::SCIP_PYTHON_VERSION);
            c
        }
        _ => unreachable!(),
    }
}

fn read_scip_output(spec: &LangSpec, out_path: &Path) -> Result<Vec<u8>> {
    if out_path.is_file() {
        return std::fs::read(out_path)
            .with_context(|| format!("reading SCIP at {}", out_path.display()));
    }
    anyhow::bail!(
        "{} reported success but produced no SCIP file at {}",
        spec.binary,
        out_path.display()
    );
}

/// Multi-module Go pass. Enumerates `go.mod` modules under the workspace, runs
/// `scip-go` per module, rebases each module's document paths back to repo-root,
/// and pushes the SCIP bytes into the shared `parts` for the single merged load.
/// Per-module failures are isolated (counted, not fatal) so one unbuildable
/// module can't sink the rest.
fn run_go_modules(
    spec: &LangSpec,
    repo: &Path,
    bazel: &Path,
    scip_dir: &Path,
    parts: &mut Vec<Vec<u8>>,
) -> Result<LangIndexResult> {
    let start = Instant::now();
    let skip = |status: LangStatus| LangIndexResult {
        language: spec.language,
        scip_bytes: 0,
        elapsed_ms: start.elapsed().as_millis() as i64,
        status,
    };

    let targets = bazel_count_targets(bazel, repo, spec.rule_kinds)?;
    if targets == 0 {
        return Ok(skip(LangStatus::SkippedNoTargets));
    }
    if !indexer_present(spec.binary) {
        return Ok(skip(LangStatus::SkippedNoIndexer {
            binary: spec.binary,
        }));
    }
    let modules = discover_go_modules(repo);
    if modules.is_empty() {
        // go_library targets exist but no go.mod anywhere (a pure gazelle repo):
        // scip-go has no module root to index from, so only a user-supplied
        // command can reach them — the same state Java is in when its command
        // is unset, and reported the same way.
        return Ok(skip(LangStatus::SkippedNotConfigured {
            targets,
            env: GO_CMD_ENV,
        }));
    }

    let mut indexed = 0usize;
    let mut failed = 0usize;
    let mut scip_bytes = 0usize;
    for (n, rel) in modules.iter().enumerate() {
        let module_root = if rel.as_os_str().is_empty() {
            Path::new(".")
        } else {
            rel.as_path()
        };
        let out_path = scip_dir.join(format!("index-go-{n}.scip"));
        let _ = std::fs::remove_file(&out_path); // ignore-if-absent

        let mut cmd = Command::new(spec.binary);
        cmd.arg("index")
            .arg("--module-root")
            .arg(module_root)
            .arg("--module-version")
            .arg(crate::driver::SCIP_GO_VERSION)
            // Skip test compilation: a module whose test-only deps don't resolve
            // would otherwise fail wholesale, losing its library symbols too.
            .arg("--skip-tests")
            .arg("--output")
            .arg(&out_path)
            .current_dir(repo)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        tracing::info!(driver = spec.binary, module = %module_root.display(), "running scip-go for module");
        let output = cmd
            .output()
            .with_context(|| format!("spawning {} for {}", spec.binary, module_root.display()))?;
        if !output.status.success() {
            failed += 1;
            tracing::warn!(
                module = %module_root.display(),
                "scip-go failed (isolated): {}",
                tail_lines(&String::from_utf8_lossy(&output.stderr), 20)
            );
            continue;
        }
        let bytes = match std::fs::read(&out_path) {
            Ok(b) => b,
            Err(e) => {
                failed += 1;
                tracing::warn!(module = %module_root.display(), "scip-go succeeded but its SCIP output was unreadable: {e}");
                continue;
            }
        };
        let rebased = match rebase_documents(&bytes, rel) {
            Ok(b) => b,
            Err(e) => {
                failed += 1;
                tracing::warn!(module = %module_root.display(), "rebasing SCIP paths failed (isolated): {e:#}");
                continue;
            }
        };
        scip_bytes += rebased.len();
        parts.push(rebased);
        indexed += 1;
    }

    Ok(LangIndexResult {
        language: spec.language,
        scip_bytes,
        elapsed_ms: start.elapsed().as_millis() as i64,
        status: LangStatus::IndexedModules {
            indexed,
            failed,
            targets,
        },
    })
}

/// Enumerate every Go module (directory containing `go.mod`) under `repo`, as
/// repo-relative paths in deterministic order. Prunes symlinked subtrees (which
/// excludes Bazel's `bazel-*` convenience symlinks without parsing `.gitignore`),
/// `vendor` / `testdata` (Go conventions for third-party / non-buildable code),
/// and hidden dirs (`.git`, `.claude/worktrees`, …). Recurses into module dirs
/// so nested modules are discovered independently. An empty path element means
/// the repo root itself is a module.
fn discover_go_modules(repo: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    walk_go_modules(repo, repo, &mut out);
    out.sort();
    out
}

fn walk_go_modules(repo: &Path, dir: &Path, out: &mut Vec<PathBuf>) {
    if dir.join("go.mod").is_file()
        && let Ok(rel) = dir.strip_prefix(repo)
    {
        out.push(rel.to_path_buf());
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let Ok(ft) = entry.file_type() else { continue };
        // Skip symlinks (Bazel's bazel-* convenience symlinks point into the
        // output_base and would explode the walk) and non-directories.
        if ft.is_symlink() || !ft.is_dir() {
            continue;
        }
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name.starts_with('.') || name == "vendor" || name == "testdata" {
            continue;
        }
        walk_go_modules(repo, &entry.path(), out);
    }
}

/// Rewrite each `Document.relative_path` in a per-module SCIP blob to be
/// repo-relative by prepending the module's repo-relative directory `prefix`.
/// scip-go emits paths relative to `--module-root`, but the loader stores
/// `relative_path` verbatim as `entities.file_path` — so without this, two
/// modules' `main.go` collide. Idempotent: a path already under `prefix/` is
/// left alone (tolerates scip-go builds that already emit repo-relative paths).
/// Monikers (which encode the module *path*, not the file path) and
/// `external_symbols` are untouched. Empty prefix (repo-root module) is a no-op.
pub(crate) fn rebase_documents(bytes: &[u8], prefix: &Path) -> Result<Vec<u8>> {
    use protobuf::Message;
    use scip::types::Index;

    let prefix_str = slash_prefix(prefix);
    if prefix_str.is_empty() {
        return Ok(bytes.to_vec());
    }
    let mut idx = Index::parse_from_bytes(bytes).context("decoding SCIP protobuf for rebase")?;
    prefix_documents(&mut idx, &prefix_str);
    idx.write_to_bytes()
        .context("serializing rebased SCIP index")
}

/// `rebase_documents` for an index run in `dir`, the directory of the subtree
/// `prefix`. Some indexers also list files outside `dir`, relative to their
/// build root: scip-java in a Gradle sub-project writes the sibling modules it
/// compiled, relative to the Gradle root (and sets `project_root` to `dir`, so
/// the metadata can't tell them apart). A document that is no file under `dir`
/// but is one at the repo root, outside `prefix`, belongs to another subtree
/// and is dropped: that subtree's own index owns it.
pub(crate) fn rebase_subtree_documents(bytes: &[u8], prefix: &Path, dir: &Path) -> Result<Vec<u8>> {
    use protobuf::Message;
    use scip::types::Index;

    let prefix_str = slash_prefix(prefix);
    if prefix_str.is_empty() {
        return Ok(bytes.to_vec());
    }
    let needle = format!("{prefix_str}/");
    let dir = dir.canonicalize().unwrap_or_else(|_| dir.to_path_buf());
    let root = dir
        .ancestors()
        .nth(prefix.components().count())
        .unwrap_or(&dir);

    let mut idx = Index::parse_from_bytes(bytes).context("decoding SCIP protobuf for rebase")?;
    let before = idx.documents.len();
    idx.documents.retain(|d| {
        let p = &d.relative_path;
        *p == prefix_str || p.starts_with(&needle) || dir.join(p).exists() || !root.join(p).exists()
    });
    let dropped = before - idx.documents.len();
    if dropped > 0 {
        tracing::info!(dropped, subtree = %prefix_str, "dropped SCIP documents outside the subtree");
    }
    prefix_documents(&mut idx, &prefix_str);
    idx.write_to_bytes()
        .context("serializing rebased SCIP index")
}

fn slash_prefix(prefix: &Path) -> String {
    prefix
        .components()
        .map(|c| c.as_os_str().to_string_lossy())
        .collect::<Vec<_>>()
        .join("/")
}

fn prefix_documents(idx: &mut scip::types::Index, prefix_str: &str) {
    let needle = format!("{prefix_str}/");
    for doc in &mut idx.documents {
        if doc.relative_path == prefix_str || doc.relative_path.starts_with(&needle) {
            continue;
        }
        doc.relative_path = format!("{prefix_str}/{}", doc.relative_path);
    }
}

fn bazel_has_targets(bazel: &Path, repo: &Path, kinds: &[&str]) -> Result<bool> {
    Ok(bazel_count_targets(bazel, repo, kinds)? > 0)
}

/// Count the workspace's targets of the given rule kinds via one
/// `bazel query --output=label_kind`. A failed query is an error, not zero:
/// zero reports "no targets" and skips a configured command. A rule kind the
/// workspace does not have is not a failure (Bazel 9.3.2 exits 0 with
/// `INFO: Empty results`), and the caller turns the error into a `Failed`
/// status for this language only. Sharing the target-level
/// pass's `output_base` is intentional: the analysis cache is warm and this
/// returns near-instantly. The Go pass uses the count to report the gazelle gap.
fn bazel_count_targets(bazel: &Path, repo: &Path, kinds: &[&str]) -> Result<usize> {
    let expr = kinds
        .iter()
        .map(|k| format!("kind({}, //...)", k))
        .collect::<Vec<_>>()
        .join(" union ");
    let output_base = output_base_for(repo);
    let mut cmd = Command::new(bazel);
    cmd.arg(format!("--output_base={}", output_base.display()))
        .arg("query")
        .arg("--output=label_kind")
        .arg(&expr)
        .current_dir(repo)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let out = cmd
        .output()
        .with_context(|| format!("spawning {} query", bazel.display()))?;
    if !out.status.success() {
        anyhow::bail!(
            "bazel query for {} exited with {}\n{}",
            kinds.join(", "),
            out.status,
            failure_tail(&out.stderr, &out.stdout, 20)
        );
    }
    Ok(count_label_kind_lines(&String::from_utf8_lossy(
        &out.stdout,
    )))
}

pub(crate) fn count_label_kind_lines(stdout: &str) -> usize {
    stdout.lines().filter(|l| !l.trim().is_empty()).count()
}

fn indexer_present(bin: &str) -> bool {
    Command::new(bin)
        .arg("--version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

fn scip_output_dir(repo: &Path) -> Result<PathBuf> {
    // Keep symbol-level SCIP outputs out of the user's workspace (so re-runs
    // don't dirty git state) and out of Bazel's own output_base (so a
    // `bazel clean` doesn't blow them away mid-load). Hash by canonical
    // repo path for stability across runs.
    let canonical = repo.canonicalize().unwrap_or_else(|_| repo.to_path_buf());
    let mut hasher = Sha256::new();
    hasher.update(canonical.to_string_lossy().as_bytes());
    let digest = hasher.finalize();
    let hex: String = digest.iter().map(|b| format!("{:02x}", b)).collect();
    let base = dirs::cache_dir()
        .unwrap_or_else(|| dirs::home_dir().unwrap_or_default().join(".cache"))
        .join("engraph")
        .join("bazel-scip-out")
        .join(&hex[..16]);
    std::fs::create_dir_all(&base)
        .with_context(|| format!("creating scip output dir {}", base.display()))?;
    Ok(base)
}

/// Merge several SCIP byte streams into one by concatenating their
/// `documents` and `external_symbols` vectors. The protobuf `Index`
/// message wraps both as repeated fields, so a merge is just `Vec::extend`
/// on each. Metadata from the first non-empty input wins.
pub(crate) fn merge_scip_bytes(parts: &[Vec<u8>]) -> Result<Vec<u8>> {
    use protobuf::Message;
    use scip::types::Index;
    let mut merged = Index::new();
    let mut took_metadata = false;
    for bytes in parts {
        if bytes.is_empty() {
            continue;
        }
        let idx = Index::parse_from_bytes(bytes).context("decoding SCIP protobuf for merge")?;
        if !took_metadata && idx.metadata.is_some() {
            merged.metadata = idx.metadata.clone();
            took_metadata = true;
        }
        merged.documents.extend(idx.documents);
        merged.external_symbols.extend(idx.external_symbols);
    }
    merged
        .write_to_bytes()
        .context("serializing merged SCIP index")
}

#[cfg(test)]
mod tests {
    use super::*;
    use protobuf::{Message, MessageField};
    use scip::types::{Document, Index, Metadata, SymbolInformation};

    fn make_index(doc_paths: &[&str], external: &[&str]) -> Vec<u8> {
        let mut idx = Index::new();
        idx.metadata = MessageField::some(Metadata::new());
        for p in doc_paths {
            let mut d = Document::new();
            d.relative_path = (*p).to_string();
            idx.documents.push(d);
        }
        for s in external {
            let mut si = SymbolInformation::new();
            si.symbol = (*s).to_string();
            idx.external_symbols.push(si);
        }
        idx.write_to_bytes().unwrap()
    }

    #[test]
    fn merge_scip_bytes_preserves_documents_and_externals() {
        let a = make_index(&["a/a.rs", "a/b.rs"], &["ext-a"]);
        let b = make_index(&["b/c.go"], &["ext-b", "ext-c"]);
        let merged_bytes = merge_scip_bytes(&[a, b]).unwrap();
        let merged = Index::parse_from_bytes(&merged_bytes).unwrap();
        assert_eq!(merged.documents.len(), 3);
        assert_eq!(merged.external_symbols.len(), 3);
        let paths: Vec<&str> = merged
            .documents
            .iter()
            .map(|d| d.relative_path.as_str())
            .collect();
        assert!(paths.contains(&"a/a.rs"));
        assert!(paths.contains(&"a/b.rs"));
        assert!(paths.contains(&"b/c.go"));
    }

    #[test]
    fn merge_scip_bytes_empty_input_is_valid_empty_index() {
        let merged = merge_scip_bytes(&[]).unwrap();
        let parsed = Index::parse_from_bytes(&merged).unwrap();
        assert_eq!(parsed.documents.len(), 0);
        assert_eq!(parsed.external_symbols.len(), 0);
    }

    #[test]
    fn merge_scip_bytes_skips_empty_byte_blobs() {
        let a = make_index(&["a.rs"], &[]);
        let merged = merge_scip_bytes(&[Vec::new(), a, Vec::new()]).unwrap();
        let parsed = Index::parse_from_bytes(&merged).unwrap();
        assert_eq!(parsed.documents.len(), 1);
    }

    #[test]
    fn count_label_kind_lines_counts_nonempty() {
        assert_eq!(count_label_kind_lines(""), 0);
        assert_eq!(count_label_kind_lines("\n  \n"), 0);
        assert_eq!(count_label_kind_lines("java_library //foo:foo\n"), 1);
        assert_eq!(
            count_label_kind_lines("go_library //a:a\n\n  \ngo_library //b:b\n"),
            2
        );
    }

    #[test]
    fn lang_status_display_mentions_binary() {
        let s = LangStatus::SkippedNoIndexer {
            binary: "scip-java",
        };
        assert!(format!("{}", s).contains("scip-java"));
    }

    #[test]
    fn lang_status_display_failed_message() {
        let s = LangStatus::Failed("boom".to_string());
        assert_eq!(format!("{}", s), "failed: boom");
    }

    #[test]
    fn lang_status_not_configured_names_the_target_count_and_the_env_var() {
        // One text for every language: the count sizes the gap, the env var
        // says how to close it. `SkippedNoTargets` would claim the workspace
        // has no code of this language at all.
        let s = LangStatus::SkippedNotConfigured {
            targets: 3,
            env: GO_CMD_ENV,
        };
        assert_eq!(
            format!("{s}"),
            "skipped (3 targets, set ENGRAPH_BAZEL_SCIP_GO_CMD to index them)"
        );
        let s = LangStatus::SkippedNotConfigured {
            targets: 2,
            env: "ENGRAPH_BAZEL_SCIP_JAVA_CMD",
        };
        assert_eq!(
            format!("{s}"),
            "skipped (2 targets, set ENGRAPH_BAZEL_SCIP_JAVA_CMD to index them)"
        );
    }

    /// A stand-in `bazel` that answers any query with `n` `label_kind` lines,
    /// so a test can hand `bazel_count_targets` a nonzero count without a real
    /// workspace. Needs the exec bit, hence unix-only — as is `run_delegated`
    /// itself, which shells out through `sh`.
    #[cfg(unix)]
    fn fake_bazel(dir: &Path, n: usize) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let p = dir.join("fake-bazel.sh");
        let mut body = String::from("#!/bin/sh\n");
        for i in 0..n {
            body.push_str(&format!("echo 'java_library //t{i}:t{i}'\n"));
        }
        std::fs::write(&p, body).unwrap();
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
        p
    }

    /// Write an executable script from a child process. If this process held
    /// the write fd, a fork by another test thread could inherit it, and the
    /// exec of the script would fail with ETXTBSY ("Text file busy").
    #[cfg(unix)]
    fn write_script(p: &Path, body: &str) {
        let status = Command::new("sh")
            .args([
                "-c",
                "printf '%s' \"$1\" > \"$2\" && chmod 755 \"$2\"",
                "sh",
            ])
            .arg(body)
            .arg(p)
            .status()
            .unwrap();
        assert!(status.success());
    }

    /// A stand-in `bazel` whose query fails with exit status 2.
    #[cfg(unix)]
    fn failing_bazel(dir: &Path, stderr: &str, stdout: &str) -> PathBuf {
        let p = dir.join("failing-bazel.sh");
        let body = format!("#!/bin/sh\necho '{stdout}'\necho '{stderr}' >&2\nexit 2\n");
        write_script(&p, &body);
        p
    }

    #[cfg(unix)]
    #[test]
    fn a_failed_target_count_is_an_error_not_zero() {
        // Zero would report `skipped (no targets in this workspace)` and skip
        // a configured command. The first line carries the reason, because
        // `print_symbol_langs` shows only the first line of a status.
        let kinds = &["java_library", "java_binary", "java_test"];
        let first =
            "bazel query for java_library, java_binary, java_test exited with exit status: 2";

        let tmp = tempfile::tempdir().unwrap();
        let bazel = failing_bazel(tmp.path(), "ERROR: count failed", "");
        let msg = format!(
            "{:#}",
            bazel_count_targets(&bazel, tmp.path(), kinds).unwrap_err()
        );
        assert_eq!(msg.lines().next(), Some(first), "{msg}");
        assert!(msg.contains("stderr (tail):\nERROR: count failed"), "{msg}");

        let tmp = tempfile::tempdir().unwrap();
        let bazel = failing_bazel(tmp.path(), "", "ERROR: on stdout");
        let msg = format!(
            "{:#}",
            bazel_count_targets(&bazel, tmp.path(), kinds).unwrap_err()
        );
        assert_eq!(msg.lines().next(), Some(first), "{msg}");
        assert!(msg.contains("stdout (tail):\nERROR: on stdout"), "{msg}");

        // The delegated path stops at the count, before it reads its env var.
        let spec = LANGS.iter().find(|s| s.language == "java").unwrap();
        let scip_dir = tmp.path().join("out");
        std::fs::create_dir_all(&scip_dir).unwrap();
        let mut parts: Vec<Vec<u8>> = Vec::new();
        assert!(run_java(spec, tmp.path(), &bazel, &scip_dir, &mut parts).is_err());
    }

    /// Serializes the tests that read or write `ENGRAPH_BAZEL_SCIP_JAVA_CMD`
    /// or `ENGRAPH_BAZEL_SCIP_GO_CMD`. The env is process-global, so without
    /// it a parallel test can unset a command another test just set.
    static CMD_ENV: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn cmd_env_lock() -> std::sync::MutexGuard<'static, ()> {
        // A failed test poisons the mutex; the next test still gets the lock.
        CMD_ENV.lock().unwrap_or_else(|e| e.into_inner())
    }

    #[cfg(unix)]
    #[test]
    fn a_delegated_language_reports_no_targets_before_it_reads_the_env_var() {
        // The Java gap this mirrors the Go one from bug 5: with no java
        // targets, telling the user to configure a java indexer is noise.
        let tmp = tempfile::tempdir().unwrap();
        let spec = LANGS.iter().find(|s| s.language == "java").unwrap();
        let scip_dir = tmp.path().join("out");
        std::fs::create_dir_all(&scip_dir).unwrap();
        let bazel = fake_bazel(tmp.path(), 0);

        let _env = cmd_env_lock();
        // SAFETY: `_env` serializes every test that touches this variable.
        unsafe { std::env::remove_var("ENGRAPH_BAZEL_SCIP_JAVA_CMD") };
        let mut parts: Vec<Vec<u8>> = Vec::new();
        let r = run_java(spec, tmp.path(), &bazel, &scip_dir, &mut parts).unwrap();
        assert_eq!(r.status, LangStatus::SkippedNoTargets);
        assert!(parts.is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn a_delegated_language_names_the_targets_it_misses_when_unconfigured() {
        let tmp = tempfile::tempdir().unwrap();
        let spec = LANGS.iter().find(|s| s.language == "java").unwrap();
        let scip_dir = tmp.path().join("out");
        std::fs::create_dir_all(&scip_dir).unwrap();
        let bazel = fake_bazel(tmp.path(), 2);

        let _env = cmd_env_lock();
        // SAFETY: `_env` serializes every test that touches this variable.
        unsafe { std::env::remove_var("ENGRAPH_BAZEL_SCIP_JAVA_CMD") };
        let mut parts: Vec<Vec<u8>> = Vec::new();
        let r = run_java(spec, tmp.path(), &bazel, &scip_dir, &mut parts).unwrap();
        assert_eq!(
            r.status,
            LangStatus::SkippedNotConfigured {
                targets: 2,
                env: "ENGRAPH_BAZEL_SCIP_JAVA_CMD",
            }
        );
        assert!(parts.is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn run_java_delegates_to_configured_command() {
        let tmp = tempfile::tempdir().unwrap();
        let spec = LANGS.iter().find(|s| s.language == "java").unwrap();
        let scip_dir = tmp.path().join("out");
        std::fs::create_dir_all(&scip_dir).unwrap();
        // Targets are counted before the env var is read, so the delegated
        // path needs a bazel that reports some.
        let bazel = fake_bazel(tmp.path(), 2);

        // Unconfigured → skipped, nothing merged. SAFETY: `_env` serializes
        // every test that touches this variable.
        let _env = cmd_env_lock();
        unsafe { std::env::remove_var("ENGRAPH_BAZEL_SCIP_JAVA_CMD") };
        let mut parts: Vec<Vec<u8>> = Vec::new();
        let r = run_java(spec, tmp.path(), &bazel, &scip_dir, &mut parts).unwrap();
        assert!(matches!(r.status, LangStatus::SkippedNotConfigured { .. }));
        assert!(parts.is_empty());

        // A fake command that writes a known SCIP to its 2nd arg → merged.
        let scip = make_index(&["Hello.java"], &[]);
        let scip_path = tmp.path().join("fake.scip");
        std::fs::write(&scip_path, &scip).unwrap();
        let cmd_path = tmp.path().join("fake-cmd.sh");
        std::fs::write(
            &cmd_path,
            format!("cp \"{}\" \"$2\"\n", scip_path.display()),
        )
        .unwrap();
        unsafe {
            std::env::set_var(
                "ENGRAPH_BAZEL_SCIP_JAVA_CMD",
                format!("sh \"{}\"", cmd_path.display()),
            )
        };
        let mut parts: Vec<Vec<u8>> = Vec::new();
        let r = run_java(spec, tmp.path(), &bazel, &scip_dir, &mut parts).unwrap();
        unsafe { std::env::remove_var("ENGRAPH_BAZEL_SCIP_JAVA_CMD") };
        assert_eq!(r.status, LangStatus::Indexed);
        assert_eq!(parts.len(), 1);
        assert_eq!(parts[0], scip);
    }

    #[cfg(unix)]
    #[test]
    fn run_go_delegates_to_configured_command() {
        let tmp = tempfile::tempdir().unwrap();
        let spec = LANGS.iter().find(|s| s.language == "go").unwrap();
        let scip_dir = tmp.path().join("out");
        std::fs::create_dir_all(&scip_dir).unwrap();
        // Both branches of run_go now count targets first, so the delegated
        // branch needs a bazel that reports some.
        let bazel = fake_bazel(tmp.path(), 3);

        // A fake command that writes a known SCIP to its 2nd arg → delegated and
        // merged. SAFETY: `_env` serializes every test that touches this
        // variable.
        let _env = cmd_env_lock();
        let scip = make_index(&["pkg/widget.go"], &[]);
        let scip_path = tmp.path().join("fake.scip");
        std::fs::write(&scip_path, &scip).unwrap();
        let cmd_path = tmp.path().join("fake-go-cmd.sh");
        std::fs::write(
            &cmd_path,
            format!("cp \"{}\" \"$2\"\n", scip_path.display()),
        )
        .unwrap();
        unsafe {
            std::env::set_var(
                "ENGRAPH_BAZEL_SCIP_GO_CMD",
                format!("sh \"{}\"", cmd_path.display()),
            )
        };
        let mut parts: Vec<Vec<u8>> = Vec::new();
        let r = run_go(spec, tmp.path(), &bazel, &scip_dir, &mut parts).unwrap();
        unsafe { std::env::remove_var("ENGRAPH_BAZEL_SCIP_GO_CMD") };
        assert_eq!(r.status, LangStatus::Indexed);
        assert_eq!(parts.len(), 1);
        assert_eq!(parts[0], scip);
    }

    #[test]
    fn langs_includes_python() {
        let py = LANGS
            .iter()
            .find(|s| s.language == "python")
            .expect("python LangSpec present");
        assert_eq!(py.binary, "scip-python");
        assert!(py.rule_kinds.contains(&"py_library"));
    }

    #[test]
    fn build_indexer_command_python_argv() {
        let py = LANGS.iter().find(|s| s.language == "python").unwrap();
        let repo = Path::new("/tmp/some-repo");
        let out = Path::new("/tmp/out/index-python.scip");
        let cmd = build_indexer_command(py, repo, out);
        let args: Vec<String> = cmd
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert!(args.contains(&"index".to_string()));
        assert!(args.contains(&"--cwd".to_string()));
        assert!(args.contains(&"--output".to_string()));
        assert!(args.contains(&"--project-name".to_string()));
        // --project-name derives from the repo dir name.
        assert!(args.contains(&"some-repo".to_string()));
        // --project-version is pinned to avoid scip-python's undefined-version crash.
        assert!(args.contains(&"--project-version".to_string()));
        assert!(args.contains(&crate::driver::SCIP_PYTHON_VERSION.to_string()));
    }

    #[test]
    fn plan_symbol_langs_covers_all_langs() {
        let plan = plan_symbol_langs();
        assert_eq!(plan.len(), LANGS.len());
        let langs: Vec<&str> = plan.iter().map(|p| p.language).collect();
        assert!(langs.contains(&"java"));
        assert!(langs.contains(&"python"));
    }

    #[test]
    fn lang_status_indexed_modules_display() {
        let s = LangStatus::IndexedModules {
            indexed: 3,
            failed: 0,
            targets: 1234,
        };
        assert_eq!(
            format!("{s}"),
            "indexed 3 go.mod modules of 1234 go targets"
        );
        let s = LangStatus::IndexedModules {
            indexed: 2,
            failed: 1,
            targets: 1234,
        };
        let out = format!("{s}");
        assert!(out.contains("2 go.mod modules"), "{out}");
        assert!(out.contains("1 failed"), "{out}");
    }

    #[test]
    fn discover_go_modules_finds_nested_and_prunes() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let mk = |rel: &str| {
            let dir = root.join(rel);
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join("go.mod"), "module x\n").unwrap();
        };
        // Two real modules, one nested under the other.
        mk("svc");
        mk("svc/inner");
        // Pruned: vendored, testdata, and hidden trees each carry a go.mod.
        mk("svc/vendor/dep");
        mk("testdata/fixturemod");
        mk(".worktree/copy");
        // A plain dir without go.mod is still walked, just not recorded.
        std::fs::create_dir_all(root.join("plain")).unwrap();

        let got: Vec<String> = discover_go_modules(root)
            .iter()
            .map(|p| p.to_string_lossy().replace('\\', "/"))
            .collect();
        assert_eq!(got, vec!["svc".to_string(), "svc/inner".to_string()]);
    }

    #[cfg(unix)]
    #[test]
    fn discover_go_modules_skips_symlinked_trees() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let real = root.join("real");
        std::fs::create_dir_all(&real).unwrap();
        std::fs::write(real.join("go.mod"), "module x\n").unwrap();
        // A bazel-* style symlink whose target (with its own go.mod) lives
        // OUTSIDE the walked root, so it is only reachable by following the
        // symlink — which the walk must refuse to do.
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("go.mod"), "module y\n").unwrap();
        std::os::unix::fs::symlink(outside.path(), root.join("bazel-root")).unwrap();

        let got: Vec<String> = discover_go_modules(root)
            .iter()
            .map(|p| p.to_string_lossy().into_owned())
            .collect();
        assert_eq!(got, vec!["real".to_string()]);
    }

    #[test]
    fn rebase_documents_prefixes_paths_keeps_monikers() {
        let mut idx = Index::new();
        let mut d = Document::new();
        d.relative_path = "handlers/widget.go".to_string();
        let mut si = SymbolInformation::new();
        si.symbol = "scip-go gomod example.com/team/widget v Widget#".to_string();
        d.symbols.push(si);
        idx.documents.push(d);
        // A document already carrying the prefix must not be double-prefixed.
        let mut d2 = Document::new();
        d2.relative_path = "team/widget/main.go".to_string();
        idx.documents.push(d2);
        let bytes = idx.write_to_bytes().unwrap();

        let out = rebase_documents(&bytes, Path::new("team/widget")).unwrap();
        let parsed = Index::parse_from_bytes(&out).unwrap();
        let paths: Vec<&str> = parsed
            .documents
            .iter()
            .map(|d| d.relative_path.as_str())
            .collect();
        assert_eq!(parsed.documents.len(), 2);
        assert!(paths.contains(&"team/widget/handlers/widget.go"));
        // Already-prefixed path left alone (no double prefix).
        assert!(paths.contains(&"team/widget/main.go"));
        // Moniker untouched by the rebase.
        assert_eq!(
            parsed.documents[0].symbols[0].symbol,
            "scip-go gomod example.com/team/widget v Widget#"
        );
    }

    #[test]
    fn rebase_documents_empty_prefix_is_noop() {
        let mut idx = Index::new();
        let mut d = Document::new();
        d.relative_path = "main.go".to_string();
        idx.documents.push(d);
        let bytes = idx.write_to_bytes().unwrap();
        let out = rebase_documents(&bytes, Path::new("")).unwrap();
        let parsed = Index::parse_from_bytes(&out).unwrap();
        assert_eq!(parsed.documents[0].relative_path, "main.go");
    }

    #[test]
    fn rebase_subtree_documents_drops_sibling_files() {
        // scip-java run in a Gradle sub-project also lists the sibling modules
        // it compiled, relative to the Gradle (repo) root.
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        for f in ["sub/src/A.java", "sub/src/B.java", "other/src/C.java"] {
            std::fs::create_dir_all(root.join(f).parent().unwrap()).unwrap();
            std::fs::write(root.join(f), "").unwrap();
        }
        let mut idx = Index::new();
        for p in [
            "src/A.java",       // relative to the subtree
            "sub/src/B.java",   // relative to the repo root, inside the subtree
            "other/src/C.java", // relative to the repo root, a sibling module
            "gen/Missing.java", // no file anywhere: rebased as before
        ] {
            let mut d = Document::new();
            d.relative_path = p.to_string();
            idx.documents.push(d);
        }
        let bytes = idx.write_to_bytes().unwrap();

        let out = rebase_subtree_documents(&bytes, Path::new("sub"), &root.join("sub")).unwrap();
        let parsed = Index::parse_from_bytes(&out).unwrap();
        let paths: Vec<&str> = parsed
            .documents
            .iter()
            .map(|d| d.relative_path.as_str())
            .collect();
        assert_eq!(
            paths,
            vec!["sub/src/A.java", "sub/src/B.java", "sub/gen/Missing.java"]
        );
    }
}
