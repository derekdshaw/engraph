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
    /// Claude Code's branching/sub-agent feature emits these on a sidechain
    /// that shouldn't pollute the main session transcript.
    #[serde(rename = "isSidechain", default)]
    is_sidechain: bool,
}

#[derive(Debug, Deserialize)]
struct RawMessage {
    role: Option<String>,
    /// String for user turns, or array of content blocks for assistant turns.
    content: Option<serde_json::Value>,
}

/// RAII guard for an explicit BEGIN/COMMIT around a pooled SQLite connection.
/// Rolls back on drop if `commit()` wasn't called, so an error path doesn't
/// return the connection to the pool with an open transaction.
struct TxGuard<'a> {
    conn: &'a PooledConn,
    finished: bool,
}

impl<'a> TxGuard<'a> {
    fn begin(conn: &'a PooledConn) -> Result<Self> {
        conn.execute_batch("BEGIN")?;
        Ok(Self {
            conn,
            finished: false,
        })
    }

    fn commit(mut self) -> Result<()> {
        self.conn.execute_batch("COMMIT")?;
        self.finished = true;
        Ok(())
    }
}

impl Drop for TxGuard<'_> {
    fn drop(&mut self) {
        if !self.finished {
            let _ = self.conn.execute_batch("ROLLBACK");
        }
    }
}

pub struct SweepStats {
    pub rows_scanned: usize,
    pub rows_compressed: usize,
    pub bytes_before: u64,
    pub bytes_after: u64,
    pub elapsed_ms: u128,
}

/// Sweep messages and context_items, compressing in place any row whose
/// content is uncompressed AND exceeds the token threshold. Idempotent:
/// rows already marked content_compressed=1 are skipped without re-tokenizing,
/// and the sentinel check inside compress() makes accidental double-compress
/// a no-op anyway.
pub fn compress_existing(conn: &PooledConn, batch: usize) -> Result<SweepStats> {
    let start = Instant::now();
    let mut stats = SweepStats {
        rows_scanned: 0,
        rows_compressed: 0,
        bytes_before: 0,
        bytes_after: 0,
        elapsed_ms: 0,
    };

    for table in ["messages", "context_items"] {
        sweep_table(conn, table, batch, &mut stats)?;
    }

    stats.elapsed_ms = start.elapsed().as_millis();
    Ok(stats)
}

