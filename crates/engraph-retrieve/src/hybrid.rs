//! Hybrid retrieval: Reciprocal Rank Fusion (RRF) over a lexical ranking
//! (BM25 from FTS5), a semantic ranking (cosine over stored embeddings), and
//! a recency ranking (newest `ts` first). Gated behind the `embeddings`
//! Cargo feature.
//!
//! ## Algorithm
//!
//! Pure weighted-sum hybrids — `α·BM25 + β·cosine` — are scale-broken: BM25
//! scores are unbounded positive while cosine sits in [−1, 1], so the larger
//! scale dominates regardless of weights. The fix is to combine **ranks**
//! rather than scores. For each candidate document `d`:
//!
//! ```text
//! rrf_score(d) = w_lex / (k + rank_lex(d)) + w_sem / (k + rank_sem(d)) + w_rec / (k + rank_rec(d))
//! ```
//!
//! Constants:
//! - `k = K_RRF = 60` — the classic value from Cormack, Clarke, Büttcher
//!   (SIGIR 2009). Larger `k` flattens the weighting of the top of each list.
//! - `w_lex = W_LEXICAL = 1.0` — weight applied to the BM25/FTS ranking.
//! - `w_sem = W_SEMANTIC = 1.0` — weight applied to the embedding-cosine ranking.
//! - `w_rec = W_RECENCY = 0.5` — weight applied to the recency ranking. Lower
//!   default than the content signals because freshness alone should not beat
//!   content matches; it's a tiebreaker, not a primary signal.
//!
//! Documents missing from a list contribute zero to that term (RRF handles
//! missing positions naturally — equivalent to rank = ∞). Candidates without
//! a `ts` are treated as missing from the recency list.
//!
//! ## Pipeline
//! 1. Run the FTS5 path with a `q.limit * CANDIDATE_MULT` candidate pool.
//! 2. Embed the query text once; fetch stored vectors for every candidate
//!    under the current model id; rank candidates by cosine descending.
//! 3. Rank candidates by `ts` descending (RFC3339 ISO sorts chronologically).
//! 4. Compute RRF score per candidate by combining its FTS, cosine, and
//!    recency ranks (1-based; missing → contributes 0 for that source).
//! 5. Sort by RRF score descending; stable secondary sort by `target_id`.
//! 6. Truncate to `q.limit`.

use crate::{search, Hit, Query, ScopeFilter, Strategy};
use engraph_core::{
    db::PooledConn,
    embedding::{cosine, EmbeddingProvider},
    Result,
};

/// RRF smoothing constant; standard value from the original RRF paper.
pub const K_RRF: f64 = 60.0;
/// Weight on the lexical (BM25/FTS) ranking.
pub const W_LEXICAL: f64 = 1.0;
/// Weight on the semantic (embedding-cosine) ranking.
pub const W_SEMANTIC: f64 = 1.0;
/// Weight on the recency ranking (newest `ts` first). Half the weight of the
/// content signals: freshness is a tiebreaker, not a primary criterion.
pub const W_RECENCY: f64 = 0.5;
/// Pull this many candidates from the FTS stage per output slot. Larger pools
/// give the semantic reranker more headroom at the cost of more embedding
/// lookups.
pub const CANDIDATE_MULT: usize = 4;

