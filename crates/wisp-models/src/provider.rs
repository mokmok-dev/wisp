//! Provider-neutral model lifecycle contracts and value types.
//!
//! The [`ModelProvider`] trait is the stable boundary every backend
//! implements: a filesystem cache, an in-memory fake for tests, or the
//! optional Foundry Local service adapter. It mirrors the capture/transcriber
//! backend/factory style used elsewhere in the workspace — small, object-safe,
//! `Send + Sync`, with `thiserror`-based errors — so a provider can be handed
//! around as `Box<dyn ModelProvider>` / `Arc<dyn ModelProvider>`.

use std::path::PathBuf;
use std::sync::Mutex;
use std::sync::PoisonError;

use crate::error::{ModelError, ModelResult};

/// Stable identifier for a model within a provider (e.g. `"nemotron"` or a
/// Foundry alias such as `"phi-4-mini"`).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ModelId(String);

impl ModelId {
    /// Wrap a string as a [`ModelId`].
    #[must_use]
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    /// Borrow the identifier as a string slice.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for ModelId {
    fn fmt(
        &self,
        formatter: &mut std::fmt::Formatter<'_>,
    ) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

/// Broad task category a model serves. Kept coarse on purpose; providers can
/// carry finer detail out of band.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum ModelClass {
    /// Speech-to-text / transcription models (Wisp's current use).
    Transcription,
    /// Chat / text-generation models.
    Chat,
    /// Embedding models.
    Embedding,
    /// Anything not covered above.
    Other,
}

/// Where a resolved model can be consumed from.
///
/// Local bundles resolve to an on-disk [`Self::Artifact`]; service-backed
/// models (Foundry Local) resolve to an OpenAI-compatible [`Self::Endpoint`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ModelLocation {
    /// A filesystem path to a model file or provider-owned bundle directory.
    Artifact(PathBuf),
    /// A network endpoint that serves the model.
    Endpoint {
        /// Base URL of the serving endpoint.
        url: String,
        /// The model identifier to reference against the endpoint.
        model: ModelId,
    },
}

impl ModelLocation {
    /// Borrow the artifact path when this is a local bundle.
    #[must_use]
    pub fn as_artifact(&self) -> Option<&std::path::Path> {
        match self {
            Self::Artifact(path) => Some(path.as_path()),
            Self::Endpoint { .. } => None,
        }
    }

    /// Consume the location, yielding the artifact path when local.
    #[must_use]
    pub fn into_artifact(self) -> Option<PathBuf> {
        match self {
            Self::Artifact(path) => Some(path),
            Self::Endpoint { .. } => None,
        }
    }
}

/// Cache/availability state of a single model.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ModelStatus {
    /// Known to the provider but not present in the local cache.
    NotDownloaded,
    /// Present in the local cache and ready to resolve.
    Downloaded,
    /// Loaded into memory / actively served.
    Loaded,
    /// Known but currently unusable, with a human-readable reason.
    Unavailable(String),
}

impl ModelStatus {
    /// Whether the model can be resolved and used right now.
    #[must_use]
    pub const fn is_ready(&self) -> bool {
        matches!(self, Self::Downloaded | Self::Loaded)
    }
}

/// Lifecycle state of a provider's backing service.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ServiceStatus {
    /// The service is not running.
    Stopped,
    /// The service is running, optionally exposing an endpoint URL.
    Running {
        /// Base endpoint URL, when the service exposes one.
        endpoint: Option<String>,
    },
    /// The service cannot be used, with a human-readable reason.
    Unavailable(String),
}

impl ServiceStatus {
    /// Whether the service is currently running.
    #[must_use]
    pub const fn is_running(&self) -> bool {
        matches!(self, Self::Running { .. })
    }
}

/// Summary of a single model as reported by a provider.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelDescriptor {
    /// Provider-stable identifier.
    pub id: ModelId,
    /// Human-readable name for display.
    pub display_name: String,
    /// Coarse task category.
    pub class: ModelClass,
    /// Total on-disk size in bytes, when known.
    pub size_bytes: Option<u64>,
    /// Whether the model is present in the local cache.
    pub downloaded: bool,
}

/// A downloaded/installed-byte progress callback: `(downloaded, total)`.
pub type ProgressCallback<'a> = &'a mut dyn FnMut(u64, u64);

