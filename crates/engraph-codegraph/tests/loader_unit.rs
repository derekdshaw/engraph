//! Build a small SCIP Index in-memory using the `scip` crate's protobuf
//! types, serialize it to bytes, feed it to the loader, and assert that the
//! expected entities and relations are present. In-memory generation avoids
//! the bytes-rot problem of checking in a binary fixture.

use engraph_codegraph::scip_loader::load;
use engraph_core::db::open_pool;
use protobuf::{EnumOrUnknown, Message, MessageField};
use scip::types::{
    Document, Index, Occurrence, SymbolInformation, SymbolRole, symbol_information::Kind as SymKind,
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

fn build_single_foo(symbol: &str, line: i32) -> Vec<u8> {
    let mut lib = Document::new();
    lib.relative_path = "src/lib.rs".to_string();
    lib.symbols = vec![sym(symbol, "foo", SymKind::Function)];
    lib.occurrences = vec![occ(
        symbol,
        vec![line, 0, line, 3],
        SymbolRole::Definition as i32,
    )];

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
fn reload_prunes_stale_symbol_locations_absent_from_new_scip() {
    let dir = tempdir().unwrap();
    let pool = open_pool(&dir.path().join("t.db")).unwrap();
    let conn = pool.get().unwrap();

    let _ = load(&conn, "/proj/demo", &build_fixture()).unwrap();
    let new_foo = "scip-test cargo demo 0.0.0 `foo_v2()`.";
    let stats = load(&conn, "/proj/demo", &build_single_foo(new_foo, 20)).unwrap();
    assert!(
        stats.entities_pruned >= 1,
        "expected stale foo/bar rows to be pruned"
    );

    let foos: Vec<(String, String)> = {
        let mut stmt = conn
            .prepare(
                "SELECT id, line_range
                 FROM entities
                 WHERE project = ?1 AND name = 'foo' AND file_path = 'src/lib.rs'
                 ORDER BY id",
            )
            .unwrap();
        stmt.query_map(["/proj/demo"], |row| Ok((row.get(0)?, row.get(1)?)))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap()
    };

    assert_eq!(foos, vec![(new_foo.to_string(), "21:21".to_string())]);
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

/// One document with `symbol`, whose `display_name` the caller chooses, and
/// a `main` that calls it, so the row has a relation and the pre-load GC
/// does not remove it between loads.
fn build_named(symbol: &str, display_name: &str) -> Vec<u8> {
    let main = "scip-test cargo demo 0.0.0 main().";
    let mut lib = Document::new();
    lib.relative_path = "src/lib.rs".to_string();
    lib.symbols = vec![
        sym(symbol, display_name, SymKind::Function),
        sym(main, "main", SymKind::Function),
    ];
    lib.occurrences = vec![
        occ(symbol, vec![3, 0, 3, 3], SymbolRole::Definition as i32),
        occ(main, vec![10, 0, 10, 4], SymbolRole::Definition as i32),
        occ(symbol, vec![11, 4, 11, 7], 0),
    ];
    let mut idx = Index::new();
    idx.metadata = MessageField::some(Default::default());
    idx.documents.push(lib);
    idx.write_to_bytes().unwrap()
}

fn name_of(conn: &engraph_core::db::PooledConn, id: &str) -> String {
    conn.query_row("SELECT name FROM entities WHERE id = ?1", [id], |r| {
        r.get(0)
    })
    .unwrap()
}

#[test]
fn reload_replaces_the_name_of_an_existing_symbol() {
    // A row written by an older engraph (or by a placeholder) keeps an
    // incorrect name unless a later load of the same id overwrites it.
    let dir = tempdir().unwrap();
    let pool = open_pool(&dir.path().join("t.db")).unwrap();
    let conn = pool.get().unwrap();
    let id = "scip-test cargo demo 0.0.0 config/Config#full_path_base().";

    load(
        &conn,
        "/proj/demo",
        &build_named(id, "Config#full_path_base"),
    )
    .unwrap();
    assert_eq!(name_of(&conn, id), "Config#full_path_base");

    load(&conn, "/proj/demo", &build_named(id, "full_path_base")).unwrap();
    assert_eq!(
        name_of(&conn, id),
        "full_path_base",
        "a second load must replace the stored name"
    );
}

#[test]
fn definition_names_a_placeholder_written_by_a_reference() {
    // Document A references `helper` before document B defines it. The
    // placeholder row that A's reference creates must take the name that B's
    // definition gives, whatever the document order.
    let helper = "scip-test cargo demo 0.0.0 util/Util#helper().";
    let caller = "scip-test cargo demo 0.0.0 app/run().";

    let mut a = Document::new();
    a.relative_path = "src/app.rs".to_string();
    a.symbols = vec![sym(caller, "", SymKind::Function)];
    a.occurrences = vec![
        occ(caller, vec![0, 0, 0, 3], SymbolRole::Definition as i32),
        occ(helper, vec![1, 4, 1, 10], 0),
    ];
    let mut b = Document::new();
    b.relative_path = "src/util.rs".to_string();
    b.symbols = vec![sym(helper, "Util.helper (display)", SymKind::Method)];
    b.occurrences = vec![occ(helper, vec![5, 0, 5, 6], SymbolRole::Definition as i32)];

    let mut idx = Index::new();
    idx.metadata = MessageField::some(Default::default());
    idx.documents.push(a);
    idx.documents.push(b);
    let bytes = idx.write_to_bytes().unwrap();

    let dir = tempdir().unwrap();
    let pool = open_pool(&dir.path().join("t.db")).unwrap();
    let conn = pool.get().unwrap();
    load(&conn, "/proj/demo", &bytes).unwrap();
    assert_eq!(name_of(&conn, helper), "Util.helper (display)");
    assert_eq!(name_of(&conn, caller), "run");
}

#[test]
fn loader_writes_the_qualified_name_of_each_symbol() {
    // qualified_name: the descriptors after the namespace, joined with `.`,
    // so `Config::full_path_base` / `Config#full_path_base` lookups can match.
    let method = "scip-test cargo demo 0.0.0 config/Config#full_path_base().";
    let class = "scip-test cargo demo 0.0.0 config/Config#";
    let func = "scip-test cargo demo 0.0.0 config/load().";
    let mut lib = Document::new();
    lib.relative_path = "src/config.rs".to_string();
    lib.symbols = vec![
        sym(class, "", SymKind::Struct),
        sym(method, "", SymKind::Method),
        sym(func, "", SymKind::Function),
    ];
    lib.occurrences = vec![
        occ(class, vec![0, 0, 0, 6], SymbolRole::Definition as i32),
        occ(method, vec![2, 4, 2, 18], SymbolRole::Definition as i32),
        occ(func, vec![8, 0, 8, 4], SymbolRole::Definition as i32),
        // load() calls the method, so the rows have relations.
        occ(method, vec![9, 4, 9, 18], 0),
    ];
    let mut idx = Index::new();
    idx.metadata = MessageField::some(Default::default());
    idx.documents.push(lib);
    let bytes = idx.write_to_bytes().unwrap();

    let dir = tempdir().unwrap();
    let pool = open_pool(&dir.path().join("t.db")).unwrap();
    let conn = pool.get().unwrap();
    load(&conn, "/proj/demo", &bytes).unwrap();

    let qn = |id: &str| -> Option<String> {
        conn.query_row(
            "SELECT qualified_name FROM entities WHERE id = ?1",
            [id],
            |r| r.get(0),
        )
        .unwrap()
    };
    assert_eq!(qn(method).as_deref(), Some("Config.full_path_base"));
    assert_eq!(qn(class).as_deref(), Some("Config"));
    assert_eq!(qn(func).as_deref(), Some("load"));
    assert_eq!(name_of(&conn, method), "full_path_base");

    // A placeholder written by a reference gets a qualified name too.
    let extern_method = "scip-test cargo other 0.0.0 ext/Ext#call().";
    let mut doc = Document::new();
    doc.relative_path = "src/use.rs".to_string();
    doc.symbols = vec![sym(func, "", SymKind::Function)];
    doc.occurrences = vec![
        occ(func, vec![0, 0, 0, 4], SymbolRole::Definition as i32),
        occ(extern_method, vec![1, 4, 1, 8], 0),
    ];
    let mut idx = Index::new();
    idx.metadata = MessageField::some(Default::default());
    idx.documents.push(doc);
    load(&conn, "/proj/demo", &idx.write_to_bytes().unwrap()).unwrap();
    assert_eq!(qn(extern_method).as_deref(), Some("Ext.call"));
}
