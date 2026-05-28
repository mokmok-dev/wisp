/// Storage-layer error type.
#[derive(Debug, thiserror::Error)]
pub enum StorageError {
    #[error(transparent)]
    Sqlite(#[from] rusqlite::Error),

    #[error(transparent)]
    Io(#[from] std::io::Error),

    #[error("migration failed at target version {target}: {source}")]
    Migration {
        target: u32,
        #[source]
        source: rusqlite::Error,
    },

    #[error("unrecognized source label in database: {0}")]
    UnknownSource(String),
}

pub type Result<T> = std::result::Result<T, StorageError>;
