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

// scip-python sets neither `SymbolInformation.kind` nor `display_name`; the
// symbol id is the only source of the kind. The document models:
//
//   0  class C:
//   1      def m(self):
//   2          f(self)
//   4  def f(): ...
const PY_CLASS: &str = "scip-python python demo 0 `pkg.mod`/C#";
const PY_METHOD: &str = "scip-python python demo 0 `pkg.mod`/C#m().";
const PY_PARAM: &str = "scip-python python demo 0 `pkg.mod`/C#m().(self)";
const PY_FUNC: &str = "scip-python python demo 0 `pkg.mod`/f().";

fn build_kindless_python() -> Vec<u8> {
    let no_kind = |symbol: &str| {
        let mut s = SymbolInformation::new();
        s.symbol = symbol.to_string();
        s
    };
    let def = SymbolRole::Definition as i32;
    let mut doc = Document::new();
    doc.relative_path = "pkg/mod.py".to_string();
    doc.symbols = vec![
        no_kind(PY_CLASS),
        no_kind(PY_METHOD),
        no_kind(PY_PARAM),
        no_kind(PY_FUNC),
    ];
    doc.occurrences = vec![
        occ(PY_CLASS, vec![0, 6, 0, 7], def),
        occ(PY_METHOD, vec![1, 8, 1, 9], def),
        occ(PY_PARAM, vec![1, 10, 1, 14], def),
        occ(PY_FUNC, vec![2, 8, 2, 9], 0),
        occ(PY_PARAM, vec![2, 10, 2, 14], 0),
        occ(PY_FUNC, vec![4, 4, 4, 5], def),
    ];
    let mut idx = Index::new();
    idx.metadata = MessageField::some(Default::default());
    idx.documents.push(doc);
    idx.write_to_bytes().unwrap()
}

fn count_relations(conn: &engraph_core::db::PooledConn, sql_where: &str, id: &str) -> i64 {
    conn.query_row(
        &format!("SELECT COUNT(*) FROM relations WHERE {sql_where}"),
        [id],
        |r| r.get(0),
    )
    .unwrap()
}

#[test]
fn kindless_method_anchors_a_call_by_its_descriptor_suffix() {
    let dir = tempdir().unwrap();
    let pool = open_pool(&dir.path().join("t.db")).unwrap();
    let conn = pool.get().unwrap();
    load(&conn, "/proj/demo", &build_kindless_python()).unwrap();

    let calls: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM relations
             WHERE src_entity = ?1 AND dst_entity = ?2 AND kind = 'CALLS'",
            rusqlite::params![PY_METHOD, PY_FUNC],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(calls, 1, "expected one C#m() -> f() CALLS edge");
    // A parameter is not an anchor, and a use of it is not a call.
    assert_eq!(count_relations(&conn, "src_entity = ?1", PY_PARAM), 0);
    assert_eq!(count_relations(&conn, "dst_entity = ?1", PY_PARAM), 0);
}

// SCIP local symbols (`local 0`) name a symbol only inside one document. Two
// documents that both use `local 0` mean two different symbols.
const LOCAL_FUNC: &str = "scip-test cargo demo 0.0.0 pkg/f().";

fn doc_with_local_zero(path: &str, local_name: &str, def_line: i32) -> Document {
    let def = SymbolRole::Definition as i32;
    let mut doc = Document::new();
    doc.relative_path = path.to_string();
    doc.symbols = vec![sym("local 0", local_name, SymKind::Function)];
    doc.occurrences = vec![
        occ("local 0", vec![def_line, 0, def_line, 4], def),
        // A call of pkg/f() in the body of `local 0`.
        occ(LOCAL_FUNC, vec![def_line + 1, 4, def_line + 1, 5], 0),
    ];
    doc
}

fn build_two_documents_with_local_zero() -> Vec<u8> {
    let def = SymbolRole::Definition as i32;
    let mut lib = Document::new();
    lib.relative_path = "src/pkg.rs".to_string();
    lib.symbols = vec![sym(LOCAL_FUNC, "f", SymKind::Function)];
    lib.occurrences = vec![occ(LOCAL_FUNC, vec![0, 0, 0, 1], def)];
    let mut idx = Index::new();
    idx.metadata = MessageField::some(Default::default());
    idx.documents.push(lib);
    idx.documents
        .push(doc_with_local_zero("src/a.rs", "helper_a", 3));
    idx.documents
        .push(doc_with_local_zero("src/b.rs", "helper_b", 7));
    idx.write_to_bytes().unwrap()
}

