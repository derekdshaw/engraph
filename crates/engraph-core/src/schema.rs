use crate::{Error, Result};
use rusqlite::{Connection, OptionalExtension};

const MIGRATIONS: &[&str] = &[
    // v1 — Phase 0 foundation
    r#"
    CREATE TABLE IF NOT EXISTS migrations (
        version INTEGER PRIMARY KEY,
        applied_at TEXT NOT NULL DEFAULT (datetime('now'))
    );

    CREATE TABLE IF NOT EXISTS sessions (
        id TEXT PRIMARY KEY,
        project TEXT,
        cwd TEXT,
        started_at TEXT NOT NULL,
        ended_at TEXT,
        transcript_path TEXT
    );

    CREATE INDEX IF NOT EXISTS idx_sessions_project ON sessions(project);

    CREATE TABLE IF NOT EXISTS messages (
        id TEXT PRIMARY KEY,
        session_id TEXT NOT NULL,
        role TEXT NOT NULL,
        content TEXT NOT NULL,
        content_compressed INTEGER NOT NULL DEFAULT 0,
        content_hash BLOB,
        ts TEXT NOT NULL,
        FOREIGN KEY(session_id) REFERENCES sessions(id)
    );

    CREATE INDEX IF NOT EXISTS idx_messages_session ON messages(session_id);
    CREATE INDEX IF NOT EXISTS idx_messages_session_ts ON messages(session_id, ts);

    CREATE TABLE IF NOT EXISTS events (
        seq INTEGER PRIMARY KEY,
        id TEXT NOT NULL UNIQUE,
        session_id TEXT,
        kind TEXT NOT NULL CHECK (kind IN ('compress','retrieve','hook','wrapped_cmd')),
        feature TEXT NOT NULL,
        filter_id TEXT,
        input_tokens INTEGER NOT NULL DEFAULT 0,
        output_tokens INTEGER NOT NULL DEFAULT 0,
        latency_ms INTEGER NOT NULL DEFAULT 0,
        ts TEXT NOT NULL DEFAULT (datetime('now'))
    );

    CREATE INDEX IF NOT EXISTS idx_events_kind ON events(kind);
    CREATE INDEX IF NOT EXISTS idx_events_feature ON events(feature);
    CREATE INDEX IF NOT EXISTS idx_events_session ON events(session_id);
    CREATE INDEX IF NOT EXISTS idx_events_ts ON events(ts);

    CREATE TABLE IF NOT EXISTS session_budget (
        session_id TEXT PRIMARY KEY,
        soft_limit INTEGER NOT NULL,
        hard_limit INTEGER NOT NULL,
        used_tokens INTEGER NOT NULL DEFAULT 0,
        escalation_level INTEGER NOT NULL DEFAULT 0 CHECK (escalation_level IN (0,1,2,3)),
        updated_at TEXT NOT NULL DEFAULT (datetime('now'))
    );
    "#,
    // v2 — Phase 3 retrieval + ingest
    r#"
    CREATE TABLE IF NOT EXISTS scopes (
        id TEXT PRIMARY KEY,
        parent_id TEXT REFERENCES scopes(id),
        kind TEXT NOT NULL CHECK (kind IN ('project','topic','time_window','custom')),
        name TEXT NOT NULL,
        created_at TEXT NOT NULL DEFAULT (datetime('now')),
        archived INTEGER NOT NULL DEFAULT 0
    );
    CREATE INDEX IF NOT EXISTS idx_scopes_kind_name ON scopes(kind, name);
    CREATE INDEX IF NOT EXISTS idx_scopes_parent ON scopes(parent_id);

    CREATE TABLE IF NOT EXISTS scope_members (
        scope_id TEXT NOT NULL REFERENCES scopes(id),
        target_kind TEXT NOT NULL,
        target_id TEXT NOT NULL,
        PRIMARY KEY (scope_id, target_kind, target_id)
    );
    CREATE INDEX IF NOT EXISTS idx_scope_members_target ON scope_members(target_kind, target_id);

    CREATE TABLE IF NOT EXISTS context_items (
        id TEXT PRIMARY KEY,
        session_id TEXT,
        kind TEXT NOT NULL,
        content TEXT NOT NULL,
        content_compressed INTEGER NOT NULL DEFAULT 0,
        content_hash BLOB,
        ts TEXT NOT NULL DEFAULT (datetime('now'))
    );
    CREATE INDEX IF NOT EXISTS idx_context_items_session ON context_items(session_id);
    CREATE INDEX IF NOT EXISTS idx_context_items_kind ON context_items(kind);

    CREATE TABLE IF NOT EXISTS bugs (
        id TEXT PRIMARY KEY,
        project TEXT,
        summary TEXT NOT NULL,
        content TEXT,
        ts TEXT NOT NULL DEFAULT (datetime('now')),
        resolved INTEGER NOT NULL DEFAULT 0
    );
    CREATE INDEX IF NOT EXISTS idx_bugs_project ON bugs(project);

    CREATE TABLE IF NOT EXISTS do_not_repeat (
        id TEXT PRIMARY KEY,
        project TEXT,
        rule TEXT NOT NULL,
        ts TEXT NOT NULL DEFAULT (datetime('now'))
    );
    CREATE INDEX IF NOT EXISTS idx_dnr_project ON do_not_repeat(project);

    CREATE TABLE IF NOT EXISTS ingestion_log (
        jsonl_path TEXT PRIMARY KEY,
        last_offset INTEGER NOT NULL DEFAULT 0,
        last_mtime TEXT,
        ingested_at TEXT NOT NULL DEFAULT (datetime('now'))
    );

    CREATE TABLE IF NOT EXISTS entities (
        id TEXT PRIMARY KEY,
        kind TEXT NOT NULL,
        name TEXT NOT NULL,
        project TEXT,
        created_at TEXT NOT NULL DEFAULT (datetime('now'))
    );
    CREATE INDEX IF NOT EXISTS idx_entities_kind_name ON entities(kind, name);
    CREATE INDEX IF NOT EXISTS idx_entities_project ON entities(project);

    CREATE TABLE IF NOT EXISTS relations (
        id TEXT PRIMARY KEY,
        src_entity TEXT NOT NULL REFERENCES entities(id),
        dst_entity TEXT NOT NULL REFERENCES entities(id),
        kind TEXT NOT NULL,
        valid_from TEXT NOT NULL DEFAULT (datetime('now')),
        valid_to TEXT,
        confidence REAL NOT NULL DEFAULT 1.0,
        provenance TEXT NOT NULL DEFAULT 'extracted'
            CHECK (provenance IN ('extracted','inferred','ambiguous','generated')),
        source_message_id TEXT
    );
    CREATE INDEX IF NOT EXISTS idx_relations_src ON relations(src_entity);
    CREATE INDEX IF NOT EXISTS idx_relations_dst ON relations(dst_entity);

    -- FTS5 indexes. external-content tables; rowid links to messages.rowid /
    -- context_items.rowid. Both content tables have implicit rowid even with
    -- TEXT primary keys.
    CREATE VIRTUAL TABLE IF NOT EXISTS messages_fts USING fts5(
        content,
        content='messages',
        content_rowid='rowid'
    );
    CREATE VIRTUAL TABLE IF NOT EXISTS context_items_fts USING fts5(
        content,
        content='context_items',
        content_rowid='rowid'
    );

    -- Sync triggers keep FTS aligned with content tables.
    CREATE TRIGGER IF NOT EXISTS messages_ai AFTER INSERT ON messages BEGIN
        INSERT INTO messages_fts(rowid, content) VALUES (new.rowid, new.content);
    END;
    CREATE TRIGGER IF NOT EXISTS messages_ad AFTER DELETE ON messages BEGIN
        INSERT INTO messages_fts(messages_fts, rowid, content) VALUES('delete', old.rowid, old.content);
    END;
    CREATE TRIGGER IF NOT EXISTS messages_au AFTER UPDATE ON messages BEGIN
        INSERT INTO messages_fts(messages_fts, rowid, content) VALUES('delete', old.rowid, old.content);
        INSERT INTO messages_fts(rowid, content) VALUES (new.rowid, new.content);
    END;

    CREATE TRIGGER IF NOT EXISTS context_items_ai AFTER INSERT ON context_items BEGIN
        INSERT INTO context_items_fts(rowid, content) VALUES (new.rowid, new.content);
    END;
    CREATE TRIGGER IF NOT EXISTS context_items_ad AFTER DELETE ON context_items BEGIN
        INSERT INTO context_items_fts(context_items_fts, rowid, content) VALUES('delete', old.rowid, old.content);
    END;
    CREATE TRIGGER IF NOT EXISTS context_items_au AFTER UPDATE ON context_items BEGIN
        INSERT INTO context_items_fts(context_items_fts, rowid, content) VALUES('delete', old.rowid, old.content);
        INSERT INTO context_items_fts(rowid, content) VALUES (new.rowid, new.content);
    END;
    "#,
    // v3 — Phase 6 embeddings (feature-gated at query time, table always present)
    r#"
    CREATE TABLE IF NOT EXISTS embeddings (
        target_kind TEXT NOT NULL,
        target_id TEXT NOT NULL,
        vector BLOB NOT NULL,
        model_id TEXT NOT NULL,
        created_at TEXT NOT NULL DEFAULT (datetime('now')),
        PRIMARY KEY (target_kind, target_id, model_id)
    );
    CREATE INDEX IF NOT EXISTS idx_embeddings_target ON embeddings(target_kind, target_id);
    "#,
    // v4 — ingest_file file-rotation guard (last_inode + last_size for detection)
    r#"
    ALTER TABLE ingestion_log ADD COLUMN last_inode INTEGER;
    ALTER TABLE ingestion_log ADD COLUMN last_size INTEGER;
    "#,
    // v5 — drop the AFTER UPDATE triggers on messages / context_items so that
    // in-place compression (engraph compress-existing) does NOT replace the
    // original text in the FTS index. Recall continues to hit the user's
    // original phrasing after compression. The INSERT trigger still indexes
    // new rows, and the DELETE trigger still removes them; only UPDATE is
    // intentionally non-propagating. SQLite rowids are stable across UPDATEs
    // when the primary key (TEXT) is unchanged, so FTS stays anchored.
    r#"
    DROP TRIGGER IF EXISTS messages_au;
    DROP TRIGGER IF EXISTS context_items_au;
    "#,
];

