use crate::{schema, Result, SCHEMA_VERSION};
use r2d2_sqlite::SqliteConnectionManager;
use rusqlite::{Connection, OpenFlags};
use std::path::{Path, PathBuf};

pub type Pool = r2d2::Pool<SqliteConnectionManager>;
pub type PooledConn = r2d2::PooledConnection<SqliteConnectionManager>;

#[derive(Debug)]
struct Pragmas;

impl r2d2::CustomizeConnection<Connection, rusqlite::Error> for Pragmas {
    fn on_acquire(&self, conn: &mut Connection) -> std::result::Result<(), rusqlite::Error> {
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        Ok(())
    }
}

pub fn default_db_path() -> PathBuf {
    if let Ok(p) = std::env::var("ENGRAPH_DB_PATH") {
        return PathBuf::from(p);
    }
    let base = dirs::data_local_dir()
        .unwrap_or_else(|| dirs::home_dir().expect("HOME").join(".local/share"));
    base.join("engraph").join("engraph.db")
}

pub fn open_pool(path: &Path) -> Result<Pool> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let manager = SqliteConnectionManager::file(path).with_flags(
        OpenFlags::SQLITE_OPEN_READ_WRITE
            | OpenFlags::SQLITE_OPEN_CREATE
            | OpenFlags::SQLITE_OPEN_URI,
    );
    let pool = r2d2::Pool::builder()
        .max_size(4)
        .connection_customizer(Box::new(Pragmas))
        .build(manager)?;
    {
        let mut conn = pool.get()?;
        // WAL is database-wide and persists; set once.
        conn.pragma_update(None, "journal_mode", "WAL")?;
        schema::run_migrations(&mut conn)?;
        schema::check_drift(&conn, SCHEMA_VERSION)?;
    }
    Ok(pool)
}

pub fn open_default_pool() -> Result<Pool> {
    open_pool(&default_db_path())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn open_pool_creates_and_migrates() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("test.db");
        let pool = open_pool(&path).unwrap();
        let conn = pool.get().unwrap();
        let v = schema::current_version(&conn).unwrap();
        assert_eq!(v, SCHEMA_VERSION);
    }
}
