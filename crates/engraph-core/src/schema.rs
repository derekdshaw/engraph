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
    // v6 — F2 codegraph: enrich entities with source location so the subgraph
    // formatter can render file:line references. The `relations.kind` value is
    // validated in Rust (RelationKind enum in engraph-codegraph) rather than at
    // the DB level — SQLite cannot ALTER ADD CHECK on an existing column and a
    // table rebuild would risk dropping FK-referenced rows.
    r#"
    ALTER TABLE entities ADD COLUMN file_path TEXT;
    ALTER TABLE entities ADD COLUMN line_range TEXT;
    ALTER TABLE entities ADD COLUMN signature TEXT;
    CREATE INDEX IF NOT EXISTS idx_entities_file_path ON entities(file_path);
    "#,
    // v7 — widen events.kind CHECK to allow 'index' (codegraph index telemetry,
    // a non-savings kind). SQLite can't ALTER a CHECK constraint, so rebuild the
    // table with the new constraint. `events` has no foreign keys in or out, so
    // the rebuild is safe; seq and all rows are preserved.
    r#"
    CREATE TABLE events_new (
        seq INTEGER PRIMARY KEY,
        id TEXT NOT NULL UNIQUE,
        session_id TEXT,
        kind TEXT NOT NULL CHECK (kind IN ('compress','retrieve','hook','wrapped_cmd','index')),
        feature TEXT NOT NULL,
        filter_id TEXT,
        input_tokens INTEGER NOT NULL DEFAULT 0,
        output_tokens INTEGER NOT NULL DEFAULT 0,
        latency_ms INTEGER NOT NULL DEFAULT 0,
        ts TEXT NOT NULL DEFAULT (datetime('now'))
    );
    INSERT INTO events_new (seq, id, session_id, kind, feature, filter_id, input_tokens, output_tokens, latency_ms, ts)
        SELECT seq, id, session_id, kind, feature, filter_id, input_tokens, output_tokens, latency_ms, ts FROM events;
    DROP TABLE events;
    ALTER TABLE events_new RENAME TO events;
    CREATE INDEX IF NOT EXISTS idx_events_kind ON events(kind);
    CREATE INDEX IF NOT EXISTS idx_events_feature ON events(feature);
    CREATE INDEX IF NOT EXISTS idx_events_session ON events(session_id);
    CREATE INDEX IF NOT EXISTS idx_events_ts ON events(ts);
    "#,
    // v8 — project-scope context_items so `engraph save` decisions surface in
    // the SessionStart brief without joining sessions (sessions.project is only
    // populated at ingest, so a live-session decision has no session row to join
    // on). ADD COLUMN is metadata-only and leaves rowids untouched, so the
    // external-content FTS triggers on context_items keep working — same pattern
    // as v6's entity columns.
    r#"
    ALTER TABLE context_items ADD COLUMN project TEXT;
    CREATE INDEX IF NOT EXISTS idx_context_items_project ON context_items(project);
    "#,
    // v9 — the commit each codegraph project was last indexed at, so a lookup
    // from a worktree can tell which indexed files its branch has since changed.
    r#"
    CREATE TABLE IF NOT EXISTS index_state (
        project TEXT PRIMARY KEY,
        commit_sha TEXT NOT NULL,
        indexed_at TEXT NOT NULL DEFAULT (datetime('now'))
    );
    "#,
    // v10 — index_state per subtree: a monorepo sub-project index refreshes only
    // its directory of the repo's project, at its own commit. SQLite can't
    // change a primary key in place, so rebuild; v9 rows become subtree ''.
    r#"
    CREATE TABLE index_state_new (
        project TEXT NOT NULL,
        subtree TEXT NOT NULL DEFAULT '',
        commit_sha TEXT NOT NULL,
        indexed_at TEXT NOT NULL DEFAULT (datetime('now')),
        PRIMARY KEY (project, subtree)
    );
    INSERT INTO index_state_new (project, commit_sha, indexed_at)
        SELECT project, commit_sha, indexed_at FROM index_state;
    DROP TABLE index_state;
    ALTER TABLE index_state_new RENAME TO index_state;
    "#,
    // v11 — how each subtree was indexed, so `index --if-changed` repeats it:
    // `source` is where the SCIP came from (`drivers` run by engraph, or a
    // prebuilt `scip` / `manifest` it can't rebuild), plus the `--lang` pin
    // and whether the symbol-level Bazel pass ran. Older rows were indexed by
    // drivers with neither, which the defaults say.
    r#"
    ALTER TABLE index_state ADD COLUMN source TEXT NOT NULL DEFAULT 'drivers';
    ALTER TABLE index_state ADD COLUMN lang TEXT;
    ALTER TABLE index_state ADD COLUMN bazel_symbols INTEGER NOT NULL DEFAULT 0;
    "#,
    // v12 — entities.qualified_name: the SCIP descriptors after the namespace,
    // joined with `.` (`Config.full_path_base` for `…/Config#full_path_base().`,
    // `Config` for `…/Config#`), so `subgraph` and the grep redirect can match
    // a qualified lookup (`Config::full_path_base`) when the bare name is
    // shared by many symbols. NULL until the project is indexed again (the
    // loader fills it); bazel_target rows never have one. ADD COLUMN is
    // metadata-only, same as v6/v8. Names change in the same load, so the
    // commit of each driver run is cleared: git can't resolve '', which
    // counts as changed, so the next `--if-changed` refresh (SessionStart)
    // re-indexes it. Prebuilt SCIP subtrees are skipped by that refresh.
    r#"
    ALTER TABLE entities ADD COLUMN qualified_name TEXT;
    CREATE INDEX IF NOT EXISTS idx_entities_qualified_name ON entities(qualified_name);
    UPDATE index_state SET commit_sha = '' WHERE source = 'drivers';
    "#,
    // v13 — SCIP local symbols per project and document. The loader now gives
    // each local the id `local <n>@<project tag>:<relative_path>`; before, one
    // `local <n>` row stood for the locals of all documents and projects, with
    // the edges of all of them. Those rows and their edges are removed. The
    // new `scheme` column is the SCIP scheme (indexer) that wrote a symbol:
    // the first word of a global id, and for a local the scheme of its
    // document, so a load replaces only the locals of its own indexers. NULL
    // for bazel_target rows. Driver-indexed subtrees are cleared for a new
    // index, as in v12, so the next refresh writes the local edges.
    r#"
    ALTER TABLE entities ADD COLUMN scheme TEXT;
    UPDATE entities SET scheme = substr(id, 1, instr(id, ' ') - 1)
        WHERE kind = 'symbol' AND instr(id, ' ') > 0 AND id NOT LIKE 'local %';
    DELETE FROM relations
        WHERE (src_entity LIKE 'local %' AND instr(src_entity, '@') = 0)
           OR (dst_entity LIKE 'local %' AND instr(dst_entity, '@') = 0);
    DELETE FROM entities WHERE id LIKE 'local %' AND instr(id, '@') = 0;
    UPDATE index_state SET commit_sha = '' WHERE source = 'drivers';
    "#,
    // v14 — `relations.line`: the 1-based line of the occurrence an edge comes
    // from (the call or the reference), in the file of its source, so
    // `subgraph` can show where a caller calls. NULL for edges from
    // SymbolInformation relationships and Bazel dependencies. Driver-indexed subtrees are cleared
    // for a new index, as in v12, so the next refresh writes the lines.
    r#"
    ALTER TABLE relations ADD COLUMN line INTEGER;
    UPDATE index_state SET commit_sha = '' WHERE source = 'drivers';
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
    run_migrations_to(conn, MIGRATIONS.len() as i64)
}

