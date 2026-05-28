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

    #[error(transparent)]
    SourceLabel(#[from] wisp_core::SourceLabelError),
}

pub type Result<T> = std::result::Result<T, StorageError>;
