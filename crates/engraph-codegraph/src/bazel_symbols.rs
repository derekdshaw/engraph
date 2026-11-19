//! F2 Phase 2.3 #2 — symbol-level Bazel indexing.
//!
//! Sits on top of the target-level pass in `bazel.rs`. For each of
//! {Java, Go, TypeScript, Python, Rust} we (a) probe `bazel query` for any target of that
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
//! - **TypeScript and JavaScript** (language id `ts`): if
//!   `ENGRAPH_BAZEL_SCIP_TS_CMD` is set, delegated exactly like Java. Unset →
//!   `scip-typescript index --infer-tsconfig` at the workspace root, with the
//!   standalone driver's workspace flag. `rules_js` makes `node_modules` under
//!   `bazel-bin` during a build, not at the root, and scip-typescript fails
//!   without them, so no root `node_modules` → reported not-configured.
//! - **Python**: `scip-python index --cwd .` at the workspace root. In a
//!   Bazel-`rules_python` monorepo the sub-projects share no single venv, so
//!   import resolution from the root is best-effort — unresolved cross-package
//!   imports surface as external symbols rather than edges (analogous to the
//!   `rules_ts` and multi-`go.mod` caveats above).
//! - **Rust**: if `ENGRAPH_BAZEL_SCIP_RUST_CMD` is set, delegated exactly like
//!   Java. Unset → `rust-analyzer scip` on the root `Cargo.toml`, as the
//!   standalone driver runs it. No root `Cargo.toml` → reported not-configured,
//!   as Go is with no `go.mod`.
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

use crate::bazel::{FAILURE_TAIL_LINES, bazel_binary, failure_tail, output_base_for};
use crate::scip_loader::{self, LoadOptions};
use anyhow::{Context, Result};
use engraph_core::db::PooledConn;
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
    pub rust: Option<LangIndexResult>,
    pub entities_inserted: usize,
    pub relations_inserted: usize,
    pub entities_pruned: usize,
    pub scip_bytes_total: usize,
    pub elapsed_ms: i64,
}

impl BazelSymbolStats {
    /// Per-language results in stable order (java, go, ts, python, rust),
    /// skipping languages that were never attempted (None). After a real run
    /// every `LANGS` entry is present, including skipped/failed ones.
    pub fn results(&self) -> Vec<&LangIndexResult> {
        [&self.java, &self.go, &self.ts, &self.python, &self.rust]
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
    /// `env` names a command, but none of the rule kinds engraph counts exist,
    /// so it did not run. Names both: a plain "no targets" would read as a
    /// bug to the user who exported the variable, and the kinds tell them
    /// whether their code is built with rule classes engraph does not count.
    SkippedNoTargetsForCommand {
        rule_kinds: &'static [&'static str],
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
            LangStatus::SkippedNoTargetsForCommand { rule_kinds, env } => write!(
                f,
                "skipped (no {} targets; {env} not run)",
                rule_kinds.join(", ")
            ),
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
        // scip-typescript indexes JavaScript too, so the `aspect_rules_js`
        // source rules count here. `js_run_binary` runs a tool at build time
        // and is not source.
        rule_kinds: &[
            "ts_project",
            "ts_library",
            "js_library",
            "js_binary",
            "js_test",
        ],
    },
    LangSpec {
        language: "python",
        binary: "scip-python",
        rule_kinds: &["py_library", "py_binary", "py_test"],
    },
    LangSpec {
        language: "rust",
        binary: "rust-analyzer",
        // Named classes, not `rust_.*`: that also counts toolchain rules
        // (`rust_prost_toolchain`). `rust_library_group` and
        // `rust_prost_library` cover repos with no top-level `rust_library`.
        rule_kinds: &[
            "rust_library",
            "rust_binary",
            "rust_test",
            "rust_proc_macro",
            "rust_library_group",
            "rust_prost_library",
        ],
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
    go: crate::index::GoOptions,
) -> Result<BazelSymbolStats> {
    let start = Instant::now();
    let mut stats = BazelSymbolStats::default();
    let scip_dir = scip_output_dir(repo)?;
    let bazel = bazel_binary()?;

    let mut parts: Vec<Vec<u8>> = Vec::new();
    for spec in LANGS {
        // Go: delegate if ENGRAPH_BAZEL_SCIP_GO_CMD is set, else the native
        // multi-module pass. Java: always delegated (its SCIP build is
        // repo-specific). Rust / TS: delegate if ENGRAPH_BAZEL_SCIP_RUST_CMD /
        // ENGRAPH_BAZEL_SCIP_TS_CMD is set, else the indexer on the root
        // Cargo.toml / node_modules. Python: single-shot standalone indexer
        // at the workspace root.
        let outcome = match spec.language {
            "go" => run_go(spec, repo, &bazel, &scip_dir, &mut parts, go),
            "java" => run_java(spec, repo, &bazel, &scip_dir, &mut parts),
            "rust" => run_rust(spec, repo, &bazel, &scip_dir, &mut parts),
            "ts" => run_ts(spec, repo, &bazel, &scip_dir, &mut parts),
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
            "rust" => stats.rust = Some(result),
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
    let cmd = build_indexer_command(spec, repo, &out_path);
    run_indexer(spec, cmd, &out_path, parts, start)
}

/// Run one single-shot SCIP indexer `cmd` that writes to `out_path`, and merge
/// its output: remove any stale `out_path`, spawn, report a non-zero exit as
/// `Failed` with the stderr tail, else push the SCIP at `out_path` onto
/// `parts`. `start` is the start of the whole language pass, so `elapsed_ms`
/// covers the target probe too.
fn run_indexer(
    spec: &LangSpec,
    mut cmd: Command,
    out_path: &Path,
    parts: &mut Vec<Vec<u8>>,
    start: Instant,
) -> Result<LangIndexResult> {
    let _ = std::fs::remove_file(out_path); // ignore-if-absent
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
                failure_tail(&output.stderr, &output.stdout, FAILURE_TAIL_LINES)
            )),
        });
    }

    let bytes = read_scip_output(spec, out_path)?;
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

    // The count gates the command either way (a `--workspace` run must not
    // start the Java command in every repo without Java); the variable only
    // decides what a zero count is reported as.
    let cmd = std::env::var(cmd_env).ok().filter(|c| !c.trim().is_empty());
    let targets = bazel_count_targets(bazel, repo, spec.rule_kinds)?;
    if targets == 0 {
        return Ok(skip(match cmd {
            Some(_) => LangStatus::SkippedNoTargetsForCommand {
                rule_kinds: spec.rule_kinds,
                env: cmd_env,
            },
            None => LangStatus::SkippedNoTargets,
        }));
    }
    let Some(cmd) = cmd else {
        return Ok(skip(LangStatus::SkippedNotConfigured {
            targets,
            env: cmd_env,
        }));
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
            failure_tail(&output.stderr, &output.stdout, FAILURE_TAIL_LINES)
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
    go: crate::index::GoOptions,
) -> Result<LangIndexResult> {
    const CMD_ENV: &str = GO_CMD_ENV;
    let configured = std::env::var(CMD_ENV)
        .map(|c| !c.trim().is_empty())
        .unwrap_or(false);
    if configured {
        run_delegated(spec, repo, bazel, scip_dir, parts, CMD_ENV, "index-go.scip")
    } else {
        run_go_modules(spec, repo, bazel, scip_dir, parts, go)
    }
}