/// Unified lifecycle boundary for local models.
///
/// Implementations manage the full lifecycle: enumerate available/downloaded
/// models, ensure a model is cached, resolve it to an artifact path or
/// endpoint, evict it, control the backing service, and report status.
pub trait ModelProvider: Send + Sync {
    /// Stable identifier for this provider implementation.
    fn provider_id(&self) -> &str;

    /// Current state of the provider's backing service.
    fn service_status(&self) -> ServiceStatus;

    /// Start the backing service if the provider has one.
    ///
    /// # Errors
    /// Returns an error when the service cannot be started or reached.
    fn start_service(&self) -> ModelResult<ServiceStatus>;

    /// Stop the backing service if the provider has one.
    ///
    /// # Errors
    /// Returns an error when the service cannot be stopped, or
    /// [`ModelError::Unsupported`] when the provider has no controllable
    /// service.
    fn stop_service(&self) -> ModelResult<()>;

    /// List every model the provider can offer, downloaded or not.
    ///
    /// # Errors
    /// Returns an error when the catalog cannot be read.
    fn list_available(&self) -> ModelResult<Vec<ModelDescriptor>>;

    /// List only models present in the local cache.
    ///
    /// # Errors
    /// Returns an error when the cache cannot be read.
    fn list_downloaded(&self) -> ModelResult<Vec<ModelDescriptor>>;

    /// Report the cache/availability status of a single model.
    ///
    /// # Errors
    /// Returns [`ModelError::NotFound`] for unknown models, or a backend
    /// error when status cannot be determined.
    fn status(
        &self,
        id: &ModelId,
    ) -> ModelResult<ModelStatus>;

    /// Ensure a model is downloaded/cached, reporting progress, and resolve it.
    ///
    /// # Errors
    /// Returns [`ModelError::NotFound`] for unknown models or
    /// [`ModelError::Download`] when acquisition fails.
    fn ensure(
        &self,
        id: &ModelId,
        progress: ProgressCallback<'_>,
    ) -> ModelResult<ModelLocation>;

    /// Resolve an already-cached model to an artifact path or endpoint.
    ///
    /// # Errors
    /// Returns [`ModelError::NotDownloaded`] when the model is absent locally.
    fn resolve(
        &self,
        id: &ModelId,
    ) -> ModelResult<ModelLocation>;

    /// Remove a model from the local cache.
    ///
    /// # Errors
    /// Returns [`ModelError::Unsupported`] when eviction is not possible, or a
    /// backend error when removal fails.
    fn evict(
        &self,
        id: &ModelId,
    ) -> ModelResult<()>;

    /// Ensure a model is cached without observing progress.
    ///
    /// # Errors
    /// See [`ModelProvider::ensure`].
    fn ensure_cached(
        &self,
        id: &ModelId,
    ) -> ModelResult<ModelLocation> {
        self.ensure(id, &mut |_, _| {})
    }
}

/// A single entry inside [`InMemoryModelProvider`].
#[derive(Debug, Clone)]
struct InMemoryEntry {
    descriptor: ModelDescriptor,
    artifact: PathBuf,
}

/// In-memory [`ModelProvider`] used by tests and as a graceful fallback.
///
/// It performs no I/O: `ensure` simply flips a model to downloaded and reports
/// its full size once. This keeps unit tests deterministic and offline while
/// still exercising the full trait surface.
#[derive(Debug)]
pub struct InMemoryModelProvider {
    provider_id: String,
    running: Mutex<bool>,
    entries: Mutex<Vec<InMemoryEntry>>,
}

impl InMemoryModelProvider {
    /// Create an empty provider with the given identifier. The backing
    /// "service" starts stopped.
    #[must_use]
    pub fn new(provider_id: impl Into<String>) -> Self {
        Self {
            provider_id: provider_id.into(),
            running: Mutex::new(false),
            entries: Mutex::new(Vec::new()),
        }
    }

    /// Register a model. `downloaded` seeds the initial cache state and
    /// `artifact` is the path `resolve`/`ensure` report for it.
    #[must_use]
    pub fn with_model(
        self,
        id: ModelId,
        display_name: impl Into<String>,
        class: ModelClass,
        size_bytes: Option<u64>,
        downloaded: bool,
        artifact: impl Into<PathBuf>,
    ) -> Self {
        {
            let mut entries = self.lock_entries();
            entries.push(InMemoryEntry {
                descriptor: ModelDescriptor {
                    id,
                    display_name: display_name.into(),
                    class,
                    size_bytes,
                    downloaded,
                },
                artifact: artifact.into(),
            });
        }
        self
    }

