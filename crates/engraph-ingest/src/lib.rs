//! Claude Code / Codex JSONL transcript → SQLite ingestion.
//!
//! Parses the subset of events that carry textual content (user / assistant
//! messages), populates `sessions` and `messages`, compresses oversized
//! messages via `engraph-compress` during ingest, derives a project scope from
//! `cwd`, and tracks file offsets in `ingestion_log` for incremental re-runs.

mod common;
mod ingest;
mod sweep;

pub use common::COMPRESS_THRESHOLD_TOKENS;
pub use ingest::{IngestStats, ingest_file};
pub use sweep::{SweepStats, compress_existing};
#[cfg(test)]
mod tests {
    use super::*;
    use crate::common::sha256;
    use engraph_core::db::open_pool;
    use std::fs::File;
    use std::io::Write;
    use std::path::Path;
    use tempfile::tempdir;

    fn write_jsonl(path: &Path, lines: &[&str]) {
        let mut f = File::create(path).unwrap();
        for l in lines {
            writeln!(f, "{l}").unwrap();
        }
    }

    #[test]
    fn ingest_minimal_user_assistant() {
        let dir = tempdir().unwrap();
        let pool = open_pool(&dir.path().join("ingest.db")).unwrap();
        let conn = pool.get().unwrap();
        let jp = dir.path().join("t.jsonl");
        write_jsonl(
            &jp,
            &[
                r#"{"type":"user","sessionId":"s1","cwd":"/proj","timestamp":"2026-05-24T00:00:00Z","uuid":"u1","message":{"role":"user","content":"hello world"}}"#,
                r#"{"type":"assistant","sessionId":"s1","cwd":"/proj","timestamp":"2026-05-24T00:00:01Z","uuid":"a1","message":{"role":"assistant","content":[{"type":"text","text":"hi there"}]}}"#,
                r#"{"type":"tool_use","sessionId":"s1"}"#,
            ],
        );
        let stats = ingest_file(&conn, &jp).unwrap();
        assert_eq!(stats.messages_inserted, 2);

        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM messages", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 2);
    }

    #[test]
    fn ingest_codex_rollout() {
        let dir = tempdir().unwrap();
        let pool = open_pool(&dir.path().join("codex.db")).unwrap();
        let conn = pool.get().unwrap();
        let jp = dir.path().join("rollout.jsonl");
        write_jsonl(
            &jp,
            &[
                r#"{"timestamp":"2025-09-24T18:20:50.392Z","type":"session_meta","payload":{"id":"cx1","cwd":"/proj","git":{"branch":"main"}}}"#,
                r#"{"timestamp":"2025-09-24T18:21:48.721Z","type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"examine the file"}]}}"#,
                // event_msg duplicates the user turn — must be skipped.
                r#"{"timestamp":"2025-09-24T18:21:48.722Z","type":"event_msg","payload":{"type":"user_message","message":"examine the file","kind":"plain"}}"#,
                r#"{"timestamp":"2025-09-24T18:21:48.722Z","type":"turn_context","payload":{"cwd":"/proj","model":"gpt-5-codex"}}"#,
                r#"{"timestamp":"2025-09-24T18:22:08.851Z","type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"nodes are heap-allocated"}]}}"#,
            ],
        );
        let stats = ingest_file(&conn, &jp).unwrap();
        assert_eq!(
            stats.messages_inserted, 2,
            "two messages, event_msg skipped"
        );

        // Session metadata comes from the header, not per-message.
        let (sid, cwd): (String, String) = conn
            .query_row("SELECT id, cwd FROM sessions", [], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })
            .unwrap();
        assert_eq!(sid, "cx1");
        assert_eq!(cwd, "/proj");