pub fn search_hybrid(
    conn: &PooledConn,
    q: &Query<'_>,
    provider: &dyn EmbeddingProvider,
) -> Result<Vec<Hit>> {
    // Step 1: lexical candidate pool from FTS, sorted by BM25.
    let widened = Query {
        text: q.text,
        scope: match &q.scope {
            ScopeFilter::All => ScopeFilter::All,
            ScopeFilter::Project(p) => ScopeFilter::Project(p.clone()),
            ScopeFilter::Scope(s) => ScopeFilter::Scope(s.clone()),
        },
        kinds: q.kinds,
        limit: q.limit.saturating_mul(CANDIDATE_MULT).max(q.limit),
        strategy: Strategy::Fts,
    };
    let candidates = search(conn, &widened)?;
    if candidates.is_empty() {
        return Ok(vec![]);
    }

    // Lex rank is identity-by-index: FTS returns candidates in BM25 order, so
    // candidates[i] has lex rank i + 1. No lookup table needed.

    // Step 2: embed the query once.
    let q_vec = provider.embed(q.text)?;
    let model_id = provider.model_id();

    // Step 3a: batch-fetch all candidate embeddings in one SQL round-trip
    // instead of one SELECT per candidate. SQLite 3.15+ supports
    // `(col1, col2) IN (VALUES (?,?), ...)` which lets us tuple-match
    // (target_kind, target_id) without juggling separate per-kind queries.
    let vectors: std::collections::HashMap<(String, String), Vec<u8>> = {
        let pairs: Vec<(&str, &str)> = candidates
            .iter()
            .map(|h| (target_kind_key(&h.target_kind), h.target_id.as_str()))
            .collect();
        let placeholders = std::iter::repeat("(?,?)")
            .take(pairs.len())
            .collect::<Vec<_>>()
            .join(",");
        let sql = format!(
            "SELECT target_kind, target_id, vector FROM embeddings
             WHERE model_id = ?1 AND (target_kind, target_id) IN (VALUES {placeholders})"
        );
        let mut params: Vec<rusqlite::types::Value> = Vec::with_capacity(1 + pairs.len() * 2);
        params.push(rusqlite::types::Value::Text(model_id.to_string()));
        for (kind, id) in &pairs {
            params.push(rusqlite::types::Value::Text((*kind).to_string()));
            params.push(rusqlite::types::Value::Text((*id).to_string()));
        }
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map(rusqlite::params_from_iter(params), |r| {
            Ok((
                (r.get::<_, String>(0)?, r.get::<_, String>(1)?),
                r.get::<_, Vec<u8>>(2)?,
            ))
        })?;
        rows.collect::<rusqlite::Result<std::collections::HashMap<_, _>>>()?
    };

    // Step 3b: cosine score per candidate by index; unembedded candidates rank
    // last via NEG_INFINITY.
    let mut sem_scored: Vec<(usize, f64)> = candidates
        .iter()
        .enumerate()
        .map(|(idx, h)| {
            let key = (
                target_kind_key(&h.target_kind).to_string(),
                h.target_id.clone(),
            );
            let cos = vectors
                .get(&key)
                .map(|b| cosine(&q_vec, &decode_f32_vec(b)) as f64)
                .unwrap_or(f64::NEG_INFINITY);
            (idx, cos)
        })
        .collect();
    sem_scored.sort_by(|a, b| {
        b.1.partial_cmp(&a.1)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| candidates[a.0].target_id.cmp(&candidates[b.0].target_id))
    });
    // Only assign a rank to candidates that actually had an embedding; rest are "absent".
    let sem_rank: std::collections::HashMap<usize, usize> = sem_scored
        .iter()
        .enumerate()
        .filter(|(_, (_, score))| score.is_finite())
        .map(|(rank, (idx, _))| (*idx, rank + 1))
        .collect();

    // Step 3c: recency rank. Newest `ts` first; RFC3339 strings sort
    // chronologically, so a lexicographic descending sort is correct.
    // Candidates without a `ts` are absent from the recency list (rank = ∞).
    let mut rec_scored: Vec<(usize, &str)> = candidates
        .iter()
        .enumerate()
        .filter_map(|(i, h)| h.ts.as_deref().map(|ts| (i, ts)))
        .collect();
    rec_scored.sort_by(|a, b| {
        b.1.cmp(a.1)
            .then_with(|| candidates[a.0].target_id.cmp(&candidates[b.0].target_id))
    });
    let rec_rank: std::collections::HashMap<usize, usize> = rec_scored
        .into_iter()
        .enumerate()
        .map(|(rank, (idx, _))| (idx, rank + 1))
        .collect();

    // Step 4: RRF score per candidate. Missing source → 0 contribution.
    let mut hits: Vec<Hit> = candidates
        .into_iter()
        .enumerate()
        .map(|(idx, mut h)| {
            let l = W_LEXICAL / (K_RRF + (idx + 1) as f64);
            let s = sem_rank
                .get(&idx)
                .map(|r| W_SEMANTIC / (K_RRF + *r as f64))
                .unwrap_or(0.0);
            let r = rec_rank
                .get(&idx)
                .map(|r| W_RECENCY / (K_RRF + *r as f64))
                .unwrap_or(0.0);
            h.score = l + s + r;
            h
        })
        .collect();
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
