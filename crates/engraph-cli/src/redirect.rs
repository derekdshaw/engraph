use crate::rewrite::{has_heredoc, has_unquoted_shell_meta, normalize_argv0, strip_command_prefix};
use engraph_core::{
    db,
    models::EventKind,
    telemetry::{self, EventInput},
};

/// Definition/visibility keywords that prefix a symbol in a search, across the
/// languages engraph indexes. Stripped (one or more, e.g. `pub fn`) to reach the
/// symbol they introduce.
const SYMBOL_KEYWORDS: &[&str] = &[
    "fn",
    "pub",
    "struct",
    "enum",
    "trait",
    "impl",
    "type",
    "const",
    "static",
    "mod",
    "async",
    "unsafe",
    "class",
    "def",
    "func",
    "function",
    "interface",
    "export",
    "public",
    "private",
    "var",
    "let",
    "val",
];

/// A bareword identifier of length >= 3 (alpha/underscore start, alphanumeric
/// body). Short tokens (`id`, `if`) are rejected to avoid noise.
fn is_bareword(s: &str) -> bool {
    s.len() >= 3
        && s.chars()
            .next()
            .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// The leading identifier of `s` (e.g. `parse` from `parse(input: &str)`).
fn first_ident(s: &str) -> Option<&str> {
    let s = s.trim_start();
    let mut end = 0;
    for (i, c) in s.char_indices() {
        let ok = if i == 0 {
            c.is_ascii_alphabetic() || c == '_'
        } else {
            c.is_ascii_alphanumeric() || c == '_'
        };
        if ok {
            end = i + c.len_utf8();
        } else {
            break;
        }
    }
    (end >= 3).then_some(&s[..end])
}

/// Extract the symbol a grep pattern is really looking for, from the shapes a
/// code search actually takes — not just a lone bareword:
/// - `parse`                  → `parse`
/// - `fn parse` / `pub fn p`  → strip leading def keywords, take the name
/// - `parse(` / `parse(args)` → the callee before `(`
/// - `Foo::new`               → the last path segment
///
/// Returns `None` for multi-word free text (`error handling`) and regexes, so
/// those still grep normally.
fn extract_symbol(pattern: &str) -> Option<String> {
    let p = pattern.trim();
    if is_bareword(p) {
        return Some(p.to_string());
    }
    // Call shape: `name(` — the identifier immediately before the first paren.
    if let Some(idx) = p.find('(') {
        let head = p[..idx].trim();
        if is_bareword(head) {
            return Some(head.to_string());
        }
    }
    // Path shape: `Foo::new`, `a::b::c` — every segment a bareword; take the last.
    if p.contains("::") && p.split("::").all(is_bareword) {
        return p.rsplit("::").next().map(str::to_string);
    }
    // Keyword shape: strip leading def/visibility keywords, take the next ident.
    let mut rest = p;
    let mut stripped = false;
    while let Some((head, tail)) = rest.split_once(char::is_whitespace) {
        if SYMBOL_KEYWORDS.contains(&head) {
            rest = tail.trim_start();
            stripped = true;
        } else {
            break;
        }
    }
    if stripped {
        return first_ident(rest).map(str::to_string);
    }
    None
}

/// Shared subgraph-redirect message generator. Returns Some(reason) when the
/// pattern resolves to a symbol indexed with 1-8 matches in the codegraph; None
/// otherwise (not a symbol shape, 0 matches, or 9+ matches — too ambiguous to
/// be a useful neighborhood). The `LIMIT 9` inner scan caps the count work.
pub(crate) fn try_subgraph_redirect(pattern: &str, conn: &db::PooledConn) -> Option<String> {
    let symbol = extract_symbol(pattern)?;
    let count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM (SELECT 1 FROM entities WHERE name = ?1 OR id = ?1 LIMIT 9)",
            rusqlite::params![symbol],
            |r| r.get(0),
        )
        .ok()?;
    if !(1..=8).contains(&count) {
        return None;
    }
    let plural = if count == 1 { "" } else { "es" };
    Some(format!(
        "`{symbol}` is indexed in the engraph code graph ({count} match{plural}). \
         Run `engraph subgraph {symbol}` for a 2-hop neighborhood (calls, callers, \
         siblings) instead of grepping. If you still need a raw search, add a regex \
         metachar (e.g. `{symbol}\\b`) to bypass this redirect."
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

#[cfg(test)]
mod tests {
    use super::extract_symbol;

    #[test]
    fn extracts_from_common_search_shapes() {
        assert_eq!(extract_symbol("parse").as_deref(), Some("parse"));
        assert_eq!(extract_symbol("fn parse").as_deref(), Some("parse"));
        assert_eq!(extract_symbol("pub fn parse").as_deref(), Some("parse"));
        assert_eq!(extract_symbol("struct Foo").as_deref(), Some("Foo"));
        assert_eq!(extract_symbol("impl Widget").as_deref(), Some("Widget"));
        assert_eq!(extract_symbol("parse(").as_deref(), Some("parse"));
        assert_eq!(
            extract_symbol("parse(input: &str)").as_deref(),
            Some("parse")
        );
        assert_eq!(extract_symbol("Foo::new").as_deref(), Some("new"));
    }

    #[test]
    fn rejects_freetext_and_regex_and_short() {
        // Multi-word natural language must keep grepping.
        assert_eq!(extract_symbol("error handling"), None);
        assert_eq!(extract_symbol("the quick brown"), None);
        // Regex / metachars aren't barewords.
        assert_eq!(extract_symbol("parse.*"), None);
        assert_eq!(extract_symbol("^foo$"), None);
        // Too short.
        assert_eq!(extract_symbol("id"), None);
        assert_eq!(extract_symbol("fn x"), None);
    }
}