/// The command that indexes Rust under Bazel when the root has no `Cargo.toml`
/// (`run_rust`), and the `env` of the `SkippedNotConfigured` reported then.
const RUST_CMD_ENV: &str = "ENGRAPH_BAZEL_SCIP_RUST_CMD";

/// The command that indexes TypeScript and JavaScript under Bazel when the root
/// has no `node_modules` (`run_ts`), and the `env` of the
/// `SkippedNotConfigured` reported then.
const TS_CMD_ENV: &str = "ENGRAPH_BAZEL_SCIP_TS_CMD";

/// Rust symbol pass. Unset `ENGRAPH_BAZEL_SCIP_RUST_CMD` → rust-analyzer on the
/// root `Cargo.toml`, with the argv of the standalone driver
/// (`driver::RustAnalyzer`). Without a root `Cargo.toml` rust-analyzer has no
/// project to load, so only a user-supplied command can reach the targets (for
/// example one that has `rules_rust` write a `rust-project.json`).
fn run_rust(
    spec: &LangSpec,
    repo: &Path,
    bazel: &Path,
    scip_dir: &Path,
    parts: &mut Vec<Vec<u8>>,
) -> Result<LangIndexResult> {
    use crate::driver::Driver;
    run_rooted_or_delegated(
        spec,
        repo,
        bazel,
        scip_dir,
        parts,
        RUST_CMD_ENV,
        "Cargo.toml",
        |out| crate::driver::RustAnalyzer.command(repo, out),
    )
}

/// The two configs `--infer-tsconfig` writes verbatim (scip-typescript's
/// `inferTsconfig.ts`). Only a new file whose content is exactly one of them
/// is removed after a ts run.
const INFERRED_TSCONFIGS: [&str; 2] = ["{}", r#"{"compilerOptions":{"allowJs":true}}"#];

/// Every `tsconfig.json` under `repo`, pruning `node_modules`, `bazel-*`,
/// hidden directories, and symlinked trees.
fn tsconfig_paths(repo: &Path) -> std::collections::BTreeSet<PathBuf> {
    fn walk(dir: &Path, out: &mut std::collections::BTreeSet<PathBuf>) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if name == "node_modules" || name.starts_with('.') || name.starts_with("bazel-") {
                continue;
            }
            let path = entry.path();
            if path.is_symlink() {
                continue;
            }
            if path.is_dir() {
                walk(&path, out);
            } else if name == "tsconfig.json" {
                out.insert(path);
            }
        }
    }
    let mut out = std::collections::BTreeSet::new();
    walk(repo, &mut out);
    out
}

/// Remove the `tsconfig.json` files a ts run created: `--infer-tsconfig`
/// writes one into each project that has none and leaves it in the repo. Only
/// a file that `before` does not have, with exactly an inferred content
/// (`INFERRED_TSCONFIGS`), goes.
fn remove_inferred_tsconfigs(repo: &Path, before: &std::collections::BTreeSet<PathBuf>) {
    for path in tsconfig_paths(repo).difference(before) {
        let Ok(text) = std::fs::read_to_string(path) else {
            continue;
        };
        if INFERRED_TSCONFIGS.contains(&text.trim()) && std::fs::remove_file(path).is_ok() {
            tracing::info!(
                path = %path.display(),
                "removed a tsconfig.json that --infer-tsconfig wrote"
            );
        }
    }
}

/// TypeScript and JavaScript symbol pass. Unset `ENGRAPH_BAZEL_SCIP_TS_CMD` →
/// scip-typescript at the root (`build_indexer_command`). It needs the
/// packages installed, and `rules_js` makes `node_modules` under `bazel-bin`
/// during a build, not at the root; without a root `node_modules`
/// scip-typescript would fail, so only a user-supplied command (one that
/// installs the packages first) can reach the targets.
fn run_ts(
    spec: &LangSpec,
    repo: &Path,
    bazel: &Path,
    scip_dir: &Path,
    parts: &mut Vec<Vec<u8>>,
) -> Result<LangIndexResult> {
    // `--infer-tsconfig` writes into the repo; sweep what this run created,
    // whichever branch ran and however it ended.
    let before = tsconfig_paths(repo);
    let mut result = run_ts_inner(spec, repo, bazel, scip_dir, parts);
    remove_inferred_tsconfigs(repo, &before);
    if let Ok(r) = &mut result
        && let LangStatus::Failed(msg) = &r.status
        && let Some(help) = pnpm_broken_lockfile_help(repo, msg)
    {
        r.status = LangStatus::Failed(help);
    }
    result
}

/// A clear replacement for a scip-typescript failure that means the pnpm on
/// `PATH` cannot read `pnpm-lock.yaml` (`--pnpm-workspaces` makes
/// scip-typescript run `pnpm ls`): the same two fixes the example driver
/// names. `None` for any other failure, and for a lockfile of a current
/// version — then the file is genuinely broken and pnpm's own error says why.
///
/// The trigger is pnpm's `ERR_PNPM_BROKEN_LOCKFILE` token, or the words
/// `lockfileVersion` and `incompatible`: scip-typescript reports the pnpm
/// error as a throw dump without the token, and pnpm's error box wraps its
/// lines between words, so a single word is the longest text that always
/// survives the wrapping.
fn pnpm_broken_lockfile_help(repo: &Path, failure: &str) -> Option<String> {
    let dump_words = failure.contains("lockfileVersion") && failure.contains("incompatible");
    if !failure.contains("ERR_PNPM_BROKEN_LOCKFILE") && !dump_words {
        return None;
    }
    let version = pnpm_lockfile_version(repo)?;
    // The pnpm major that writes this lockfile version, for the message only;
    // the check itself is pnpm's own error.
    let writes_it = match version.split('.').next() {
        Some("5") => "7",
        Some("6") => "8",
        _ => return None,
    };
    let have = Command::new("pnpm")
        .arg("--version")
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| format!("pnpm {}", String::from_utf8_lossy(&o.stdout).trim()));
    Some(format!(
        "{have} cannot read pnpm-lock.yaml (lockfileVersion {version})\n\
         upgrade the lockfile: run `pnpm install --lockfile-only` in the repo \
         (it rewrites pnpm-lock.yaml), or put pnpm {writes_it} on PATH \
         (it writes lockfileVersion {version})",
        have = have.as_deref().unwrap_or("the pnpm on PATH"),
    ))
}

/// The `lockfileVersion` of the repo's `pnpm-lock.yaml`, e.g. `6.0`.
fn pnpm_lockfile_version(repo: &Path) -> Option<String> {
    let text = std::fs::read_to_string(repo.join("pnpm-lock.yaml")).ok()?;
    let line = text.lines().find(|l| l.starts_with("lockfileVersion"))?;
    let value: String = line
        .chars()
        .filter(|c| c.is_ascii_digit() || *c == '.')
        .collect();
    (!value.is_empty()).then_some(value)
}