pub fn current_version(conn: &Connection) -> Result<i64> {
    let table_present: Option<i64> = conn
        .query_row(
            "SELECT 1 FROM sqlite_master WHERE type='table' AND name='migrations'",
            [],
            |r| r.get(0),
        )
        .optional()?;
    if table_present.is_none() {
        return Ok(0);
    }
    let v: i64 = conn.query_row(
        "SELECT COALESCE(MAX(version), 0) FROM migrations",
        [],
        |r| r.get(0),
    )?;
    Ok(v)
}

pub fn run_migrations(conn: &mut Connection) -> Result<()> {
    let current = current_version(conn)?;
    for (idx, sql) in MIGRATIONS.iter().enumerate() {
        let target = (idx as i64) + 1;
        if target <= current {
            continue;
        }
        let tx = conn.transaction()?;
        tx.execute_batch(sql)?;
        tx.execute(
            "INSERT INTO migrations (version) VALUES (?1)",
            [target],
        )?;
        tx.commit()?;
        tracing::info!(version = target, "applied migration");
    }
    Ok(())
}

pub fn check_drift(conn: &Connection, expected: i64) -> Result<()> {
    let found = current_version(conn)?;
    if found > expected {
        // DB is newer than this binary — running would risk corrupting newer tables.
        return Err(Error::SchemaDrift { expected, found });
    }
    if found < expected {
        // Should not happen after run_migrations; warn so the discrepancy surfaces.
        tracing::warn!(expected, found, "schema behind code after migrations");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::Connection;

    #[test]
    fn migrations_apply_idempotently() {
        let mut conn = Connection::open_in_memory().unwrap();
        run_migrations(&mut conn).unwrap();
        assert_eq!(current_version(&conn).unwrap(), MIGRATIONS.len() as i64);
        run_migrations(&mut conn).unwrap();
        assert_eq!(current_version(&conn).unwrap(), MIGRATIONS.len() as i64);
    }

    #[test]
    fn tables_exist_after_migration() {
        let mut conn = Connection::open_in_memory().unwrap();
        run_migrations(&mut conn).unwrap();
        for table in &["sessions", "messages", "events", "session_budget"] {
            let exists: bool = conn
                .query_row(
                    "SELECT 1 FROM sqlite_master WHERE type='table' AND name=?1",
                    [table],
                    |_| Ok(true),
                )
                .unwrap_or(false);
            assert!(exists, "missing table {table}");
        }
    }
}