        // Re-ingest from offset 0 (drop the log) must not duplicate rows: Codex
        // msg_ids are derived deterministically from the byte offset, so the
        // replayed lines collide with the existing PKs under INSERT OR IGNORE.
        // (`messages_inserted` counts insert *attempts*, so the row COUNT — not
        // the stat — is what proves dedup.)
        conn.execute("DELETE FROM ingestion_log", []).unwrap();
        ingest_file(&conn, &jp).unwrap();
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM messages", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 2, "deterministic ids dedupe the replay");
    }

    #[test]
    fn ingest_detects_truncation_and_replays() {
        let dir = tempdir().unwrap();
        let pool = open_pool(&dir.path().join("rot.db")).unwrap();
        let conn = pool.get().unwrap();
        let jp = dir.path().join("t.jsonl");

        // Initial write of 2 messages.
        write_jsonl(
            &jp,
            &[
                r#"{"type":"user","sessionId":"s","cwd":"/p","timestamp":"t","uuid":"old1","message":{"role":"user","content":"original first"}}"#,
                r#"{"type":"user","sessionId":"s","cwd":"/p","timestamp":"t","uuid":"old2","message":{"role":"user","content":"original second"}}"#,
            ],
        );
        let s1 = ingest_file(&conn, &jp).unwrap();
        assert_eq!(s1.messages_inserted, 2);

        // Overwrite (truncating) with a single shorter line.
        write_jsonl(
            &jp,
            &[
                r#"{"type":"user","sessionId":"s","cwd":"/p","timestamp":"t","uuid":"new1","message":{"role":"user","content":"replacement only"}}"#,
            ],
        );
        // Without rotation handling, this would return 0 inserted; with the fix,
        // the new content gets re-ingested from offset 0.
        let s2 = ingest_file(&conn, &jp).unwrap();
        assert_eq!(
            s2.messages_inserted, 1,
            "rotation should replay the new file"
        );

        let total: i64 = conn
            .query_row("SELECT COUNT(*) FROM messages", [], |r| r.get(0))
            .unwrap();
        // Old rows remain (INSERT OR IGNORE keeps them); the new row joins.
        assert_eq!(total, 3);
    }

    #[test]
    fn ingest_appends_without_rescanning() {
        let dir = tempdir().unwrap();
        let pool = open_pool(&dir.path().join("app.db")).unwrap();
        let conn = pool.get().unwrap();
        let jp = dir.path().join("t.jsonl");
        write_jsonl(
            &jp,
            &[
                r#"{"type":"user","sessionId":"s","cwd":"/p","timestamp":"t","uuid":"a","message":{"role":"user","content":"one"}}"#,
            ],
        );
        let s1 = ingest_file(&conn, &jp).unwrap();
        assert_eq!(s1.messages_inserted, 1);

        // True append (growth) — not a rotation; re-ingest only sees the new line.
        let mut f = std::fs::OpenOptions::new().append(true).open(&jp).unwrap();
        writeln!(
            f,
            r#"{{"type":"user","sessionId":"s","cwd":"/p","timestamp":"t","uuid":"b","message":{{"role":"user","content":"two"}}}}"#
        )
        .unwrap();
        drop(f);
        let s2 = ingest_file(&conn, &jp).unwrap();
        assert_eq!(s2.messages_inserted, 1);
        assert!(s2.bytes_read < s1.bytes_read + 200);
    }

    #[test]
    fn compress_existing_only_touches_large_uncompressed_rows() {
        let dir = tempdir().unwrap();
        let pool = open_pool(&dir.path().join("sweep.db")).unwrap();
        let conn = pool.get().unwrap();
        // Insert one small row (below threshold) and one large row (above).
        let small = "hello world".to_string();
        // Diverse English sentences to ensure > COMPRESS_THRESHOLD_TOKENS (2000).
        let mut large = String::new();
        for i in 0..400 {
            large.push_str(&format!(
                "Note {i}: the engineer reviewed the proposal and recorded decision number {i} with rationale about scaling and observability tradeoffs.\n",
            ));
        }
        conn.execute(
            "INSERT INTO sessions (id, project, cwd, started_at) VALUES ('s', '/p', '/p', 't')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO messages (id, session_id, role, content, ts) VALUES ('m1','s','user',?1,'t')",
            [&small],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO messages (id, session_id, role, content, ts) VALUES ('m2','s','user',?1,'t')",
            [&large],
        )
        .unwrap();
        let stats = compress_existing(&conn, 100).unwrap();
        assert_eq!(stats.rows_scanned, 2);
        assert_eq!(
            stats.rows_compressed, 1,
            "only the large row should compress"
        );
        assert!(stats.bytes_after < stats.bytes_before);

        // Second pass: idempotent, nothing left to scan.
        let stats2 = compress_existing(&conn, 100).unwrap();
        assert_eq!(stats2.rows_scanned, 0);
        assert_eq!(stats2.rows_compressed, 0);
    }

    #[test]
    fn compress_existing_keeps_fts_pointed_at_original() {
        // v5 fix: dropping the messages_au trigger means UPDATE-by-compression
        // does NOT replace FTS index content. Recall against the original
        // phrasing must still hit after compression.
        let dir = tempdir().unwrap();
        let pool = open_pool(&dir.path().join("fts.db")).unwrap();
        let conn = pool.get().unwrap();
        // Distinctive phrase the compressor will likely drop or paraphrase.
        let phrase = "PINEAPPLE_BANANA_SENTINEL_quokka";
        // Pad it past the COMPRESS_THRESHOLD_TOKENS so the sweep actually rewrites.
        let mut large = String::from(phrase);
        large.push('\n');
        for i in 0..500 {
            large.push_str(&format!(
                "Note {i}: the engineer reviewed the proposal and recorded decision number {i} with rationale.\n",
            ));
        }
        conn.execute(
            "INSERT INTO sessions (id, project, cwd, started_at) VALUES ('s','/p','/p','t')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO messages (id, session_id, role, content, ts) VALUES ('m','s','user',?1,'t')",
            [&large],
        )
        .unwrap();

        // Sanity: FTS hits the phrase before compression.
        let pre: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM messages_fts WHERE messages_fts MATCH ?1",
                [phrase],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(pre, 1, "phrase should be in FTS before compression");

        let stats = compress_existing(&conn, 100).unwrap();
        assert_eq!(stats.rows_compressed, 1);

        // After compression the stored content has shrunk, but the FTS row
        // still holds the original — recall still hits.
        let stored: String = conn
            .query_row("SELECT content FROM messages WHERE id = 'm'", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert!(
            stored.len() < large.len(),
            "content should be smaller after compress"
        );
        let post: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM messages_fts WHERE messages_fts MATCH ?1",
                [phrase],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            post, 1,
            "FTS must still match original phrase after compression"
        );
    }

    #[test]
    fn compress_existing_preserves_recoverability_hash() {
        let dir = tempdir().unwrap();
        let pool = open_pool(&dir.path().join("rec.db")).unwrap();
        let conn = pool.get().unwrap();
        let large = "decision: ".repeat(800);
        let expected_hash = sha256(large.as_bytes());
        conn.execute(
            "INSERT INTO sessions (id, project, cwd, started_at) VALUES ('s', '/p', '/p', 't')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO messages (id, session_id, role, content, ts) VALUES ('m','s','user',?1,'t')",
            [&large],
        )
        .unwrap();
        compress_existing(&conn, 100).unwrap();
        let stored_hash: Vec<u8> = conn
            .query_row(
                "SELECT content_hash FROM messages WHERE id = 'm'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(stored_hash, expected_hash);
    }

    #[test]
    fn ingest_holds_offset_when_trailing_line_is_partial() {
        // M2 regression: a writer can flush half a line. We must not advance
        // current_offset past unterminated bytes, or the completed line is
        // permanently skipped on the next ingest.
        let dir = tempdir().unwrap();
        let pool = open_pool(&dir.path().join("partial.db")).unwrap();
        let conn = pool.get().unwrap();
        let jp = dir.path().join("t.jsonl");

        // First write: one full line + an unterminated trailing fragment.
        let full = r#"{"type":"user","sessionId":"s","cwd":"/p","timestamp":"t","uuid":"u1","message":{"role":"user","content":"complete one"}}"#;
        let partial_head = r#"{"type":"user","sessionId":"s","cwd":"/p","timestamp":"t","uui"#;
        {
            let mut f = File::create(&jp).unwrap();
            writeln!(f, "{full}").unwrap();
            // no trailing newline on the partial
            f.write_all(partial_head.as_bytes()).unwrap();
        }
        let s1 = ingest_file(&conn, &jp).unwrap();
        assert_eq!(
            s1.messages_inserted, 1,
            "only the complete line should ingest"
        );
        let offset1: i64 = conn
            .query_row(
                "SELECT last_offset FROM ingestion_log WHERE jsonl_path = ?1",
                [jp.canonicalize().unwrap().to_string_lossy().as_ref()],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            offset1,
            (full.len() + 1) as i64,
            "offset must stop at end of completed line"
        );

        // Now the writer completes the partial line. Re-ingest must pick it up.
        let tail = r#"d":"u2","message":{"role":"user","content":"completed two"}}"#;
        {
            use std::io::Write;
            let mut f = std::fs::OpenOptions::new().append(true).open(&jp).unwrap();
            f.write_all(tail.as_bytes()).unwrap();
            writeln!(f).unwrap();
        }
        let s2 = ingest_file(&conn, &jp).unwrap();
        assert_eq!(
            s2.messages_inserted, 1,
            "completed second line must ingest on next run"
        );
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM messages", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 2);
    }

    #[test]
    fn ingest_skips_sidechain_events() {
        let dir = tempdir().unwrap();
        let pool = open_pool(&dir.path().join("side.db")).unwrap();
        let conn = pool.get().unwrap();
        let jp = dir.path().join("t.jsonl");
        write_jsonl(
            &jp,
            &[
                r#"{"type":"user","sessionId":"s","cwd":"/p","timestamp":"t","uuid":"main1","message":{"role":"user","content":"main turn"}}"#,
                r#"{"type":"user","sessionId":"s","cwd":"/p","timestamp":"t","uuid":"side1","isSidechain":true,"message":{"role":"user","content":"sub-agent turn"}}"#,
                r#"{"type":"assistant","sessionId":"s","cwd":"/p","timestamp":"t","uuid":"side2","isSidechain":true,"message":{"role":"assistant","content":[{"type":"text","text":"sub-agent reply"}]}}"#,
            ],
        );
        let s = ingest_file(&conn, &jp).unwrap();
        assert_eq!(
            s.messages_inserted, 1,
            "sidechain events must not be ingested"
        );
        let stored: String = conn
            .query_row("SELECT content FROM messages", [], |r| r.get(0))
            .unwrap();
        assert_eq!(stored, "main turn");
    }

    #[test]
    fn re_ingest_is_incremental() {
        let dir = tempdir().unwrap();
        let pool = open_pool(&dir.path().join("ingest.db")).unwrap();
        let conn = pool.get().unwrap();
        let jp = dir.path().join("t.jsonl");
        write_jsonl(
            &jp,
            &[
                r#"{"type":"user","sessionId":"s1","cwd":"/p","timestamp":"t","uuid":"u1","message":{"role":"user","content":"first"}}"#,
            ],
        );
        let s1 = ingest_file(&conn, &jp).unwrap();
        assert_eq!(s1.messages_inserted, 1);

        // Append a second line, re-ingest.
        let mut f = std::fs::OpenOptions::new().append(true).open(&jp).unwrap();
        let line = r#"{"type":"user","sessionId":"s1","cwd":"/p","timestamp":"t","uuid":"u2","message":{"role":"user","content":"second"}}"#;
        writeln!(f, "{line}").unwrap();
        drop(f);
        let s2 = ingest_file(&conn, &jp).unwrap();
        assert_eq!(s2.messages_inserted, 1);
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM messages", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 2);
    }
}
