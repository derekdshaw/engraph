//! Hybrid retrieval smoke test using a deterministic mock provider.
//! Gated behind the `embeddings` feature.

#![cfg(feature = "embeddings")]

use engraph_core::{db::open_pool, embedding::EmbeddingProvider, Result};
use engraph_retrieve::{
    hybrid::{reindex_messages, search_hybrid, upsert_embedding},
    Query, ScopeFilter, Strategy, Target,
};
use tempfile::tempdir;

struct MockProvider;
impl EmbeddingProvider for MockProvider {
    fn embed(&self, text: &str) -> Result<Vec<f32>> {
        // Tiny deterministic vector: presence of a few keywords drives axes.
        let lower = text.to_lowercase();
        Ok(vec![
            if lower.contains("auth") || lower.contains("login") { 1.0 } else { 0.0 },
            if lower.contains("database") || lower.contains("sql") { 1.0 } else { 0.0 },
            if lower.contains("ui") || lower.contains("css") { 1.0 } else { 0.0 },
        ])
    }
    fn model_id(&self) -> &str {
        "mock-3dim"
    }
    fn dim(&self) -> usize {
        3
    }
}

#[test]
fn hybrid_reranks_via_cosine() {
    let dir = tempdir().unwrap();
    let pool = open_pool(&dir.path().join("hyb.db")).unwrap();
    let conn = pool.get().unwrap();

    // Insert three messages and embed them under the mock model.
    let rows = [
        ("m1", "the cat sat on the mat unrelated"),
        ("m2", "login flow needs auth fix"),
        ("m3", "database query in sql"),
    ];
    for (id, content) in rows {
        conn.execute(
            "INSERT INTO sessions (id, project, cwd, started_at) VALUES ('s','/p','/p','t')",
            [],
        )
        .ok();
        conn.execute(
            "INSERT INTO messages (id, session_id, role, content, ts) VALUES (?1,'s','user',?2,'t')",
            rusqlite::params![id, content],
        )
        .unwrap();
    }

    let provider = MockProvider;
    let n = reindex_messages(&conn, &provider, 100).unwrap();
    assert_eq!(n, 3);

    // FTS query "auth" finds m2 via keyword. Hybrid reranks to also boost m2
    // via the cosine match on the auth axis.
    let hits_fts = engraph_retrieve::search(
        &conn,
        &Query {
            text: "auth",
            scope: ScopeFilter::All,
            kinds: &[Target::Messages],
            limit: 5,
            strategy: Strategy::Fts,
        },
    )
    .unwrap();
    assert!(hits_fts.iter().any(|h| h.target_id == "m2"));

    let hits_hybrid = search_hybrid(
        &conn,
        &Query {
            text: "auth",
            scope: ScopeFilter::All,
            kinds: &[Target::Messages],
            limit: 5,
            strategy: Strategy::Hybrid,
        },
        &provider,
    )
    .unwrap();
    assert_eq!(hits_hybrid[0].target_id, "m2", "expected m2 at top, got: {hits_hybrid:?}");
}

#[test]
fn upsert_is_idempotent() {
    let dir = tempdir().unwrap();
    let pool = open_pool(&dir.path().join("up.db")).unwrap();
    let conn = pool.get().unwrap();
    let v = vec![1.0, 2.0, 3.0];
    upsert_embedding(&conn, "message", "x", "mock", &v).unwrap();
    upsert_embedding(&conn, "message", "x", "mock", &v).unwrap();
    let count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM embeddings WHERE target_kind='message' AND target_id='x'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(count, 1);
}
