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
        let lower = text.to_lowercase();
        let auth = ["auth", "login", "password", "oauth"]
            .iter()
            .any(|w| lower.contains(*w));
        let db = ["database", "sql", "schema"]
            .iter()
            .any(|w| lower.contains(*w));
        let ui = ["ui", "css", "layout"].iter().any(|w| lower.contains(*w));
        let topic = lower.contains("engineer") || lower.contains("note");
        Ok(vec![
            if auth { 1.0 } else { 0.0 },
            if db { 1.0 } else { 0.0 },
            if ui { 1.0 } else { 0.0 },
            if topic { 1.0 } else { 0.0 },
        ])
    }
    fn model_id(&self) -> &str {
        "mock-4dim"
    }
    fn dim(&self) -> usize {
        4
    }
}

#[test]
fn hybrid_reorders_vs_fts() {
    let dir = tempdir().unwrap();
    let pool = open_pool(&dir.path().join("hyb.db")).unwrap();
    let conn = pool.get().unwrap();
    conn.execute(
        "INSERT INTO sessions (id, project, cwd, started_at) VALUES ('s','/p','/p','t')",
        [],
    )
    .unwrap();

    // All four rows contain "engineer", so FTS returns all four ranked by BM25
    // (shorter docs rank higher under length normalization). The pure-topic row
    // m_pure aligns most cleanly with the query's semantic axes; rows that
    // carry off-axis concepts (db, ui) have higher-dimensional vectors and a
    // lower cosine to the query. RRF surfaces m_pure ahead of the FTS winner.
    let rows = [
        ("m_pure", "engineer note about cooking"),
        ("m_db", "engineer database hello world"),
        ("m_ui", "engineer ui css today"),
        ("m_short", "engineer brief"),
    ];
    for (id, content) in rows {
        conn.execute(
            "INSERT INTO messages (id, session_id, role, content, ts) VALUES (?1,'s','user',?2,'t')",
            rusqlite::params![id, content],
        )
        .unwrap();
    }
    let provider = MockProvider;
    assert_eq!(reindex_messages(&conn, &provider, 100).unwrap(), 4);

    let q = Query {
        text: "engineer",
        scope: ScopeFilter::All,
        kinds: &[Target::Messages],
        limit: 4,
        strategy: Strategy::Hybrid,
    };
    let hits_fts = engraph_retrieve::search(
        &conn,
        &Query { strategy: Strategy::Fts, ..q.clone() },
    )
    .unwrap();
    let hits_hybrid = search_hybrid(&conn, &q, &provider).unwrap();

    // FTS ranks shortest (m_short) first by length normalization.
    assert_eq!(
        hits_fts[0].target_id, "m_short",
        "FTS should put the shortest doc first; got {hits_fts:?}"
    );
    // Hybrid surfaces the row whose vector aligns purely with the query axes
    // (m_pure has only the topic axis lit; m_db and m_ui carry an extra axis
    // that hurts cosine against the topic-only query).
    let pure_pos = hits_hybrid
        .iter()
        .position(|h| h.target_id == "m_pure")
        .expect("m_pure missing");
    let db_pos = hits_hybrid
        .iter()
        .position(|h| h.target_id == "m_db")
        .expect("m_db missing");
    let ui_pos = hits_hybrid
        .iter()
        .position(|h| h.target_id == "m_ui")
        .expect("m_ui missing");
    assert!(
        pure_pos < db_pos && pure_pos < ui_pos,
        "m_pure should outrank off-axis rows; got {hits_hybrid:?}"
    );

    // RRF score bound: with W_LEXICAL = W_SEMANTIC = 1 and best rank = 1,
    // any score is at most 2 / (K_RRF + 1).
    let max_rrf = 2.0 / (engraph_retrieve::hybrid::K_RRF + 1.0);
    for h in &hits_hybrid {
        assert!(h.score <= max_rrf + 1e-9, "score {} > {}", h.score, max_rrf);
    }
}

#[test]
fn hybrid_handles_unembedded_candidates() {
    let dir = tempdir().unwrap();
    let pool = open_pool(&dir.path().join("partial.db")).unwrap();
    let conn = pool.get().unwrap();
    conn.execute(
        "INSERT INTO sessions (id, project, cwd, started_at) VALUES ('s','/p','/p','t')",
        [],
    )
    .unwrap();
    for (id, content) in [
        ("u1", "login problem one"),
        ("u2", "login problem two"),
    ] {
        conn.execute(
            "INSERT INTO messages (id, session_id, role, content, ts) VALUES (?1,'s','user',?2,'t')",
            rusqlite::params![id, content],
        )
        .unwrap();
    }
    // Note: deliberately skip reindex_messages — neither row has an embedding.
    // Hybrid must still return results purely off the lexical signal.
    let provider = MockProvider;
    let hits = search_hybrid(
        &conn,
        &Query {
            text: "login",
            scope: ScopeFilter::All,
            kinds: &[Target::Messages],
            limit: 5,
            strategy: Strategy::Hybrid,
        },
        &provider,
    )
    .unwrap();
    assert_eq!(hits.len(), 2);
    assert!(hits.iter().all(|h| h.score > 0.0));
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
