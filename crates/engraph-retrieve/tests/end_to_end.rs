//! End-to-end retrieval gates from the Phase 3 plan:
//! - Ingest a known JSONL file; recall <known phrase> returns the message
//! - Scoped query returns only in-scope hits
//! - KG: insert one relation; recall --kind entity <name> returns it

use engraph_core::db::open_pool;
use engraph_ingest::ingest_file;
use engraph_retrieve::{scope, search, Query, ScopeFilter, Target};
use std::io::Write;
use tempfile::tempdir;
use uuid::Uuid;

fn jsonl_with_phrase(phrase: &str, cwd: &str, session: &str) -> String {
    format!(
        r#"{{"type":"user","sessionId":"{session}","cwd":"{cwd}","timestamp":"2026-05-24T00:00:00Z","uuid":"{}","message":{{"role":"user","content":"{phrase}"}}}}
"#,
        Uuid::now_v7()
    )
}

#[test]
fn ingest_then_recall_finds_known_phrase() {
    let dir = tempdir().unwrap();
    let pool = open_pool(&dir.path().join("e2e.db")).unwrap();
    let conn = pool.get().unwrap();

    let jp = dir.path().join("t.jsonl");
    let mut f = std::fs::File::create(&jp).unwrap();
    write!(
        f,
        "{}",
        jsonl_with_phrase("the rare token xyzzyplover appears here", "/proj", "s1")
    )
    .unwrap();
    drop(f);

    ingest_file(&conn, &jp).unwrap();

    let hits = search(
        &conn,
        &Query::new("xyzzyplover"),
    )
    .unwrap();
    assert_eq!(hits.len(), 1, "expected exactly one hit");
    assert!(hits[0].preview.to_lowercase().contains("xyzzyplover"));
    assert_eq!(hits[0].target_kind, "message");
}

#[test]
fn scoped_query_restricts_to_project() {
    let dir = tempdir().unwrap();
    let pool = open_pool(&dir.path().join("scope.db")).unwrap();
    let conn = pool.get().unwrap();

    let jp = dir.path().join("t.jsonl");
    let mut f = std::fs::File::create(&jp).unwrap();
    write!(f, "{}", jsonl_with_phrase("alpha bravo charlie", "/projA", "sA")).unwrap();
    write!(f, "{}", jsonl_with_phrase("alpha delta echo", "/projB", "sB")).unwrap();
    drop(f);
    ingest_file(&conn, &jp).unwrap();

    let q = Query {
        text: "alpha",
        scope: ScopeFilter::Project("/projA".to_string()),
        kinds: &[Target::Messages],
        limit: 10,
        strategy: Default::default(),
    };
    let hits = search(&conn, &q).unwrap();
    assert_eq!(hits.len(), 1);
    assert!(hits[0].preview.contains("bravo"));
}

#[test]
fn entity_search_returns_inserted_relation_endpoint() {
    let dir = tempdir().unwrap();
    let pool = open_pool(&dir.path().join("kg.db")).unwrap();
    let conn = pool.get().unwrap();

    let scope_id = scope::ensure_project_scope(&conn, "/proj").unwrap();
    let _ = scope_id;

    // Insert two entities and one relation between them
    let e1 = Uuid::now_v7().to_string();
    let e2 = Uuid::now_v7().to_string();
    conn.execute(
        "INSERT INTO entities (id, kind, name, project) VALUES (?1, 'symbol', 'unique_function_name_xyz', '/proj')",
        rusqlite::params![e1],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO entities (id, kind, name, project) VALUES (?1, 'symbol', 'caller', '/proj')",
        rusqlite::params![e2],
    )
    .unwrap();
    let rel = Uuid::now_v7().to_string();
    conn.execute(
        "INSERT INTO relations (id, src_entity, dst_entity, kind) VALUES (?1, ?2, ?3, 'calls')",
        rusqlite::params![rel, e2, e1],
    )
    .unwrap();

    let q = Query {
        text: "unique_function_name_xyz",
        scope: ScopeFilter::All,
        kinds: &[Target::Entities],
        limit: 10,
        strategy: Default::default(),
    };
    let hits = search(&conn, &q).unwrap();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].target_kind, "entity");
    assert!(hits[0].preview.contains("unique_function_name_xyz"));
}
