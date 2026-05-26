//! F2 Phase 2.3 — target-level Bazel indexing via `bazel query`.
//!
//! This module is the language-agnostic, deterministic half of Phase 2.3.
//! It runs one `bazel query --output=streamed_jsonproto` invocation, parses
//! the resulting JSON-lines stream, and writes one entity per Bazel target
//! plus `BAZEL_DEPENDS_ON` relations between them. It does NOT drive
//! `scip-java` / `scip-go` / `scip-typescript` via Bazel-resolved classpaths
//! — that's the symbol-level half of 2.3 and is deferred (see ROADMAP.md).
//!
//! Output base: `~/.cache/engraph/bazel-out/<sha-of-workspace-path>` by
//! default. Keeps Bazel state out of the workspace (which causes a
//! self-referencing symlink loop on `query`) and out of the user's main
//! `~/.cache/bazel` (so an engraph-driven build doesn't churn the user's
//! own Bazel cache). Override with `ENGRAPH_BAZEL_OUTPUT_BASE`.
//!
//! Bazel itself is invoked through whatever `bazel` is on `PATH`; install
//! bazelisk and symlink it as `bazel` to get per-workspace version pinning
//! via `.bazelversion`.

use anyhow::{Context, Result};
use engraph_core::db::PooledConn;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use uuid::Uuid;

/// Probe: is this directory the root of a Bazel workspace? Checks the same
/// markers `bazel query` itself uses to find a workspace.
pub fn detect_bazel(repo: &Path) -> bool {
    repo.join("WORKSPACE").is_file()
        || repo.join("WORKSPACE.bazel").is_file()
        || repo.join("MODULE.bazel").is_file()
}

#[derive(Debug, Default, Clone, Copy)]
pub struct BazelStats {
    pub targets_inserted: usize,
    pub deps_inserted: usize,
}

/// Run `bazel query` against `repo` and load its target + dependency graph
/// into the codegraph. Idempotent: re-running drops only the
/// `BAZEL_DEPENDS_ON` edges originating from this project, then re-inserts.
/// Targets are upserted (their `bazel_target` entities accumulate across runs).
pub fn index_bazel_workspace(conn: &PooledConn, repo: &Path, project: &str) -> Result<BazelStats> {
    let bazel = bazel_binary()?;
    let output_base = output_base_for(repo);
    std::fs::create_dir_all(&output_base)
        .with_context(|| format!("creating bazel output_base {}", output_base.display()))?;

    let mut cmd = Command::new(&bazel);
    cmd.arg(format!("--output_base={}", output_base.display()))
        .arg("query")
        .arg("--output=streamed_jsonproto")
        .arg("kind(rule, //...)")
        .current_dir(repo);
    tracing::info!(?cmd, "running bazel query");
    let out = cmd
        .output()
        .with_context(|| format!("spawning {}", bazel.display()))?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        anyhow::bail!(
            "bazel query exited with {}\nstderr (tail):\n{}",
            out.status,
            tail_lines(&stderr, 20)
        );
    }
    let stdout = String::from_utf8_lossy(&out.stdout);
    let targets = parse_jsonproto_lines(&stdout)?;
    load_targets(conn, project, repo, &targets)
}

/// Parsed subset of one `bazel query --output=streamed_jsonproto` rule entry.
/// We intentionally model only the fields we consume; the rest of the
/// attribute list is irrelevant noise that bloats memory and parse time.
#[derive(Debug, Clone)]
pub struct BazelTarget {
    pub label: String,
    pub rule_class: String,
    pub location: Option<BazelLocation>,
    pub deps: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct BazelLocation {
    pub file: String,
    pub line: u32,
}

#[derive(Deserialize)]
struct RawEntry<'a> {
    #[serde(rename = "type")]
    ty: Option<&'a str>,
    rule: Option<RawRule>,
}

#[derive(Deserialize)]
struct RawRule {
    name: String,
    #[serde(rename = "ruleClass", default)]
    rule_class: String,
    #[serde(default)]
    location: String,
    #[serde(rename = "ruleInput", default)]
    rule_input: Vec<String>,
}

fn parse_jsonproto_lines(stdout: &str) -> Result<Vec<BazelTarget>> {
    let mut out = Vec::new();
    for (i, line) in stdout.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        // streamed_jsonproto can intersperse non-rule entries (`SOURCE_FILE`,
        // `PACKAGE_GROUP`, …). Decode loosely and skip anything that isn't
        // a RULE.
        let raw: RawEntry = serde_json::from_str(line)
            .with_context(|| format!("parsing bazel query JSON at line {}", i + 1))?;
        if raw.ty.unwrap_or("") != "RULE" {
            continue;
        }
        let Some(rule) = raw.rule else { continue };
        out.push(BazelTarget {
            label: rule.name,
            rule_class: rule.rule_class,
            location: parse_location(&rule.location),
            deps: rule.rule_input,
        });
    }
    Ok(out)
}

