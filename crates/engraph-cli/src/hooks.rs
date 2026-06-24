use anyhow::Result;
use engraph_core::{
    budget, db,
    models::EventKind,
    telemetry::{self, EventInput},
    tokens,
};
use std::path::{Path, PathBuf};
use std::time::Instant;

use crate::redirect::{emit_subgraph_deny, try_subgraph_redirect, try_subgraph_redirect_for_bash};
use crate::rewrite::{RewriteOutcome, try_auto_rewrite};

/// Hard cap for the session-start brief, in bytes. Claude Code injects this
/// into context at session start; keep it small.
const MAX_BRIEF_BYTES: usize = 2048;

pub(crate) fn run_session_start_hook(conn: &db::PooledConn) -> Result<()> {
    use std::io::Read;
    let mut buf = String::new();
    std::io::stdin().read_to_string(&mut buf)?;

    let parsed: Option<serde_json::Value> = if buf.trim().is_empty() {
        None
    } else {
        // Malformed JSON falls back to "no stdin info" rather than failing the hook.
        serde_json::from_str(&buf).ok()
    };
    let cwd = match parsed.as_ref() {
        Some(v) => v.get("cwd").and_then(|c| c.as_str()).map(|s| s.to_string()),
        None => std::env::current_dir()
            .ok()
            .map(|p| p.to_string_lossy().into_owned()),
    };
    let session_id = parsed
        .as_ref()
        .and_then(|v| v.get("session_id").and_then(|c| c.as_str()))
        .map(|s| s.to_string());

    // Catch-up capture: SessionEnd only fires on a clean exit, so killed or
    // abruptly-closed sessions never get ingested. Sweep the project's
    // transcripts here (incremental + idempotent via `ingestion_log`), which
    // recovers anything a missed SessionEnd dropped on the next start.
    ingest_project_transcripts(conn, parsed.as_ref(), cwd.as_deref(), session_id.as_deref());

    let mut signal_sections: Vec<String> = Vec::new();
    if let Some(cwd) = cwd.as_deref() {
        let dnr = recent_do_not_repeat(conn, cwd, 5)?;
        if !dnr.is_empty() {
            signal_sections.push("## do-not-repeat".to_string());
            for r in dnr {
                signal_sections.push(format!("- {r}"));
            }
        }
        let bugs = open_bugs(conn, cwd, 5)?;
        if !bugs.is_empty() {
            signal_sections.push("## open bugs".to_string());
            for b in bugs {
                signal_sections.push(format!("- {b}"));
            }
        }
        let decisions = recent_decisions(conn, cwd, 5)?;
        if !decisions.is_empty() {
            signal_sections.push("## decisions".to_string());
            for d in decisions {
                signal_sections.push(format!("- {d}"));
            }
        }
    }
    if let Some(sid) = session_id.as_deref() {
        let g = budget::get_or_init(conn, sid)?;
        // Surface when usage is non-zero OR limits diverge from defaults.
        let limits_default =
            g.soft == budget::DEFAULT_SOFT_LIMIT && g.hard == budget::DEFAULT_HARD_LIMIT;
        if g.used > 0 || !limits_default {
            signal_sections.push(format!(
                "## budget\nsession={sid} used={used} soft={soft} hard={hard} level={lvl}",
                used = g.used,
                soft = g.soft,
                hard = g.hard,
                lvl = g.escalation_level()
            ));
        }
    }

    // Empty additionalContext on a truly-fresh project: zero injected tokens.
    let body = if signal_sections.is_empty() {
        String::new()
    } else {
        let mut full = String::new();
        if let Some(cwd) = cwd.as_deref() {
            full.push_str(&format!("# engraph brief — {cwd}\n"));
        }
        full.push_str(&signal_sections.join("\n"));
        truncate_to_bytes(&full, MAX_BRIEF_BYTES)
    };

    let start = Instant::now();
    let decision = serde_json::json!({
        "hookSpecificOutput": {
            "hookEventName": "SessionStart",
            "additionalContext": body,
        }
    });
    println!("{decision}");

    if !body.is_empty() {
        telemetry::record_event(
            conn,
            EventInput {
                session_id: session_id.as_deref(),
                kind: EventKind::Hook,
                feature: "session_brief",
                filter_id: Some("session_start"),
                input_tokens: 0,
                output_tokens: tokens::count(&body) as i64,
                latency_ms: start.elapsed().as_millis() as i64,
            },
        )?;
    }
    Ok(())
}

