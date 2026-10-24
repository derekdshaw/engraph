//! Per-session digest: every real user prompt plus the final visible assistant
//! reply of its turn, stored as one `session_digest` context item. Built from
//! transcript structure alone (no model, no prose rewriting), so it is
//! deterministic and verbatim. A Claude Code compaction summary, when present,
//! stands in for everything before it.

use anyhow::Result;
use engraph_compress::is_compressed;
use engraph_core::db::PooledConn;
use engraph_retrieve::scope;

pub(crate) const DIGEST_KIND: &str = "session_digest";
/// Role given at ingest to Claude Code's `isCompactSummary` user line.
pub(crate) const COMPACT_SUMMARY_ROLE: &str = "compact_summary";
/// Prefix `extract_text` puts on each thinking block.
const THINKING_PREFIX: &str = "[thinking] ";

/// Rebuild the digest for `session_id` from its stored messages. The id is
/// derived from the session so the rebuild is an explicit DELETE + INSERT:
/// `context_items_fts` is kept in sync only by insert/delete triggers.
pub(crate) fn rebuild(conn: &PooledConn, session_id: &str) -> Result<()> {
    let mut stmt = conn
        .prepare("SELECT role, content FROM messages WHERE session_id = ?1 ORDER BY ts, rowid")?;
    let rows = stmt
        .query_map([session_id], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;

    let id = format!("digest-{session_id}");
    conn.execute("DELETE FROM context_items WHERE id = ?1", [&id])?;
    let body = render(&rows);
    if body.is_empty() {
        conn.execute("DELETE FROM scope_members WHERE target_id = ?1", [&id])?;
        return Ok(());
    }

    let cwd: Option<String> = conn.query_row(
        "SELECT cwd FROM sessions WHERE id = ?1",
        [session_id],
        |r| r.get(0),
    )?;
    conn.execute(
        "INSERT INTO context_items (id, session_id, kind, content, project) \
         VALUES (?1, ?2, ?3, ?4, ?5)",
        rusqlite::params![id, session_id, DIGEST_KIND, body, cwd],
    )?;
    if let Some(cwd) = cwd.as_deref() {
        let scope_id = scope::ensure_project_scope(conn, cwd)?;
        scope::add_member(conn, &scope_id, "context_item", &id)?;
    }
    Ok(())
}

fn render(rows: &[(String, String)]) -> String {
    let last_summary = rows
        .iter()
        .rposition(|(role, _)| role == COMPACT_SUMMARY_ROLE);
    let mut out = String::new();
    if let Some(i) = last_summary {
        out.push_str("[compacted]\n");
        out.push_str(rows[i].1.trim());
    }

    let tail = &rows[last_summary.map_or(0, |i| i + 1)..];
    let mut turns: Vec<(String, Option<&str>)> = Vec::new();
    for (role, content) in tail {
        match role.as_str() {
            "user" => {
                if let Some(p) = prompt_text(content) {
                    turns.push((p, None));
                }
            }
            "assistant" => {
                if let (Some(turn), Some(reply)) = (turns.last_mut(), reply_text(content)) {
                    turn.1 = Some(reply);
                }
            }
            _ => {}
        }
    }
    for (prompt, reply) in turns {
        if !out.is_empty() {
            out.push_str("\n\n");
        }
        out.push_str("U: ");
        out.push_str(&prompt);
        if let Some(reply) = reply {
            out.push_str("\nA: ");
            out.push_str(reply);
        }
    }
    out
}

/// The human-typed part of a user message, or `None` for harness-injected
/// rows. Claude Code wraps its injections (`<local-command-caveat>`,
/// `<command-name>`, `<task-notification>`, …) in leading tag blocks that can
/// share a message with a real prompt, so strip them rather than skip the row.
fn prompt_text(content: &str) -> Option<String> {
    if is_compressed(content) || content.starts_with("Base directory for this skill:") {
        return None;
    }
    let mut rest = content.trim_start();
    while let Some(after) = strip_leading_tag(rest) {
        rest = after.trim_start();
    }
    let text = rest
        .lines()
        .filter(|l| !l.starts_with("[Request interrupted by user"))
        .collect::<Vec<_>>()
        .join("\n");
    let text = text.trim();
    (!text.is_empty()).then(|| text.to_string())
}

fn strip_leading_tag(s: &str) -> Option<&str> {
    let inner = s.strip_prefix('<')?;
    let name_len = inner.find(|c: char| !(c.is_ascii_alphanumeric() || c == '-' || c == '_'))?;
    if name_len == 0 {
        return None;
    }
    let close = format!("</{}>", &inner[..name_len]);
    let end = s.find(&close)? + close.len();
    Some(&s[end..])
}

/// Visible text of an assistant message: everything before its first thinking
/// block. Stored rows only ever have visible text ahead of thinking, not after.
fn reply_text(content: &str) -> Option<&str> {
    let cut = if content.starts_with(THINKING_PREFIX) {
        0
    } else {
        content
            .find(&format!("\n{THINKING_PREFIX}"))
            .unwrap_or(content.len())
    };
    let visible = content[..cut].trim();
    (!visible.is_empty()).then_some(visible)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rows(r: &[(&str, &str)]) -> Vec<(String, String)> {
        r.iter()
            .map(|(a, b)| (a.to_string(), b.to_string()))
            .collect()
    }

    #[test]
    fn keeps_final_visible_reply_per_turn() {
        let d = render(&rows(&[
            ("user", "why does it fail"),
            ("assistant", "[thinking]  look at the hook"),
            ("assistant", "Checking the hook."),
            ("assistant", "It fails because X.\n[thinking] done"),
            ("user", "fix it"),
            ("assistant", "Fixed."),
        ]));
        assert_eq!(
            d,
            "U: why does it fail\nA: It fails because X.\n\nU: fix it\nA: Fixed."
        );
    }

    #[test]
    fn harness_rows_are_not_turns() {
        let d = render(&rows(&[
            (
                "user",
                "<local-command-caveat>Caveat: ignore</local-command-caveat>",
            ),
            (
                "user",
                "<command-name>/advisor</command-name>\n<command-args></command-args>",
            ),
            ("user", "Base directory for this skill: /x\n# Skill"),
            ("user", "<<engraph:v1:compressed>>\nold body"),
            ("user", "[Request interrupted by user]"),
            (
                "user",
                "<ide_opened_file>a.rs</ide_opened_file>\nreal question",
            ),
            ("assistant", "answer"),
            (
                "user",
                "<task-notification>\n<task-id>1</task-id>\n</task-notification>",
            ),
            ("assistant", "later answer"),
        ]));
        assert_eq!(d, "U: real question\nA: later answer");
    }

    #[test]
    fn last_compaction_summary_replaces_earlier_turns() {
        let d = render(&rows(&[
            ("user", "first ask"),
            ("assistant", "first reply"),
            (COMPACT_SUMMARY_ROLE, "Summary one"),
            ("user", "second ask"),
            (COMPACT_SUMMARY_ROLE, "Summary two"),
            ("user", "third ask"),
            ("assistant", "third reply"),
        ]));
        assert_eq!(
            d,
            "[compacted]\nSummary two\n\nU: third ask\nA: third reply"
        );
    }

    #[test]
    fn empty_when_no_prompts() {
        assert_eq!(render(&rows(&[("assistant", "orphan reply")])), "");
    }
}
