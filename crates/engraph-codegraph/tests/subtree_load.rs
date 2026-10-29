//! Partial refreshes of one project's codegraph: a load replaces only what it
//! could have produced — symbols from the indexers (SCIP schemes) that ran,
//! under the subtree that was indexed — so sub-projects of a monorepo merge
//! into one repo index, and a language indexed by another run survives.

use engraph_codegraph::{IndexScope, index_repo, index_repo_in};
use engraph_core::db::{PooledConn, open_pool};
use protobuf::{EnumOrUnknown, Message, MessageField};
use scip::types::{
    Document, Index, Occurrence, SymbolInformation, SymbolRole, symbol_information::Kind as SymKind,
};
use std::path::Path;
use tempfile::tempdir;

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

/// One document at `path`: `caller` (line 0) calls `callee` (line 10) from
/// line 5, plus an edgeless `lonely` at line 20. Symbols are
/// `<scheme> cargo <pkg> 0.0.0 <name>().`.
fn doc(path: &str, scheme: &str, pkg: &str, names: [&str; 3]) -> Document {
    let id = |n: &str| format!("{scheme} cargo {pkg} 0.0.0 {n}().");
    let [caller, callee, lonely] = names;
    let mut d = Document::new();
    d.relative_path = path.to_string();
    d.symbols = vec![
        sym(&id(caller), caller),
        sym(&id(callee), callee),
        sym(&id(lonely), lonely),
    ];
    d.occurrences = vec![
        occ(&id(caller), vec![0, 0, 0, 3], SymbolRole::Definition as i32),
        occ(
            &id(callee),
            vec![10, 0, 10, 3],
            SymbolRole::Definition as i32,
        ),
        occ(
            &id(lonely),
            vec![20, 0, 20, 3],
            SymbolRole::Definition as i32,
        ),
        occ(&id(callee), vec![5, 4, 5, 7], 0),
    ];
    d
}

fn scip(docs: Vec<Document>) -> Vec<u8> {
    let mut idx = Index::new();
    idx.metadata = MessageField::some(Default::default());
    idx.documents = docs;
    idx.write_to_bytes().unwrap()
}

fn write(dir: &Path, name: &str, bytes: Vec<u8>) -> std::path::PathBuf {
    let p = dir.join(name);
    std::fs::write(&p, bytes).unwrap();
    p
}

fn names(conn: &PooledConn) -> Vec<String> {
    let mut stmt = conn
        .prepare("SELECT name FROM entities WHERE project = '/repo' ORDER BY name")
        .unwrap();
    stmt.query_map([], |r| r.get(0))
        .unwrap()
        .map(Result::unwrap)
        .collect()
}

fn file_of(conn: &PooledConn, name: &str) -> String {
    conn.query_row(
        "SELECT file_path FROM entities WHERE name = ?1",
        [name],
        |r| r.get(0),
    )
    .unwrap()
}

fn relation_count(conn: &PooledConn) -> i64 {
    conn.query_row("SELECT COUNT(*) FROM relations", [], |r| r.get(0))
        .unwrap()
}

fn sub<'a>(subtree: &'a str) -> IndexScope<'a> {
    IndexScope {
        project: "/repo",
        subtree,
    }
}

#[test]
fn subtree_loads_merge_into_one_project_with_rebased_paths() {
    let dir = tempdir().unwrap();
    let pool = open_pool(&dir.path().join("eg.db")).unwrap();
    let conn = pool.get().unwrap();
    let a = write(
        dir.path(),
        "a.scip",
        scip(vec![doc(
            "src/lib.rs",
            "rust-analyzer",
            "svc-a",
            ["a1", "a2", "a3"],
        )]),
    );
    let b = write(
        dir.path(),
        "b.scip",
        scip(vec![doc(
            "src/lib.rs",
            "rust-analyzer",
            "svc-b",
            ["b1", "b2", "b3"],
        )]),
    );

    index_repo_in(&conn, dir.path(), Some(&a), None, sub("svc-a"), false, true).unwrap();
    index_repo_in(&conn, dir.path(), Some(&b), None, sub("svc-b"), false, true).unwrap();

    // svc-b's run (GC + prune on) must leave svc-a alone, edgeless a3 included.
    assert_eq!(names(&conn), ["a1", "a2", "a3", "b1", "b2", "b3"]);
    assert_eq!(file_of(&conn, "a1"), "svc-a/src/lib.rs");
    assert_eq!(file_of(&conn, "b1"), "svc-b/src/lib.rs");
}

