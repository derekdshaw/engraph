//! The view of one checkout (plan 10, part 3, step 13): the base graph with
//! that checkout's overlay laid over it.
//!
//! The view is installed as two **temp views that shadow the base tables**.
//! SQLite resolves an unqualified name in the temp schema before the main
//! one, so after `install_worktree_view` every query that reads `entities` or
//! `relations` on this connection reads the worktree's view instead — the
//! neighborhood queries, `MATCH_WHERE_SQL`, the grep redirect and the
//! post-read listing, none of which change. The base tables stay reachable as
//! `main.entities` and `main.relations`, which is what the view definitions
//! and every writer use.
//!
//! Two consequences worth knowing when reading SQL in this crate:
//!
//! - A connection with the view installed is **read-only** for these two
//!   names: SQLite refuses a write through a view, which is the guard that a
//!   loader never runs on such a connection.
//! - Temp views live and die with the connection, and engraph's lookups are
//!   one process per invocation, so nothing leaks between runs.

use anyhow::Result;
use engraph_core::db::PooledConn;

/// The columns of `entities`, in table order, for both arms of the union.
const ENTITY_COLUMNS: &str = "id, kind, name, project, created_at, file_path, line_range, \
                              signature, qualified_name, scheme";

/// The columns of `relations` the lookups read. `valid_from` and
/// `source_message_id` are left out: the codegraph never writes them and no
/// lookup reads them, and the overlay table has neither.
const RELATION_COLUMNS: &str =
    "id, src_entity, dst_entity, kind, provenance, confidence, valid_to, line";

/// Point this connection's `entities` and `relations` at the view of the
/// checkout at `worktree` in `project`.
///
/// A row of the view is:
///
/// - every overlay row of this worktree, and
/// - every base row this worktree has not replaced: not a symbol the overlay
///   defines, and not in a file the overlay covers (a changed file's old
///   symbols go, a deleted file's rows go — which is what `overlay_files`
///   does for a file the overlay has no rows for).
///
/// A base edge goes when the file of its *source* is one the overlay covers;
/// the overlay's own edges replace it. An edge to a symbol the view does not
/// have disappears by itself, since the neighborhood queries join the other
/// end against `entities`.
///
/// Rows of other projects are never hidden: a monorepo's sibling project and
/// a cross-repo reference read as before.
pub fn install_worktree_view(conn: &PooledConn, project: &str, worktree: &str) -> Result<()> {
    let wt = quote(worktree);
    let proj = quote(project);
    let sql = format!(
        "DROP VIEW IF EXISTS temp.entities;
         DROP VIEW IF EXISTS temp.relations;

         CREATE TEMP VIEW entities AS
             SELECT {ENTITY_COLUMNS}
             FROM main.overlay_entities WHERE worktree = {wt}
             UNION ALL
             SELECT {ENTITY_COLUMNS}
             FROM main.entities e
             WHERE NOT EXISTS (
                     SELECT 1 FROM main.overlay_entities o
                     WHERE o.worktree = {wt} AND o.id = e.id
                   )
               AND NOT (
                     e.project IS {proj}
                     AND e.file_path IS NOT NULL
                     AND EXISTS (
                           SELECT 1 FROM main.overlay_files f
                           WHERE f.worktree = {wt} AND f.file_path = e.file_path
                         )
                   );

         CREATE TEMP VIEW relations AS
             SELECT {RELATION_COLUMNS}
             FROM main.overlay_relations WHERE worktree = {wt}
             UNION ALL
             SELECT {RELATION_COLUMNS}
             FROM main.relations r
             WHERE NOT EXISTS (
                     SELECT 1 FROM main.entities e
                     JOIN main.overlay_files f
                       ON f.worktree = {wt} AND f.file_path = e.file_path
                     WHERE e.id = r.src_entity AND e.project IS {proj}
                   );"
    );
    conn.execute_batch(&sql)?;
    Ok(())
}

/// Drop the view, so this connection reads the base tables again.
pub fn clear_worktree_view(conn: &PooledConn) -> Result<()> {
    conn.execute_batch("DROP VIEW IF EXISTS temp.entities; DROP VIEW IF EXISTS temp.relations;")?;
    Ok(())
}

/// A string as a SQL literal. The view definitions carry the worktree and the
/// project, which are paths, and a path may hold a quote.
fn quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quote_escapes_a_quote_in_a_path() {
        assert_eq!(quote("/home/o'brien/repo"), "'/home/o''brien/repo'");
        assert_eq!(quote("/plain"), "'/plain'");
    }
}
