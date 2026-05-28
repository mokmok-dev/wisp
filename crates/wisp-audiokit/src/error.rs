/// Errors surfaced by [`crate::Session`] operations.
#[derive(Debug, Clone, thiserror::Error)]
pub enum SessionError {
    #[cfg(target_os = "macos")]
    #[error("path contains a NUL byte or is not representable as a C string: {0:?}")]
    InvalidPath(std::path::PathBuf),

    #[cfg(target_os = "macos")]
    #[error("locale contains a NUL byte: {0}")]
    InvalidLocale(String),

    #[cfg(target_os = "macos")]
    #[error("WispAudioKit session construction failed")]
    Construction,

    #[cfg(target_os = "macos")]
    #[error("WispAudioKit session start failed: {0}")]
    Start(String),

    #[cfg(not(target_os = "macos"))]
    #[error("WispAudioKit is only available on macOS")]
    UnsupportedPlatform,
}

/// Result alias for session operations.
pub type Result<T> = std::result::Result<T, SessionError>;