#[test]
fn local_symbols_of_two_documents_stay_two_entities() {
    let dir = tempdir().unwrap();
    let pool = open_pool(&dir.path().join("t.db")).unwrap();
    let conn = pool.get().unwrap();
    load(&conn, "/proj/demo", &build_two_documents_with_local_zero()).unwrap();

    // One entity per `local 0`, each with the name and file of its document.
    let mut stmt = conn
        .prepare(
            "SELECT name, file_path, line_range FROM entities
             WHERE id LIKE 'local %' ORDER BY file_path",
        )
        .unwrap();
    let locals: Vec<(String, String, String)> = stmt
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
        .unwrap()
        .map(Result::unwrap)
        .collect();
    assert_eq!(
        locals,
        vec![
            ("helper_a".into(), "src/a.rs".into(), "4:4".into()),
            ("helper_b".into(), "src/b.rs".into(), "8:8".into()),
        ]
    );

    // Each of the two has its own CALLS edge to pkg/f().
    let mut stmt = conn
        .prepare(
            "SELECT e.file_path FROM relations r JOIN entities e ON e.id = r.src_entity
             WHERE r.dst_entity = ?1 AND r.kind = 'CALLS' ORDER BY e.file_path",
        )
        .unwrap();
    let callers: Vec<String> = stmt
        .query_map([LOCAL_FUNC], |r| r.get(0))
        .unwrap()
        .map(Result::unwrap)
        .collect();
    assert_eq!(callers, vec!["src/a.rs", "src/b.rs"]);
}

#[test]
fn local_symbols_of_two_projects_with_one_path_stay_apart() {
    // Two repos with the same file (`src/a.rs`) and the same `local 0`: the
    // load of the second must not take the row, or the edges, of the first.
    let bytes = |name: &str| {
        let mut idx = Index::new();
        idx.metadata = MessageField::some(Default::default());
        idx.documents.push(doc_with_local_zero("src/a.rs", name, 3));
        idx.write_to_bytes().unwrap()
    };
    let dir = tempdir().unwrap();
    let pool = open_pool(&dir.path().join("t.db")).unwrap();
    let conn = pool.get().unwrap();
    load(&conn, "/proj/one", &bytes("helper_one")).unwrap();
    load(&conn, "/proj/two", &bytes("helper_two")).unwrap();

    let mut stmt = conn
        .prepare(
            "SELECT e.project, e.name FROM relations r JOIN entities e ON e.id = r.src_entity
             WHERE r.dst_entity = ?1 AND r.kind = 'CALLS' ORDER BY e.project",
        )
        .unwrap();
    let callers: Vec<(String, String)> = stmt
        .query_map([LOCAL_FUNC], |r| Ok((r.get(0)?, r.get(1)?)))
        .unwrap()
        .map(Result::unwrap)
        .collect();
    assert_eq!(
        callers,
        vec![
            ("/proj/one".into(), "helper_one".into()),
            ("/proj/two".into(), "helper_two".into()),
        ]
    );
}

fn index_of(docs: Vec<Document>) -> Vec<u8> {
    let mut idx = Index::new();
    idx.metadata = MessageField::some(Default::default());
    idx.documents = docs;
    idx.write_to_bytes().unwrap()
}

fn pkg_doc() -> Document {
    let mut lib = Document::new();
    lib.relative_path = "src/pkg.rs".to_string();
    lib.symbols = vec![sym(LOCAL_FUNC, "f", SymKind::Function)];
    lib.occurrences = vec![occ(
        LOCAL_FUNC,
        vec![0, 0, 0, 1],
        SymbolRole::Definition as i32,
    )];
    lib
}