/// Best-effort catch-up ingest of every transcript in the project's directory.
/// Per-file errors are logged and skipped, and the whole pass is a no-op if the
/// directory can't be located — a broken transcript never blocks the brief.
/// `ingest_file` tracks byte offsets in `ingestion_log`, so re-running each
/// session start only reads bytes appended since last time.
fn ingest_project_transcripts(
    conn: &db::PooledConn,
    parsed: Option<&serde_json::Value>,
    cwd: Option<&str>,
    session_id: Option<&str>,
) {
    let Some(dir) = transcript_dir(parsed, cwd) else {
        return;
    };
    let entries = match std::fs::read_dir(&dir) {
        Ok(e) => e,
        Err(e) => {
            tracing::debug!(?e, dir = %dir.display(), "no transcript dir to ingest");
            return;
        }
    };
    let mut inserted = 0usize;
    let mut scanned = 0usize;
    for entry in entries.flatten() {
        let path = entry.path();
        // Top-level session transcripts only; `read_dir` is non-recursive, so
        // per-session `subagents/` sidechains are skipped automatically.
        if !path.is_file() || path.extension().and_then(|s| s.to_str()) != Some("jsonl") {
            continue;
        }
        scanned += 1;
        match engraph_ingest::ingest_file(conn, &path) {
            Ok(s) => inserted += s.messages_inserted,
            Err(e) => {
                tracing::warn!(?e, path = %path.display(), "session-start ingest failed; skipping")
            }
        }
    }
    // Cost scales with the project's lifetime session count (one open+seek per
    // transcript every start); surface it so the scan can't grow silently.
    tracing::debug!(scanned, inserted, dir = %dir.display(), "session-start catch-up ingest");
    if inserted > 0 {
        telemetry::record_event(
            conn,
            EventInput {
                session_id,
                kind: EventKind::Hook,
                feature: "session_ingest",
                filter_id: Some("session_start"),
                input_tokens: 0,
                output_tokens: inserted as i64,
                latency_ms: 0,
            },
        )
        .ok();
    }
}

/// Locate the directory holding this project's transcripts. Prefer the parent
/// of the current session's `transcript_path` (exact, encoding-agnostic); fall
/// back to reconstructing Claude Code's
/// `~/.claude/projects/<cwd-with-slashes-as-dashes>` layout from `cwd`.
fn transcript_dir(parsed: Option<&serde_json::Value>, cwd: Option<&str>) -> Option<PathBuf> {
    if let Some(parent) = parsed
        .and_then(|v| v.get("transcript_path"))
        .and_then(|s| s.as_str())
        .filter(|s| !s.is_empty())
        .and_then(|tp| Path::new(tp).parent())
    {
        return Some(parent.to_path_buf());
    }
    let home = std::env::var("HOME").ok()?;
    let encoded = cwd?.replace('/', "-");
    Some(
        Path::new(&home)
            .join(".claude")
            .join("projects")
            .join(encoded),
    )
}

const TRUNCATE_MARKER: &str = "\n…[truncated]";

fn truncate_to_bytes(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let marker_len = TRUNCATE_MARKER.len();
    if max <= marker_len {
        // No room for content; emit marker alone, clipped to max.
        return TRUNCATE_MARKER.chars().take(max).collect();
    }
    let mut cut = max - marker_len;
    while !s.is_char_boundary(cut) && cut > 0 {
        cut -= 1;
    }
    let mut out = s[..cut].to_string();
    out.push_str(TRUNCATE_MARKER);
    out
}