fn sweep_table(conn: &PooledConn, table: &str, batch: usize, stats: &mut SweepStats) -> Result<()> {
    // Read candidate rows in one prepared statement, then update in a loop.
    // `batch` caps the number of rows processed per call.
    let select_sql = format!(
        "SELECT rowid, id, content FROM {table}
         WHERE content_compressed = 0
         ORDER BY rowid ASC LIMIT ?1"
    );
    let update_sql = format!(
        "UPDATE {table} SET content = ?2, content_compressed = 1, content_hash = ?3 WHERE rowid = ?1"
    );

    let mut select = conn.prepare(&select_sql)?;
    let mut update = conn.prepare(&update_sql)?;

    let rows = select
        .query_map([batch as i64], |r| {
            let rowid: i64 = r.get(0)?;
            let id: String = r.get(1)?;
            let content: String = r.get(2)?;
            Ok((rowid, id, content))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;

    for (rowid, id, content) in rows {
        stats.rows_scanned += 1;
        let original_bytes = content.len() as u64;
        let original_tokens = tokens::count(&content);
        if original_tokens < COMPRESS_THRESHOLD_TOKENS {
            // Below threshold — still mark as compressed so we don't re-scan
            // the same rows every sweep.
            update.execute(rusqlite::params![
                rowid,
                content,
                sha256(content.as_bytes())
            ])?;
            continue;
        }
        let r = compress(CompressInput {
            text: &content,
            kind: CompressKind::SessionMessage,
            target_ratio: 0.5,
            brevity: false,
        });
        update.execute(rusqlite::params![
            rowid,
            &r.text,
            sha256(content.as_bytes()),
        ])?;
        let after_bytes = r.text.len() as u64;
        stats.bytes_before += original_bytes;
        stats.bytes_after += after_bytes;
        stats.rows_compressed += 1;

        telemetry::record_event(
            conn,
            telemetry::EventInput {
                session_id: None,
                kind: EventKind::Compress,
                feature: "compress_sweep",
                filter_id: Some(table),
                input_tokens: r.original_tokens as i64,
                output_tokens: r.compressed_tokens as i64,
                latency_ms: 0,
            },
        )?;
        tracing::debug!(
            id = %id,
            orig = original_tokens,
            comp = r.compressed_tokens,
            "compressed existing row"
        );
    }
    Ok(())
}

pub fn ingest_file(conn: &PooledConn, path: &Path) -> Result<IngestStats> {
    let start = Instant::now();
    let abs = path
        .canonicalize()
        .with_context(|| format!("canonicalize {}", path.display()))?;
    let abs_str = abs.to_string_lossy().to_string();

    let mut f = File::open(&abs).with_context(|| format!("open {}", abs.display()))?;
    let meta = f.metadata()?;
    let total_size = meta.len();
    let current_inode = file_inode(&meta);

    // Detect rotation / truncation / replacement before trusting `last_offset`.
    // M1 fix: a transcript can be rotated (new inode at the same path), truncated
    // by a writer that opened with O_TRUNC, or replaced wholesale with shorter
    // content. Any of these makes the stored offset point at the wrong bytes.
    // The fingerprint is (inode, size): if either regresses, restart from zero.
    let last = conn
        .query_row(
            "SELECT last_offset, last_inode, last_size FROM ingestion_log WHERE jsonl_path = ?1",
            [&abs_str],
            |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, Option<i64>>(1)?,
                    r.get::<_, Option<i64>>(2)?,
                ))
            },
        )
        .ok();

    let last_offset: i64 = match last {
        None => 0,
        Some((off, prev_inode, prev_size)) => {
            let rotated = matches!(prev_inode, Some(p) if Some(p) != current_inode);
            let truncated = (total_size as i64) < off;
            let shrunk = matches!(prev_size, Some(s) if (total_size as i64) < s);
            if rotated || truncated || shrunk {
                tracing::info!(
                    path = %abs_str,
                    last_offset = off,
                    current_size = total_size,
                    rotated,
                    truncated,
                    shrunk,
                    "detected JSONL rotation/truncation; re-ingesting from offset 0"
                );
                0
            } else {
                off
            }
        }
    };

    if (last_offset as u64) >= total_size {
        // Still record the up-to-date fingerprint so a later append sees fresh metadata.
        write_ingestion_log(
            conn,
            &abs_str,
            last_offset,
            current_inode,
            total_size as i64,
            &meta,
        )?;
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

    // Wrap the per-message writes in a single transaction. SQLite WAL mode
    // fsyncs once per implicit txn; the previous auto-commit path issued ~3
    // statements per message and proportionally many fsyncs. Per-file batching
    // is a 5–10× ingest throughput improvement on typical transcripts.
    let tx_guard = TxGuard::begin(conn)?;

    loop {
        line.clear();
        let n = reader.read_line(&mut line)?;
        if n == 0 {
            break;
        }
        // M2: a writer can still be appending to this file. If the buffer ends
        // without a newline we've read a partial line — don't commit the offset
        // past it, or the completed line is permanently skipped on re-ingest.
        if !line.ends_with('\n') {
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

        if ev.is_sidechain {
            continue;
        }

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

        let (stored, compressed_flag, orig_tokens, comp_tokens) = maybe_compress(&content_str)?;
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
                    feature: "compress_ingest",
                    filter_id: Some("session_message"),
                    input_tokens: orig_tokens as i64,
                    output_tokens: comp_tokens as i64,
                    latency_ms: 0,
                },
            )
            .ok();
        }
    }

    // Refresh metadata after read so size reflects what we actually processed.
    let final_meta = std::fs::metadata(&abs)?;
    write_ingestion_log(
        conn,
        &abs_str,
        current_offset as i64,
        file_inode(&final_meta),
        final_meta.len() as i64,
        &final_meta,
    )?;
    tx_guard.commit()?;

    Ok(IngestStats {
        messages_inserted,
        messages_compressed,
        bytes_read,
        elapsed_ms: start.elapsed().as_millis(),
    })
}

