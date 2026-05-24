//! Claude Code JSONL transcript → SQLite ingestion.
//!
//! Parses the subset of events that carry textual content (user / assistant
//! messages), populates `sessions` and `messages`, compresses oversized
//! messages via `engraph-compress` during ingest, derives a project scope from
//! `cwd`, and tracks file offsets in `ingestion_log` for incremental re-runs.

use anyhow::{Context, Result};
use engraph_compress::{compress, CompressInput, CompressKind};
use engraph_core::{db::PooledConn, models::EventKind, telemetry, tokens};
use engraph_retrieve::scope;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::fs::File;
use std::io::{BufRead, BufReader, Seek, SeekFrom};
use std::path::Path;
use std::time::Instant;
use uuid::Uuid;

/// Tokens; messages above this get compressed during ingest.
pub const COMPRESS_THRESHOLD_TOKENS: u32 = 2_000;

pub struct IngestStats {
    pub messages_inserted: usize,
    pub messages_compressed: usize,
    pub bytes_read: u64,
    pub elapsed_ms: u128,
}

#[derive(Debug, Deserialize)]
struct RawEvent {
    #[serde(rename = "type")]
    kind: Option<String>,
    #[serde(rename = "sessionId")]
    session_id: Option<String>,
    timestamp: Option<String>,
    uuid: Option<String>,
    cwd: Option<String>,
    #[serde(rename = "gitBranch")]
    git_branch: Option<String>,
    message: Option<RawMessage>,
}

#[derive(Debug, Deserialize)]
struct RawMessage {
    role: Option<String>,
    /// String for user turns, or array of content blocks for assistant turns.
    content: Option<serde_json::Value>,
}

pub fn ingest_file(conn: &PooledConn, path: &Path) -> Result<IngestStats> {
    let start = Instant::now();
    let abs = path
        .canonicalize()
        .with_context(|| format!("canonicalize {}", path.display()))?;
    let abs_str = abs.to_string_lossy().to_string();

    let last_offset: i64 = conn
        .query_row(
            "SELECT last_offset FROM ingestion_log WHERE jsonl_path = ?1",
            [&abs_str],
            |r| r.get(0),
        )
        .unwrap_or(0);

    let mut f = File::open(&abs).with_context(|| format!("open {}", abs.display()))?;
    let total_size = f.metadata()?.len();
    if (last_offset as u64) >= total_size {
        return Ok(IngestStats {
            messages_inserted: 0,
            messages_compressed: 0,
            bytes_read: 0,
            elapsed_ms: start.elapsed().as_millis(),
        });
    }
    f.seek(SeekFrom::Start(last_offset as u64))?;
    let mut reader = BufReader::new(f);

    let mut messages_inserted = 0usize;
    let mut messages_compressed = 0usize;
    let mut bytes_read = 0u64;
    let mut current_offset = last_offset as u64;
    let mut line = String::new();

    loop {
        line.clear();
        let n = reader.read_line(&mut line)?;
        if n == 0 {
            break;
        }
        bytes_read += n as u64;
        current_offset += n as u64;

        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let ev: RawEvent = match serde_json::from_str(trimmed) {
            Ok(e) => e,
            Err(e) => {
                tracing::warn!(?e, line = %&trimmed.chars().take(80).collect::<String>(), "skip malformed event");
                continue;
            }
        };

        let kind = ev.kind.as_deref().unwrap_or("");
        if kind != "user" && kind != "assistant" {
            continue;
        }

        let session_id = match ev.session_id.as_deref() {
            Some(s) => s.to_string(),
            None => continue,
        };

        upsert_session(
            conn,
            &session_id,
            ev.cwd.as_deref(),
            ev.timestamp.as_deref(),
            ev.git_branch.as_deref(),
        )?;

        let content_str = ev
            .message
            .as_ref()
            .and_then(|m| m.content.as_ref())
            .map(extract_text)
            .unwrap_or_default();
        if content_str.is_empty() {
            continue;
        }

        let msg_id = ev
            .uuid
            .clone()
            .unwrap_or_else(|| Uuid::now_v7().to_string());
        let role = ev
            .message
            .as_ref()
            .and_then(|m| m.role.as_deref())
            .unwrap_or(kind);

        let (stored, compressed_flag, orig_tokens, comp_tokens) =
            maybe_compress(&content_str)?;
        if compressed_flag {
            messages_compressed += 1;
        }

        let hash = sha256(content_str.as_bytes());
        conn.execute(
            "INSERT OR IGNORE INTO messages
                (id, session_id, role, content, content_compressed, content_hash, ts)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            rusqlite::params![
                msg_id,
                session_id,
                role,
                stored,
                if compressed_flag { 1 } else { 0 },
                hash,
                ev.timestamp.unwrap_or_else(now_iso),
            ],
        )?;
        messages_inserted += 1;

        // Project scope membership.
        if let Some(cwd) = ev.cwd.as_deref() {
            let scope_id = scope::ensure_project_scope(conn, cwd)?;
            scope::add_member(conn, &scope_id, "message", &msg_id)?;
        }

        if compressed_flag {
            telemetry::record_event(
                conn,
                telemetry::EventInput {
                    session_id: Some(&session_id),
                    kind: EventKind::Compress,
                    feature: "F6_ingest",
                    filter_id: Some("session_message"),
                    input_tokens: orig_tokens as i64,
                    output_tokens: comp_tokens as i64,
                    latency_ms: 0,
                },
            )
            .ok();
        }
    }

    // Update ingestion log
    let mtime = std::fs::metadata(&abs)
        .ok()
        .and_then(|m| m.modified().ok())
        .map(|t| {
            let dt: chrono::DateTime<chrono::Utc> = t.into();
            dt.to_rfc3339()
        });
    conn.execute(
        "INSERT INTO ingestion_log (jsonl_path, last_offset, last_mtime)
         VALUES (?1, ?2, ?3)
         ON CONFLICT(jsonl_path) DO UPDATE SET
            last_offset = ?2, last_mtime = ?3, ingested_at = datetime('now')",
        rusqlite::params![abs_str, current_offset as i64, mtime],
    )?;

    Ok(IngestStats {
        messages_inserted,
        messages_compressed,
        bytes_read,
        elapsed_ms: start.elapsed().as_millis(),
    })
}

