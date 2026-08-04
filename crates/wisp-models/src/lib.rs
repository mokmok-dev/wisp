//! Model lifecycle abstraction for Wisp.
//!
//! Wisp's local transcription model was historically a Nemotron ONNX bundle
//! loaded from a filesystem path with bespoke download/cache helpers. This
//! crate introduces a single, backend-neutral boundary — [`ModelProvider`] —
//! for the full model lifecycle: enumerate available/downloaded models, ensure
//! a model is cached, resolve it to an artifact path or endpoint, evict it,
//! control the backing service, and report status.
//!
//! Two implementations ship here:
//!
//! * [`InMemoryModelProvider`] — a dependency-free fake used by tests and as a
//!   graceful fallback.
//! * `foundry::FoundryLocalProvider` — an adapter over the Microsoft Foundry
//!   Local SDK, compiled only with the `foundry` feature so the default build
//!   stays hermetic and offline. (Referenced as plain text because the item
//!   only exists under that feature.)
//!
//! Wisp's existing filesystem-backed Nemotron cache is exposed as a provider by
//! the `wisp-audiokit` crate (`FilesystemModelProvider`), which delegates to
//! the same download/verify/status routines it always used — so wiring the
//! selection path through this abstraction preserves current behavior.

mod error;
mod provider;

#[cfg(feature = "foundry")]
pub mod foundry;

pub use error::{ModelError, ModelResult};
pub use provider::{
    InMemoryModelProvider, ModelClass, ModelDescriptor, ModelId, ModelLocation, ModelProvider,
    ModelStatus, ProgressCallback, ServiceStatus,
};
