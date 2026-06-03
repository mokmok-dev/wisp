/// Errors surfaced by [`crate::Session`] operations.
#[derive(Debug, Clone, thiserror::Error)]
pub enum SessionError {
    #[error("path contains a NUL byte or is not representable as a C string: {0:?}")]
    InvalidPath(std::path::PathBuf),

    #[error("locale contains a NUL byte: {0}")]
    InvalidLocale(String),

    #[error("WispAudioKit session construction failed")]
    Construction,

    #[error("WispAudioKit session start failed: {0}")]
    Start(String),

    #[error("WispAudioKit is not available on this platform")]
    UnsupportedPlatform,
}

/// Result alias for session operations.
pub type Result<T> = std::result::Result<T, SessionError>;

/// Errors surfaced by setup helpers such as local model download.
#[derive(Debug, Clone, thiserror::Error)]
pub enum SetupError {
    #[error("failed to create local model directory {path:?}: {message}")]
    CreateModelDirectory {
        path: std::path::PathBuf,
        message: String,
    },

    #[error("failed to download local model: {0}")]
    Download(String),

    #[error("failed to move local model into place: {0}")]
    Install(String),
}

/// Result alias for setup operations.
pub type SetupResult<T> = std::result::Result<T, SetupError>;
