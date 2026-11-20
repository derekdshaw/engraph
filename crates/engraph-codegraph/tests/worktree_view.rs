//! The view of a checkout (plan 10, part 3, step 13): the base graph with
//! that checkout's overlay over it, as every lookup sees it.

use engraph_codegraph::{OverlayFile, install_worktree_view, load, load_overlay, subgraph_for};
use engraph_core::db::{PooledConn, open_pool};
use protobuf::{EnumOrUnknown, Message, MessageField};
use scip::types::{
    Document, Index, Occurrence, SymbolInformation, SymbolRole, symbol_information::Kind as SymKind,
};
use tempfile::tempdir;

const PROJECT: &str = "/repo";
const A: &str = "/wt/a";
const B: &str = "/wt/b";

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

/// `ops.rs` has `caller`, which calls `helper` in the unchanged `util.rs`.
/// `variant` decides what `ops.rs` holds: the base, a worktree that renamed
/// `caller` to `renamed`, or one that only edited it.
fn fixture(variant: &str) -> Vec<u8> {
    let caller = if variant == "renamed" {
        "renamed"
    } else {
        "caller"
    };
    let mut ops = Document::new();
    ops.relative_path = "src/ops.rs".to_string();
    ops.symbols = vec![sym(&id(caller), caller, SymKind::Function)];
    ops.occurrences = vec![
        occ(&id(caller), vec![0, 0, 0, 6], SymbolRole::Definition as i32),
        occ(&id("helper"), vec![2, 4, 2, 10], 0),
    ];

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

fn ops_overlay() -> Vec<OverlayFile> {
    vec![OverlayFile {
        path: "src/ops.rs".to_string(),
        deleted: false,
    }]
}

fn names(conn: &PooledConn) -> Vec<String> {
    let mut stmt = conn
        .prepare("SELECT name FROM entities WHERE file_path IS NOT NULL ORDER BY name")
        .unwrap();
    let rows: Vec<String> = stmt
        .query_map([], |r| r.get(0))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap();
    rows
}

#[test]
fn each_worktree_sees_its_own_code_and_not_another_worktree_s() {
    let dir = tempdir().unwrap();
    let pool = open_pool(&dir.path().join("t.db")).unwrap();
    let conn = pool.get().unwrap();
    load(&conn, PROJECT, &fixture("base")).unwrap();
    // A renamed `caller` in worktree A; worktree B keeps the base version.
    load_overlay(&conn, PROJECT, A, &ops_overlay(), &fixture("renamed")).unwrap();
    load_overlay(&conn, PROJECT, B, &ops_overlay(), &fixture("base")).unwrap();

    // Worktree A: its own symbol, and the unchanged file from the base.
    install_worktree_view(&conn, PROJECT, A).unwrap();
    assert_eq!(names(&conn), vec!["helper", "renamed"]);

    // Worktree B is unaffected by A's rename.
    install_worktree_view(&conn, PROJECT, B).unwrap();
    assert_eq!(names(&conn), vec!["caller", "helper"]);

    // A checkout with no overlay reads the base.
    install_worktree_view(&conn, PROJECT, "/wt/none").unwrap();
    assert_eq!(names(&conn), vec!["caller", "helper"]);

    // The base tables themselves never changed.
    let base: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM main.entities WHERE name = 'renamed'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(base, 0);
}

#[test]
fn a_file_the_worktree_deleted_takes_its_symbols_and_their_edges() {
    let dir = tempdir().unwrap();
    let pool = open_pool(&dir.path().join("t.db")).unwrap();
    let conn = pool.get().unwrap();
    load(&conn, PROJECT, &fixture("base")).unwrap();

    // Worktree A deleted src/util.rs, where `helper` lives.
    let files = vec![OverlayFile {
        path: "src/util.rs".to_string(),
        deleted: true,
    }];
    load_overlay(&conn, PROJECT, A, &files, &fixture("base")).unwrap();
    install_worktree_view(&conn, PROJECT, A).unwrap();
    assert_eq!(names(&conn), vec!["caller"], "helper is gone from the view");

    // `caller` still has its row, and the edge to the symbol the view no
    // longer has does not reach it: the neighborhood joins the other end
    // against the view.
    let n = subgraph_for(&conn, "caller", 30).unwrap();
    assert_eq!(n.matches.len(), 1);
    assert!(
        n.outgoing.iter().all(|e| e.other.name != "helper"),
        "an edge to a deleted symbol survived: {:?}",
        n.outgoing.iter().map(|e| &e.other.name).collect::<Vec<_>>()
    );
}

#[test]
fn the_neighborhood_of_a_changed_file_comes_from_the_overlay() {
    let dir = tempdir().unwrap();
    let pool = open_pool(&dir.path().join("t.db")).unwrap();
    let conn = pool.get().unwrap();
    load(&conn, PROJECT, &fixture("base")).unwrap();
    load_overlay(&conn, PROJECT, A, &ops_overlay(), &fixture("renamed")).unwrap();
    install_worktree_view(&conn, PROJECT, A).unwrap();

    // The worktree's own name resolves...
    let n = subgraph_for(&conn, "renamed", 30).unwrap();
    assert_eq!(n.matches.len(), 1, "{:?}", n.matches);
    // ...and still calls the base symbol in the file it did not change.
    assert!(
        n.outgoing.iter().any(|e| e.other.name == "helper"),
        "{:?}",
        n.outgoing.iter().map(|e| &e.other.name).collect::<Vec<_>>()
    );

    // The name the base had is not in this worktree's view, so a lookup of it
    // finds nothing — the base row for the changed file is hidden.
    let gone = subgraph_for(&conn, "caller", 30).unwrap();
    assert!(gone.matches.is_empty(), "{:?}", gone.matches);
}

#[test]
fn the_view_is_read_only_and_leaves_the_base_reachable() {
    let dir = tempdir().unwrap();
    let pool = open_pool(&dir.path().join("t.db")).unwrap();
    let conn = pool.get().unwrap();
    load(&conn, PROJECT, &fixture("base")).unwrap();
    load_overlay(&conn, PROJECT, A, &ops_overlay(), &fixture("renamed")).unwrap();
    install_worktree_view(&conn, PROJECT, A).unwrap();

    // A write through the shadowed name is refused, which is what keeps a
    // loader off a connection with a view installed.
    assert!(
        conn.execute(
            "INSERT INTO entities (id, kind, name) VALUES ('x', 'symbol', 'x')",
            [],
        )
        .is_err()
    );
    // The base tables stay reachable by their qualified name.
    let n: i64 = conn
        .query_row("SELECT COUNT(*) FROM main.entities", [], |r| r.get(0))
        .unwrap();
    assert!(n > 0);

    // Clearing it puts the connection back on the base tables.
    engraph_codegraph::clear_worktree_view(&conn).unwrap();
    assert_eq!(names(&conn), vec!["caller", "helper"]);
}
