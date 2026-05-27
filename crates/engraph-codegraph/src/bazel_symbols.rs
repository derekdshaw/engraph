//! F2 Phase 2.3 #2 — symbol-level Bazel indexing.
//!
//! Sits on top of the target-level pass in `bazel.rs`. For each of
//! {Java, Go, TypeScript} we (a) probe `bazel query` for any target of that
//! language in the workspace; (b) probe PATH for the per-language SCIP
//! indexer binary; (c) on both-present, run the indexer, capture its SCIP
//! bytes. We MERGE the per-language SCIP byte streams in memory and call
//! `scip_loader::load` exactly once — calling it per language would have
//! each language wipe the previous one's edges (the loader's DELETE is
//! per-project, kind-blind except for BAZEL_DEPENDS_ON).
//!
//! Per-language strategy:
//! - **Java**: `scip-java index` from the workspace root. scip-java
//!   auto-detects Bazel, materializes its own aspect, runs
//!   `bazel build --aspects=...%scip_java_aspect --output_groups=scip //...`,
//!   merges the per-target SCIP outputs into one file. We do NOT
//!   reimplement aspect dispatch.
//! - **Go**: `scip-go --module-root .` at the workspace root, after
//!   verifying `go.mod` is present. Multi-`go.mod` Bazel-go monorepos are
//!   out of scope for the MVP; they surface as `SkippedNoTargets` today.
//! - **TypeScript**: `scip-typescript index` at the workspace root.
//!   `rules_ts`-based repos may need a prior `bazel build //...` to
//!   populate `bazel-bin/<pkg>/node_modules` symlinks; documented
//!   limitation, not addressed here.
//!
//! Off by default (`engraph index --bazel-symbols`); toolchain downloads
//! and full builds make it heavy. The target-level pass remains the fast
//! deterministic default.
//!
//! **Bazel server isolation caveat (Java).** The target-level pass pins
//! Bazel's `--output_base` into `~/.cache/engraph/bazel-out/<hash>` so
//! engraph's Bazel state stays out of the user's `~/.cache/bazel`. The
//! Java symbol-level pass does NOT carry that isolation: `scip-java`
//! invokes Bazel internally (its bundled aspect) without exposing a
//! startup-options pass-through we can plumb the output_base through.
//! Net effect: `--bazel-symbols` may touch the user's default Bazel
//! cache via scip-java. Documented in ROADMAP.md as a known followup.
//! Go and TS don't have this issue (no Bazel subprocess — they read
//! sources directly).

use crate::bazel::{bazel_binary, output_base_for, tail_lines};
use crate::scip_loader;
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
    pub entities_inserted: usize,
    pub relations_inserted: usize,
    pub scip_bytes_total: usize,
    pub elapsed_ms: i64,
}

#[derive(Debug)]
pub struct LangIndexResult {
    pub language: &'static str,
    pub scip_bytes: usize,
    pub elapsed_ms: i64,
    pub status: LangStatus,
}

#[derive(Debug, PartialEq, Eq)]
pub enum LangStatus {
    Indexed,
    SkippedNoTargets,
    SkippedNoIndexer { binary: &'static str },
    Failed(String),
}

impl fmt::Display for LangStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            LangStatus::Indexed => write!(f, "indexed"),
            LangStatus::SkippedNoTargets => write!(f, "skipped (no targets in this workspace)"),
            LangStatus::SkippedNoIndexer { binary } => {
                write!(f, "skipped ({} not on PATH)", binary)
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
];

pub fn index_bazel_symbols(
    conn: &PooledConn,
    repo: &Path,
    project: &str,
) -> Result<BazelSymbolStats> {
    let start = Instant::now();
    let mut stats = BazelSymbolStats::default();
    let scip_dir = scip_output_dir(repo)?;
    let bazel = bazel_binary()?;

    let mut parts: Vec<Vec<u8>> = Vec::new();
    for spec in LANGS {
        let outcome = run_language(spec, repo, &bazel, &scip_dir, &mut parts);
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
            _ => unreachable!(),
        }
    }

