use thiserror::Error;

#[derive(Debug, Error)]
pub enum Error {
    #[error("sqlite: {0}")]
    Sqlite(#[from] rusqlite::Error),

    #[error("connection pool: {0}")]
    Pool(#[from] r2d2::Error),

    #[error("io: {0}")]
    Io(#[from] std::io::Error),

    #[error("schema drift: expected v{expected}, found v{found}")]
    SchemaDrift { expected: i64, found: i64 },

    #[error("invalid config: {0}")]
    Config(String),
}

pub type Result<T> = std::result::Result<T, Error>;
