use crate::Result;
use rusqlite::Connection;

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

    CREATE TABLE IF NOT EXISTS events (
        rowid INTEGER PRIMARY KEY AUTOINCREMENT,
        id TEXT NOT NULL UNIQUE,
        session_id TEXT,
        kind TEXT NOT NULL,
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
        escalation_level INTEGER NOT NULL DEFAULT 0,
        updated_at TEXT NOT NULL DEFAULT (datetime('now'))
    );
    "#,
];

pub fn current_version(conn: &Connection) -> Result<i64> {
    let exists: bool = conn
        .query_row(
            "SELECT 1 FROM sqlite_master WHERE type='table' AND name='migrations'",
            [],
            |_| Ok(true),
        )
        .unwrap_or(false);
    if !exists {
        return Ok(0);
    }
    let v: i64 = conn
        .query_row("SELECT COALESCE(MAX(version), 0) FROM migrations", [], |r| {
            r.get(0)
        })
        .unwrap_or(0);
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
    if found != expected {
        tracing::warn!(expected, found, "schema drift detected");
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
