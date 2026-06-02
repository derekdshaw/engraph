//! Curated-memory writers: the producer side of the do-not-repeat / bugs /
//! decisions tables that the SessionStart brief and `engraph recall` consume.
//! Mirrors mnemosyne's capture primitives (add_do_not_repeat / log_bug /
//! save_context) as plain inserts so the CLI can expose them as subcommands.
//! All rows are keyed by `project` (a cwd path string) so the brief's
//! per-project readers find them.

use crate::{Result, db::PooledConn};
use uuid::Uuid;

/// Record a do-not-repeat rule scoped to `project`. Returns the new row id.
pub fn add_do_not_repeat(conn: &PooledConn, project: &str, rule: &str) -> Result<String> {
    let id = Uuid::now_v7().to_string();
    conn.execute(
        "INSERT INTO do_not_repeat (id, project, rule) VALUES (?1, ?2, ?3)",
        rusqlite::params![id, project, rule],
    )?;
    Ok(id)
}

/// Log a bug (open by default). `content` is optional long-form detail (root
/// cause, repro, fix notes). Returns the new row id.
pub fn log_bug(
    conn: &PooledConn,
    project: &str,
    summary: &str,
    content: Option<&str>,
) -> Result<String> {
    let id = Uuid::now_v7().to_string();
    conn.execute(
        "INSERT INTO bugs (id, project, summary, content) VALUES (?1, ?2, ?3, ?4)",
        rusqlite::params![id, project, summary, content],
    )?;
    Ok(id)
}

/// Mark a bug resolved by id. Returns the number of rows changed (0 = no such
/// bug, which the caller can surface as an error).
pub fn resolve_bug(conn: &PooledConn, id: &str) -> Result<usize> {
    let n = conn.execute(
        "UPDATE bugs SET resolved = 1 WHERE id = ?1",
        rusqlite::params![id],
    )?;
    Ok(n)
}

/// Save a curated context item (`kind` = decision / architecture / convention /
/// performance) scoped to `project`. `session_id` is recorded for provenance
/// when available. Returns the new row id so the caller can add a project scope
/// member for recall parity with ingested messages.
pub fn save_context(
    conn: &PooledConn,
    project: &str,
    kind: &str,
    content: &str,
    session_id: Option<&str>,
) -> Result<String> {
    let id = Uuid::now_v7().to_string();
    conn.execute(
        "INSERT INTO context_items (id, session_id, kind, content, project) \
         VALUES (?1, ?2, ?3, ?4, ?5)",
        rusqlite::params![id, session_id, kind, content, project],
    )?;
    Ok(id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::open_pool;
    use tempfile::tempdir;

    fn conn() -> (tempfile::TempDir, PooledConn) {
        let dir = tempdir().unwrap();
        let pool = open_pool(&dir.path().join("m.db")).unwrap();
        let c = pool.get().unwrap();
        (dir, c)
    }

    #[test]
    fn do_not_repeat_inserts_findable_row() {
        let (_d, c) = conn();
        let id = add_do_not_repeat(&c, "/p", "never force-push main").unwrap();
        assert!(!id.is_empty());
        let rule: String = c
            .query_row(
                "SELECT rule FROM do_not_repeat WHERE project = '/p'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(rule, "never force-push main");
    }

    #[test]
    fn log_bug_defaults_open_and_stores_content() {
        let (_d, c) = conn();
        let id = log_bug(&c, "/p", "race in worker", Some("missing mutex")).unwrap();
        let (summary, content, resolved): (String, String, i64) = c
            .query_row(
                "SELECT summary, content, resolved FROM bugs WHERE id = ?1",
                rusqlite::params![id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap();
        assert_eq!(summary, "race in worker");
        assert_eq!(content, "missing mutex");
        assert_eq!(resolved, 0);
    }

    #[test]
    fn resolve_bug_sets_resolved_and_reports_missing() {
        let (_d, c) = conn();
        let id = log_bug(&c, "/p", "boom", None).unwrap();
        assert_eq!(resolve_bug(&c, &id).unwrap(), 1);
        let resolved: i64 = c
            .query_row(
                "SELECT resolved FROM bugs WHERE id = ?1",
                rusqlite::params![id],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(resolved, 1);
        assert_eq!(resolve_bug(&c, "nope").unwrap(), 0);
    }

    #[test]
    fn save_context_stores_project_kind_content() {
        let (_d, c) = conn();
        let id = save_context(&c, "/p", "architecture", "use rusqlite pool", None).unwrap();
        let (kind, content, project, sid): (String, String, String, Option<String>) = c
            .query_row(
                "SELECT kind, content, project, session_id FROM context_items WHERE id = ?1",
                rusqlite::params![id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .unwrap();
        assert_eq!(kind, "architecture");
        assert_eq!(content, "use rusqlite pool");
        assert_eq!(project, "/p");
        assert_eq!(sid, None);
    }
}
