//! Error and result types for the model lifecycle layer.

use crate::ModelId;

/// Error surfaced by a [`crate::ModelProvider`].
///
/// The variants intentionally mirror the shapes a provider can fail in
/// regardless of whether it is filesystem-backed or talks to a running
/// service (such as Foundry Local): a model can be unknown, absent from the
/// cache, fail to download, or the operation can be unsupported by the
/// provider entirely. Keeping the set small lets callers degrade gracefully
/// without matching on backend-specific detail.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum ModelError {
    /// The requested model is not known to this provider.
    #[error("model not found: {0}")]
    NotFound(ModelId),

    /// The provider's backing service could not be reached or started.
    #[error("model provider service is unavailable: {0}")]
    ServiceUnavailable(String),

    /// The model is known but has not been downloaded/cached yet.
    #[error("model {id} is not downloaded")]
    NotDownloaded {
        /// Identifier of the model that is missing locally.
        id: ModelId,
    },

    /// Acquiring (downloading/caching) the model failed.
    #[error("failed to download model {id}: {message}")]
    Download {
        /// Identifier of the model that failed to download.
        id: ModelId,
        /// Human-readable cause reported by the backend.
        message: String,
    },

    /// The provider does not implement the requested operation.
    #[error("operation not supported by provider {provider}: {operation}")]
    Unsupported {
        /// Identifier of the provider that rejected the call.
        provider: String,
        /// The operation that is not supported.
        operation: String,
    },

    /// A catch-all for backend-internal failures.
    #[error("model backend error: {0}")]
    Backend(String),
}

/// Convenience result alias for model lifecycle operations.
pub type ModelResult<T> = Result<T, ModelError>;