/// Apply the migrations after the current version, up to and including
/// version `last`. Tests use it to make a database of an older version.
fn run_migrations_to(conn: &mut Connection, last: i64) -> Result<()> {
    let current = current_version(conn)?;
    for (idx, sql) in MIGRATIONS.iter().enumerate() {
        let target = (idx as i64) + 1;
        if target <= current || target > last {
            continue;
        }
        let tx = conn.transaction()?;
        tx.execute_batch(sql)?;
        tx.execute("INSERT INTO migrations (version) VALUES (?1)", [target])?;
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
    fn migrates_through_v6_entity_columns() {
        let mut conn = Connection::open_in_memory().unwrap();
        run_migrations(&mut conn).unwrap();
        // v6 added file_path/line_range/signature to entities.
        for col in &["file_path", "line_range", "signature"] {
            let present: bool = conn
                .query_row(
                    "SELECT 1 FROM pragma_table_info('entities') WHERE name = ?1",
                    [col],
                    |_| Ok(true),
                )
                .unwrap_or(false);
            assert!(present, "missing entities column {col}");
        }
        // Inserting a row that exercises the new columns must succeed.
        conn.execute(
            "INSERT INTO entities (id, kind, name, project, file_path, line_range, signature)
             VALUES ('e1', 'symbol', 'foo', '/p', 'src/lib.rs', '10:20', 'fn foo() -> i32')",
            [],
        )
        .unwrap();
    }

    #[test]
    fn migrates_through_v8_context_items_project() {
        let mut conn = Connection::open_in_memory().unwrap();
        run_migrations(&mut conn).unwrap();
        // v8 added a `project` column to context_items.
        let present: bool = conn
            .query_row(
                "SELECT 1 FROM pragma_table_info('context_items') WHERE name = 'project'",
                [],
                |_| Ok(true),
            )
            .unwrap_or(false);
        assert!(present, "missing context_items.project column");
        // Inserting a row using the new column must succeed.
        conn.execute(
            "INSERT INTO context_items (id, kind, content, project)
             VALUES ('c1', 'decision', 'use rusqlite pool', '/p')",
            [],
        )
        .unwrap();
    }

    #[test]
    fn migrates_v10_index_state_rows_to_v11_driver_runs() {
        let mut conn = Connection::open_in_memory().unwrap();
        let tx = conn.transaction().unwrap();
        for (i, sql) in MIGRATIONS[..10].iter().enumerate() {
            tx.execute_batch(sql).unwrap();
            tx.execute(
                "INSERT INTO migrations (version) VALUES (?1)",
                [i as i64 + 1],
            )
            .unwrap();
        }
        tx.execute(
            "INSERT INTO index_state (project, subtree, commit_sha) VALUES ('/p', 'svc-a', 'abc')",
            [],
        )
        .unwrap();
        tx.commit().unwrap();
        run_migrations(&mut conn).unwrap();
        let row: (String, Option<String>, i64) = conn
            .query_row(
                "SELECT source, lang, bazel_symbols FROM index_state WHERE project = '/p'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap();
        assert_eq!(row, ("drivers".into(), None, 0));
    }

    #[test]
    fn migrates_v11_driver_runs_to_a_commit_that_forces_a_refresh() {
        // v12 names come from the loader, so a v11 index keeps its old names
        // until it is indexed again: v12 clears the commit of driver runs so
        // the next `--if-changed` refresh re-indexes them. Prebuilt SCIP
        // subtrees are skipped by the refresh, so they keep their commit.
        let mut conn = Connection::open_in_memory().unwrap();
        let tx = conn.transaction().unwrap();
        for (i, sql) in MIGRATIONS[..11].iter().enumerate() {
            tx.execute_batch(sql).unwrap();
            tx.execute(
                "INSERT INTO migrations (version) VALUES (?1)",
                [i as i64 + 1],
            )
            .unwrap();
        }
        tx.execute(
            "INSERT INTO index_state (project, subtree, commit_sha, source)
             VALUES ('/p', '', 'abc', 'drivers'), ('/p', 'gen', 'def', 'scip')",
            [],
        )
        .unwrap();
        tx.commit().unwrap();
        run_migrations(&mut conn).unwrap();
        let commit = |subtree: &str| -> String {
            conn.query_row(
                "SELECT commit_sha FROM index_state WHERE project = '/p' AND subtree = ?1",
                [subtree],
                |r| r.get(0),
            )
            .unwrap()
        };
        assert_eq!(commit(""), "");
        assert_eq!(commit("gen"), "def");
    }

    #[test]
    fn migrates_v9_index_state_rows_to_v10_subtrees() {
        let mut conn = Connection::open_in_memory().unwrap();
        // Apply through v9 (v1 creates `migrations`), with a v9-era row.
        let tx = conn.transaction().unwrap();
        for (i, sql) in MIGRATIONS[..9].iter().enumerate() {
            tx.execute_batch(sql).unwrap();
            tx.execute(
                "INSERT INTO migrations (version) VALUES (?1)",
                [i as i64 + 1],
            )
            .unwrap();
        }
        tx.execute(
            "INSERT INTO index_state (project, commit_sha) VALUES ('/p', 'abc123')",
            [],
        )
        .unwrap();
        // Only v10: v12 clears the commit of driver runs on purpose.
        tx.execute_batch(MIGRATIONS[9]).unwrap();
        tx.commit().unwrap();
        let row: (String, String) = conn
            .query_row(
                "SELECT subtree, commit_sha FROM index_state WHERE project = '/p'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(row, (String::new(), "abc123".into()));
        // Two subtrees of one project.
        conn.execute(
            "INSERT INTO index_state (project, subtree, commit_sha) VALUES ('/p', 'svc-a', 'x')",
            [],
        )
        .unwrap();
    }

    #[test]
    fn migrates_through_v9_index_state() {
        let mut conn = Connection::open_in_memory().unwrap();
        run_migrations(&mut conn).unwrap();
        conn.execute(
            "INSERT INTO index_state (project, commit_sha) VALUES ('/p', 'abc123')",
            [],
        )
        .unwrap();
        let at: String = conn
            .query_row(
                "SELECT indexed_at FROM index_state WHERE project = '/p'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert!(!at.is_empty());
    }

    #[test]
    fn migrates_through_v12_entities_qualified_name() {
        let mut conn = Connection::open_in_memory().unwrap();
        run_migrations(&mut conn).unwrap();
        let present: bool = conn
            .query_row(
                "SELECT 1 FROM pragma_table_info('entities') WHERE name = 'qualified_name'",
                [],
                |_| Ok(true),
            )
            .unwrap_or(false);
        assert!(present, "missing entities.qualified_name column");
        conn.execute(
            "INSERT INTO entities (id, kind, name, qualified_name, project)
             VALUES ('e1', 'symbol', 'full_path_base', 'Config.full_path_base', '/p')",
            [],
        )
        .unwrap();
        let n: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM entities WHERE qualified_name = 'Config.full_path_base'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(n, 1);
    }

    #[test]
    fn migrates_through_v13_entities_scheme_and_scoped_locals() {
        let mut conn = Connection::open_in_memory().unwrap();
        run_migrations_to(&mut conn, 12).unwrap();
        // A v12 database: a symbol, a Bazel target, a `local 0` of the old
        // form (one row for the whole database) with an edge in and out, and
        // a scoped local with an edge.
        conn.execute_batch(
            "INSERT INTO entities (id, kind, name, project) VALUES
                ('rust-analyzer cargo x 0.1.0 a/f().', 'symbol', 'f', '/p'),
                ('//a:b', 'bazel_target', 'b', '/p'),
                ('local 0', 'symbol', 'old', '/p'),
                ('local 1@0123456789abcdef:src/a.rs', 'symbol', 'new', '/p');
             INSERT INTO relations (id, src_entity, dst_entity, kind) VALUES
                ('r1', 'local 0', 'rust-analyzer cargo x 0.1.0 a/f().', 'CALLS'),
                ('r2', 'rust-analyzer cargo x 0.1.0 a/f().', 'local 0', 'REFERENCES'),
                ('r3', 'local 1@0123456789abcdef:src/a.rs', 'rust-analyzer cargo x 0.1.0 a/f().', 'CALLS');
             INSERT INTO index_state (project, subtree, commit_sha, source) VALUES
                ('/p', '', 'abc', 'drivers'), ('/p', 'sub', 'def', 'scip');",
        )
        .unwrap();
        run_migrations_to(&mut conn, 13).unwrap();
        assert_eq!(current_version(&conn).unwrap(), 13);

        let scheme = |id: &str| -> Option<String> {
            conn.query_row("SELECT scheme FROM entities WHERE id = ?1", [id], |r| {
                r.get(0)
            })
            .unwrap()
        };
        assert_eq!(
            scheme("rust-analyzer cargo x 0.1.0 a/f().").as_deref(),
            Some("rust-analyzer")
        );
        assert_eq!(scheme("//a:b"), None);
        // The old local and its edges are gone; the scoped local stays.
        let ids: Vec<String> = conn
            .prepare("SELECT id FROM relations ORDER BY id")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .map(|r| r.unwrap())
            .collect();
        assert_eq!(ids, vec!["r3"]);
        let old: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM entities WHERE id = 'local 0'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(old, 0);
        // Driver-indexed subtrees are indexed again at the next refresh, so
        // the local edges come back; a prebuilt one keeps its commit.
        let commits: Vec<(String, String)> = conn
            .prepare("SELECT subtree, commit_sha FROM index_state ORDER BY subtree")
            .unwrap()
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap()
            .map(|r| r.unwrap())
            .collect();
        assert_eq!(
            commits,
            vec![("".into(), "".into()), ("sub".into(), "def".into())]
        );
    }

    #[test]
    fn migrates_through_v14_relations_line() {
        let mut conn = Connection::open_in_memory().unwrap();
        run_migrations_to(&mut conn, 13).unwrap();
        conn.execute_batch(
            "INSERT INTO index_state (project, subtree, commit_sha, source) VALUES
                ('/p', '', 'abc', 'drivers'), ('/p', 'sub', 'def', 'scip');",
        )
        .unwrap();
        run_migrations(&mut conn).unwrap();
        assert_eq!(current_version(&conn).unwrap(), 14);
        let line_column: bool = conn
            .query_row(
                "SELECT 1 FROM pragma_table_info('relations') WHERE name = 'line'",
                [],
                |_| Ok(true),
            )
            .unwrap_or(false);
        assert!(line_column, "missing relations.line column");
        // The next refresh indexes driver subtrees again to write the lines.
        let commits: Vec<(String, String)> = conn
            .prepare("SELECT subtree, commit_sha FROM index_state ORDER BY subtree")
            .unwrap()
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap()
            .map(|r| r.unwrap())
            .collect();
        assert_eq!(
            commits,
            vec![("".into(), "".into()), ("sub".into(), "def".into())]
        );
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