fn local_rows(conn: &engraph_core::db::PooledConn) -> (i64, i64) {
    let entities = conn
        .query_row(
            "SELECT COUNT(*) FROM entities WHERE id LIKE 'local %'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    let relations = conn
        .query_row(
            "SELECT COUNT(*) FROM relations WHERE src_entity LIKE 'local %'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    (entities, relations)
}

#[test]
fn a_reindex_removes_the_locals_of_a_deleted_file() {
    let dir = tempdir().unwrap();
    let pool = open_pool(&dir.path().join("t.db")).unwrap();
    let conn = pool.get().unwrap();
    let a = || doc_with_local_zero("src/a.rs", "helper_a", 3);
    let b = || doc_with_local_zero("src/b.rs", "helper_b", 7);
    load(&conn, "/proj/demo", &index_of(vec![pkg_doc(), a(), b()])).unwrap();
    assert_eq!(local_rows(&conn), (2, 2));
    // src/a.rs is deleted: its local and the local's edge go.
    load(&conn, "/proj/demo", &index_of(vec![pkg_doc(), b()])).unwrap();
    assert_eq!(local_rows(&conn), (1, 1));
}

#[test]
fn a_load_of_another_indexer_keeps_the_locals() {
    // `engraph index --lang …` runs, or one prebuilt SCIP file per language,
    // load one project in parts. A part must not remove the locals (or their
    // edges) of an indexer that is not in it.
    let dir = tempdir().unwrap();
    let pool = open_pool(&dir.path().join("t.db")).unwrap();
    let conn = pool.get().unwrap();
    let a = doc_with_local_zero("src/a.rs", "helper_a", 3);
    load(&conn, "/proj/demo", &index_of(vec![pkg_doc(), a])).unwrap();
    assert_eq!(local_rows(&conn), (1, 1));

    let other = "other-indexer npm demo 1.0.0 src/`x.ts`/g().";
    let mut doc = Document::new();
    doc.relative_path = "src/x.ts".to_string();
    doc.symbols = vec![
        sym(other, "g", SymKind::Function),
        sym("local 0", "inner", SymKind::Variable),
    ];
    doc.occurrences = vec![
        occ(other, vec![0, 0, 0, 1], SymbolRole::Definition as i32),
        occ("local 0", vec![1, 0, 1, 1], SymbolRole::Definition as i32),
    ];
    load(&conn, "/proj/demo", &index_of(vec![doc])).unwrap();
    // The scip-test local and its edge stay; the other indexer adds its own.
    assert_eq!(local_rows(&conn), (2, 1));
}

#[test]
fn the_loader_writes_the_scheme_of_each_symbol() {
    let dir = tempdir().unwrap();
    let pool = open_pool(&dir.path().join("t.db")).unwrap();
    let conn = pool.get().unwrap();
    let a = doc_with_local_zero("src/a.rs", "helper_a", 3);
    load(&conn, "/proj/demo", &index_of(vec![pkg_doc(), a])).unwrap();
    let mut stmt = conn
        .prepare("SELECT id, scheme FROM entities ORDER BY id")
        .unwrap();
    let rows: Vec<(String, Option<String>)> = stmt
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
        .unwrap()
        .map(Result::unwrap)
        .collect();
    assert_eq!(rows.len(), 2, "{rows:?}");
    // The id of a global symbol starts with its scheme; a local gets the
    // scheme of the other symbols of its document.
    assert!(rows[0].0.starts_with("local 0@"), "{rows:?}");
    assert_eq!(rows[1].0, LOCAL_FUNC);
    for (_, scheme) in &rows {
        assert_eq!(scheme.as_deref(), Some("scip-test"));
    }
}

#[test]
fn loader_records_the_line_of_each_occurrence_edge() {
    // build_fixture: foo() calls bar() on line 5 (0-based), so line 6.
    let dir = tempdir().unwrap();
    let pool = open_pool(&dir.path().join("t.db")).unwrap();
    let conn = pool.get().unwrap();
    load(&conn, "/proj/demo", &build_fixture()).unwrap();
    let line: Option<i64> = conn
        .query_row("SELECT line FROM relations WHERE kind = 'CALLS'", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(line, Some(6));

    // An edge from SymbolInformation.relationships has no occurrence.
    let mut doc = Document::new();
    doc.relative_path = "src/impl.rs".to_string();
    let mut class = sym(
        "scip-test cargo demo 0.0.0 x/Impl#",
        "Impl",
        SymKind::Struct,
    );
    let mut rel = scip::types::Relationship::new();
    rel.symbol = "scip-test cargo demo 0.0.0 x/Trait#".to_string();
    rel.is_implementation = true;
    class.relationships.push(rel);
    doc.symbols = vec![class];
    load(&conn, "/proj/demo", &index_of(vec![doc])).unwrap();
    let line: Option<i64> = conn
        .query_row(
            "SELECT line FROM relations WHERE kind = 'IMPLEMENTS'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(line, None);
}

#[test]
fn an_anchor_defined_in_several_documents_records_no_line() {
    // A Go package is defined in each of its files, but its entity has one
    // file_path; a line from another file would point at the wrong code.
    let pkg = "scip-go gomod demo v1 `demo`/";
    let func = "scip-go gomod demo v1 `demo`/New().";
    let doc = |path: &str, call_line: i32| {
        let def = SymbolRole::Definition as i32;
        let mut d = Document::new();
        d.relative_path = path.to_string();
        d.symbols = vec![sym(pkg, "demo", SymKind::Package)];
        d.occurrences = vec![
            occ(pkg, vec![0, 8, 0, 12], def),
            occ(func, vec![call_line, 4, call_line, 7], 0),
        ];
        d
    };
    let mut a = doc("a.go", 1);
    a.symbols.push(sym(func, "New", SymKind::Function));
    a.occurrences
        .push(occ(func, vec![2, 5, 2, 8], SymbolRole::Definition as i32));
    let dir = tempdir().unwrap();
    let pool = open_pool(&dir.path().join("t.db")).unwrap();
    let conn = pool.get().unwrap();
    load(&conn, "/proj/demo", &index_of(vec![a, doc("b.go", 9)])).unwrap();
    let lines: Vec<Option<i64>> = conn
        .prepare("SELECT line FROM relations WHERE src_entity = ?1 AND kind = 'CALLS'")
        .unwrap()
        .query_map([pkg], |r| r.get(0))
        .unwrap()
        .map(Result::unwrap)
        .collect();
    assert_eq!(lines, vec![None, None]);
}

// A definition's `enclosing_range` is the whole function or class. The
// document models (0-based lines):
//
//   0  import helper            (module)
//   2  @deco                    decorator of `outer`, in its range
//   3  def outer():             range 2..8
//   4      def inner():         range 4..5
//   5          helper()         -> inner
//   6      helper()             -> outer, after the range of inner
//   8      return
//  10  helper()                 module level: no anchor (Python)
const SCOPE_MOD: &str = "scip-python python demo 0 `pkg.mod`/__init__:";
const SCOPE_OUTER: &str = "scip-python python demo 0 `pkg.mod`/outer().";
const SCOPE_INNER: &str = "scip-python python demo 0 `pkg.mod`/outer().inner().";
const SCOPE_DECO: &str = "scip-python python demo 0 `pkg.mod`/deco().";
const SCOPE_HELPER: &str = "scip-python python demo 0 `pkg.mod`/helper().";

fn scoped_def(symbol: &str, line: i32, scope: Vec<i32>) -> Occurrence {
    let mut o = occ(
        symbol,
        vec![line, 4, line, 9],
        SymbolRole::Definition as i32,
    );
    o.enclosing_range = scope;
    o
}

fn callers_by_line(conn: &engraph_core::db::PooledConn, dst: &str) -> Vec<(String, String)> {
    // (caller, callee) of every CALLS edge to `dst`, by caller.
    conn.prepare(
        "SELECT r.src_entity, r.dst_entity FROM relations r
         WHERE r.dst_entity = ?1 AND r.kind = 'CALLS' ORDER BY r.src_entity",
    )
    .unwrap()
    .query_map([dst], |r| Ok((r.get(0)?, r.get(1)?)))
    .unwrap()
    .map(Result::unwrap)
    .collect()
}

#[test]
fn an_occurrence_after_an_anchor_range_does_not_go_to_that_anchor() {
    let mut doc = Document::new();
    doc.relative_path = "pkg/mod.py".to_string();
    // scip-python: symbols without a kind.
    doc.symbols = [SCOPE_MOD, SCOPE_OUTER, SCOPE_INNER]
        .into_iter()
        .map(|symbol| {
            let mut s = SymbolInformation::new();
            s.symbol = symbol.to_string();
            s
        })
        .collect();
    doc.occurrences = vec![
        occ(SCOPE_MOD, vec![0, 0, 0], SymbolRole::Definition as i32),
        occ(SCOPE_DECO, vec![2, 1, 2, 5], 0),
        scoped_def(SCOPE_OUTER, 3, vec![2, 0, 8, 10]),
        scoped_def(SCOPE_INNER, 4, vec![4, 4, 5, 16]),
        occ(SCOPE_HELPER, vec![5, 8, 5, 14], 0),
        occ(SCOPE_HELPER, vec![6, 4, 6, 10], 0),
        occ(SCOPE_HELPER, vec![10, 0, 10, 6], 0),
    ];
    let dir = tempdir().unwrap();
    let pool = open_pool(&dir.path().join("t.db")).unwrap();
    let conn = pool.get().unwrap();
    load(&conn, "/proj/demo", &index_of(vec![doc])).unwrap();

    // The call in `inner`, and the one in `outer` after `inner` ended. The
    // module-level call has no anchor (`__init__:` is not one).
    assert_eq!(
        callers_by_line(&conn, SCOPE_HELPER),
        vec![
            (SCOPE_OUTER.to_string(), SCOPE_HELPER.to_string()),
            (SCOPE_INNER.to_string(), SCOPE_HELPER.to_string()),
        ]
    );
    // The decorator is in the range of the function it decorates.
    assert_eq!(
        callers_by_line(&conn, SCOPE_DECO),
        vec![(SCOPE_OUTER.to_string(), SCOPE_DECO.to_string())]
    );
}
