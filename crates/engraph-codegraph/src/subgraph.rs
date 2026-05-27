//! 2-hop neighborhood query + markdown formatter.
//!
//! The output is intentionally terse — ~8KB ceiling matches the F2 goal of
//! ~100x compression vs an exploratory Read+grep loop. If the symbol name is
//! ambiguous we emit a disambiguation block instead of guessing.

use anyhow::Result;
use engraph_core::db::PooledConn;
use serde::Serialize;
use std::fmt::Write;

#[derive(Debug, Clone, Serialize)]
pub struct EntityRow {
    pub id: String,
    pub name: String,
    pub project: Option<String>,
    pub file_path: Option<String>,
    pub line_range: Option<String>,
    pub signature: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct EdgeRow {
    pub kind: String,
    pub other: EntityRow,
}

#[derive(Debug, Clone, Serialize, Default)]
pub struct Neighborhood {
    /// Always populated. If `matches.len() > 1` the request is ambiguous and
    /// no edges are returned — the caller renders a disambiguation block.
    pub matches: Vec<EntityRow>,
    pub outgoing: Vec<EdgeRow>,
    pub incoming: Vec<EdgeRow>,
    pub siblings: Vec<EntityRow>,
}

impl Neighborhood {
    pub fn is_ambiguous(&self) -> bool {
        self.matches.len() > 1
    }
    pub fn is_empty(&self) -> bool {
        self.matches.is_empty()
    }
}

pub const DEFAULT_BYTE_CAP: usize = 8192;
pub const DEFAULT_SIBLINGS_LIMIT: usize = 10;

pub fn subgraph_for(conn: &PooledConn, symbol: &str, max_nodes: usize) -> Result<Neighborhood> {
    let matches = resolve_matches(conn, symbol)?;
    if matches.len() != 1 {
        return Ok(Neighborhood {
            matches,
            ..Default::default()
        });
    }
    let target = &matches[0];
    let half = max_nodes / 2;

    let outgoing = query_edges(conn, &target.id, EdgeDir::Out, half.max(1))?;
    let remaining_for_in = max_nodes.saturating_sub(outgoing.len()).max(1);
    let incoming = query_edges(conn, &target.id, EdgeDir::In, remaining_for_in)?;
    let siblings = match target.file_path.as_deref() {
        Some(path) => query_siblings(conn, path, &target.id, DEFAULT_SIBLINGS_LIMIT)?,
        None => Vec::new(),
    };

    Ok(Neighborhood {
        matches,
        outgoing,
        incoming,
        siblings,
    })
}

fn resolve_matches(conn: &PooledConn, symbol: &str) -> Result<Vec<EntityRow>> {
    // Match by name first (the common case), then by exact moniker. Limit to
    // 8 — more than that is a sign the user needs a more specific identifier.
    let mut stmt = conn.prepare(
        "SELECT id, name, project, file_path, line_range, signature FROM entities
         WHERE name = ?1 OR id = ?1
         ORDER BY (CASE WHEN id = ?1 THEN 0 ELSE 1 END)
         LIMIT 8",
    )?;
    let rows = stmt
        .query_map([symbol], row_to_entity)?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

#[derive(Copy, Clone)]
enum EdgeDir {
    Out,
    In,
}

fn query_edges(
    conn: &PooledConn,
    sym_id: &str,
    dir: EdgeDir,
    limit: usize,
) -> Result<Vec<EdgeRow>> {
    let sql = match dir {
        EdgeDir::Out => {
            "SELECT r.kind, e.id, e.name, e.project, e.file_path, e.line_range, e.signature
             FROM relations r
             JOIN entities e ON e.id = r.dst_entity
             WHERE r.src_entity = ?1 AND r.valid_to IS NULL
             ORDER BY r.kind, e.name
             LIMIT ?2"
        }
        EdgeDir::In => {
            "SELECT r.kind, e.id, e.name, e.project, e.file_path, e.line_range, e.signature
             FROM relations r
             JOIN entities e ON e.id = r.src_entity
             WHERE r.dst_entity = ?1 AND r.valid_to IS NULL
             ORDER BY r.kind, e.name
             LIMIT ?2"
        }
    };
    let mut stmt = conn.prepare(sql)?;
    let rows = stmt
        .query_map(rusqlite::params![sym_id, limit as i64], |r| {
            Ok(EdgeRow {
                kind: r.get::<_, String>(0)?,
                other: EntityRow {
                    id: r.get(1)?,
                    name: r.get(2)?,
                    project: r.get(3)?,
                    file_path: r.get(4)?,
                    line_range: r.get(5)?,
                    signature: r.get(6)?,
                },
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

/// All entities defined in `file_path`, ordered by line. Used by the
/// PostToolUse(Read) augment to enrich Claude's view of a just-read file
/// with the indexed-symbol map.
pub fn entities_in_file(
    conn: &PooledConn,
    file_path: &str,
    limit: usize,
) -> Result<Vec<EntityRow>> {
    let mut stmt = conn.prepare(
        "SELECT id, name, project, file_path, line_range, signature FROM entities
         WHERE file_path = ?1
         ORDER BY line_range
         LIMIT ?2",
    )?;
    let rows = stmt
        .query_map(rusqlite::params![file_path, limit as i64], row_to_entity)?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

fn query_siblings(
    conn: &PooledConn,
    file_path: &str,
    self_id: &str,
    limit: usize,
) -> Result<Vec<EntityRow>> {
    let mut stmt = conn.prepare(
        "SELECT id, name, project, file_path, line_range, signature FROM entities
         WHERE file_path = ?1 AND id != ?2
         ORDER BY line_range
         LIMIT ?3",
    )?;
    let rows = stmt
        .query_map(
            rusqlite::params![file_path, self_id, limit as i64],
            row_to_entity,
        )?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

fn row_to_entity(r: &rusqlite::Row<'_>) -> rusqlite::Result<EntityRow> {
    Ok(EntityRow {
        id: r.get(0)?,
        name: r.get(1)?,
        project: r.get(2)?,
        file_path: r.get(3)?,
        line_range: r.get(4)?,
        signature: r.get(5)?,
    })
}

pub fn format_markdown(n: &Neighborhood, byte_cap: usize) -> String {
    let mut out = String::new();
    if n.is_empty() {
        out.push_str("No symbols matched.\n");
        return out;
    }
    if n.is_ambiguous() {
        out.push_str("## Multiple matches\n");
        for m in &n.matches {
            push_capped(
                &mut out,
                &format!(
                    "- `{}` ({})\n",
                    m.name,
                    location(m, None).unwrap_or_else(|| "?".into())
                ),
                byte_cap,
            );
        }
        return out;
    }
    let head = &n.matches[0];
    let head_project = head.project.as_deref();
    let head_loc = location(head, None).unwrap_or_else(|| "?".into());
    push_capped(
        &mut out,
        &format!("## Symbol `{}` (defined in {})\n", head.name, head_loc),
        byte_cap,
    );
    if let Some(sig) = head.signature.as_deref() {
        if !sig.is_empty() {
            push_capped(&mut out, &format!("```\n{sig}\n```\n"), byte_cap);
        }
    }

    let mut calls = Vec::new();
    let mut refs = Vec::new();
    let mut imports = Vec::new();
    let mut bazel_deps = Vec::new();
    let mut seen_out: std::collections::HashSet<(&str, &str)> = std::collections::HashSet::new();
    for e in &n.outgoing {
        if e.other.name.is_empty() {
            continue;
        }
        if !seen_out.insert((e.kind.as_str(), e.other.id.as_str())) {
            continue;
        }
        match e.kind.as_str() {
            "CALLS" => calls.push(e),
            "IMPORTS" => imports.push(e),
            "BAZEL_DEPENDS_ON" => bazel_deps.push(e),
            _ => refs.push(e),
        }
    }
    if !calls.is_empty() {
        push_capped(
            &mut out,
            &format!("**Calls**: {}\n", join_short(&calls, head_project)),
            byte_cap,
        );
    }
    if !refs.is_empty() {
        push_capped(
            &mut out,
            &format!("**References**: {}\n", join_short(&refs, head_project)),
            byte_cap,
        );
    }
    if !bazel_deps.is_empty() {
        push_capped(
            &mut out,
            &format!(
                "**Bazel deps**: {}\n",
                join_short(&bazel_deps, head_project)
            ),
            byte_cap,
        );
    }
    if !n.incoming.is_empty() {
        let mut seen_in: std::collections::HashSet<&str> = std::collections::HashSet::new();
        let inc: Vec<_> = n
            .incoming
            .iter()
            .filter(|e| !e.other.name.is_empty() && seen_in.insert(e.other.id.as_str()))
            .collect();
        if !inc.is_empty() {
            push_capped(
                &mut out,
                &format!("**Called by**: {}\n", join_short(&inc, head_project)),
                byte_cap,
            );
        }
    }
    if !n.siblings.is_empty() {
        let names: Vec<String> = n
            .siblings
            .iter()
            .filter(|e| !e.name.is_empty())
            .map(|e| format!("`{}`", e.name))
            .collect();
        push_capped(
            &mut out,
            &format!("**Sibling symbols** (same file): {}\n", names.join(", ")),
            byte_cap,
        );
    }
    if !imports.is_empty() {
        let names: Vec<String> = imports
            .iter()
            .map(|e| format!("`{}`", e.other.name))
            .collect();
        push_capped(
            &mut out,
            &format!("**Imports**: {}\n", names.join(", ")),
            byte_cap,
        );
    }
    out
}

/// Render a "file_path:line" location for an entity. When `head_project` is
/// passed and the entity belongs to a *different* project, prepends
/// "repo:<basename> :: " so cross-repo edges are visually distinct in the
/// subgraph markdown.
fn location(e: &EntityRow, head_project: Option<&str>) -> Option<String> {
    let path = e.file_path.as_deref()?;
    let line = e.line_range.as_deref();
    let line_head = line.and_then(|s| s.split(':').next());
    let base = match line_head {
        Some(l) if !l.is_empty() => format!("{path}:{l}"),
        _ => path.to_string(),
    };
    let foreign = match (head_project, e.project.as_deref()) {
        (Some(h), Some(p)) if h != p => Some(p),
        _ => None,
    };
    Some(match foreign {
        Some(other) => {
            let repo = std::path::Path::new(other)
                .file_name()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_else(|| other.to_string());
            format!("repo:{repo} :: {base}")
        }
        None => base,
    })
}

fn join_short(edges: &[&EdgeRow], head_project: Option<&str>) -> String {
    edges
        .iter()
        .map(|e| {
            let loc = location(&e.other, head_project).unwrap_or_default();
            if loc.is_empty() {
                format!("`{}`", e.other.name)
            } else {
                format!("`{}` ({loc})", e.other.name)
            }
        })
        .collect::<Vec<_>>()
        .join(", ")
}

fn push_capped(out: &mut String, s: &str, cap: usize) {
    if out.len() >= cap {
        return;
    }
    let room = cap - out.len();
    if s.len() <= room {
        out.push_str(s);
    } else {
        // Cut at a char boundary.
        let mut end = room;
        while end > 0 && !s.is_char_boundary(end) {
            end -= 1;
        }
        let _ = write!(out, "{}\n…[truncated]\n", &s[..end]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use engraph_core::db::open_pool;
    use tempfile::tempdir;

    fn seed(conn: &PooledConn) {
        conn.execute(
            "INSERT INTO entities (id, kind, name, project, file_path, line_range, signature)
             VALUES ('e_foo', 'symbol', 'foo', '/p', 'src/lib.rs', '10:20', 'fn foo() -> i32')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO entities (id, kind, name, project, file_path, line_range)
             VALUES ('e_bar', 'symbol', 'bar', '/p', 'src/lib.rs', '30:35')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO entities (id, kind, name, project, file_path, line_range)
             VALUES ('e_baz', 'symbol', 'baz', '/p', 'src/other.rs', '5:5')",
            [],
        )
        .unwrap();
        // foo CALLS bar, baz CALLS foo
        conn.execute(
            "INSERT INTO relations (id, src_entity, dst_entity, kind, provenance)
             VALUES ('r1', 'e_foo', 'e_bar', 'CALLS', 'extracted')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO relations (id, src_entity, dst_entity, kind, provenance)
             VALUES ('r2', 'e_baz', 'e_foo', 'CALLS', 'extracted')",
            [],
        )
        .unwrap();
    }

    #[test]
    fn subgraph_returns_calls_called_by_and_siblings() {
        let dir = tempdir().unwrap();
        let pool = open_pool(&dir.path().join("t.db")).unwrap();
        let conn = pool.get().unwrap();
        seed(&conn);

        let n = subgraph_for(&conn, "foo", 30).unwrap();
        assert_eq!(n.matches.len(), 1);
        assert_eq!(n.outgoing.len(), 1);
        assert_eq!(n.outgoing[0].other.name, "bar");
        assert_eq!(n.incoming.len(), 1);
        assert_eq!(n.incoming[0].other.name, "baz");
        assert_eq!(n.siblings.len(), 1);
        assert_eq!(n.siblings[0].name, "bar");

        let md = format_markdown(&n, DEFAULT_BYTE_CAP);
        assert!(md.contains("## Symbol `foo`"), "{md}");
        assert!(md.contains("**Calls**: `bar`"), "{md}");
        assert!(md.contains("**Called by**: `baz`"), "{md}");
        assert!(
            md.contains("**Sibling symbols** (same file): `bar`"),
            "{md}"
        );
        assert!(md.contains("fn foo() -> i32"), "signature missing: {md}");
    }

    #[test]
    fn ambiguous_name_emits_disambiguation() {
        let dir = tempdir().unwrap();
        let pool = open_pool(&dir.path().join("t.db")).unwrap();
        let conn = pool.get().unwrap();
        conn.execute(
            "INSERT INTO entities (id, kind, name, project, file_path, line_range)
             VALUES ('e1', 'symbol', 'dup', '/p', 'a.rs', '1:1'),
                    ('e2', 'symbol', 'dup', '/p', 'b.rs', '2:2')",
            [],
        )
        .unwrap();
        let n = subgraph_for(&conn, "dup", 30).unwrap();
        assert!(n.is_ambiguous());
        let md = format_markdown(&n, DEFAULT_BYTE_CAP);
        assert!(md.starts_with("## Multiple matches"));
        assert!(md.contains("a.rs"));
        assert!(md.contains("b.rs"));
    }

    #[test]
    fn cross_repo_edge_gets_repo_annotation() {
        let dir = tempdir().unwrap();
        let pool = open_pool(&dir.path().join("t.db")).unwrap();
        let conn = pool.get().unwrap();
        conn.execute(
            "INSERT INTO entities (id, kind, name, project, file_path, line_range, signature)
             VALUES ('e_caller', 'symbol', 'caller', '/proj/app_b', 'src/main.rs', '5:5', 'fn caller()')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO entities (id, kind, name, project, file_path, line_range)
             VALUES ('e_lib_foo', 'symbol', 'lib_foo', '/proj/lib_a', 'src/lib.rs', '10:10')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO relations (id, src_entity, dst_entity, kind, provenance)
             VALUES ('r1', 'e_caller', 'e_lib_foo', 'CALLS', 'extracted')",
            [],
        )
        .unwrap();
        let n = subgraph_for(&conn, "caller", 30).unwrap();
        let md = format_markdown(&n, DEFAULT_BYTE_CAP);
        assert!(
            md.contains("repo:lib_a"),
            "missing cross-repo annotation: {md}"
        );
        assert!(md.contains("`lib_foo`"), "{md}");

        // Same-project edge should NOT carry the annotation.
        conn.execute(
            "INSERT INTO entities (id, kind, name, project, file_path, line_range)
             VALUES ('e_local', 'symbol', 'local_helper', '/proj/app_b', 'src/main.rs', '20:20')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO relations (id, src_entity, dst_entity, kind, provenance)
             VALUES ('r2', 'e_caller', 'e_local', 'CALLS', 'extracted')",
            [],
        )
        .unwrap();
        let n = subgraph_for(&conn, "caller", 30).unwrap();
        let md = format_markdown(&n, DEFAULT_BYTE_CAP);
        // local_helper is same-project → no repo: prefix on its location.
        assert!(
            !md.contains("repo:app_b"),
            "same-project edge should not carry annotation: {md}"
        );
    }

    #[test]
    fn unknown_symbol_says_so() {
        let dir = tempdir().unwrap();
        let pool = open_pool(&dir.path().join("t.db")).unwrap();
        let conn = pool.get().unwrap();
        let n = subgraph_for(&conn, "missing", 30).unwrap();
        assert!(n.is_empty());
        let md = format_markdown(&n, DEFAULT_BYTE_CAP);
        assert!(md.contains("No symbols matched"));
    }

    #[test]
    fn byte_cap_truncates_with_marker() {
        let dir = tempdir().unwrap();
        let pool = open_pool(&dir.path().join("t.db")).unwrap();
        let conn = pool.get().unwrap();
        seed(&conn);
        let n = subgraph_for(&conn, "foo", 30).unwrap();
        let md = format_markdown(&n, 30);
        assert!(md.len() <= 60, "got {} bytes: {md}", md.len());
        // Either truncated mid-section or stopped before adding more.
        assert!(md.contains("foo"));
    }
}