fn recent_do_not_repeat(conn: &db::PooledConn, project: &str, limit: i64) -> Result<Vec<String>> {
    let mut stmt = conn
        .prepare("SELECT rule FROM do_not_repeat WHERE project = ?1 ORDER BY ts DESC LIMIT ?2")?;
    let out = stmt
        .query_map(rusqlite::params![project, limit], |r| r.get::<_, String>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(out)
}

fn open_bugs(conn: &db::PooledConn, project: &str, limit: i64) -> Result<Vec<String>> {
    let mut stmt = conn.prepare(
        "SELECT summary FROM bugs WHERE project = ?1 AND resolved = 0 ORDER BY ts DESC LIMIT ?2",
    )?;
    let out = stmt
        .query_map(rusqlite::params![project, limit], |r| r.get::<_, String>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(out)
}

fn recent_decisions(conn: &db::PooledConn, project: &str, limit: i64) -> Result<Vec<String>> {
    let mut stmt = conn.prepare(
        "SELECT content FROM context_items \
         WHERE project = ?1 \
           AND kind IN ('decision','architecture','convention','performance') \
         ORDER BY ts DESC LIMIT ?2",
    )?;
    let out = stmt
        .query_map(rusqlite::params![project, limit], |r| r.get::<_, String>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(out)
}

pub(crate) fn run_pre_bash_hook(conn: &db::PooledConn) -> Result<()> {
    use std::io::Read;
    let mut buf = String::new();
    std::io::stdin().read_to_string(&mut buf)?;
    if buf.trim().is_empty() {
        return Ok(());
    }
    let v: serde_json::Value = match serde_json::from_str::<serde_json::Value>(&buf) {
        Ok(v) => v,
        Err(_) => return Ok(()),
    };
    let command = v
        .pointer("/tool_input/command")
        .and_then(|c| c.as_str())
        .unwrap_or("")
        .trim()
        .to_string();
    if command.is_empty() {
        return Ok(());
    }

    // Subgraph redirect wins over the compression rewrite: `rg <symbol>` or
    // `grep <symbol>` on an indexed bareword (1-3 matches) gets a deny+suggest
    // pointing at `engraph subgraph <symbol>`. That's an order of magnitude
    // smaller than even the compressed grep output, and gives structured edges.
    if let Some(reason) = try_subgraph_redirect_for_bash(&command, conn) {
        emit_subgraph_deny(conn, &reason, "grep_redirect");
        return Ok(());
    }

    let session_id = std::env::var("CLAUDE_SESSION_ID").ok();
    match try_auto_rewrite(&command) {
        RewriteOutcome::Rewrite {
            new_command,
            filter_id,
        } => {
            let decision = serde_json::json!({
                "hookSpecificOutput": {
                    "hookEventName": "PreToolUse",
                    "permissionDecision": "allow",
                    "updatedInput": { "command": new_command }
                }
            });
            println!("{decision}");
            telemetry::record_event(
                conn,
                EventInput {
                    session_id: session_id.as_deref(),
                    kind: EventKind::Hook,
                    feature: "cmd_rewrite",
                    filter_id: Some(filter_id),
                    input_tokens: 0,
                    output_tokens: 0,
                    latency_ms: 0,
                },
            )
            .ok();
        }
        RewriteOutcome::Passthrough => {}
    }
    Ok(())
}

/// PreToolUse(Grep) hook: redirect bareword Grep on an indexed symbol to
/// `engraph subgraph`. See `try_subgraph_redirect` for the gate.
pub(crate) fn run_pre_grep_hook(conn: &db::PooledConn) -> Result<()> {
    use std::io::Read;
    let mut buf = String::new();
    std::io::stdin().read_to_string(&mut buf)?;
    if buf.trim().is_empty() {
        return Ok(());
    }
    let v: serde_json::Value = match serde_json::from_str::<serde_json::Value>(&buf) {
        Ok(v) => v,
        Err(_) => return Ok(()),
    };
    let pattern = v
        .pointer("/tool_input/pattern")
        .and_then(|s| s.as_str())
        .unwrap_or("")
        .trim()
        .to_string();
    if let Some(reason) = try_subgraph_redirect(&pattern, conn) {
        emit_subgraph_deny(conn, &reason, "grep_redirect");
    }
    Ok(())
}

/// PostToolUse(Read) hook: when Claude reads a file that engraph has indexed,
/// append a listing of symbols defined in that file (name, line range,
/// signature) as `additionalContext`. Often answers "what's in this file"
/// without a follow-up subgraph or grep.
pub(crate) fn run_post_read_hook(conn: &db::PooledConn) -> Result<()> {
    use std::io::Read;
    let mut buf = String::new();
    std::io::stdin().read_to_string(&mut buf)?;
    if buf.trim().is_empty() {
        return Ok(());
    }
    let v: serde_json::Value = match serde_json::from_str::<serde_json::Value>(&buf) {
        Ok(v) => v,
        Err(_) => return Ok(()),
    };
    let file_path = v
        .pointer("/tool_input/file_path")
        .and_then(|s| s.as_str())
        .unwrap_or("")
        .trim();
    if file_path.is_empty() {
        return Ok(());
    }
    let entities = engraph_codegraph::subgraph::entities_in_file(conn, file_path, 30)?;
    if entities.is_empty() {
        return Ok(());
    }
    let context = truncate_to_bytes(&build_read_context(file_path, &entities), MAX_BRIEF_BYTES);
    let decision = serde_json::json!({
        "hookSpecificOutput": {
            "hookEventName": "PostToolUse",
            "additionalContext": context
        }
    });
    println!("{decision}");
    let session_id = std::env::var("CLAUDE_SESSION_ID").ok();
    telemetry::record_event(
        conn,
        EventInput {
            session_id: session_id.as_deref(),
            kind: EventKind::Hook,
            feature: "read_augment",
            filter_id: Some("read-augment"),
            input_tokens: 0,
            output_tokens: tokens::count(&context) as i64,
            latency_ms: 0,
        },
    )
    .ok();
    Ok(())
}

/// SessionEnd hook: Claude Code emits a JSON envelope on stdin that carries
/// `transcript_path` — the path to the JSONL transcript file for the session
/// that just ended. Ingest it into the codegraph store so subsequent sessions'
/// `engraph recall` queries can surface this session's messages. Empty stdin
/// or a missing `transcript_path` is treated as a no-op (not every SessionEnd
/// reason carries a transcript).
pub(crate) fn run_session_end_hook(conn: &db::PooledConn) -> Result<()> {
    use std::io::Read;
    let mut buf = String::new();
    std::io::stdin().read_to_string(&mut buf)?;
    if buf.trim().is_empty() {
        return Ok(());
    }
    let v: serde_json::Value = match serde_json::from_str::<serde_json::Value>(&buf) {
        Ok(v) => v,
        Err(_) => return Ok(()),
    };
    let transcript_path = match v.pointer("/transcript_path").and_then(|s| s.as_str()) {
        Some(p) if !p.is_empty() => p,
        _ => return Ok(()),
    };
    let start = std::time::Instant::now();
    let stats = engraph_ingest::ingest_file(conn, std::path::Path::new(transcript_path))?;
    let session_id = std::env::var("CLAUDE_SESSION_ID").ok();
    telemetry::record_event(
        conn,
        EventInput {
            session_id: session_id.as_deref(),
            kind: EventKind::Hook,
            feature: "session_ingest",
            filter_id: Some("ingest"),
            input_tokens: stats.bytes_read as i64,
            output_tokens: stats.messages_inserted as i64,
            latency_ms: start.elapsed().as_millis() as i64,
        },
    )
    .ok();
    Ok(())
}

fn build_read_context(
    file_path: &str,
    entities: &[engraph_codegraph::subgraph::EntityRow],
) -> String {
    let mut out = format!(
        "This file is indexed in the engraph code graph. To trace how any symbol \
         below connects — its callers, callees, and siblings — run \
         `engraph subgraph <name>` instead of grepping or reading more files; \
         it's the fast path for code context.\n\nIndexed symbols in {file_path}:\n"
    );
    for e in entities {
        let line = e.line_range.as_deref().unwrap_or("?");
        match e.signature.as_deref() {
            Some(sig) if !sig.is_empty() => {
                out.push_str(&format!("- `{}` @ {line} — `{sig}`\n", e.name));
            }
            _ => {
                out.push_str(&format!("- `{}` @ {line}\n", e.name));
            }
        }
    }
    out
}
