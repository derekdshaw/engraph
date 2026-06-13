//! Pre-index garbage collection: prune orphan entities.
//!
//! The SCIP loader ([`crate::scip_loader::load`]) upserts entities but never
//! deletes them — on each load it wipes a project's *relations* and re-inserts,
//! leaving the entity rows in place (see the rationale in `scip_loader`). So
//! when source code is removed, its entity rows linger as orphans: present in
//! `entities` but referenced by no relation, in either direction.
//! [`collect_orphans`] prunes those rows for one project.
//!
//! Run this BEFORE a re-index, not after. The subsequent load re-creates any
//! still-valid symbol that happens to be edgeless (e.g. an unreferenced private
//! helper), so GC-before only permanently removes entities that are *also*
//! absent from the new index — i.e. genuinely deleted source. GC-after would
//! drop valid-but-unreferenced symbols until the next index.
//!
//! Caveat: with a partial load — a `--scip-manifest` covering only some of a
//! project's languages (e.g. `engraph-atoms-scip --languages go`) — the loader
//! wipes the *other* languages' relations under the same project, orphaning
//! their entities; a later GC then deletes those rows, recoverable only by a
//! full all-language re-index.

use anyhow::Result;
use engraph_core::db::PooledConn;

/// Delete entities of `project` referenced by no relation (as `src_entity` or
/// `dst_entity`), returning the number pruned.
///
/// FK-safe and cross-repo-safe: `relations.src_entity`/`dst_entity` are
/// `REFERENCES entities(id)` with the default RESTRICT, so an entity any
/// relation still points at — including an inbound edge owned by *another*
/// project — fails the predicate (and could not be deleted even if it didn't).
/// Kind-agnostic: prunes orphan `symbol` and `bazel_target` rows alike.
pub fn collect_orphans(conn: &PooledConn, project: &str) -> Result<usize> {
    // One statement → atomic under auto-commit, no explicit transaction needed.
    // Two separate NOT EXISTS (rather than `OR src_entity = .. OR dst_entity =
    // ..`) let each subquery use its own index (idx_relations_src /
    // idx_relations_dst); idx_entities_project scopes the outer scan.
    let pruned = conn.execute(
        "DELETE FROM entities
         WHERE project = ?1
           AND NOT EXISTS (SELECT 1 FROM relations r WHERE r.src_entity = entities.id)
           AND NOT EXISTS (SELECT 1 FROM relations r WHERE r.dst_entity = entities.id)",
        [project],
    )?;
    Ok(pruned)
}

#[cfg(test)]
mod tests {
    use super::*;
    use engraph_core::db::{Pool, open_pool};
    use tempfile::tempdir;

    fn setup() -> (tempfile::TempDir, Pool) {
        let dir = tempdir().unwrap();
        let pool = open_pool(&dir.path().join("gc.db")).unwrap();
        (dir, pool)
    }

    fn ins_entity(conn: &PooledConn, id: &str, kind: &str, project: &str) {
        conn.execute(
            "INSERT INTO entities (id, kind, name, project) VALUES (?1, ?2, ?3, ?4)",
            rusqlite::params![id, kind, id, project],
        )
        .unwrap();
    }

    fn ins_relation(conn: &PooledConn, id: &str, src: &str, dst: &str, kind: &str) {
        conn.execute(
            "INSERT INTO relations (id, src_entity, dst_entity, kind) VALUES (?1, ?2, ?3, ?4)",
            rusqlite::params![id, src, dst, kind],
        )
        .unwrap();
    }

    fn exists(conn: &PooledConn, id: &str) -> bool {
        conn.query_row("SELECT COUNT(*) FROM entities WHERE id = ?1", [id], |r| {
            r.get::<_, i64>(0)
        })
        .unwrap()
            > 0
    }

    #[test]
    fn prunes_orphan_symbol() {
        let (_d, pool) = setup();
        let conn = pool.get().unwrap();
        ins_entity(&conn, "e1", "symbol", "/p");
        assert_eq!(collect_orphans(&conn, "/p").unwrap(), 1);
        assert!(!exists(&conn, "e1"));
    }

    #[test]
    fn keeps_entity_with_outbound_or_inbound_relation() {
        let (_d, pool) = setup();
        let conn = pool.get().unwrap();
        ins_entity(&conn, "s", "symbol", "/p");
        ins_entity(&conn, "d", "symbol", "/p");
        ins_relation(&conn, "r1", "s", "d", "CALLS");
        assert_eq!(collect_orphans(&conn, "/p").unwrap(), 0);
        assert!(exists(&conn, "s")); // protected as src
        assert!(exists(&conn, "d")); // protected as dst
    }

    #[test]
    fn keeps_entity_referenced_inbound_from_another_project() {
        // app_b → lib_a::foo. GC of lib_a must NOT drop foo: it has no outbound
        // edge, only the inbound CALLS owned by app_b. This is the cross-repo
        // safety guarantee.
        let (_d, pool) = setup();
        let conn = pool.get().unwrap();
        ins_entity(&conn, "lib_foo", "symbol", "/proj/lib_a");
        ins_entity(&conn, "app_caller", "symbol", "/proj/app_b");
        ins_relation(&conn, "r", "app_caller", "lib_foo", "CALLS");
        assert_eq!(collect_orphans(&conn, "/proj/lib_a").unwrap(), 0);
        assert!(exists(&conn, "lib_foo"));
    }

    #[test]
    fn prunes_unreferenced_placeholder_keeps_referenced() {
        let (_d, pool) = setup();
        let conn = pool.get().unwrap();
        ins_entity(&conn, "o", "symbol", "/p");
        ins_entity(&conn, "ph_ref", "symbol", "/p"); // referenced placeholder
        ins_relation(&conn, "r", "o", "ph_ref", "IMPLEMENTS");
        ins_entity(&conn, "ph_orphan", "symbol", "/p"); // unreferenced placeholder
        assert_eq!(collect_orphans(&conn, "/p").unwrap(), 1);
        assert!(exists(&conn, "ph_ref"));
        assert!(exists(&conn, "o"));
        assert!(!exists(&conn, "ph_orphan"));
    }

    #[test]
    fn prunes_orphan_bazel_target_keeps_linked() {
        let (_d, pool) = setup();
        let conn = pool.get().unwrap();
        ins_entity(&conn, "//a:a", "bazel_target", "/p"); // orphan
        ins_entity(&conn, "//b:b", "bazel_target", "/p");
        ins_entity(&conn, "//c:c", "bazel_target", "/p");
        ins_relation(&conn, "r", "//b:b", "//c:c", "BAZEL_DEPENDS_ON");
        assert_eq!(collect_orphans(&conn, "/p").unwrap(), 1);
        assert!(!exists(&conn, "//a:a"));
        assert!(exists(&conn, "//b:b"));
        assert!(exists(&conn, "//c:c"));
    }

    #[test]
    fn scopes_delete_to_the_named_project() {
        let (_d, pool) = setup();
        let conn = pool.get().unwrap();
        ins_entity(&conn, "mine", "symbol", "/p");
        ins_entity(&conn, "theirs", "symbol", "/other");
        assert_eq!(collect_orphans(&conn, "/p").unwrap(), 1);
        assert!(!exists(&conn, "mine"));
        assert!(exists(&conn, "theirs")); // other project's orphan untouched
    }
}