#[test]
fn subtree_reindex_prunes_only_its_own_subtree() {
    let dir = tempdir().unwrap();
    let pool = open_pool(&dir.path().join("eg.db")).unwrap();
    let conn = pool.get().unwrap();
    let a = write(
        dir.path(),
        "a.scip",
        scip(vec![doc(
            "src/lib.rs",
            "rust-analyzer",
            "svc-a",
            ["a1", "a2", "a3"],
        )]),
    );
    let b = write(
        dir.path(),
        "b.scip",
        scip(vec![doc(
            "src/lib.rs",
            "rust-analyzer",
            "svc-b",
            ["b1", "b2", "b3"],
        )]),
    );
    index_repo_in(&conn, dir.path(), Some(&a), None, sub("svc-a"), false, true).unwrap();
    index_repo_in(&conn, dir.path(), Some(&b), None, sub("svc-b"), false, true).unwrap();

    // a3 deleted from svc-a's source.
    let mut d = doc("src/lib.rs", "rust-analyzer", "svc-a", ["a1", "a2", "a3"]);
    d.symbols.pop();
    d.occurrences.remove(2);
    let a2 = write(dir.path(), "a2.scip", scip(vec![d]));
    index_repo_in(
        &conn,
        dir.path(),
        Some(&a2),
        None,
        sub("svc-a"),
        false,
        true,
    )
    .unwrap();
    assert_eq!(names(&conn), ["a1", "a2", "b1", "b2", "b3"]);
}

#[test]
fn a_load_leaves_other_indexers_symbols_alone() {
    let dir = tempdir().unwrap();
    let pool = open_pool(&dir.path().join("eg.db")).unwrap();
    let conn = pool.get().unwrap();
    let rust = write(
        dir.path(),
        "r.scip",
        scip(vec![doc(
            "src/lib.rs",
            "rust-analyzer",
            "app",
            ["r1", "r2", "r3"],
        )]),
    );
    let ts = write(
        dir.path(),
        "t.scip",
        scip(vec![doc(
            "web/app.ts",
            "scip-typescript",
            "app",
            ["t1", "t2", "t3"],
        )]),
    );
    index_repo(&conn, dir.path(), Some(&ts), None, "/repo", false, true).unwrap();
    let before = relation_count(&conn);
    // A Rust-only run of the whole repo keeps the TypeScript symbols and edges.
    index_repo(&conn, dir.path(), Some(&rust), None, "/repo", false, true).unwrap();
    assert_eq!(names(&conn), ["r1", "r2", "r3", "t1", "t2", "t3"]);
    assert_eq!(relation_count(&conn), before * 2);
}

#[test]
fn root_and_subtree_runs_over_the_same_code_converge() {
    let dir = tempdir().unwrap();
    let pool = open_pool(&dir.path().join("eg.db")).unwrap();
    let conn = pool.get().unwrap();
    let root = write(
        dir.path(),
        "root.scip",
        scip(vec![
            doc(
                "svc-a/src/lib.rs",
                "rust-analyzer",
                "svc-a",
                ["a1", "a2", "a3"],
            ),
            doc(
                "svc-b/src/lib.rs",
                "rust-analyzer",
                "svc-b",
                ["b1", "b2", "b3"],
            ),
        ]),
    );
    let a = write(
        dir.path(),
        "a.scip",
        scip(vec![doc(
            "src/lib.rs",
            "rust-analyzer",
            "svc-a",
            ["a1", "a2", "a3"],
        )]),
    );
    index_repo(&conn, dir.path(), Some(&root), None, "/repo", false, true).unwrap();
    let (n, r) = (names(&conn), relation_count(&conn));
    index_repo_in(&conn, dir.path(), Some(&a), None, sub("svc-a"), false, true).unwrap();
    assert_eq!((names(&conn), relation_count(&conn)), (n.clone(), r));
    index_repo(&conn, dir.path(), Some(&root), None, "/repo", false, true).unwrap();
    assert_eq!((names(&conn), relation_count(&conn)), (n, r));
    assert_eq!(file_of(&conn, "a1"), "svc-a/src/lib.rs");
}

#[test]
fn an_empty_index_prunes_nothing() {
    let dir = tempdir().unwrap();
    let pool = open_pool(&dir.path().join("eg.db")).unwrap();
    let conn = pool.get().unwrap();
    let full = write(
        dir.path(),
        "f.scip",
        scip(vec![doc(
            "src/lib.rs",
            "rust-analyzer",
            "app",
            ["r1", "r2", "r3"],
        )]),
    );
    let empty = write(dir.path(), "e.scip", scip(vec![]));
    index_repo(&conn, dir.path(), Some(&full), None, "/repo", false, true).unwrap();
    index_repo(&conn, dir.path(), Some(&empty), None, "/repo", false, true).unwrap();
    assert_eq!(names(&conn), ["r1", "r2", "r3"]);
}
