use crate::rewrite::{has_heredoc, has_unquoted_shell_meta, normalize_argv0, strip_command_prefix};
use engraph_core::{
    db,
    models::EventKind,
    telemetry::{self, EventInput},
};

/// "Looks like a single-symbol lookup": bareword identifier of length >= 3,
/// no regex metachars. Common short tokens (`id`, `if`) and any regex
/// (`.+*?()[]{}|\^$`) pass through silently.
fn is_symbol_lookup(pattern: &str) -> bool {
    if pattern.len() < 3 {
        return false;
    }
    let mut chars = pattern.chars();
    let first = match chars.next() {
        Some(c) => c,
        None => return false,
    };
    if !(first.is_ascii_alphabetic() || first == '_') {
        return false;
    }
    chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// Shared subgraph-redirect message generator. Returns Some(reason) when the
/// pattern is a bareword resolving to 1-3 entities in the codegraph; None
/// otherwise (not a symbol shape, 0 matches, or 4+ matches — ambiguous).
/// The LIMIT 4 inner scan caps work and aligns with the 8-cap in
/// `resolve_matches` (subgraph.rs:84).
pub(crate) fn try_subgraph_redirect(pattern: &str, conn: &db::PooledConn) -> Option<String> {
    if !is_symbol_lookup(pattern) {
        return None;
    }
    let count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM (SELECT 1 FROM entities WHERE name = ?1 OR id = ?1 LIMIT 4)",
            rusqlite::params![pattern],
            |r| r.get(0),
        )
        .ok()?;
    if !(1..=3).contains(&count) {
        return None;
    }
    let plural = if count == 1 { "" } else { "es" };
    Some(format!(
        "`{pattern}` is indexed in the engraph code graph ({count} match{plural}). \
         Run `engraph subgraph {pattern}` for a 2-hop neighborhood (calls, callers, \
         siblings) instead of grepping. If you still need a raw search, add a regex \
         metachar (e.g. `{pattern}\\b`) to bypass this redirect."
    ))
}

/// `rg`/`grep <bareword>` on an indexed symbol takes precedence over the
/// compression rewrite. Reuses the same parser as `try_auto_rewrite` so
/// `/usr/bin/rg foo` and `FOO=bar rg foo` get caught too. Skips compounds
/// and heredocs — the existing rewrite path handles those.
pub(crate) fn try_subgraph_redirect_for_bash(
    command: &str,
    conn: &db::PooledConn,
) -> Option<String> {
    if has_heredoc(command) || has_unquoted_shell_meta(command) {
        return None;
    }
    let mut argv = shell_words::split(command).ok()?;
    let _prefix = strip_command_prefix(&mut argv)?;
    if argv.is_empty() {
        return None;
    }
    normalize_argv0(&mut argv);
    if argv[0] != "rg" && argv[0] != "grep" {
        return None;
    }
    // First non-flag token is the pattern. Handles `rg -i foo`, `grep -r foo .`.
    let pattern = argv.iter().skip(1).find(|a| !a.starts_with('-'))?;
    try_subgraph_redirect(pattern, conn)
}

pub(crate) fn emit_subgraph_deny(conn: &db::PooledConn, reason: &str, feature: &'static str) {
    let decision = serde_json::json!({
        "hookSpecificOutput": {
            "hookEventName": "PreToolUse",
            "permissionDecision": "deny",
            "permissionDecisionReason": reason
        }
    });
    println!("{decision}");
    let session_id = std::env::var("CLAUDE_SESSION_ID").ok();
    telemetry::record_event(
        conn,
        EventInput {
            session_id: session_id.as_deref(),
            kind: EventKind::Hook,
            feature,
            filter_id: Some("subgraph-redirect"),
            input_tokens: 0,
            output_tokens: 0,
            latency_ms: 0,
        },
    )
    .ok();
}
