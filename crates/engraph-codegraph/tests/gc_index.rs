//! Integration test: the pre-index GC pass prunes orphan entities before a
//! re-index, gated on the `gc` flag. Drives the prebuilt-`--scip` arm of
//! `index_repo` so no real indexer is required.

use engraph_codegraph::index_repo;
use engraph_core::db::{PooledConn, open_pool};
use protobuf::{EnumOrUnknown, Message, MessageField};
use scip::types::{
    Document, Index, Occurrence, SymbolInformation, SymbolRole, symbol_information::Kind as SymKind,
};
use tempfile::tempdir;

const FOO: &str = "scip-test cargo demo 0.0.0 `foo()`.";
const BAR: &str = "scip-test cargo demo 0.0.0 `bar()`.";

fn occ(symbol: &str, range: Vec<i32>, roles: i32) -> Occurrence {
    let mut o = Occurrence::new();
    o.symbol = symbol.to_string();
    o.range = range;
    o.symbol_roles = roles;
    o
}

fn sym(symbol: &str, name: &str) -> SymbolInformation {
    let mut s = SymbolInformation::new();
    s.symbol = symbol.to_string();
    s.display_name = name.to_string();
    s.kind = EnumOrUnknown::new(SymKind::Function);
    s
}

/// foo() at line 0, bar() at line 10; foo's body (line 5) calls bar, so the
/// load yields entities {foo, bar} and a CALLS edge foo -> bar — both entities
/// are referenced, so neither is an orphan.
fn fixture() -> Vec<u8> {
    let mut lib = Document::new();
    lib.relative_path = "src/lib.rs".to_string();
    lib.symbols = vec![sym(FOO, "foo"), sym(BAR, "bar")];
    lib.occurrences = vec![
        occ(FOO, vec![0, 0, 0, 3], SymbolRole::Definition as i32),
        occ(BAR, vec![10, 0, 10, 3], SymbolRole::Definition as i32),
        occ(BAR, vec![5, 4, 5, 7], 0),
    ];
    let mut idx = Index::new();
    idx.metadata = MessageField::some(Default::default());
    idx.documents.push(lib);
    idx.write_to_bytes().unwrap()
}

fn exists(conn: &PooledConn, id: &str) -> bool {
    conn.query_row("SELECT COUNT(*) FROM entities WHERE id = ?1", [id], |r| {
        r.get::<_, i64>(0)
    })
    .unwrap()
        > 0
}

#[test]
fn gc_flag_controls_orphan_pruning_on_reindex() {
    let dir = tempdir().unwrap();
    let scip = dir.path().join("index.scip");
    std::fs::write(&scip, fixture()).unwrap();
    let pool = open_pool(&dir.path().join("eg.db")).unwrap();
    let conn = pool.get().unwrap();
    let project = "/proj/demo";

    // Seed the graph (prebuilt-SCIP arm; gc off for the initial load).
    index_repo(&conn, dir.path(), Some(&scip), None, project, false, false).unwrap();
    assert!(exists(&conn, FOO));

    // A stale entity left behind by previously-deleted source (no relations).
    conn.execute(
        "INSERT INTO entities (id, kind, name, project) VALUES ('dead', 'symbol', 'dead', ?1)",
        [project],
    )
    .unwrap();

    // --no-gc: re-index leaves the orphan in place.
    let s = index_repo(&conn, dir.path(), Some(&scip), None, project, false, false).unwrap();
    assert_eq!(s.entities_pruned, 0);
    assert!(exists(&conn, "dead"), "orphan survives a --no-gc re-index");

    // --gc (default): re-index prunes the orphan before loading.
    let s = index_repo(&conn, dir.path(), Some(&scip), None, project, false, true).unwrap();
    assert_eq!(s.entities_pruned, 1);
    assert!(!exists(&conn, "dead"), "orphan pruned by a --gc re-index");
    // The real symbols survive — the load re-creates them after GC.
    assert!(exists(&conn, FOO));
    assert!(exists(&conn, BAR));
}
