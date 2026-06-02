//! FTS5-backed retrieval with mempalace-style hierarchical scoping and a
//! lightweight knowledge graph layer. Embeddings + hybrid scoring are
//! reserved for Phase 6 behind a Cargo feature.

use engraph_core::{Result, db::PooledConn};
use serde::Serialize;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum Target {
    Messages,
    ContextItems,
    Bugs,
    Entities,
}

#[derive(Debug, Clone)]
pub enum ScopeFilter {
    All,
    Project(String),
    Scope(String),
}

#[derive(Debug, Clone, Copy, Default)]
pub enum Strategy {
    #[default]
    Fts,
    /// Hybrid (BM25 + cosine similarity + recency). Requires the `embeddings`
    /// feature. Falls back to FTS when no embeddings are stored for candidates.
    #[cfg(feature = "embeddings")]
    Hybrid,
}

#[derive(Debug, Clone)]
pub struct Query<'a> {
    pub text: &'a str,
    pub scope: ScopeFilter,
    pub kinds: &'a [Target],
    pub limit: usize,
    pub strategy: Strategy,
}

impl<'a> Query<'a> {
    pub fn new(text: &'a str) -> Self {
        const DEFAULT_KINDS: &[Target] = &[Target::Messages, Target::ContextItems];
        Self {
            text,
            scope: ScopeFilter::All,
            kinds: DEFAULT_KINDS,
            limit: 10,
            strategy: Strategy::Fts,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct Hit {
    pub target_kind: String,
    pub target_id: String,
    pub session_id: Option<String>,
    pub score: f64,
    pub preview: String,
    pub ts: Option<String>,
}

#[cfg(feature = "embeddings")]
pub mod hybrid;
pub mod scope;

pub fn search(conn: &PooledConn, q: &Query<'_>) -> Result<Vec<Hit>> {
    let scope_ids = scope::resolve(conn, &q.scope)?;
    let mut hits: Vec<Hit> = Vec::new();
    let fts_query = sanitize_fts(q.text);

    for kind in q.kinds {
        match kind {
            Target::Messages => {
                hits.extend(search_messages(
                    conn,
                    &fts_query,
                    scope_ids.as_deref(),
                    q.limit,
                )?);
            }
            Target::ContextItems => {
                hits.extend(search_context_items(
                    conn,
                    &fts_query,
                    scope_ids.as_deref(),
                    q.limit,
                )?);
            }
            Target::Bugs => {
                hits.extend(search_bugs(conn, &q.text.to_lowercase(), q.limit)?);
            }
            Target::Entities => {
                hits.extend(search_entities(conn, &q.text.to_lowercase(), q.limit)?);
            }
        }
    }
    // Stable sort by score desc, then ts desc, then target_id asc.
    hits.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| b.ts.cmp(&a.ts))
            .then_with(|| a.target_id.cmp(&b.target_id))
    });
    hits.truncate(q.limit);
    Ok(hits)
}

