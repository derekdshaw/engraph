use crate::rewrite::{has_heredoc, has_unquoted_shell_meta, normalize_argv0, strip_command_prefix};
use engraph_core::{
    db,
    models::EventKind,
    telemetry::{self, EventInput},
};
use std::path::Path;

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
    s.len() >= 3 && is_ident(s)
}

/// An identifier of any length: a leading segment of a path (`io` in
/// `io::Read`) may be short.
fn is_ident(s: &str) -> bool {
    !s.is_empty()
        && s.chars()
            .next()
            .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// What a grep pattern asks for, as the code graph can answer it.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct SymbolQuery {
    /// The last path segment: the symbol's own name.
    pub(crate) name: String,
    /// For a path shape (`Config::new`, `Config#new`, `Config.new`): the
    /// whole path in `qualified_name` form (`Config.new`). Matched first, so
    /// a qualified search finds one symbol when the bare name has many.
    pub(crate) qualified: Option<String>,
    /// The shape as typed, for the `engraph subgraph …` suggestion.
    pub(crate) shown: String,
}

impl SymbolQuery {
    fn bare(name: &str) -> Self {
        Self {
            name: name.to_string(),
            qualified: None,
            shown: name.to_string(),
        }
    }
}

/// A path of identifiers joined by `::`, `#`, or `.` (`Config::new`,
/// `Config#new`, `pkg.Config.new`), with at least two segments and a bareword
/// last segment. `None` for anything else, including a lone bareword.
fn symbol_path(s: &str) -> Option<SymbolQuery> {
    let segments: Vec<&str> = s
        .split("::")
        .flat_map(|part| part.split(['#', '.']))
        .collect();
    if segments.len() < 2 || !segments.iter().all(|seg| is_ident(seg)) {
        return None;
    }
    let last = *segments.last()?;
    if !is_bareword(last) {
        return None;
    }
    Some(SymbolQuery {
        name: last.to_string(),
        qualified: Some(segments.join(".")),
        shown: s.to_string(),
    })
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
/// - `Foo::new` / `Foo#new` / `Foo.new` → the path (`Foo.new`) and its last
///   segment (`new`)
///
/// Returns `None` for multi-word free text (`error handling`) and regexes, so
/// those still grep normally.
fn extract_symbol(pattern: &str) -> Option<SymbolQuery> {
    let p = pattern.trim();
    // Path shape first: its `.` is a separator, not a regex metachar.
    if let Some(q) = symbol_path(p) {
        return Some(q);
    }
    // A regex metachar is the documented "I really want grep" bypass, so it
    // opts out in every shape — the keyword shape below would otherwise read
    // `fn foo\b` as `foo`. Parens stay allowed for the call shape.
    if p.contains(['\\', '.', '*', '+', '?', '[', ']', '{', '}', '^', '$', '|']) {
        return None;
    }
    if is_bareword(p) {
        return Some(SymbolQuery::bare(p));
    }
    // Call shape: `name(` / `Foo::new(` — what stands before the first paren.
    if let Some(idx) = p.find('(') {
        let head = p[..idx].trim();
        if is_bareword(head) {
            return Some(SymbolQuery::bare(head));
        }
        if let Some(q) = symbol_path(head) {
            return Some(q);
        }
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
        return first_ident(rest).map(SymbolQuery::bare);
    }
    None
}

/// `(project, file_path)` of the entities a lookup names, at most 9 (the
/// gate only needs to know whether there are more than 8). Same rule as
/// `engraph subgraph` (`MATCH_WHERE_SQL`): by name, by exact moniker, or by
/// qualified name, with global symbols shadowing locals.
fn entities_matching(
    conn: &db::PooledConn,
    symbol: &str,
    qualified: &str,
) -> Option<Vec<(Option<String>, Option<String>)>> {
    let sql = format!(
        "SELECT project, file_path FROM entities WHERE {} LIMIT 9",
        engraph_codegraph::MATCH_WHERE_SQL
    );
    let mut stmt = conn.prepare(&sql).ok()?;
    stmt.query_map(rusqlite::params![symbol, qualified], |r| {
        Ok((
            r.get::<_, Option<String>>(0)?,
            r.get::<_, Option<String>>(1)?,
        ))
    })
    .ok()?
    .collect::<rusqlite::Result<Vec<_>>>()
    .ok()
}

/// Shared subgraph-redirect message generator. Returns Some(reason) when the
/// pattern resolves to a symbol indexed with 1-8 matches in the codegraph; None
/// otherwise (not a symbol shape, 0 matches, or 9+ matches — too ambiguous to
/// be a useful neighborhood). The `LIMIT 9` scan caps the count work. Also None
/// when the checkout at `cwd` has changed a matched symbol's file since the
/// index: the graph would show the indexed version, not this branch's.
pub(crate) fn try_subgraph_redirect(
    pattern: &str,
    conn: &db::PooledConn,
    cwd: Option<&Path>,
) -> Option<String> {
    let query = extract_symbol(pattern)?;
    // A path (`Config::new`) is matched as a whole against `qualified_name`
    // first; only when nothing has that qualified name does its last segment
    // count as a plain name lookup.
    let mut symbol = query.name.as_str();
    let mut rows = Vec::new();
    if let Some(qualified) = query.qualified.as_deref() {
        let normalized = engraph_codegraph::normalize_qualified(qualified);
        // The whole path is never a name (names have no `.`), so passing it
        // as both parameters leaves only the qualified-name arm to match.
        rows = entities_matching(conn, &normalized, &normalized)?;
        if !rows.is_empty() {
            symbol = query.shown.as_str();
        }
    }
    if rows.is_empty() {
        rows = entities_matching(conn, &query.name, &query.name)?;
    }
    let count = rows.len();
    if !(1..=8).contains(&count) {
        return None;
    }
    if let Some(cwd) = cwd
        && crate::divergence::any_changed_since_index(
            conn,
            cwd,
            rows.iter().map(|(p, f)| (p.as_deref(), f.as_deref())),
        )
    {
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
    cwd: Option<&Path>,
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
    try_subgraph_redirect(pattern, conn, cwd)
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
    use super::{SymbolQuery, extract_symbol};

    fn name(pattern: &str) -> Option<String> {
        extract_symbol(pattern).map(|q| q.name)
    }

    #[test]
    fn extracts_from_common_search_shapes() {
        assert_eq!(name("parse").as_deref(), Some("parse"));
        assert_eq!(name("fn parse").as_deref(), Some("parse"));
        assert_eq!(name("pub fn parse").as_deref(), Some("parse"));
        assert_eq!(name("struct Foo").as_deref(), Some("Foo"));
        assert_eq!(name("impl Widget").as_deref(), Some("Widget"));
        assert_eq!(name("parse(").as_deref(), Some("parse"));
        assert_eq!(name("parse(input: &str)").as_deref(), Some("parse"));
        assert_eq!(name("Foo::new").as_deref(), Some("new"));
        assert_eq!(extract_symbol("parse").unwrap().qualified, None);
    }

    #[test]
    fn path_shapes_keep_the_whole_path() {
        for (pattern, qualified) in [
            ("Config::new", "Config.new"),
            ("Config#new", "Config.new"),
            ("Config.new", "Config.new"),
            ("a::b::Config", "a.b.Config"),
            ("io::Read", "io.Read"),
            ("Config::new(", "Config.new"),
            ("pkg.Config#add_alias", "pkg.Config.add_alias"),
        ] {
            assert_eq!(
                extract_symbol(pattern),
                Some(SymbolQuery {
                    name: qualified.rsplit('.').next().unwrap().to_string(),
                    qualified: Some(qualified.to_string()),
                    shown: pattern.trim_end_matches('(').to_string(),
                }),
                "{pattern}"
            );
        }
        // A short last segment, a regex, and a trailing separator are not paths.
        assert_eq!(extract_symbol("Config::id"), None);
        assert_eq!(extract_symbol("Config.*"), None);
        assert_eq!(extract_symbol("Config.new.*"), None);
        assert_eq!(extract_symbol("Config::"), None);
        assert_eq!(extract_symbol("foo.bar\\b"), None);
    }

    #[test]
    fn rejects_freetext_and_regex_and_short() {
        // Multi-word natural language must keep grepping.
        assert_eq!(name("error handling"), None);
        assert_eq!(name("the quick brown"), None);
        // Regex / metachars aren't barewords.
        assert_eq!(name("parse.*"), None);
        assert_eq!(name("^foo$"), None);
        // The documented bypass (a regex metachar) must work in every shape.
        assert_eq!(name("fn ids_by_project\\b"), None);
        assert_eq!(name("pub fn parse.*"), None);
        assert_eq!(name("struct Foo\\s"), None);
        // Too short.
        assert_eq!(name("id"), None);
        assert_eq!(name("fn x"), None);
    }
}