fn upsert_session(
    conn: &PooledConn,
    id: &str,
    cwd: Option<&str>,
    timestamp: Option<&str>,
    _git_branch: Option<&str>,
) -> Result<()> {
    conn.execute(
        "INSERT OR IGNORE INTO sessions (id, project, cwd, started_at)
         VALUES (?1, ?2, ?3, ?4)",
        rusqlite::params![
            id,
            cwd, // project = cwd for now; can be refined later
            cwd,
            timestamp.unwrap_or(&now_iso()),
        ],
    )?;
    Ok(())
}

fn extract_text(v: &serde_json::Value) -> String {
    match v {
        serde_json::Value::String(s) => s.clone(),
        serde_json::Value::Array(items) => {
            let mut out = String::new();
            for item in items {
                if let Some(t) = item.get("type").and_then(|t| t.as_str()) {
                    match t {
                        "text" => {
                            if let Some(text) = item.get("text").and_then(|x| x.as_str()) {
                                if !out.is_empty() {
                                    out.push('\n');
                                }
                                out.push_str(text);
                            }
                        }
                        "thinking" => {
                            if let Some(text) = item.get("thinking").and_then(|x| x.as_str()) {
                                if !out.is_empty() {
                                    out.push('\n');
                                }
                                out.push_str("[thinking] ");
                                out.push_str(text);
                            }
                        }
                        _ => {}
                    }
                }
            }
            out
        }
        _ => String::new(),
    }
}

fn maybe_compress(content: &str) -> Result<(String, bool, u32, u32)> {
    let tk = tokens::count(content);
    if tk < COMPRESS_THRESHOLD_TOKENS {
        return Ok((content.to_string(), false, tk, tk));
    }
    let r = compress(CompressInput {
        text: content,
        kind: CompressKind::SessionMessage,
        target_ratio: 0.5,
        brevity: false,
    });
    Ok((r.text, true, r.original_tokens, r.compressed_tokens))
}

fn sha256(bytes: &[u8]) -> Vec<u8> {
    let mut h = Sha256::new();
    h.update(bytes);
    h.finalize().to_vec()
}

fn now_iso() -> String {
    chrono::Utc::now().to_rfc3339()
}

#[cfg(test)]
mod tests {
    use super::*;
    use engraph_core::db::open_pool;
    use std::io::Write;
    use tempfile::tempdir;

    fn write_jsonl(path: &Path, lines: &[&str]) {
        let mut f = File::create(path).unwrap();
        for l in lines {
            writeln!(f, "{l}").unwrap();
        }
    }

    #[test]
    fn ingest_minimal_user_assistant() {
        let dir = tempdir().unwrap();
        let pool = open_pool(&dir.path().join("ingest.db")).unwrap();
        let conn = pool.get().unwrap();
        let jp = dir.path().join("t.jsonl");
        write_jsonl(
            &jp,
            &[
                r#"{"type":"user","sessionId":"s1","cwd":"/proj","timestamp":"2026-05-24T00:00:00Z","uuid":"u1","message":{"role":"user","content":"hello world"}}"#,
                r#"{"type":"assistant","sessionId":"s1","cwd":"/proj","timestamp":"2026-05-24T00:00:01Z","uuid":"a1","message":{"role":"assistant","content":[{"type":"text","text":"hi there"}]}}"#,
                r#"{"type":"tool_use","sessionId":"s1"}"#,
            ],
        );
        let stats = ingest_file(&conn, &jp).unwrap();
        assert_eq!(stats.messages_inserted, 2);

        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM messages", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 2);
    }

    #[test]
    fn re_ingest_is_incremental() {
        let dir = tempdir().unwrap();
        let pool = open_pool(&dir.path().join("ingest.db")).unwrap();
        let conn = pool.get().unwrap();
        let jp = dir.path().join("t.jsonl");
        write_jsonl(
            &jp,
            &[r#"{"type":"user","sessionId":"s1","cwd":"/p","timestamp":"t","uuid":"u1","message":{"role":"user","content":"first"}}"#],
        );
        let s1 = ingest_file(&conn, &jp).unwrap();
        assert_eq!(s1.messages_inserted, 1);

        // Append a second line, re-ingest.
        let mut f = std::fs::OpenOptions::new().append(true).open(&jp).unwrap();
        let line = r#"{"type":"user","sessionId":"s1","cwd":"/p","timestamp":"t","uuid":"u2","message":{"role":"user","content":"second"}}"#;
        writeln!(f, "{line}").unwrap();
        drop(f);
        let s2 = ingest_file(&conn, &jp).unwrap();
        assert_eq!(s2.messages_inserted, 1);
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM messages", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 2);
    }
}