fn run_ts_inner(
    spec: &LangSpec,
    repo: &Path,
    bazel: &Path,
    scip_dir: &Path,
    parts: &mut Vec<Vec<u8>>,
) -> Result<LangIndexResult> {
    run_rooted_or_delegated(
        spec,
        repo,
        bazel,
        scip_dir,
        parts,
        TS_CMD_ENV,
        "node_modules",
        |out| build_indexer_command(spec, repo, out),
    )
}

/// The pass of a language whose native indexer needs `root` (a file or a
/// directory) at the workspace root. If `cmd_env` is set, delegate the whole
/// pass to it (see `run_delegated`). Unset → count the targets, and with
/// targets but no `root`, report `SkippedNotConfigured` naming `cmd_env` — the
/// same state as Go with no `go.mod`, reported the same way. Else run the
/// command `command` builds for the output path.
#[allow(clippy::too_many_arguments)]
fn run_rooted_or_delegated(
    spec: &LangSpec,
    repo: &Path,
    bazel: &Path,
    scip_dir: &Path,
    parts: &mut Vec<Vec<u8>>,
    cmd_env: &'static str,
    root: &str,
    command: impl FnOnce(&Path) -> Command,
) -> Result<LangIndexResult> {
    let out_name = format!("index-{}.scip", spec.language);
    let configured = std::env::var(cmd_env)
        .map(|c| !c.trim().is_empty())
        .unwrap_or(false);
    if configured {
        return run_delegated(spec, repo, bazel, scip_dir, parts, cmd_env, &out_name);
    }

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
    // Before the PATH probe, which starts the indexer: without `root` there is
    // nothing for it to index whether it is installed or not.
    if !repo.join(root).exists() {
        return Ok(skip(LangStatus::SkippedNotConfigured {
            targets,
            env: cmd_env,
        }));
    }
    if !indexer_present(spec.binary) {
        return Ok(skip(LangStatus::SkippedNoIndexer {
            binary: spec.binary,
        }));
    }

    let out_path = scip_dir.join(out_name);
    let cmd = command(&out_path);
    run_indexer(spec, cmd, &out_path, parts, start)
}

/// The heap ceiling, in MB, of a Node indexer (scip-python, scip-typescript)
/// when `ENGRAPH_NODE_HEAP_MB` is not set. Node's own default, about 4 GB,
/// runs out on ray (5,100 Python files). Measured on ray (2026-11-18,
/// scip-python 0.6.8): an 8 GB ceiling completes but is GC-bound — 7m54s with
/// RSS pinned at 8.3 GiB, against 5m21s and a 9.85 GiB peak under 16 GB, with
/// identical SCIP. 12 GB leaves about 20% above what ray uses. It is a
/// ceiling, not a reservation: Node only grows to what a repo needs.
const DEFAULT_NODE_HEAP_MB: u32 = 12288;

/// `NODE_OPTIONS` for a Node indexer: `existing` (the user's `NODE_OPTIONS`)
/// with `--max-old-space-size` added. A heap the user set in `existing` wins
/// over the default; `override_mb` (`ENGRAPH_NODE_HEAP_MB`) wins over both,
/// because Node honors the last of repeated flags. An `override_mb` that is
/// not a positive number of MB gives the default.
fn node_options(existing: Option<&str>, override_mb: Option<&str>) -> String {
    let existing = existing.map(str::trim).filter(|s| !s.is_empty());
    let mb = match override_mb {
        Some(v) => match v.trim().parse::<u32>() {
            Ok(mb) if mb > 0 => Some(mb),
            _ => {
                tracing::warn!(
                    value = v,
                    "ENGRAPH_NODE_HEAP_MB is not a positive number of MB; using {DEFAULT_NODE_HEAP_MB}"
                );
                None
            }
        },
        None => None,
    };
    let mb = match (mb, existing) {
        (Some(mb), _) => mb,
        (None, Some(e)) if e.contains("--max-old-space-size") => return e.to_string(),
        (None, _) => DEFAULT_NODE_HEAP_MB,
    };
    match existing {
        Some(e) => format!("{e} --max-old-space-size={mb}"),
        None => format!("--max-old-space-size={mb}"),
    }
}

/// Give a Node indexer its heap (`node_options`).
fn set_node_heap(cmd: &mut Command) {
    let existing = std::env::var("NODE_OPTIONS").ok();
    let override_mb = std::env::var("ENGRAPH_NODE_HEAP_MB").ok();
    cmd.env(
        "NODE_OPTIONS",
        node_options(existing.as_deref(), override_mb.as_deref()),
    );
}

