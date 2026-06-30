use anyhow::{Context, Result};
use engraph_compress::{CompressInput, CompressKind, compress};
use engraph_core::{db::PooledConn, models::EventKind, telemetry, tokens};
use engraph_retrieve::scope;
use serde::Deserialize;
use std::fs::File;
use std::io::{BufRead, BufReader, Seek, SeekFrom};
use std::path::Path;
use std::time::Instant;
use uuid::Uuid;

use crate::common::{COMPRESS_THRESHOLD_TOKENS, sha256};

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

/// One textual message normalized out of either transcript format, ready for the
/// shared insert/compress/scope path.
struct ParsedLine {
    session_id: String,
    cwd: Option<String>,
    branch: Option<String>,
    role: String,
    content: String,
    msg_id: String,
    ts: Option<String>,
}

/// Session-level metadata hoisted from a Codex rollout's `session_meta` header
/// (Codex doesn't repeat these per message, unlike Claude Code).
struct CodexHeader {
    session_id: String,
    cwd: Option<String>,
    branch: Option<String>,
}

/// Read line 1 and, if it's a Codex `session_meta` header, extract the session
/// id / cwd / branch. `None` means "not Codex" → treat the file as Claude Code.
fn peek_codex_header(path: &Path) -> Option<CodexHeader> {
    let f = File::open(path).ok()?;
    let mut line = String::new();
    BufReader::new(f).read_line(&mut line).ok()?;
    let v: serde_json::Value = serde_json::from_str(line.trim()).ok()?;
    if v.get("type").and_then(|t| t.as_str()) != Some("session_meta") {
        return None;
    }
    let p = v.get("payload")?;
    Some(CodexHeader {
        session_id: p.get("id").and_then(|i| i.as_str())?.to_string(),
        cwd: p.get("cwd").and_then(|c| c.as_str()).map(|s| s.to_string()),
        branch: p
            .get("git")
            .and_then(|g| g.get("branch"))
            .and_then(|b| b.as_str())
            .map(|s| s.to_string()),
    })
}

/// Parse one Claude Code transcript line into a normalized message, or `None`
/// for events we don't ingest (sidechain, non-message, empty).
fn parse_claude_line(trimmed: &str) -> Result<Option<ParsedLine>> {
    let ev: RawEvent = serde_json::from_str(trimmed)?;
    if ev.is_sidechain {
        return Ok(None);
    }
    let kind = ev.kind.as_deref().unwrap_or("");
    if kind != "user" && kind != "assistant" {
        return Ok(None);
    }
    let Some(session_id) = ev.session_id.clone() else {
        return Ok(None);
    };
    let content = ev
        .message
        .as_ref()
        .and_then(|m| m.content.as_ref())
        .map(extract_text)
        .unwrap_or_default();
    if content.is_empty() {
        return Ok(None);
    }
    let role = ev
        .message
        .as_ref()
        .and_then(|m| m.role.as_deref())
        .unwrap_or(kind)
        .to_string();
    let msg_id = ev.uuid.unwrap_or_else(|| Uuid::now_v7().to_string());
    Ok(Some(ParsedLine {
        session_id,
        cwd: ev.cwd,
        branch: ev.git_branch,
        role,
        content,
        msg_id,
        ts: ev.timestamp,
    }))
}

/// Parse one Codex rollout line. We ingest only `response_item` messages; the
/// `event_msg` duplicates (user_message / agent_reasoning) and `turn_context`
/// lines are skipped. Session-level fields come from `hdr`; the `msg_id` is
/// derived from the byte offset so re-ingest from offset 0 stays idempotent.
fn parse_codex_line(
    trimmed: &str,
    hdr: &CodexHeader,
    line_start: u64,
) -> Result<Option<ParsedLine>> {
    let v: serde_json::Value = serde_json::from_str(trimmed)?;
    if v.get("type").and_then(|t| t.as_str()) != Some("response_item") {
        return Ok(None);
    }
    let Some(payload) = v.get("payload") else {
        return Ok(None);
    };
    if payload.get("type").and_then(|t| t.as_str()) != Some("message") {
        return Ok(None);
    }
    let role = match payload.get("role").and_then(|r| r.as_str()) {
        Some(r @ ("user" | "assistant")) => r.to_string(),
        _ => return Ok(None),
    };
    let content = payload.get("content").map(extract_text).unwrap_or_default();
    if content.is_empty() {
        return Ok(None);
    }
    Ok(Some(ParsedLine {
        session_id: hdr.session_id.clone(),
        cwd: hdr.cwd.clone(),
        branch: hdr.branch.clone(),
        role,
        content,
        msg_id: format!("codex-{}-{}", hdr.session_id, line_start),
        ts: v
            .get("timestamp")
            .and_then(|t| t.as_str())
            .map(|s| s.to_string()),
    }))
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

pub fn ingest_file(conn: &PooledConn, path: &Path) -> Result<IngestStats> {
    let start = Instant::now();
    let abs = path
        .canonicalize()
        .with_context(|| format!("canonicalize {}", path.display()))?;
    let abs_str = abs.to_string_lossy().to_string();

    // Peek line 1 to detect the transcript format. Codex rollout files lead with
    // a `session_meta` header carrying the session id / cwd / branch (none of
    // which recur per message, so this must run even on an incremental resume).
    // `None` => Claude Code format (the per-line shape `RawEvent` parses).
    let codex = peek_codex_header(&abs);

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
        // `current_offset` still points at the start of this line; capture it
        // before advancing, so Codex (which has no per-message uuid) can derive
        // a deterministic `msg_id` from it and stay idempotent on re-ingest.
        let line_start = current_offset;
        bytes_read += n as u64;
        current_offset += n as u64;

        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let parsed = match &codex {
            Some(hdr) => parse_codex_line(trimmed, hdr, line_start),
            None => parse_claude_line(trimmed),
        };
        let pl = match parsed {
            Ok(Some(pl)) => pl,
            Ok(None) => continue,
            Err(e) => {
                tracing::warn!(?e, line = %&trimmed.chars().take(80).collect::<String>(), "skip malformed event");
                continue;
            }
        };

        upsert_session(
            conn,
            &pl.session_id,
            pl.cwd.as_deref(),
            pl.ts.as_deref(),
            pl.branch.as_deref(),
        )?;

        let (stored, compressed_flag, orig_tokens, comp_tokens) = maybe_compress(&pl.content)?;
        if compressed_flag {
            messages_compressed += 1;
        }

        let hash = sha256(pl.content.as_bytes());
        conn.execute(
            "INSERT OR IGNORE INTO messages
                (id, session_id, role, content, content_compressed, content_hash, ts)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            rusqlite::params![
                pl.msg_id,
                pl.session_id,
                pl.role,
                stored,
                if compressed_flag { 1 } else { 0 },
                hash,
                pl.ts.clone().unwrap_or_else(now_iso),
            ],
        )?;
        messages_inserted += 1;

        // Project scope membership.
        if let Some(cwd) = pl.cwd.as_deref() {
            let scope_id = scope::ensure_project_scope(conn, cwd)?;
            scope::add_member(conn, &scope_id, "message", &pl.msg_id)?;
        }

        if compressed_flag {
            telemetry::record_event(
                conn,
                telemetry::EventInput {
                    session_id: Some(&pl.session_id),
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
                        // "text" is Claude; "input_text"/"output_text" are Codex.
                        "text" | "input_text" | "output_text" => {
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

fn now_iso() -> String {
    chrono::Utc::now().to_rfc3339()
}