    if !parts.is_empty() {
        let merged = merge_scip_bytes(&parts)?;
        let load_stats = scip_loader::load(conn, project, &merged)?;
        stats.entities_inserted = load_stats.entities_inserted;
        stats.relations_inserted = load_stats.relations_inserted;
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

    // Language-specific precondition: scip-go won't bootstrap without a
    // module root. We don't synthesize one; surface it as SkippedNoTargets
    // (multi-go.mod Bazel-go monorepos are an MVP limitation).
    if spec.language == "go" && !repo.join("go.mod").is_file() {
        return Ok(LangIndexResult {
            language: spec.language,
            scip_bytes: 0,
            elapsed_ms: start.elapsed().as_millis() as i64,
            status: LangStatus::SkippedNoTargets,
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
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Ok(LangIndexResult {
            language: spec.language,
            scip_bytes: 0,
            elapsed_ms: start.elapsed().as_millis() as i64,
            status: LangStatus::Failed(format!(
                "{} exited with {}\nstderr (tail):\n{}",
                spec.binary,
                output.status,
                tail_lines(&stderr, 20)
            )),
        });
    }

    // scip-java writes its merged output to <workspace>/index.scip regardless
    // of --output on some versions; check both locations.
    let bytes = read_scip_output(spec, repo, &out_path)?;
    let n = bytes.len();
    parts.push(bytes);
    Ok(LangIndexResult {
        language: spec.language,
        scip_bytes: n,
        elapsed_ms: start.elapsed().as_millis() as i64,
        status: LangStatus::Indexed,
    })
}

fn build_indexer_command(spec: &LangSpec, repo: &Path, out_path: &Path) -> Command {
    match spec.language {
        "java" => {
            let mut c = Command::new(spec.binary);
            c.arg("index").arg("--output").arg(out_path).current_dir(repo);
            c
        }
        "go" => {
            let mut c = Command::new(spec.binary);
            c.arg("--module-root")
                .arg(".")
                .arg("--output")
                .arg(out_path)
                .current_dir(repo);
            c
        }
        "ts" => {
            let mut c = Command::new(spec.binary);
            c.arg("index").arg("--output").arg(out_path).current_dir(repo);
            c
        }
        _ => unreachable!(),
    }
}

fn read_scip_output(spec: &LangSpec, repo: &Path, out_path: &Path) -> Result<Vec<u8>> {
    if out_path.is_file() {
        return std::fs::read(out_path)
            .with_context(|| format!("reading SCIP at {}", out_path.display()));
    }
    // scip-java's older builds ignore --output and write to the workspace's
    // index.scip; tolerate it.
    if spec.language == "java" {
        let fallback = repo.join("index.scip");
        if fallback.is_file() {
            return std::fs::read(&fallback)
                .with_context(|| format!("reading SCIP at {}", fallback.display()));
        }
    }
    anyhow::bail!(
        "{} reported success but produced no SCIP file at {}",
        spec.binary,
        out_path.display()
    );
}

fn bazel_has_targets(bazel: &Path, repo: &Path, kinds: &[&str]) -> Result<bool> {
    let expr = kinds
        .iter()
        .map(|k| format!("kind({}, //...)", k))
        .collect::<Vec<_>>()
        .join(" union ");
    let output_base = output_base_for(repo);
    // Sharing the output_base with the target-level pass is intentional:
    // analysis cache is warm and the second query returns near-instantly.
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
        // A failed probe shouldn't take the whole symbol-level pass down —
        // treat as "no targets" rather than fatal. The most common cause is
        // a rule kind that doesn't exist in this workspace's ruleset.
        tracing::warn!(
            "bazel query for {:?} exited non-zero; treating as no targets. stderr: {}",
            kinds,
            tail_lines(&String::from_utf8_lossy(&out.stderr), 5)
        );
        return Ok(false);
    }
    Ok(label_kind_nonempty(&String::from_utf8_lossy(&out.stdout)))
}

pub(crate) fn label_kind_nonempty(stdout: &str) -> bool {
    stdout.lines().any(|l| !l.trim().is_empty())
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
    let hex = format!("{:x}", hasher.finalize());
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
    fn label_kind_nonempty_detects_lines() {
        assert!(!label_kind_nonempty(""));
        assert!(!label_kind_nonempty("\n  \n"));
        assert!(label_kind_nonempty("java_library //foo:foo\n"));
        assert!(label_kind_nonempty("\n  \njava_library //foo:foo\n"));
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
}