/// `location` strings look like `/abs/path/to/foo/BUILD.bazel:1:8`. We keep
/// the file + line and drop the column.
fn parse_location(s: &str) -> Option<BazelLocation> {
    if s.is_empty() {
        return None;
    }
    // The path itself can contain colons on Windows. Find the LAST two
    // ':' separators rather than splitting from the left.
    let last = s.rfind(':')?;
    let before_last = &s[..last];
    let second_last = before_last.rfind(':')?;
    let file = s[..second_last].to_string();
    let line: u32 = s[second_last + 1..last].parse().ok()?;
    Some(BazelLocation { file, line })
}

fn load_targets(
    conn: &PooledConn,
    project: &str,
    repo: &Path,
    targets: &[BazelTarget],
) -> Result<BazelStats> {
    // Build a label set so we only emit BAZEL_DEPENDS_ON edges between
    // targets that are themselves in this workspace. External `@repo//...`
    // labels in `ruleInput` (typically `@bazel_tools//tools/genrule:genrule-setup.sh`)
    // refer to repos outside the workspace and get filtered out.
    let label_set: std::collections::HashSet<&str> =
        targets.iter().map(|t| t.label.as_str()).collect();

    conn.execute_batch("BEGIN IMMEDIATE")?;
    let mut guard = TxGuard { conn, done: false };

    // Mirror the v2.2 invariant: only delete OUTGOING relations for this
    // project's bazel targets, so we don't trample edges other workspaces
    // happen to have into our targets (e.g. a meta-workspace indexing two
    // sub-monorepos with cross-references).
    conn.execute(
        "DELETE FROM relations
         WHERE kind = 'BAZEL_DEPENDS_ON'
           AND src_entity IN (SELECT id FROM entities WHERE project = ?1 AND kind = 'bazel_target')",
        [project],
    )?;

    let repo_root_display = repo.canonicalize().unwrap_or_else(|_| repo.to_path_buf());
    let mut entity_insert = conn.prepare_cached(
        "INSERT INTO entities (id, kind, name, project, file_path, line_range)
         VALUES (?1, 'bazel_target', ?2, ?3, ?4, ?5)
         ON CONFLICT(id) DO UPDATE SET
            project = excluded.project,
            file_path = COALESCE(excluded.file_path, entities.file_path),
            line_range = COALESCE(excluded.line_range, entities.line_range)",
    )?;
    let mut by_label: HashMap<String, ()> = HashMap::new();
    for t in targets {
        let name = target_display_name(&t.label);
        let (file_path, line_range) = match &t.location {
            Some(loc) => {
                let rel = relative_to_repo(&loc.file, &repo_root_display);
                (Some(rel), Some(format!("{0}:{0}", loc.line)))
            }
            None => (None, None),
        };
        entity_insert.execute(rusqlite::params![
            &t.label,
            &name,
            project,
            file_path,
            line_range,
        ])?;
        by_label.insert(t.label.clone(), ());
    }
    let targets_inserted = by_label.len();

    let mut deps_inserted = 0usize;
    for t in targets {
        for dep in &t.deps {
            if !label_set.contains(dep.as_str()) {
                continue; // skip external `@repo//...` and source-file inputs
            }
            if dep == &t.label {
                continue; // skip trivial self-loops
            }
            let id = Uuid::now_v7().to_string();
            conn.execute(
                "INSERT INTO relations (id, src_entity, dst_entity, kind, provenance, confidence)
                 VALUES (?1, ?2, ?3, 'BAZEL_DEPENDS_ON', 'extracted', 1.0)",
                rusqlite::params![id, &t.label, dep],
            )?;
            deps_inserted += 1;
        }
    }

    guard.commit()?;
    Ok(BazelStats {
        targets_inserted,
        deps_inserted,
    })
}

fn target_display_name(label: &str) -> String {
    // `//pkg/sub:target` → `target`. `//pkg/sub` (no colon) → last path segment.
    if let Some(idx) = label.rfind(':') {
        return label[idx + 1..].to_string();
    }
    label
        .rsplit('/')
        .next()
        .unwrap_or(label)
        .to_string()
}

fn relative_to_repo(abs_file: &str, repo_root: &Path) -> String {
    let abs_path = Path::new(abs_file);
    match abs_path.strip_prefix(repo_root) {
        Ok(p) => p.to_string_lossy().into_owned(),
        Err(_) => abs_file.to_string(),
    }
}