fn search_messages(
    conn: &PooledConn,
    fts_q: &str,
    scope_ids: Option<&[String]>,
    limit: usize,
) -> Result<Vec<Hit>> {
    use rusqlite::types::Value;
    let base = "
        SELECT m.id, m.session_id, m.content, m.ts, bm25(messages_fts) AS rank
        FROM messages_fts
        JOIN messages m ON m.rowid = messages_fts.rowid
    ";
    let limit_val = Value::Integer(limit as i64);
    let (sql, params) = match scope_ids {
        None => (
            format!("{base} WHERE messages_fts MATCH ?1 ORDER BY rank LIMIT ?2"),
            vec![Value::Text(fts_q.to_string()), limit_val],
        ),
        Some([]) => return Ok(vec![]),
        Some(ids) => {
            let placeholders = (1..=ids.len())
                .map(|i| format!("?{i}"))
                .collect::<Vec<_>>()
                .join(",");
            let q = format!(
                "{base} JOIN scope_members sm ON sm.target_kind = 'message' AND sm.target_id = m.id
                 WHERE messages_fts MATCH ?{q_idx} AND sm.scope_id IN ({placeholders})
                 ORDER BY rank LIMIT ?{lim_idx}",
                q_idx = ids.len() + 1,
                lim_idx = ids.len() + 2,
            );
            let mut p: Vec<Value> = ids.iter().map(|i| Value::Text(i.clone())).collect();
            p.push(Value::Text(fts_q.to_string()));
            p.push(limit_val);
            (q, p)
        }
    };
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt
        .query_map(rusqlite::params_from_iter(params), |r| {
            let content: String = r.get(2)?;
            let rank: f64 = r.get(4)?;
            Ok(Hit {
                target_kind: "message".to_string(),
                target_id: r.get(0)?,
                session_id: r.get(1)?,
                score: -rank, // bm25 is negative-good; flip for ranking
                preview: preview(&content, 240),
                ts: r.get(3)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

fn search_context_items(
    conn: &PooledConn,
    fts_q: &str,
    scope_ids: Option<&[String]>,
    limit: usize,
) -> Result<Vec<Hit>> {
    use rusqlite::types::Value;
    let base = "
        SELECT c.id, c.session_id, c.content, c.ts, bm25(context_items_fts) AS rank
        FROM context_items_fts
        JOIN context_items c ON c.rowid = context_items_fts.rowid
    ";
    let limit_val = Value::Integer(limit as i64);
    let (sql, params) = match scope_ids {
        None => (
            format!("{base} WHERE context_items_fts MATCH ?1 ORDER BY rank LIMIT ?2"),
            vec![Value::Text(fts_q.to_string()), limit_val],
        ),
        Some([]) => return Ok(vec![]),
        Some(ids) => {
            let placeholders = (1..=ids.len())
                .map(|i| format!("?{i}"))
                .collect::<Vec<_>>()
                .join(",");
            let q = format!(
                "{base} JOIN scope_members sm ON sm.target_kind = 'context_item' AND sm.target_id = c.id
                 WHERE context_items_fts MATCH ?{q_idx} AND sm.scope_id IN ({placeholders})
                 ORDER BY rank LIMIT ?{lim_idx}",
                q_idx = ids.len() + 1,
                lim_idx = ids.len() + 2,
            );
            let mut p: Vec<Value> = ids.iter().map(|i| Value::Text(i.clone())).collect();
            p.push(Value::Text(fts_q.to_string()));
            p.push(limit_val);
            (q, p)
        }
    };
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt
        .query_map(rusqlite::params_from_iter(params), |r| {
            let content: String = r.get(2)?;
            let rank: f64 = r.get(4)?;
            Ok(Hit {
                target_kind: "context_item".to_string(),
                target_id: r.get(0)?,
                session_id: r.get(1)?,
                score: -rank,
                preview: preview(&content, 240),
                ts: r.get(3)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

fn search_bugs(conn: &PooledConn, q_lower: &str, limit: usize) -> Result<Vec<Hit>> {
    let like = format!("%{q_lower}%");
    let mut stmt = conn.prepare(
        "SELECT id, project, summary, content, ts FROM bugs
         WHERE LOWER(summary) LIKE ?1 OR LOWER(COALESCE(content,'')) LIKE ?1
         ORDER BY ts DESC LIMIT ?2",
    )?;
    let rows = stmt
        .query_map(rusqlite::params![like, limit as i64], |r| {
            let summary: String = r.get(2)?;
            let content: Option<String> = r.get(3)?;
            let preview_text = content.as_deref().unwrap_or(&summary);
            Ok(Hit {
                target_kind: "bug".to_string(),
                target_id: r.get(0)?,
                session_id: None,
                score: 1.0,
                preview: preview(preview_text, 240),
                ts: r.get(4)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

fn search_entities(conn: &PooledConn, q_lower: &str, limit: usize) -> Result<Vec<Hit>> {
    let like = format!("%{q_lower}%");
    let mut stmt = conn.prepare(
        "SELECT id, kind, name, project, created_at FROM entities
         WHERE LOWER(name) LIKE ?1
         ORDER BY created_at DESC LIMIT ?2",
    )?;
    let rows = stmt
        .query_map(rusqlite::params![like, limit as i64], |r| {
            let kind: String = r.get(1)?;
            let name: String = r.get(2)?;
            let project: Option<String> = r.get(3)?;
            let preview = format!(
                "{kind}: {name}{}",
                project.map(|p| format!(" [{p}]")).unwrap_or_default()
            );
            Ok(Hit {
                target_kind: "entity".to_string(),
                target_id: r.get(0)?,
                session_id: None,
                score: 1.0,
                preview,
                ts: r.get(4)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

fn preview(content: &str, max_chars: usize) -> String {
    let trimmed: String = content.chars().take(max_chars).collect();
    if content.chars().count() > max_chars {
        format!("{trimmed}…")
    } else {
        trimmed
    }
}

/// Sanitize FTS5 query: drop characters that have special meaning in MATCH
/// expressions to avoid syntax errors on user-supplied text.
fn sanitize_fts(s: &str) -> String {
    let cleaned: String = s
        .chars()
        .map(|c| match c {
            '"' | '*' | '(' | ')' | ':' => ' ',
            _ => c,
        })
        .collect();
    let words: Vec<&str> = cleaned.split_whitespace().collect();
    if words.is_empty() {
        return "\"\"".to_string();
    }
    // Quote each word for safe phrase matching; AND them implicitly.
    words
        .iter()
        .map(|w| format!("\"{w}\""))
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_quotes_words() {
        assert_eq!(sanitize_fts("hello world"), "\"hello\" \"world\"");
        assert_eq!(sanitize_fts(""), "\"\"");
        assert_eq!(sanitize_fts(r#"foo "bar" *baz"#), "\"foo\" \"bar\" \"baz\"");
    }
}
