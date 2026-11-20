//! The overlay load of plan 10, part 3, step 10: an index run in a checkout
//! writes rows for the files that differ from the base, under that checkout's
//! key, and leaves the base rows of every other file alone.

use engraph_codegraph::{OverlayFile, load, load_overlay};
use engraph_core::db::{Pool, open_pool};
use protobuf::{EnumOrUnknown, Message, MessageField};
use scip::types::{
    Document, Index, Occurrence, SymbolInformation, SymbolRole, symbol_information::Kind as SymKind,
};
use tempfile::tempdir;

const PROJECT: &str = "/repo";
const WT: &str = "/wt/elm";

fn occ(symbol: &str, range: Vec<i32>, roles: i32) -> Occurrence {
    let mut o = Occurrence::new();
    o.symbol = symbol.to_string();
    o.range = range;
    o.symbol_roles = roles;
    o
}

fn sym(symbol: &str, name: &str, kind: SymKind) -> SymbolInformation {
    let mut s = SymbolInformation::new();
    s.symbol = symbol.to_string();
    s.display_name = name.to_string();
    s.kind = EnumOrUnknown::new(kind);
    s
}

fn id(name: &str) -> String {
    format!("scip-test cargo demo 0.0.0 `{name}()`.")
}

/// `ops.rs` defines `caller`, which calls `helper` of `util.rs`; `util.rs`
/// defines `helper`. `extra` is an extra definition in `ops.rs` when
/// `with_extra` is set — the symbol a worktree adds.
fn fixture(with_extra: bool) -> Vec<u8> {
    let mut ops = Document::new();
    ops.relative_path = "src/ops.rs".to_string();
    ops.symbols = vec![sym(&id("caller"), "caller", SymKind::Function)];
    ops.occurrences = vec![
        occ(
            &id("caller"),
            vec![0, 0, 0, 6],
            SymbolRole::Definition as i32,
        ),
        occ(&id("helper"), vec![2, 4, 2, 10], 0),
    ];
    if with_extra {
        ops.symbols
            .push(sym(&id("extra"), "extra", SymKind::Function));
        ops.occurrences.push(occ(
            &id("extra"),
            vec![8, 0, 8, 5],
            SymbolRole::Definition as i32,
        ));
    }

    let mut util = Document::new();
    util.relative_path = "src/util.rs".to_string();
    util.symbols = vec![sym(&id("helper"), "helper", SymKind::Function)];
    util.occurrences = vec![occ(
        &id("helper"),
        vec![0, 0, 0, 6],
        SymbolRole::Definition as i32,
    )];

    let mut idx = Index::new();
    idx.documents = vec![ops, util];
    let mut meta = scip::types::Metadata::new();
    meta.project_root = "file:///repo".to_string();
    idx.metadata = MessageField::some(meta);
    idx.write_to_bytes().unwrap()
}

fn db() -> (tempfile::TempDir, Pool) {
    let dir = tempdir().unwrap();
    let pool = open_pool(&dir.path().join("t.db")).unwrap();
    (dir, pool)
}

fn names(conn: &engraph_core::db::PooledConn, sql: &str, p: &str) -> Vec<String> {
    let mut stmt = conn.prepare(sql).unwrap();
    let mut rows: Vec<String> = stmt
        .query_map([p], |r| r.get(0))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap();
    rows.sort();
    rows
}

