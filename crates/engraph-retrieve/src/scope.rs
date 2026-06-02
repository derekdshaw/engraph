//! Hierarchical scoping: resolve a ScopeFilter to the set of scope IDs whose
//! members should be included in a search. Returns `None` for unscoped queries.

use crate::ScopeFilter;
use engraph_core::{Result, db::PooledConn};
use uuid::Uuid;

pub fn resolve(conn: &PooledConn, f: &ScopeFilter) -> Result<Option<Vec<String>>> {
    match f {
        ScopeFilter::All => Ok(None),
        ScopeFilter::Project(name) => Ok(Some(ids_by_project(conn, name)?)),
        ScopeFilter::Scope(id) => Ok(Some(vec![id.clone()])),
    }
}

fn ids_by_project(conn: &PooledConn, name: &str) -> Result<Vec<String>> {
    let mut stmt = conn
        .prepare("SELECT id FROM scopes WHERE kind = 'project' AND name = ?1 AND archived = 0")?;
    let rows = stmt
        .query_map([name], |r| r.get::<_, String>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

/// Ensure a scope row exists for a project name; return its id (existing or new).
pub fn ensure_project_scope(conn: &PooledConn, project: &str) -> Result<String> {
    if let Ok(existing) = conn.query_row(
        "SELECT id FROM scopes WHERE kind = 'project' AND name = ?1 AND archived = 0",
        [project],
        |r| r.get::<_, String>(0),
    ) {
        return Ok(existing);
    }
    let id = Uuid::now_v7().to_string();
    conn.execute(
        "INSERT INTO scopes (id, parent_id, kind, name) VALUES (?1, NULL, 'project', ?2)",
        rusqlite::params![id, project],
    )?;
    Ok(id)
}

pub fn add_member(
    conn: &PooledConn,
    scope_id: &str,
    target_kind: &str,
    target_id: &str,
) -> Result<()> {
    conn.execute(
        "INSERT OR IGNORE INTO scope_members (scope_id, target_kind, target_id) VALUES (?1, ?2, ?3)",
        rusqlite::params![scope_id, target_kind, target_id],
    )?;
    Ok(())
}
