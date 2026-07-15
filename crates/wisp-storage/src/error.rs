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

    #[error("segment belongs to session {actual}, expected session {expected}")]
    SessionMismatch {
        expected: wisp_core::SessionId,
        actual: wisp_core::SessionId,
    },

    #[error("session {0} does not exist")]
    SessionNotFound(wisp_core::SessionId),
}

pub type Result<T> = std::result::Result<T, StorageError>;
