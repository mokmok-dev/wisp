/// Errors surfaced by [`crate::Session`] operations.
#[derive(Debug, Clone, thiserror::Error)]
pub enum SessionError {
    #[cfg(any(target_os = "macos", target_os = "windows"))]
    #[error("path contains a NUL byte or is not representable as a C string: {0:?}")]
    InvalidPath(std::path::PathBuf),

    #[cfg(any(target_os = "macos", target_os = "windows"))]
    #[error("locale contains a NUL byte: {0}")]
    InvalidLocale(String),

    #[cfg(any(target_os = "macos", target_os = "windows"))]
    #[error("WispAudioKit session construction failed")]
    Construction,

    #[cfg(any(target_os = "macos", target_os = "windows"))]
    #[error("WispAudioKit session start failed: {0}")]
    Start(String),

    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    #[error("WispAudioKit is only available on macOS and Windows")]
    UnsupportedPlatform,
}

/// Result alias for session operations.
pub type Result<T> = std::result::Result<T, SessionError>;
