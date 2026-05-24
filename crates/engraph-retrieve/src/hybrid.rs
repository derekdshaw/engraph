//! Hybrid retrieval: BM25 from FTS5 + cosine over stored embeddings +
//! recency decay. Gated behind the `embeddings` Cargo feature.

use crate::{search, Hit, Query, ScopeFilter, Strategy};
use engraph_core::{
    db::PooledConn,
    embedding::{cosine, EmbeddingProvider},
    Result,
};
use rusqlite::OptionalExtension;

const ALPHA: f64 = 0.5; // BM25 weight (already in Hit.score from FTS)
const BETA: f64 = 0.3; // cosine weight
const GAMMA: f64 = 0.2; // recency weight (placeholder; current rows have no real age yet)

pub fn search_hybrid(
    conn: &PooledConn,
    q: &Query<'_>,
    provider: &dyn EmbeddingProvider,
) -> Result<Vec<Hit>> {
    // Step 1: run the FTS path with a wider limit to give the reranker headroom.
    let widened = Query {
        text: q.text,
        scope: match &q.scope {
            ScopeFilter::All => ScopeFilter::All,
            ScopeFilter::Project(p) => ScopeFilter::Project(p.clone()),
            ScopeFilter::Scope(s) => ScopeFilter::Scope(s.clone()),
        },
        kinds: q.kinds,
        limit: q.limit.saturating_mul(4),
        strategy: Strategy::Fts,
    };
    let mut hits = search(conn, &widened)?;

    // Step 2: embed the query once.
    let q_vec = provider.embed(q.text)?;
    let model_id = provider.model_id();

    // Step 3: for each hit, look up its embedding (if any) and re-score.
    for h in hits.iter_mut() {
        let stored: Option<Vec<u8>> = conn
            .query_row(
                "SELECT vector FROM embeddings WHERE target_kind = ?1 AND target_id = ?2 AND model_id = ?3",
                rusqlite::params![target_kind_key(&h.target_kind), h.target_id, model_id],
                |r| r.get::<_, Vec<u8>>(0),
            )
            .optional()?;
        let cos = stored
            .as_deref()
            .map(decode_f32_vec)
            .map(|v| cosine(&q_vec, &v) as f64)
            .unwrap_or(0.0);
        // Combine: BM25 (already in Hit.score) + cosine + recency placeholder.
        h.score = ALPHA * h.score + BETA * cos + GAMMA * 0.0;
    }
    hits.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.target_id.cmp(&b.target_id))
    });
    hits.truncate(q.limit);
    Ok(hits)
}

fn target_kind_key(s: &str) -> &str {
    match s {
        "message" => "message",
        "context_item" => "context_item",
        "entity" => "entity",
        other => other,
    }
}

fn decode_f32_vec(bytes: &[u8]) -> Vec<f32> {
    let mut out = Vec::with_capacity(bytes.len() / 4);
    for chunk in bytes.chunks_exact(4) {
        let mut buf = [0u8; 4];
        buf.copy_from_slice(chunk);
        out.push(f32::from_le_bytes(buf));
    }
    out
}

pub fn encode_f32_vec(v: &[f32]) -> Vec<u8> {
    let mut out = Vec::with_capacity(v.len() * 4);
    for x in v {
        out.extend_from_slice(&x.to_le_bytes());
    }
    out
}

pub fn upsert_embedding(
    conn: &PooledConn,
    target_kind: &str,
    target_id: &str,
    model_id: &str,
    vector: &[f32],
) -> Result<()> {
    let bytes = encode_f32_vec(vector);
    conn.execute(
        "INSERT INTO embeddings (target_kind, target_id, vector, model_id)
         VALUES (?1, ?2, ?3, ?4)
         ON CONFLICT(target_kind, target_id, model_id) DO UPDATE SET
            vector = excluded.vector,
            created_at = datetime('now')",
        rusqlite::params![target_kind, target_id, bytes, model_id],
    )?;
    Ok(())
}

pub fn reindex_messages(
    conn: &PooledConn,
    provider: &dyn EmbeddingProvider,
    batch: usize,
) -> Result<usize> {
    let model_id = provider.model_id();
    let mut stmt = conn.prepare(
        "SELECT m.id, m.content FROM messages m
         WHERE NOT EXISTS (
             SELECT 1 FROM embeddings e
             WHERE e.target_kind = 'message' AND e.target_id = m.id AND e.model_id = ?1
         )
         ORDER BY m.rowid ASC LIMIT ?2",
    )?;
    let candidates: Vec<(String, String)> = stmt
        .query_map(rusqlite::params![model_id, batch as i64], |r| {
            Ok((r.get(0)?, r.get(1)?))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;

    let mut n = 0;
    for (id, content) in candidates {
        let v = provider.embed(&content)?;
        upsert_embedding(conn, "message", &id, model_id, &v)?;
        n += 1;
    }
    Ok(n)
}