    fn lock_entries(&self) -> std::sync::MutexGuard<'_, Vec<InMemoryEntry>> {
        self.entries.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn lock_running(&self) -> std::sync::MutexGuard<'_, bool> {
        self.running.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn location_of(entry: &InMemoryEntry) -> ModelLocation {
        ModelLocation::Artifact(entry.artifact.clone())
    }
}

impl ModelProvider for InMemoryModelProvider {
    fn provider_id(&self) -> &str {
        &self.provider_id
    }

    fn service_status(&self) -> ServiceStatus {
        let running = *self.lock_running();
        if running {
            ServiceStatus::Running { endpoint: None }
        } else {
            ServiceStatus::Stopped
        }
    }

    fn start_service(&self) -> ModelResult<ServiceStatus> {
        *self.lock_running() = true;
        Ok(ServiceStatus::Running { endpoint: None })
    }

    fn stop_service(&self) -> ModelResult<()> {
        *self.lock_running() = false;
        Ok(())
    }

    fn list_available(&self) -> ModelResult<Vec<ModelDescriptor>> {
        Ok(self
            .lock_entries()
            .iter()
            .map(|entry| entry.descriptor.clone())
            .collect())
    }

    fn list_downloaded(&self) -> ModelResult<Vec<ModelDescriptor>> {
        Ok(self
            .lock_entries()
            .iter()
            .filter(|entry| entry.descriptor.downloaded)
            .map(|entry| entry.descriptor.clone())
            .collect())
    }

    fn status(
        &self,
        id: &ModelId,
    ) -> ModelResult<ModelStatus> {
        let downloaded = {
            let entries = self.lock_entries();
            entries
                .iter()
                .find(|entry| entry.descriptor.id == *id)
                .map(|entry| entry.descriptor.downloaded)
        };
        match downloaded {
            Some(true) => Ok(ModelStatus::Downloaded),
            Some(false) => Ok(ModelStatus::NotDownloaded),
            None => Err(ModelError::NotFound(id.clone())),
        }
    }

    fn ensure(
        &self,
        id: &ModelId,
        progress: ProgressCallback<'_>,
    ) -> ModelResult<ModelLocation> {
        // Mutate the cache state under the lock, then release it before
        // invoking the progress callback so no user code runs while held.
        let (location, total) = {
            let mut entries = self.lock_entries();
            let entry = entries
                .iter_mut()
                .find(|entry| entry.descriptor.id == *id)
                .ok_or_else(|| ModelError::NotFound(id.clone()))?;
            entry.descriptor.downloaded = true;
            let location = Self::location_of(entry);
            let total = entry.descriptor.size_bytes.unwrap_or(0);
            drop(entries);
            (location, total)
        };
        progress(total, total);
        Ok(location)
    }

    fn resolve(
        &self,
        id: &ModelId,
    ) -> ModelResult<ModelLocation> {
        let found = {
            let entries = self.lock_entries();
            entries
                .iter()
                .find(|entry| entry.descriptor.id == *id)
                .map(|entry| (entry.descriptor.downloaded, Self::location_of(entry)))
        };
        match found {
            Some((true, location)) => Ok(location),
            Some((false, _)) => Err(ModelError::NotDownloaded { id: id.clone() }),
            None => Err(ModelError::NotFound(id.clone())),
        }
    }