fn bazel_binary() -> Result<PathBuf> {
    // Prefer `bazel` (so users with bazelisk symlinked benefit from
    // .bazelversion); fall back to `bazelisk` if only that's on PATH.
    for candidate in &["bazel", "bazelisk"] {
        if which::which(candidate).is_ok() {
            return Ok(PathBuf::from(candidate));
        }
    }
    anyhow::bail!(
        "neither `bazel` nor `bazelisk` is on PATH; install one of them before running engraph index on a Bazel workspace"
    )
}

fn output_base_for(repo: &Path) -> PathBuf {
    if let Ok(env) = std::env::var("ENGRAPH_BAZEL_OUTPUT_BASE") {
        return PathBuf::from(env);
    }
    let canonical = repo.canonicalize().unwrap_or_else(|_| repo.to_path_buf());
    let mut hasher = Sha256::new();
    hasher.update(canonical.to_string_lossy().as_bytes());
    let hex = format!("{:x}", hasher.finalize());
    let base = dirs::cache_dir()
        .unwrap_or_else(|| dirs::home_dir().unwrap_or_default().join(".cache"))
        .join("engraph")
        .join("bazel-out");
    base.join(&hex[..16])
}

fn tail_lines(s: &str, n: usize) -> String {
    let lines: Vec<&str> = s.lines().collect();
    let start = lines.len().saturating_sub(n);
    lines[start..].join("\n")
}

struct TxGuard<'a> {
    conn: &'a PooledConn,
    done: bool,
}
impl TxGuard<'_> {
    fn commit(&mut self) -> Result<()> {
        self.conn.execute_batch("COMMIT")?;
        self.done = true;
        Ok(())
    }
}
impl Drop for TxGuard<'_> {
    fn drop(&mut self) {
        if !self.done {
            let _ = self.conn.execute_batch("ROLLBACK");
        }
    }
}

// Avoid pulling in a separate crate for a one-line PATH lookup; `Command`
// itself resolves PATH on spawn, so this exists just for the friendlier
// error in `bazel_binary` above.
mod which {
    use std::path::PathBuf;
    pub fn which(bin: &str) -> Result<PathBuf, ()> {
        let path = std::env::var_os("PATH").ok_or(())?;
        for dir in std::env::split_paths(&path) {
            let candidate = dir.join(bin);
            if candidate.is_file() {
                return Ok(candidate);
            }
            #[cfg(windows)]
            {
                let cand = dir.join(format!("{}.exe", bin));
                if cand.is_file() {
                    return Ok(cand);
                }
            }
        }
        Err(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detect_bazel_recognizes_markers() {
        let dir = tempfile::tempdir().unwrap();
        assert!(!detect_bazel(dir.path()));
        std::fs::write(dir.path().join("WORKSPACE"), "").unwrap();
        assert!(detect_bazel(dir.path()));
    }

    #[test]
    fn target_display_name_strips_pkg_prefix() {
        assert_eq!(target_display_name("//foo/bar:baz"), "baz");
        assert_eq!(target_display_name("//foo/bar"), "bar");
        assert_eq!(target_display_name("@repo//foo:bar"), "bar");
    }

    #[test]
    fn parse_location_handles_file_line_col() {
        let loc = parse_location("/tmp/work/foo/BUILD.bazel:42:8").unwrap();
        assert_eq!(loc.file, "/tmp/work/foo/BUILD.bazel");
        assert_eq!(loc.line, 42);
    }

    #[test]
    fn parse_jsonproto_lines_extracts_rules() {
        let stdout = r#"{"type":"RULE","rule":{"name":"//bar:bar","ruleClass":"genrule","location":"/w/bar/BUILD.bazel:1:8","ruleInput":["//foo:foo","@bazel_tools//tools/genrule:genrule-setup.sh"]}}
{"type":"SOURCE_FILE","sourceFile":{"name":"//bar:src.txt"}}
{"type":"RULE","rule":{"name":"//foo:foo","ruleClass":"genrule","location":"/w/foo/BUILD.bazel:1:8","ruleInput":["@bazel_tools//tools/genrule:genrule-setup.sh"]}}
"#;
        let targets = parse_jsonproto_lines(stdout).unwrap();
        assert_eq!(targets.len(), 2);
        assert_eq!(targets[0].label, "//bar:bar");
        assert_eq!(targets[0].rule_class, "genrule");
        assert_eq!(targets[0].deps, vec!["//foo:foo", "@bazel_tools//tools/genrule:genrule-setup.sh"]);
        assert_eq!(targets[1].label, "//foo:foo");
    }
}
