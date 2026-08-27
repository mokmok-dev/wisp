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