    fn evict(
        &self,
        id: &ModelId,
    ) -> ModelResult<()> {
        let mut entries = self.lock_entries();
        let entry = entries
            .iter_mut()
            .find(|entry| entry.descriptor.id == *id)
            .ok_or_else(|| ModelError::NotFound(id.clone()))?;
        entry.descriptor.downloaded = false;
        drop(entries);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::{
        InMemoryModelProvider, ModelClass, ModelError, ModelId, ModelLocation, ModelProvider,
        ModelStatus, ServiceStatus,
    };

    fn provider() -> InMemoryModelProvider {
        InMemoryModelProvider::new("test").with_model(
            ModelId::new("demo"),
            "Demo Model",
            ModelClass::Transcription,
            Some(1_024),
            false,
            "/cache/demo",
        )
    }

    #[test]
    fn service_lifecycle_toggles_running_state() {
        let provider = provider();
        assert_eq!(provider.service_status(), ServiceStatus::Stopped);
        assert!(!provider.service_status().is_running());
        let started = provider.start_service().expect("start");
        assert!(started.is_running());
        assert!(provider.service_status().is_running());
        provider.stop_service().expect("stop");
        assert_eq!(provider.service_status(), ServiceStatus::Stopped);
    }

    #[test]
    fn listing_separates_available_from_downloaded() {
        let provider = provider();
        assert_eq!(provider.list_available().expect("available").len(), 1);
        assert!(provider.list_downloaded().expect("downloaded").is_empty());
    }

    #[test]
    fn unknown_model_is_not_found() {
        let provider = provider();
        let missing = ModelId::new("nope");
        assert_eq!(
            provider.status(&missing),
            Err(ModelError::NotFound(missing.clone()))
        );
        assert_eq!(
            provider.resolve(&missing),
            Err(ModelError::NotFound(missing))
        );
    }

    #[test]
    fn resolve_requires_download_first() {
        let provider = provider();
        let id = ModelId::new("demo");
        assert_eq!(provider.status(&id), Ok(ModelStatus::NotDownloaded));
        assert_eq!(
            provider.resolve(&id),
            Err(ModelError::NotDownloaded { id: id.clone() })
        );

        let mut seen = Vec::new();
        let location = provider
            .ensure(&id, &mut |downloaded, total| seen.push((downloaded, total)))
            .expect("ensure");
        assert_eq!(location, ModelLocation::Artifact("/cache/demo".into()));
        assert_eq!(seen, vec![(1_024, 1_024)]);

        assert_eq!(provider.status(&id), Ok(ModelStatus::Downloaded));
        assert_eq!(provider.resolve(&id), Ok(location));
        assert_eq!(provider.list_downloaded().expect("downloaded").len(), 1);
    }

    #[test]
    fn ensure_cached_default_method_downloads() {
        let provider = provider();
        let id = ModelId::new("demo");
        let location = provider.ensure_cached(&id).expect("ensure_cached");
        assert_eq!(
            location.as_artifact(),
            Some(std::path::Path::new("/cache/demo"))
        );
        assert!(provider.status(&id).expect("status").is_ready());
    }

    #[test]
    fn evict_returns_model_to_not_downloaded() {
        let provider = provider();
        let id = ModelId::new("demo");
        provider.ensure_cached(&id).expect("ensure");
        provider.evict(&id).expect("evict");
        assert_eq!(provider.status(&id), Ok(ModelStatus::NotDownloaded));
    }

    #[test]
    fn provider_is_object_safe() {
        let provider: Box<dyn ModelProvider> = Box::new(provider());
        assert_eq!(provider.provider_id(), "test");
    }

    #[test]
    fn ensure_and_evict_reject_unknown_models() {
        let provider = provider();
        let missing = ModelId::new("nope");
        assert_eq!(
            provider.ensure(&missing, &mut |_, _| {}),
            Err(ModelError::NotFound(missing.clone()))
        );
        assert_eq!(provider.evict(&missing), Err(ModelError::NotFound(missing)));
    }

    #[test]
    fn model_location_accessors_distinguish_artifact_from_endpoint() {
        let artifact = ModelLocation::Artifact("/cache/demo".into());
        assert_eq!(
            artifact.as_artifact(),
            Some(std::path::Path::new("/cache/demo"))
        );
        assert_eq!(artifact.into_artifact(), Some("/cache/demo".into()));

        let endpoint = ModelLocation::Endpoint {
            url: "http://127.0.0.1:8080".to_owned(),
            model: ModelId::new("demo"),
        };
        assert_eq!(endpoint.as_artifact(), None);
        assert_eq!(endpoint.into_artifact(), None);
    }

    #[test]
    fn status_and_service_predicates_cover_every_variant() {
        assert!(ModelStatus::Downloaded.is_ready());
        assert!(ModelStatus::Loaded.is_ready());
        assert!(!ModelStatus::NotDownloaded.is_ready());
        assert!(!ModelStatus::Unavailable("offline".to_owned()).is_ready());

        assert!(ServiceStatus::Running { endpoint: None }.is_running());
        assert!(
            ServiceStatus::Running {
                endpoint: Some("http://127.0.0.1:0".to_owned()),
            }
            .is_running()
        );
        assert!(!ServiceStatus::Stopped.is_running());
        assert!(!ServiceStatus::Unavailable("no engine".to_owned()).is_running());
    }

    #[test]
    fn model_id_displays_and_borrows_inner_string() {
        let id = ModelId::new("phi-4-mini");
        assert_eq!(id.as_str(), "phi-4-mini");
        assert_eq!(id.to_string(), "phi-4-mini");
    }
}