fn file_inode(meta: &std::fs::Metadata) -> Option<i64> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        Some(meta.ino() as i64)
    }
    #[cfg(not(unix))]
    {
        let _ = meta;
        None
    }
}

fn write_ingestion_log(
    conn: &PooledConn,
    abs_str: &str,
    last_offset: i64,
    inode: Option<i64>,
    size: i64,
    meta: &std::fs::Metadata,
) -> Result<()> {
    let mtime = meta.modified().ok().map(|t| {
        let dt: chrono::DateTime<chrono::Utc> = t.into();
        dt.to_rfc3339()
    });
    conn.execute(
        "INSERT INTO ingestion_log (jsonl_path, last_offset, last_mtime, last_inode, last_size)
         VALUES (?1, ?2, ?3, ?4, ?5)
         ON CONFLICT(jsonl_path) DO UPDATE SET
            last_offset = ?2,
            last_mtime = ?3,
            last_inode = ?4,
            last_size = ?5,
            ingested_at = datetime('now')",
        rusqlite::params![abs_str, last_offset, mtime, inode, size],
    )?;
    Ok(())
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
    fn ingest_detects_truncation_and_replays() {
        let dir = tempdir().unwrap();
        let pool = open_pool(&dir.path().join("rot.db")).unwrap();
        let conn = pool.get().unwrap();
        let jp = dir.path().join("t.jsonl");

        // Initial write of 2 messages.
        write_jsonl(
            &jp,
            &[
                r#"{"type":"user","sessionId":"s","cwd":"/p","timestamp":"t","uuid":"old1","message":{"role":"user","content":"original first"}}"#,
                r#"{"type":"user","sessionId":"s","cwd":"/p","timestamp":"t","uuid":"old2","message":{"role":"user","content":"original second"}}"#,
            ],
        );
        let s1 = ingest_file(&conn, &jp).unwrap();
        assert_eq!(s1.messages_inserted, 2);

        // Overwrite (truncating) with a single shorter line.
        write_jsonl(
            &jp,
            &[
                r#"{"type":"user","sessionId":"s","cwd":"/p","timestamp":"t","uuid":"new1","message":{"role":"user","content":"replacement only"}}"#,
            ],
        );
        // Without rotation handling, this would return 0 inserted; with the fix,
        // the new content gets re-ingested from offset 0.
        let s2 = ingest_file(&conn, &jp).unwrap();
        assert_eq!(
            s2.messages_inserted, 1,
            "rotation should replay the new file"
        );

        let total: i64 = conn
            .query_row("SELECT COUNT(*) FROM messages", [], |r| r.get(0))
            .unwrap();
        // Old rows remain (INSERT OR IGNORE keeps them); the new row joins.
        assert_eq!(total, 3);
    }

    #[test]
    fn ingest_appends_without_rescanning() {
        let dir = tempdir().unwrap();
        let pool = open_pool(&dir.path().join("app.db")).unwrap();
        let conn = pool.get().unwrap();
        let jp = dir.path().join("t.jsonl");
        write_jsonl(
            &jp,
            &[
                r#"{"type":"user","sessionId":"s","cwd":"/p","timestamp":"t","uuid":"a","message":{"role":"user","content":"one"}}"#,
            ],
        );
        let s1 = ingest_file(&conn, &jp).unwrap();
        assert_eq!(s1.messages_inserted, 1);

        // True append (growth) — not a rotation; re-ingest only sees the new line.
        let mut f = std::fs::OpenOptions::new().append(true).open(&jp).unwrap();
        writeln!(
            f,
            r#"{{"type":"user","sessionId":"s","cwd":"/p","timestamp":"t","uuid":"b","message":{{"role":"user","content":"two"}}}}"#
        )
        .unwrap();
        drop(f);
        let s2 = ingest_file(&conn, &jp).unwrap();
        assert_eq!(s2.messages_inserted, 1);
        assert!(s2.bytes_read < s1.bytes_read + 200);
    }

    #[test]
    fn compress_existing_only_touches_large_uncompressed_rows() {
        let dir = tempdir().unwrap();
        let pool = open_pool(&dir.path().join("sweep.db")).unwrap();
        let conn = pool.get().unwrap();
        // Insert one small row (below threshold) and one large row (above).
        let small = "hello world".to_string();
        // Diverse English sentences to ensure > COMPRESS_THRESHOLD_TOKENS (2000).
        let mut large = String::new();
        for i in 0..400 {
            large.push_str(&format!(
                "Note {i}: the engineer reviewed the proposal and recorded decision number {i} with rationale about scaling and observability tradeoffs.\n",
            ));
        }
        conn.execute(
            "INSERT INTO sessions (id, project, cwd, started_at) VALUES ('s', '/p', '/p', 't')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO messages (id, session_id, role, content, ts) VALUES ('m1','s','user',?1,'t')",
            [&small],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO messages (id, session_id, role, content, ts) VALUES ('m2','s','user',?1,'t')",
            [&large],
        )
        .unwrap();
        let stats = compress_existing(&conn, 100).unwrap();
        assert_eq!(stats.rows_scanned, 2);
        assert_eq!(
            stats.rows_compressed, 1,
            "only the large row should compress"
        );
        assert!(stats.bytes_after < stats.bytes_before);

        // Second pass: idempotent, nothing left to scan.
        let stats2 = compress_existing(&conn, 100).unwrap();
        assert_eq!(stats2.rows_scanned, 0);
        assert_eq!(stats2.rows_compressed, 0);
    }

    #[test]
    fn compress_existing_keeps_fts_pointed_at_original() {
        // v5 fix: dropping the messages_au trigger means UPDATE-by-compression
        // does NOT replace FTS index content. Recall against the original
        // phrasing must still hit after compression.
        let dir = tempdir().unwrap();
        let pool = open_pool(&dir.path().join("fts.db")).unwrap();
        let conn = pool.get().unwrap();
        // Distinctive phrase the compressor will likely drop or paraphrase.
        let phrase = "PINEAPPLE_BANANA_SENTINEL_quokka";
        // Pad it past the COMPRESS_THRESHOLD_TOKENS so the sweep actually rewrites.
        let mut large = String::from(phrase);
        large.push('\n');
        for i in 0..500 {
            large.push_str(&format!(
                "Note {i}: the engineer reviewed the proposal and recorded decision number {i} with rationale.\n",
            ));
        }
        conn.execute(
            "INSERT INTO sessions (id, project, cwd, started_at) VALUES ('s','/p','/p','t')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO messages (id, session_id, role, content, ts) VALUES ('m','s','user',?1,'t')",
            [&large],
        )
        .unwrap();

        // Sanity: FTS hits the phrase before compression.
        let pre: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM messages_fts WHERE messages_fts MATCH ?1",
                [phrase],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(pre, 1, "phrase should be in FTS before compression");

        let stats = compress_existing(&conn, 100).unwrap();
        assert_eq!(stats.rows_compressed, 1);

        // After compression the stored content has shrunk, but the FTS row
        // still holds the original — recall still hits.
        let stored: String = conn
            .query_row("SELECT content FROM messages WHERE id = 'm'", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert!(
            stored.len() < large.len(),
            "content should be smaller after compress"
        );
        let post: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM messages_fts WHERE messages_fts MATCH ?1",
                [phrase],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            post, 1,
            "FTS must still match original phrase after compression"
        );
    }

    #[test]
    fn compress_existing_preserves_recoverability_hash() {
        let dir = tempdir().unwrap();
        let pool = open_pool(&dir.path().join("rec.db")).unwrap();
        let conn = pool.get().unwrap();
        let large = "decision: ".repeat(800);
        let expected_hash = sha256(large.as_bytes());
        conn.execute(
            "INSERT INTO sessions (id, project, cwd, started_at) VALUES ('s', '/p', '/p', 't')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO messages (id, session_id, role, content, ts) VALUES ('m','s','user',?1,'t')",
            [&large],
        )
        .unwrap();
        compress_existing(&conn, 100).unwrap();
        let stored_hash: Vec<u8> = conn
            .query_row(
                "SELECT content_hash FROM messages WHERE id = 'm'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(stored_hash, expected_hash);
    }

    #[test]
    fn ingest_holds_offset_when_trailing_line_is_partial() {
        // M2 regression: a writer can flush half a line. We must not advance
        // current_offset past unterminated bytes, or the completed line is
        // permanently skipped on the next ingest.
        let dir = tempdir().unwrap();
        let pool = open_pool(&dir.path().join("partial.db")).unwrap();
        let conn = pool.get().unwrap();
        let jp = dir.path().join("t.jsonl");

        // First write: one full line + an unterminated trailing fragment.
        let full = r#"{"type":"user","sessionId":"s","cwd":"/p","timestamp":"t","uuid":"u1","message":{"role":"user","content":"complete one"}}"#;
        let partial_head = r#"{"type":"user","sessionId":"s","cwd":"/p","timestamp":"t","uui"#;
        {
            let mut f = File::create(&jp).unwrap();
            writeln!(f, "{full}").unwrap();
            // no trailing newline on the partial
            f.write_all(partial_head.as_bytes()).unwrap();
        }
        let s1 = ingest_file(&conn, &jp).unwrap();
        assert_eq!(
            s1.messages_inserted, 1,
            "only the complete line should ingest"
        );
        let offset1: i64 = conn
            .query_row(
                "SELECT last_offset FROM ingestion_log WHERE jsonl_path = ?1",
                [jp.canonicalize().unwrap().to_string_lossy().as_ref()],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            offset1,
            (full.len() + 1) as i64,
            "offset must stop at end of completed line"
        );

        // Now the writer completes the partial line. Re-ingest must pick it up.
        let tail = r#"d":"u2","message":{"role":"user","content":"completed two"}}"#;
        {
            use std::io::Write;
            let mut f = std::fs::OpenOptions::new().append(true).open(&jp).unwrap();
            f.write_all(tail.as_bytes()).unwrap();
            writeln!(f).unwrap();
        }
        let s2 = ingest_file(&conn, &jp).unwrap();
        assert_eq!(
            s2.messages_inserted, 1,
            "completed second line must ingest on next run"
        );
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM messages", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 2);
    }

    #[test]
    fn ingest_skips_sidechain_events() {
        let dir = tempdir().unwrap();
        let pool = open_pool(&dir.path().join("side.db")).unwrap();
        let conn = pool.get().unwrap();
        let jp = dir.path().join("t.jsonl");
        write_jsonl(
            &jp,
            &[
                r#"{"type":"user","sessionId":"s","cwd":"/p","timestamp":"t","uuid":"main1","message":{"role":"user","content":"main turn"}}"#,
                r#"{"type":"user","sessionId":"s","cwd":"/p","timestamp":"t","uuid":"side1","isSidechain":true,"message":{"role":"user","content":"sub-agent turn"}}"#,
                r#"{"type":"assistant","sessionId":"s","cwd":"/p","timestamp":"t","uuid":"side2","isSidechain":true,"message":{"role":"assistant","content":[{"type":"text","text":"sub-agent reply"}]}}"#,
            ],
        );
        let s = ingest_file(&conn, &jp).unwrap();
        assert_eq!(
            s.messages_inserted, 1,
            "sidechain events must not be ingested"
        );
        let stored: String = conn
            .query_row("SELECT content FROM messages", [], |r| r.get(0))
            .unwrap();
        assert_eq!(stored, "main turn");
    }

    #[test]
    fn re_ingest_is_incremental() {
        let dir = tempdir().unwrap();
        let pool = open_pool(&dir.path().join("ingest.db")).unwrap();
        let conn = pool.get().unwrap();
        let jp = dir.path().join("t.jsonl");
        write_jsonl(
            &jp,
            &[
                r#"{"type":"user","sessionId":"s1","cwd":"/p","timestamp":"t","uuid":"u1","message":{"role":"user","content":"first"}}"#,
            ],
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
