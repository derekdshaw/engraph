//! Build a small SCIP Index in-memory using the `scip` crate's protobuf
//! types, serialize it to bytes, feed it to the loader, and assert that the
//! expected entities and relations are present. In-memory generation avoids
//! the bytes-rot problem of checking in a binary fixture.

use engraph_codegraph::scip_loader::load;
use engraph_core::db::open_pool;
use protobuf::{EnumOrUnknown, Message, MessageField};
use scip::types::{
    symbol_information::Kind as SymKind, Document, Index, Occurrence, SymbolInformation, SymbolRole,
};
use tempfile::tempdir;

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

fn build_fixture() -> Vec<u8> {
    // Two definitions in lib.rs: foo() at line 0, bar() at line 10. foo's body
    // (line 5) calls bar, so we expect a CALLS edge foo -> bar.
    let mut lib = Document::new();
    lib.relative_path = "src/lib.rs".to_string();
    lib.symbols = vec![
        sym(
            "scip-test cargo demo 0.0.0 `foo()`.",
            "foo",
            SymKind::Function,
        ),
        sym(
            "scip-test cargo demo 0.0.0 `bar()`.",
            "bar",
            SymKind::Function,
        ),
    ];
    lib.occurrences = vec![
        occ(
            "scip-test cargo demo 0.0.0 `foo()`.",
            vec![0, 0, 0, 3],
            SymbolRole::Definition as i32,
        ),
        occ(
            "scip-test cargo demo 0.0.0 `bar()`.",
            vec![10, 0, 10, 3],
            SymbolRole::Definition as i32,
        ),
        occ("scip-test cargo demo 0.0.0 `bar()`.", vec![5, 4, 5, 7], 0),
    ];

    let mut idx = Index::new();
    idx.metadata = MessageField::some(Default::default());
    idx.documents.push(lib);
    idx.write_to_bytes().unwrap()
}

#[test]
fn loader_emits_entities_and_a_calls_edge() {
    let dir = tempdir().unwrap();
    let pool = open_pool(&dir.path().join("t.db")).unwrap();
    let conn = pool.get().unwrap();
    let bytes = build_fixture();
    let stats = load(&conn, "/proj/demo", &bytes).unwrap();
    assert_eq!(stats.documents_seen, 1);
    assert!(
        stats.entities_inserted >= 2,
        "got {}",
        stats.entities_inserted
    );

    let n_entities: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM entities WHERE project = ?1",
            ["/proj/demo"],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(n_entities, 2);

    let foo_path: String = conn
        .query_row(
            "SELECT file_path FROM entities WHERE id = ?1",
            ["scip-test cargo demo 0.0.0 `foo()`."],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(foo_path, "src/lib.rs");

    let foo_lr: String = conn
        .query_row(
            "SELECT line_range FROM entities WHERE id = ?1",
            ["scip-test cargo demo 0.0.0 `foo()`."],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(foo_lr, "1:1");

    let calls: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM relations
             WHERE src_entity = ?1 AND dst_entity = ?2 AND kind = 'CALLS'",
            rusqlite::params![
                "scip-test cargo demo 0.0.0 `foo()`.",
                "scip-test cargo demo 0.0.0 `bar()`."
            ],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(calls, 1, "expected one foo -> bar CALLS edge");
}

#[test]
fn reloading_same_bytes_is_idempotent() {
    let dir = tempdir().unwrap();
    let pool = open_pool(&dir.path().join("t.db")).unwrap();
    let conn = pool.get().unwrap();
    let bytes = build_fixture();
    let _ = load(&conn, "/proj/demo", &bytes).unwrap();
    let n1: i64 = conn
        .query_row("SELECT COUNT(*) FROM relations", [], |r| r.get(0))
        .unwrap();
    let _ = load(&conn, "/proj/demo", &bytes).unwrap();
    let n2: i64 = conn
        .query_row("SELECT COUNT(*) FROM relations", [], |r| r.get(0))
        .unwrap();
    assert_eq!(n1, n2);

    let e1: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM entities WHERE project = ?1",
            ["/proj/demo"],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(e1, 2, "re-load must not duplicate entities");
}

#[test]
fn loader_scopes_deletes_to_project() {
    let dir = tempdir().unwrap();
    let pool = open_pool(&dir.path().join("t.db")).unwrap();
    let conn = pool.get().unwrap();

    // Pre-seed an entity in a different project; the loader must not touch it.
    conn.execute(
        "INSERT INTO entities (id, kind, name, project) VALUES ('other-1', 'symbol', 'other', '/proj/other')",
        [],
    )
    .unwrap();

    let bytes = build_fixture();
    let _ = load(&conn, "/proj/demo", &bytes).unwrap();

    let other: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM entities WHERE project = '/proj/other'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(other, 1, "load() must scope its DELETE to its project");
}

#[test]
fn loader_preserves_bazel_depends_on_edges() {
    // Phase 2.3 #2 regression: a SCIP load running under the same project
    // as a prior target-level Bazel pass must not wipe BAZEL_DEPENDS_ON
    // edges (only SCIP-derived edges should be replaced).
    let dir = tempdir().unwrap();
    let pool = open_pool(&dir.path().join("t.db")).unwrap();
    let conn = pool.get().unwrap();

    conn.execute(
        "INSERT INTO entities (id, kind, name, project) VALUES ('//a:a', 'bazel_target', 'a', '/proj/demo')",
        [],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO entities (id, kind, name, project) VALUES ('//b:b', 'bazel_target', 'b', '/proj/demo')",
        [],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO relations (id, src_entity, dst_entity, kind, provenance, confidence)
         VALUES ('r1', '//a:a', '//b:b', 'BAZEL_DEPENDS_ON', 'extracted', 1.0)",
        [],
    )
    .unwrap();

    let bytes = build_fixture();
    let _ = load(&conn, "/proj/demo", &bytes).unwrap();

    let surviving: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM relations WHERE kind = 'BAZEL_DEPENDS_ON'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        surviving, 1,
        "BAZEL_DEPENDS_ON edge must survive a same-project SCIP load"
    );
}