fn build_indexer_command(spec: &LangSpec, repo: &Path, out_path: &Path) -> Command {
    match spec.language {
        "ts" => {
            // The standalone driver's command: it adds `--pnpm-workspaces` or
            // `--yarn-workspaces` for a workspace whose tool is installed.
            // `--infer-tsconfig` because a Bazel root often has no
            // tsconfig.json: each `ts_project` names its own.
            let mut c = crate::driver::typescript_command(repo, out_path, &crate::which::on_path);
            c.arg("--infer-tsconfig");
            set_node_heap(&mut c);
            c
        }
        "python" => {
            // Mirror the standalone scip-python driver (driver.rs): --output is
            // resolved relative to --cwd, so pin --cwd to the repo and pass an
            // absolute out_path. --project-name comes from pyproject.toml, else
            // the repo dir name (driver::python_project_name), and
            // --project-version is pinned (see driver::SCIP_PYTHON_VERSION)
            // to avoid the undefined-version crash and keep entity IDs stable.
            let project_name = crate::driver::python_project_name(repo);
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
            set_node_heap(&mut c);
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
    go: crate::index::GoOptions,
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

        let mut cmd = crate::driver::scip_go_command(repo, module_root, &out_path);
        if !go.tests {
            // Skip test compilation: a module whose test-only deps don't
            // resolve would otherwise fail wholesale, losing its library
            // symbols too. `--go-tests` takes that risk for the test symbols.
            cmd.arg("--skip-tests");
        }
        cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
        crate::driver::apply_go_tags(&mut cmd, go.tags);
        tracing::info!(driver = spec.binary, module = %module_root.display(), "running scip-go for module");
        let output = cmd
            .output()
            .with_context(|| format!("spawning {} for {}", spec.binary, module_root.display()))?;
        if !output.status.success() {
            failed += 1;
            tracing::warn!(
                module = %module_root.display(),
                "scip-go failed (isolated): {}",
                failure_tail(&output.stderr, &output.stdout, FAILURE_TAIL_LINES)
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

/// The `bazel query` expression for the first-party targets of `kinds`: their
/// union, minus each label with `node_modules` in it. `npm_link_all_packages`
/// makes a `js_library` per third-party npm package
/// (`//:.aspect_rules_js/node_modules/<pkg>`, `//vue:node_modules/<pkg>`), so
/// without the filter a repo that uses rules_js only for a tool (eslint,
/// prettier) counts ts targets and starts scip-typescript with no TS or JS of
/// its own. The filter is the same for every language: no other language's
/// first-party code lives under a `node_modules` label.
fn target_count_query(kinds: &[&str]) -> String {
    let union = kinds
        .iter()
        .map(|k| format!("kind({k}, //...)"))
        .collect::<Vec<_>>()
        .join(" union ");
    format!("let t = {union} in $t except filter(\"node_modules\", $t)")
}

/// Count the workspace's first-party targets of the given rule kinds
/// (`target_count_query`) via one `bazel query --output=label_kind`. A failed query is an error, not zero:
/// zero reports "no targets" and skips a configured command. A rule kind the
/// workspace does not have is not a failure (Bazel 9.3.2 exits 0 with
/// `INFO: Empty results`), and the caller turns the error into a `Failed`
/// status for this language only. Sharing the target-level
/// pass's `output_base` is intentional: the analysis cache is warm and this
/// returns near-instantly. The Go pass uses the count to report the gazelle gap.
fn bazel_count_targets(bazel: &Path, repo: &Path, kinds: &[&str]) -> Result<usize> {
    let expr = target_count_query(kinds);
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
            failure_tail(&out.stderr, &out.stdout, FAILURE_TAIL_LINES)
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
    let hex = engraph_core::utils::sha256_hex(canonical.to_string_lossy().as_bytes());
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
    use crate::index::GoOptions;
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

    #[test]
    fn lang_status_no_targets_for_command_names_the_kinds_and_the_env_var() {
        // The user set the variable, so "no targets" alone would read as a
        // bug: say which rule kinds were counted and that the command did
        // not run.
        let s = LangStatus::SkippedNoTargetsForCommand {
            rule_kinds: &["java_library", "java_binary", "java_test"],
            env: "ENGRAPH_BAZEL_SCIP_JAVA_CMD",
        };
        assert_eq!(
            format!("{s}"),
            "skipped (no java_library, java_binary, java_test targets; \
             ENGRAPH_BAZEL_SCIP_JAVA_CMD not run)"
        );
    }

    /// A stand-in `bazel` that prints `stdout`, then `stderr` (when it is not
    /// empty) on stderr, and exits with `code`. A shell script on unix; on
    /// Windows a batch file, which `Command` runs through `cmd.exe`.
    fn script_bazel(dir: &Path, name: &str, stdout: &[String], stderr: &str, code: i32) -> PathBuf {
        #[cfg(not(windows))]
        let (p, body) = {
            let mut b = String::from("#!/bin/sh\n");
            for l in stdout {
                b.push_str(&format!("echo '{l}'\n"));
            }
            if !stderr.is_empty() {
                b.push_str(&format!("echo '{stderr}' >&2\n"));
            }
            b.push_str(&format!("exit {code}\n"));
            (dir.join(format!("{name}.sh")), b)
        };
        #[cfg(windows)]
        let (p, body) = {
            // `echo` with no text prints "ECHO is off.", so empty output
            // gets no line at all.
            let mut b = String::from("@echo off\r\n");
            for l in stdout {
                b.push_str(&format!("echo {l}\r\n"));
            }
            if !stderr.is_empty() {
                b.push_str(&format!("echo {stderr} 1>&2\r\n"));
            }
            b.push_str(&format!("exit /b {code}\r\n"));
            (dir.join(format!("{name}.cmd")), b)
        };
        write_script(&p, &body);
        p
    }

    /// Write an executable script from a child process. If this process held
    /// the write fd, a fork by another test thread could inherit it, and the
    /// exec of the script would fail with ETXTBSY ("Text file busy").
    #[cfg(not(windows))]
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

    /// Windows runs a batch file by its extension; no exec bit, no ETXTBSY.
    #[cfg(windows)]
    fn write_script(p: &Path, body: &str) {
        std::fs::write(p, body).unwrap();
    }

    /// A stand-in `bazel` that answers any query with `n` `label_kind` lines,
    /// so a test can hand `bazel_count_targets` a nonzero count without a real
    /// workspace.
    fn fake_bazel(dir: &Path, n: usize) -> PathBuf {
        let lines: Vec<String> = (0..n)
            .map(|i| format!("java_library //t{i}:t{i}"))
            .collect();
        script_bazel(dir, "fake-bazel", &lines, "", 0)
    }

    /// A stand-in `bazel` whose query fails with exit status 2.
    fn failing_bazel(dir: &Path, stderr: &str, stdout: &str) -> PathBuf {
        let lines: Vec<String> = [stdout]
            .iter()
            .filter(|s| !s.is_empty())
            .map(|s| s.to_string())
            .collect();
        script_bazel(dir, "failing-bazel", &lines, stderr, 2)
    }

    #[test]
    fn a_failed_target_count_is_an_error_not_zero() {
        // Zero would report `skipped (no targets in this workspace)` and skip
        // a configured command. The first line carries the reason, because
        // `print_symbol_langs` shows only the first line of a status.
        let kinds = &["java_library", "java_binary", "java_test"];
        // The exit status reads `exit status: 2` on unix, `exit code: 2` on Windows.
        let first = "bazel query for java_library, java_binary, java_test exited with exit ";

        let tmp = tempfile::tempdir().unwrap();
        let bazel = failing_bazel(tmp.path(), "ERROR: count failed", "");
        let msg = format!(
            "{:#}",
            bazel_count_targets(&bazel, tmp.path(), kinds).unwrap_err()
        );
        let line = msg.lines().next().unwrap();
        assert!(line.starts_with(first) && line.ends_with(": 2"), "{msg}");
        assert!(msg.contains("stderr (tail):\nERROR: count failed"), "{msg}");

        let tmp = tempfile::tempdir().unwrap();
        let bazel = failing_bazel(tmp.path(), "", "ERROR: on stdout");
        let msg = format!(
            "{:#}",
            bazel_count_targets(&bazel, tmp.path(), kinds).unwrap_err()
        );
        let line = msg.lines().next().unwrap();
        assert!(line.starts_with(first) && line.ends_with(": 2"), "{msg}");
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

    #[test]
    fn a_configured_command_with_no_targets_is_not_run_and_says_so() {
        let tmp = tempfile::tempdir().unwrap();
        let spec = LANGS.iter().find(|s| s.language == "java").unwrap();
        let scip_dir = tmp.path().join("out");
        std::fs::create_dir_all(&scip_dir).unwrap();
        let bazel = fake_bazel(tmp.path(), 0);
        // A command that leaves a marker, to show it did not run. The `#`
        // swallows the `"$1" "$2"` the runner appends.
        let marker = tmp.path().join("ran");

        let _env = cmd_env_lock();
        // SAFETY: `_env` serializes every test that touches this variable.
        unsafe {
            std::env::set_var(
                "ENGRAPH_BAZEL_SCIP_JAVA_CMD",
                format!(": > \"{}\" #", marker.display()),
            )
        };
        let mut parts: Vec<Vec<u8>> = Vec::new();
        let r = run_java(spec, tmp.path(), &bazel, &scip_dir, &mut parts);
        unsafe { std::env::remove_var("ENGRAPH_BAZEL_SCIP_JAVA_CMD") };
        assert_eq!(
            r.unwrap().status,
            LangStatus::SkippedNoTargetsForCommand {
                rule_kinds: spec.rule_kinds,
                env: "ENGRAPH_BAZEL_SCIP_JAVA_CMD",
            }
        );
        assert!(!marker.exists(), "the command must not run without targets");
        assert!(parts.is_empty());
    }

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
        let r = run_go(
            spec,
            tmp.path(),
            &bazel,
            &scip_dir,
            &mut parts,
            GoOptions::default(),
        )
        .unwrap();
        unsafe { std::env::remove_var("ENGRAPH_BAZEL_SCIP_GO_CMD") };
        assert_eq!(r.status, LangStatus::Indexed);
        assert_eq!(parts.len(), 1);
        assert_eq!(parts[0], scip);
    }

    #[test]
    fn node_options_add_the_heap_and_keep_the_rest() {
        let default = format!("--max-old-space-size={DEFAULT_NODE_HEAP_MB}");
        assert_eq!(node_options(None, None), default);
        // The user's other Node flags stay.
        assert_eq!(
            node_options(Some("--require ./x.js"), None),
            format!("--require ./x.js {default}")
        );
        // A heap the user already set in NODE_OPTIONS wins over the default...
        assert_eq!(
            node_options(Some("--max-old-space-size=2048"), None),
            "--max-old-space-size=2048"
        );
        // ...and ENGRAPH_NODE_HEAP_MB over both: Node honors the last flag.
        assert_eq!(
            node_options(Some("--max-old-space-size=2048"), Some("12288")),
            "--max-old-space-size=2048 --max-old-space-size=12288"
        );
        // Not a positive number of MB: the default.
        assert_eq!(node_options(None, Some("lots")), default);
        assert_eq!(node_options(None, Some("0")), default);
    }

    #[test]
    fn node_indexers_get_the_heap_in_node_options() {
        // scip-python and scip-typescript are both Node programs; Node's own
        // default heap (about 4 GB) runs out on ray (5,100 Python files).
        let tmp = tempfile::tempdir().unwrap();
        let out = tmp.path().join("index.scip");
        let _env = cmd_env_lock();
        let saved = (
            std::env::var_os("NODE_OPTIONS"),
            std::env::var_os("ENGRAPH_NODE_HEAP_MB"),
        );
        // SAFETY: `_env` serializes every test that touches these variables.
        unsafe {
            std::env::remove_var("NODE_OPTIONS");
            std::env::set_var("ENGRAPH_NODE_HEAP_MB", "12288");
        }
        for lang in ["python", "ts"] {
            let spec = LANGS.iter().find(|s| s.language == lang).unwrap();
            let cmd = build_indexer_command(spec, tmp.path(), &out);
            let node_options = cmd
                .get_envs()
                .find(|(k, _)| *k == "NODE_OPTIONS")
                .and_then(|(_, v)| v)
                .map(|v| v.to_string_lossy().into_owned());
            assert_eq!(
                node_options.as_deref(),
                Some("--max-old-space-size=12288"),
                "{lang}"
            );
        }
        unsafe {
            match &saved.0 {
                Some(v) => std::env::set_var("NODE_OPTIONS", v),
                None => std::env::remove_var("NODE_OPTIONS"),
            }
            match &saved.1 {
                Some(v) => std::env::set_var("ENGRAPH_NODE_HEAP_MB", v),
                None => std::env::remove_var("ENGRAPH_NODE_HEAP_MB"),
            }
        }
    }

    #[test]
    fn the_target_count_leaves_out_node_modules_links() {
        // `npm_link_all_packages` makes a `js_library` per third-party npm
        // package (`//:.aspect_rules_js/node_modules/<pkg>`,
        // `//vue:node_modules/<pkg>`): 187 of the 248 ts targets of
        // bazelbuild/examples frontend. Counting them starts the ts pass in a
        // repo that uses rules_js only for tooling. Bazel does the filtering,
        // so the expression is the unit; the count (61 there) is checked live.
        assert_eq!(
            target_count_query(&["js_library", "ts_project"]),
            "let t = kind(js_library, //...) union kind(ts_project, //...) \
             in $t except filter(\"node_modules\", $t)"
        );
    }

    #[test]
    fn langs_ts_counts_the_js_rules() {
        // scip-typescript indexes JavaScript too: a repo with only
        // `aspect_rules_js` targets must not report "no targets", and a repo
        // with both must count both.
        let ts = LANGS
            .iter()
            .find(|s| s.language == "ts")
            .expect("ts LangSpec present");
        assert_eq!(ts.binary, "scip-typescript");
        for kind in [
            "ts_project",
            "ts_library",
            "js_library",
            "js_binary",
            "js_test",
        ] {
            assert!(ts.rule_kinds.contains(&kind), "missing {kind}");
        }
    }

    #[test]
    fn run_ts_without_a_root_node_modules_is_not_configured() {
        // `rules_js` makes node_modules under bazel-bin during a build, not at
        // the root, and scip-typescript fails without them. The targets are
        // reported as needing a command, like Go with no go.mod.
        let tmp = tempfile::tempdir().unwrap();
        let spec = LANGS.iter().find(|s| s.language == "ts").unwrap();
        let scip_dir = tmp.path().join("out");
        std::fs::create_dir_all(&scip_dir).unwrap();
        let bazel = fake_bazel(tmp.path(), 5);

        // A scip-typescript ahead of the real one on PATH that leaves a
        // marker, to show that neither the PATH probe nor the index run
        // starts it. On Windows `Command` does not resolve a script without
        // `.exe`, so there only the status is checked.
        let bin = tmp.path().join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        let marker = tmp.path().join("scip-typescript-ran");
        #[cfg(not(windows))]
        write_script(
            &bin.join("scip-typescript"),
            &format!("#!/bin/sh\n: > \"{}\"\n", marker.display()),
        );

        let _env = cmd_env_lock();
        let old_path = std::env::var_os("PATH").unwrap_or_default();
        let path =
            std::env::join_paths(std::iter::once(bin).chain(std::env::split_paths(&old_path)))
                .unwrap();
        // SAFETY: `_env` serializes every test that touches these variables.
        unsafe {
            std::env::remove_var("ENGRAPH_BAZEL_SCIP_TS_CMD");
            std::env::set_var("PATH", &path);
        }
        let mut parts: Vec<Vec<u8>> = Vec::new();
        let r = run_ts(spec, tmp.path(), &bazel, &scip_dir, &mut parts);
        unsafe { std::env::set_var("PATH", &old_path) };
        assert_eq!(
            r.unwrap().status,
            LangStatus::SkippedNotConfigured {
                targets: 5,
                env: "ENGRAPH_BAZEL_SCIP_TS_CMD",
            }
        );
        assert!(!marker.exists(), "scip-typescript must not start");
        assert!(parts.is_empty());
    }

    #[test]
    fn remove_inferred_tsconfigs_removes_only_what_the_run_wrote() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path();
        // A tsconfig from before the run is kept even with a matching content.
        std::fs::create_dir_all(repo.join("existing")).unwrap();
        std::fs::write(repo.join("existing/tsconfig.json"), "{}").unwrap();
        // One under node_modules is not seen at all.
        std::fs::create_dir_all(repo.join("node_modules/dep")).unwrap();
        std::fs::write(repo.join("node_modules/dep/tsconfig.json"), "{}").unwrap();
        let before = tsconfig_paths(repo);

        // The run writes three: the two exact inferred contents, and one that
        // only a user writes, which is kept.
        std::fs::write(repo.join("tsconfig.json"), "{}").unwrap();
        std::fs::create_dir_all(repo.join("pure-js")).unwrap();
        std::fs::write(
            repo.join("pure-js/tsconfig.json"),
            r#"{"compilerOptions":{"allowJs":true}}"#,
        )
        .unwrap();
        std::fs::create_dir_all(repo.join("app")).unwrap();
        std::fs::write(
            repo.join("app/tsconfig.json"),
            r#"{"compilerOptions":{"strict":true}}"#,
        )
        .unwrap();

        remove_inferred_tsconfigs(repo, &before);
        assert!(repo.join("existing/tsconfig.json").is_file());
        assert!(repo.join("node_modules/dep/tsconfig.json").is_file());
        assert!(repo.join("app/tsconfig.json").is_file());
        assert!(!repo.join("tsconfig.json").exists());
        assert!(!repo.join("pure-js/tsconfig.json").exists());
    }

    /// Unix only: on Windows `Command` does not resolve a script without
    /// `.exe`, so the fake scip-typescript on `PATH` is never found.
    #[cfg(not(windows))]
    #[test]
    fn run_ts_removes_the_tsconfigs_the_indexer_writes() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(repo.join("node_modules")).unwrap();
        std::fs::write(repo.join("package.json"), "{}").unwrap();
        let scip_dir = tmp.path().join("out");
        std::fs::create_dir_all(&scip_dir).unwrap();
        let bazel = fake_bazel(tmp.path(), 1);

        // A fake scip-typescript that writes an inferred tsconfig.json into
        // the repo, as --infer-tsconfig does, and then its SCIP output.
        let bin = tmp.path().join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        let scip = make_index(&["a.ts"], &[]);
        let scip_path = tmp.path().join("fake.scip");
        std::fs::write(&scip_path, &scip).unwrap();
        write_script(
            &bin.join("scip-typescript"),
            &format!(
                "#!/bin/sh\n\
                 [ \"$1\" = --version ] && {{ echo 0.0.0; exit 0; }}\n\
                 while [ $# -gt 1 ]; do [ \"$1\" = --output ] && out=\"$2\"; shift; done\n\
                 printf '{{}}' > \"{repo}/tsconfig.json\"\n\
                 cp \"{scip}\" \"$out\"\n",
                repo = repo.display(),
                scip = scip_path.display()
            ),
        );

        let _env = cmd_env_lock();
        let old_path = std::env::var_os("PATH").unwrap_or_default();
        let path =
            std::env::join_paths(std::iter::once(bin).chain(std::env::split_paths(&old_path)))
                .unwrap();
        // SAFETY: `_env` serializes every test that touches these variables.
        unsafe {
            std::env::remove_var("ENGRAPH_BAZEL_SCIP_TS_CMD");
            std::env::set_var("PATH", &path);
        }
        let mut parts: Vec<Vec<u8>> = Vec::new();
        let r = run_ts(ts_spec(), &repo, &bazel, &scip_dir, &mut parts);
        unsafe { std::env::set_var("PATH", &old_path) };
        assert_eq!(r.unwrap().status, LangStatus::Indexed);
        assert_eq!(parts.len(), 1);
        assert!(
            !repo.join("tsconfig.json").exists(),
            "the tsconfig.json the indexer wrote must be removed"
        );
    }

    fn ts_spec() -> &'static LangSpec {
        LANGS.iter().find(|s| s.language == "ts").unwrap()
    }

    #[test]
    fn pnpm_broken_lockfile_help_names_the_two_fixes() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(
            tmp.path().join("pnpm-lock.yaml"),
            "lockfileVersion: '6.0'\n",
        )
        .unwrap();
        let failure = "scip-typescript exited with exit status: 1\n\
                       Error: ERR_PNPM_BROKEN_LOCKFILE  broken lockfile";
        let help = pnpm_broken_lockfile_help(tmp.path(), failure).expect("a clear message");
        assert!(
            help.contains("cannot read pnpm-lock.yaml (lockfileVersion 6.0)"),
            "{help}"
        );
        assert!(help.contains("pnpm install --lockfile-only"), "{help}");
        assert!(help.contains("pnpm 8 on PATH"), "{help}");

        // The real native-path failure is scip-typescript's throw dump: pnpm's
        // boxed error as a JS string, without the ERR_PNPM_BROKEN_LOCKFILE
        // token, wrapped at word boundaries. The trigger words survive the
        // wrapping because the box never splits a word.
        let dump = "scip-typescript exited with exit status: 1\n\
            'Error:   × load the current lockfile\\n' +\n\
            '  ╰─▶ The lockfile at \"/x/node_modules/.pnpm/lock.yaml\" is broken:\\n' +\n\
            '      The lockfileVersion of 6.0 is incompatible with this version of pnpm,\\n' +\n\
            '      which supports lockfileVersion 9.x (1:18)\\n'";
        let help = pnpm_broken_lockfile_help(tmp.path(), dump).expect("a clear message");
        assert!(
            help.contains("cannot read pnpm-lock.yaml (lockfileVersion 6.0)"),
            "{help}"
        );

        // Another failure is not rewritten.
        assert_eq!(pnpm_broken_lockfile_help(tmp.path(), "tsc crashed"), None);
        // A current lockfile version that pnpm still calls broken is genuine
        // corruption: pnpm's own error says why.
        std::fs::write(
            tmp.path().join("pnpm-lock.yaml"),
            "lockfileVersion: '9.0'\n",
        )
        .unwrap();
        assert_eq!(pnpm_broken_lockfile_help(tmp.path(), failure), None);
    }

    /// Unix only: on Windows `Command` does not resolve a script without
    /// `.exe`, so the fake scip-typescript on `PATH` is never found.
    #[cfg(not(windows))]
    #[test]
    fn run_ts_reports_a_pnpm_lockfile_mismatch_clearly() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(repo.join("node_modules")).unwrap();
        std::fs::write(repo.join("package.json"), "{}").unwrap();
        std::fs::write(repo.join("pnpm-lock.yaml"), "lockfileVersion: '6.0'\n").unwrap();
        let scip_dir = tmp.path().join("out");
        std::fs::create_dir_all(&scip_dir).unwrap();
        let bazel = fake_bazel(tmp.path(), 1);

        // A fake scip-typescript that fails the way the real one does when the
        // pnpm on PATH cannot read the lockfile.
        let bin = tmp.path().join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        write_script(
            &bin.join("scip-typescript"),
            "#!/bin/sh\n\
             [ \"$1\" = --version ] && { echo 0.0.0; exit 0; }\n\
             echo 'Error: ERR_PNPM_BROKEN_LOCKFILE  load the current lockfile' >&2\n\
             exit 1\n",
        );

        let _env = cmd_env_lock();
        let old_path = std::env::var_os("PATH").unwrap_or_default();
        let path =
            std::env::join_paths(std::iter::once(bin).chain(std::env::split_paths(&old_path)))
                .unwrap();
        // SAFETY: `_env` serializes every test that touches these variables.
        unsafe {
            std::env::remove_var("ENGRAPH_BAZEL_SCIP_TS_CMD");
            std::env::set_var("PATH", &path);
        }
        let mut parts: Vec<Vec<u8>> = Vec::new();
        let r = run_ts(ts_spec(), &repo, &bazel, &scip_dir, &mut parts);
        unsafe { std::env::set_var("PATH", &old_path) };
        let status = r.unwrap().status;
        let LangStatus::Failed(msg) = status else {
            panic!("expected Failed, got {status:?}");
        };
        assert!(
            msg.starts_with("pnpm") || msg.starts_with("the pnpm on PATH"),
            "the first line must name the cause, got: {msg}"
        );
        assert!(msg.contains("lockfileVersion 6.0"), "{msg}");
        assert!(msg.contains("pnpm 8 on PATH"), "{msg}");
        assert!(parts.is_empty());
    }

    #[test]
    fn run_ts_delegates_to_configured_command() {
        let tmp = tempfile::tempdir().unwrap();
        let spec = LANGS.iter().find(|s| s.language == "ts").unwrap();
        let scip_dir = tmp.path().join("out");
        std::fs::create_dir_all(&scip_dir).unwrap();
        let bazel = fake_bazel(tmp.path(), 2);

        // A fake command that writes a known SCIP to its 2nd arg → delegated
        // and merged, even with no node_modules at the root. SAFETY: `_env`
        // serializes every test that touches this variable.
        let _env = cmd_env_lock();
        let scip = make_index(&["src/app.ts", "src/util.js"], &[]);
        let scip_path = tmp.path().join("fake.scip");
        std::fs::write(&scip_path, &scip).unwrap();
        let cmd_path = tmp.path().join("fake-ts-cmd.sh");
        std::fs::write(
            &cmd_path,
            format!("cp \"{}\" \"$2\"\n", scip_path.display()),
        )
        .unwrap();
        unsafe {
            std::env::set_var(
                "ENGRAPH_BAZEL_SCIP_TS_CMD",
                format!("sh \"{}\"", cmd_path.display()),
            )
        };
        let mut parts: Vec<Vec<u8>> = Vec::new();
        let r = run_ts(spec, tmp.path(), &bazel, &scip_dir, &mut parts).unwrap();
        unsafe { std::env::remove_var("ENGRAPH_BAZEL_SCIP_TS_CMD") };
        assert_eq!(r.status, LangStatus::Indexed);
        assert_eq!(parts.len(), 1);
        assert_eq!(parts[0], scip);
    }

    #[test]
    fn build_indexer_command_ts_infers_a_tsconfig() {
        // A Bazel root often has no tsconfig.json (each ts_project names its
        // own), and scip-typescript fails without one unless it infers it.
        // No workspace file here, so no workspace flag and no PATH lookup.
        let ts = LANGS.iter().find(|s| s.language == "ts").unwrap();
        let tmp = tempfile::tempdir().unwrap();
        let out = tmp.path().join("index-ts.scip");
        let cmd = build_indexer_command(ts, tmp.path(), &out);
        assert_eq!(cmd.get_program(), std::ffi::OsStr::new("scip-typescript"));
        let args: Vec<String> = cmd
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            args,
            [
                "index".to_string(),
                "--output".to_string(),
                out.to_string_lossy().into_owned(),
                "--infer-tsconfig".to_string(),
            ]
        );
    }

    #[test]
    fn run_rust_without_a_root_cargo_toml_is_not_configured() {
        // rust-analyzer has no project to load without a Cargo.toml, so the
        // targets are reported as needing a command, like Go with no go.mod.
        // The result must not depend on rust-analyzer being on PATH.
        let tmp = tempfile::tempdir().unwrap();
        let spec = LANGS.iter().find(|s| s.language == "rust").unwrap();
        let scip_dir = tmp.path().join("out");
        std::fs::create_dir_all(&scip_dir).unwrap();
        let bazel = fake_bazel(tmp.path(), 4);

        let _env = cmd_env_lock();
        // SAFETY: `_env` serializes every test that touches this variable.
        unsafe { std::env::remove_var("ENGRAPH_BAZEL_SCIP_RUST_CMD") };
        let mut parts: Vec<Vec<u8>> = Vec::new();
        let r = run_rust(spec, tmp.path(), &bazel, &scip_dir, &mut parts).unwrap();
        assert_eq!(
            r.status,
            LangStatus::SkippedNotConfigured {
                targets: 4,
                env: "ENGRAPH_BAZEL_SCIP_RUST_CMD",
            }
        );
        assert!(parts.is_empty());
    }

    #[test]
    fn run_rust_delegates_to_configured_command() {
        let tmp = tempfile::tempdir().unwrap();
        let spec = LANGS.iter().find(|s| s.language == "rust").unwrap();
        let scip_dir = tmp.path().join("out");
        std::fs::create_dir_all(&scip_dir).unwrap();
        // Targets are counted before the delegated branch skips, so it needs a
        // bazel that reports some.
        let bazel = fake_bazel(tmp.path(), 2);

        // A fake command that writes a known SCIP to its 2nd arg → delegated
        // and merged, even with no Cargo.toml at the root (the configured
        // command wins before the native path looks for one). SAFETY: `_env`
        // serializes every test that touches this variable.
        let _env = cmd_env_lock();
        let scip = make_index(&["src/lib.rs"], &[]);
        let scip_path = tmp.path().join("fake.scip");
        std::fs::write(&scip_path, &scip).unwrap();
        let cmd_path = tmp.path().join("fake-rust-cmd.sh");
        std::fs::write(
            &cmd_path,
            format!("cp \"{}\" \"$2\"\n", scip_path.display()),
        )
        .unwrap();
        unsafe {
            std::env::set_var(
                "ENGRAPH_BAZEL_SCIP_RUST_CMD",
                format!("sh \"{}\"", cmd_path.display()),
            )
        };
        let mut parts: Vec<Vec<u8>> = Vec::new();
        let r = run_rust(spec, tmp.path(), &bazel, &scip_dir, &mut parts).unwrap();
        unsafe { std::env::remove_var("ENGRAPH_BAZEL_SCIP_RUST_CMD") };
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
    fn langs_includes_rust() {
        let rust = LANGS
            .iter()
            .find(|s| s.language == "rust")
            .expect("rust LangSpec present");
        assert_eq!(rust.binary, "rust-analyzer");
        for kind in [
            "rust_library",
            "rust_binary",
            "rust_test",
            "rust_proc_macro",
            "rust_library_group",
            "rust_prost_library",
        ] {
            assert!(rust.rule_kinds.contains(&kind), "missing {kind}");
        }
        // `rust_.*` would also count toolchain rules (`rust_prost_toolchain`).
        assert!(
            !rust
                .rule_kinds
                .iter()
                .any(|k| k.contains('*') || k.ends_with("_toolchain")),
            "{:?}",
            rust.rule_kinds
        );
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
        // --project-name derives from the repo dir name when there is no
        // pyproject.toml (driver::python_project_name).
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
        assert!(langs.contains(&"rust"));
        let rust = plan.iter().find(|p| p.language == "rust").unwrap();
        assert_eq!(rust.binary, "rust-analyzer");
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

    /// Module `a` (`Caller`) calls `Helper` of module `b` at `root`; a
    /// `replace` resolves `b` without a go.work. `go_work` adds a root
    /// `go.work` that lists both.
    fn go_caller_and_helper(root: &Path, go_work: bool) {
        let write = |rel: &str, body: &str| {
            let p = root.join(rel);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(p, body).unwrap();
        };
        write("b/go.mod", "module example.com/b\n\ngo 1.21\n");
        write("b/b.go", "package b\n\nfunc Helper() int { return 2 }\n");
        write(
            "a/go.mod",
            "module example.com/a\n\ngo 1.21\n\nrequire example.com/b v0.0.0\n\nreplace example.com/b => ../b\n",
        );
        write(
            "a/a.go",
            "package a\n\nimport \"example.com/b\"\n\nfunc Caller() int { return b.Helper() }\n",
        );
        if go_work {
            write("go.work", "go 1.21\n\nuse (\n\t./a\n\t./b\n)\n");
        }
    }

    /// The definition and the reference symbols in `parts` that end with
    /// `suffix`.
    fn go_symbols(parts: &[Vec<u8>], suffix: &str) -> (Vec<String>, Vec<String>) {
        use protobuf::Message;
        let (mut defs, mut refs) = (Vec::new(), Vec::new());
        for part in parts {
            // As the loader reads it.
            let mut idx = scip::types::Index::parse_from_bytes(part).unwrap();
            crate::scip_loader::normalize_index(&mut idx, "/proj");
            for occ in idx.documents.iter().flat_map(|d| &d.occurrences) {
                if !occ.symbol.ends_with(suffix) {
                    continue;
                }
                if occ.symbol_roles & scip::types::SymbolRole::Definition as i32 != 0 {
                    defs.push(occ.symbol.clone());
                } else {
                    refs.push(occ.symbol.clone());
                }
            }
        }
        (defs, refs)
    }

    #[test]
    fn go_modules_pass_links_a_call_into_another_module() {
        if !indexer_present("scip-go") {
            eprintln!("skip: scip-go not installed");
            return;
        }
        let spec = LANGS.iter().find(|s| s.language == "go").unwrap();
        for go_work in [false, true] {
            let tmp = tempfile::tempdir().unwrap();
            let repo = tmp.path().join("repo");
            go_caller_and_helper(&repo, go_work);
            let scip_dir = tmp.path().join("out");
            std::fs::create_dir_all(&scip_dir).unwrap();
            let bazel = fake_bazel(tmp.path(), 2);
            let mut parts = Vec::new();
            let r = run_go_modules(
                spec,
                &repo,
                &bazel,
                &scip_dir,
                &mut parts,
                GoOptions::default(),
            )
            .unwrap();
            assert_eq!(
                r.status,
                LangStatus::IndexedModules {
                    indexed: 2,
                    failed: 0,
                    targets: 2
                },
                "go_work={go_work}"
            );
            let (defs, refs) = go_symbols(&parts, "`example.com/b`/Helper().");
            assert_eq!(defs.len(), 1, "go_work={go_work}: {defs:?}");
            assert!(
                !refs.is_empty(),
                "go_work={go_work}: no reference to Helper"
            );
            assert!(
                refs.iter().all(|r| *r == defs[0]),
                "go_work={go_work}: a reference misses the definition {}: {refs:?}",
                defs[0]
            );
        }
    }

    #[test]
    fn go_modules_pass_indexes_a_module_its_go_work_does_not_list() {
        if !indexer_present("scip-go") {
            eprintln!("skip: scip-go not installed");
            return;
        }
        let spec = LANGS.iter().find(|s| s.language == "go").unwrap();
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("repo");
        go_caller_and_helper(&repo, true);
        std::fs::create_dir_all(repo.join("c")).unwrap();
        std::fs::write(repo.join("c/go.mod"), "module example.com/c\n\ngo 1.21\n").unwrap();
        std::fs::write(
            repo.join("c/c.go"),
            "package c\n\nfunc Lone() int { return 3 }\n",
        )
        .unwrap();
        let scip_dir = tmp.path().join("out");
        std::fs::create_dir_all(&scip_dir).unwrap();
        let bazel = fake_bazel(tmp.path(), 3);
        let mut parts = Vec::new();
        run_go_modules(
            spec,
            &repo,
            &bazel,
            &scip_dir,
            &mut parts,
            GoOptions::default(),
        )
        .unwrap();
        let (defs, _) = go_symbols(&parts, "`example.com/c`/Lone().");
        assert_eq!(defs.len(), 1, "{defs:?}");
    }

    #[test]
    fn go_modules_pass_gives_the_go_tags_to_scip_go() {
        if !indexer_present("scip-go") {
            eprintln!("skip: scip-go not installed");
            return;
        }
        let spec = LANGS.iter().find(|s| s.language == "go").unwrap();
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        std::fs::write(repo.join("go.mod"), "module example.com/t\n\ngo 1.21\n").unwrap();
        std::fs::write(
            repo.join("tag.go"),
            "//go:build integration\n\npackage t\n\nfunc Tagged() int { return 1 }\n",
        )
        .unwrap();
        let scip_dir = tmp.path().join("out");
        std::fs::create_dir_all(&scip_dir).unwrap();
        let bazel = fake_bazel(tmp.path(), 1);
        for (tags, want) in [(None, 0), (Some("integration"), 1)] {
            let mut parts = Vec::new();
            run_go_modules(
                spec,
                &repo,
                &bazel,
                &scip_dir,
                &mut parts,
                GoOptions { tags, tests: false },
            )
            .unwrap();
            let (defs, _) = go_symbols(&parts, "`example.com/t`/Tagged().");
            assert_eq!(defs.len(), want, "tags={tags:?}");
        }
    }

    #[test]
    fn go_modules_pass_indexes_test_files_only_with_go_tests() {
        if !indexer_present("scip-go") {
            eprintln!("skip: scip-go not installed");
            return;
        }
        let spec = LANGS.iter().find(|s| s.language == "go").unwrap();
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        std::fs::write(repo.join("go.mod"), "module example.com/t\n\ngo 1.21\n").unwrap();
        std::fs::write(
            repo.join("t.go"),
            "package t\n\nfunc Plain() int { return 0 }\n",
        )
        .unwrap();
        std::fs::write(
            repo.join("t_test.go"),
            "package t\n\nimport \"testing\"\n\nfunc TestPlain(t *testing.T) { Plain() }\n",
        )
        .unwrap();
        let scip_dir = tmp.path().join("out");
        std::fs::create_dir_all(&scip_dir).unwrap();
        let bazel = fake_bazel(tmp.path(), 1);
        for (tests, want) in [(false, 0), (true, 1)] {
            let mut parts = Vec::new();
            run_go_modules(
                spec,
                &repo,
                &bazel,
                &scip_dir,
                &mut parts,
                GoOptions { tags: None, tests },
            )
            .unwrap();
            let (defs, _) = go_symbols(&parts, "`example.com/t`/TestPlain().");
            assert_eq!(defs.len(), want, "tests={tests}");
            let (plain, _) = go_symbols(&parts, "`example.com/t`/Plain().");
            assert_eq!(plain.len(), 1, "tests={tests}");
        }
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