#[test]
fn the_overlay_keeps_only_the_changed_documents_and_leaves_the_base_alone() {
    let (_d, pool) = db();
    let conn = pool.get().unwrap();
    load(&conn, PROJECT, &fixture(false)).unwrap();
    let base_before = names(
        &conn,
        "SELECT name FROM entities WHERE project = ?1 AND file_path IS NOT NULL",
        PROJECT,
    );
    assert_eq!(base_before, vec!["caller", "helper"]);

    // The worktree changed src/ops.rs only. The run analysed the whole
    // package, as SCIP indexers do, but the overlay keeps that one document.
    let files = vec![OverlayFile {
        path: "src/ops.rs".to_string(),
        deleted: false,
    }];
    let stats = load_overlay(&conn, PROJECT, WT, &files, &fixture(true)).unwrap();
    assert_eq!(stats.documents_loaded, 1);
    assert_eq!(stats.files, 1);

    // The overlay has the changed file's symbols, including the new one...
    assert_eq!(
        names(
            &conn,
            "SELECT name FROM overlay_entities WHERE worktree = ?1 AND file_path IS NOT NULL",
            WT
        ),
        vec!["caller", "extra"]
    );
    // ...and not the unchanged file's, which the view reads from the base.
    let overlay_files: Vec<String> = names(
        &conn,
        "SELECT file_path FROM overlay_entities WHERE worktree = ?1 AND file_path IS NOT NULL",
        WT,
    );
    assert!(
        overlay_files.iter().all(|f| f == "src/ops.rs"),
        "{overlay_files:?}"
    );

    // The base is untouched: no `extra`, and the same rows as before.
    assert_eq!(
        names(
            &conn,
            "SELECT name FROM entities WHERE project = ?1 AND file_path IS NOT NULL",
            PROJECT
        ),
        base_before
    );

    // The edge from the changed file is in the overlay, pointing at the base
    // symbol it calls.
    let edges: Vec<(String, String)> = conn
        .prepare("SELECT src_entity, dst_entity FROM overlay_relations WHERE worktree = ?1")
        .unwrap()
        .query_map([WT], |r| Ok((r.get(0)?, r.get(1)?)))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap();
    assert_eq!(edges, vec![(id("caller"), id("helper"))]);

    // `helper` is in the base, so the overlay does not copy a stub of it.
    let stub: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM overlay_entities WHERE worktree = ?1 AND id = ?2",
            rusqlite::params![WT, id("helper")],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(stub, 0, "the base already has `helper`");
}

#[test]
fn a_deleted_file_is_listed_without_rows_and_the_overlay_is_replaced_whole() {
    let (_d, pool) = db();
    let conn = pool.get().unwrap();
    load(&conn, PROJECT, &fixture(false)).unwrap();

    // The worktree deleted src/util.rs and changed src/ops.rs.
    let files = vec![
        OverlayFile {
            path: "src/ops.rs".to_string(),
            deleted: false,
        },
        OverlayFile {
            path: "src/util.rs".to_string(),
            deleted: true,
        },
    ];
    load_overlay(&conn, PROJECT, WT, &files, &fixture(true)).unwrap();
    let listed: Vec<(String, bool)> = conn
        .prepare(
            "SELECT file_path, deleted FROM overlay_files WHERE worktree = ?1 ORDER BY file_path",
        )
        .unwrap()
        .query_map([WT], |r| Ok((r.get(0)?, r.get::<_, i64>(1)? == 1)))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap();
    assert_eq!(
        listed,
        vec![
            ("src/ops.rs".to_string(), false),
            ("src/util.rs".to_string(), true),
        ]
    );
    // The deleted file has no rows of its own: the listing is what hides the
    // base rows of `helper`.
    let from_deleted: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM overlay_entities WHERE worktree = ?1 AND file_path = 'src/util.rs'",
            [WT],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(from_deleted, 0);

    // The worktree goes back to the base content of ops.rs and restores
    // util.rs: the overlay is replaced whole, so nothing of it remains.
    let stats = load_overlay(&conn, PROJECT, WT, &[], &fixture(false)).unwrap();
    assert_eq!(stats.documents_loaded, 0);
    for table in ["overlay_entities", "overlay_relations", "overlay_files"] {
        let n: i64 = conn
            .query_row(
                &format!("SELECT COUNT(*) FROM {table} WHERE worktree = ?1"),
                [WT],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(n, 0, "{table} still has rows");
    }
}

#[test]
fn two_worktrees_keep_their_own_version_of_one_symbol() {
    let (_d, pool) = db();
    let conn = pool.get().unwrap();
    load(&conn, PROJECT, &fixture(false)).unwrap();
    let files = vec![OverlayFile {
        path: "src/ops.rs".to_string(),
        deleted: false,
    }];
    load_overlay(&conn, PROJECT, "/wt/a", &files, &fixture(true)).unwrap();
    load_overlay(&conn, PROJECT, "/wt/b", &files, &fixture(false)).unwrap();

    // `caller` exists three times: once in the base, once per worktree.
    let n: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM overlay_entities WHERE id = ?1",
            [id("caller")],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(n, 2);
    // The symbol worktree A added is in A's overlay and in no other.
    let with_extra: Vec<String> = names(
        &conn,
        "SELECT worktree FROM overlay_entities WHERE id = ?1",
        &id("extra"),
    );
    assert_eq!(with_extra, vec!["/wt/a"]);
    let in_base: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM entities WHERE id = ?1",
            [id("extra")],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(in_base, 0);
}
