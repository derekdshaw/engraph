use anyhow::Result;
use engraph_compress::{CompressInput, CompressKind, compress};
use engraph_core::{db::PooledConn, models::EventKind, telemetry, tokens};
use std::time::Instant;

use crate::common::{COMPRESS_THRESHOLD_TOKENS, sha256};

pub struct SweepStats {
    pub rows_scanned: usize,
    pub rows_compressed: usize,
    pub bytes_before: u64,
    pub bytes_after: u64,
    pub elapsed_ms: u128,
}

/// Sweep messages and context_items, compressing in place any row whose
/// content is uncompressed AND exceeds the token threshold. Idempotent:
/// rows already marked content_compressed=1 are skipped without re-tokenizing,
/// and the sentinel check inside compress() makes accidental double-compress
/// a no-op anyway.
pub fn compress_existing(conn: &PooledConn, batch: usize) -> Result<SweepStats> {
    let start = Instant::now();
    let mut stats = SweepStats {
        rows_scanned: 0,
        rows_compressed: 0,
        bytes_before: 0,
        bytes_after: 0,
        elapsed_ms: 0,
    };

    for table in ["messages", "context_items"] {
        sweep_table(conn, table, batch, &mut stats)?;
    }

    stats.elapsed_ms = start.elapsed().as_millis();
    Ok(stats)
}

fn sweep_table(conn: &PooledConn, table: &str, batch: usize, stats: &mut SweepStats) -> Result<()> {
    // Read candidate rows in one prepared statement, then update in a loop.
    // `batch` caps the number of rows processed per call.
    let select_sql = format!(
        "SELECT rowid, id, content FROM {table}
         WHERE content_compressed = 0
         ORDER BY rowid ASC LIMIT ?1"
    );
    let update_sql = format!(
        "UPDATE {table} SET content = ?2, content_compressed = 1, content_hash = ?3 WHERE rowid = ?1"
    );

    let mut select = conn.prepare(&select_sql)?;
    let mut update = conn.prepare(&update_sql)?;

    let rows = select
        .query_map([batch as i64], |r| {
            let rowid: i64 = r.get(0)?;
            let id: String = r.get(1)?;
            let content: String = r.get(2)?;
            Ok((rowid, id, content))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;

    for (rowid, id, content) in rows {
        stats.rows_scanned += 1;
        let original_bytes = content.len() as u64;
        let original_tokens = tokens::count(&content);
        if original_tokens < COMPRESS_THRESHOLD_TOKENS {
            // Below threshold — still mark as compressed so we don't re-scan
            // the same rows every sweep.
            update.execute(rusqlite::params![
                rowid,
                content,
                sha256(content.as_bytes())
            ])?;
            continue;
        }
        let r = compress(CompressInput {
            text: &content,
            kind: CompressKind::SessionMessage,
            target_ratio: 0.5,
            brevity: false,
        });
        update.execute(rusqlite::params![
            rowid,
            &r.text,
            sha256(content.as_bytes()),
        ])?;
        let after_bytes = r.text.len() as u64;
        stats.bytes_before += original_bytes;
        stats.bytes_after += after_bytes;
        stats.rows_compressed += 1;

        telemetry::record_event(
            conn,
            telemetry::EventInput {
                session_id: None,
                kind: EventKind::Compress,
                feature: "compress_sweep",
                filter_id: Some(table),
                input_tokens: r.original_tokens as i64,
                output_tokens: r.compressed_tokens as i64,
                latency_ms: 0,
            },
        )?;
        tracing::debug!(
            id = %id,
            orig = original_tokens,
            comp = r.compressed_tokens,
            "compressed existing row"
        );
    }
    Ok(())
}
