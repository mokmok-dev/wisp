//! Microsoft Foundry Local backend for [`ModelProvider`].
//!
//! This module is only compiled with the `foundry` feature. It wraps the
//! published [`foundry-local-sdk`](https://crates.io/crates/foundry-local-sdk)
//! crate (repository: <https://github.com/microsoft/Foundry-Local>) behind the
//! synchronous [`ModelProvider`] trait.
//!
//! The SDK is async (tokio) and loads a native "Foundry Local Core" engine at
//! runtime; a small owned runtime bridges it to the synchronous trait via
//! `block_on`. Models expose a local on-disk path once cached, so
//! `resolve`/`ensure` return [`ModelLocation::Artifact`].
//!
//! Graceful degradation: [`FoundryLocalProvider::new`] fails with
//! [`ModelError::ServiceUnavailable`] when the native engine cannot be
//! initialised (e.g. it is not installed), so callers can fall back to another
//! provider — such as `wisp-audiokit`'s filesystem provider — without
//! panicking.
//!
//! ## Blocking bridge
//!
//! Because the trait is synchronous, every method drives the SDK's futures with
//! `Runtime::block_on`. `block_on` panics if called from *within* another Tokio
//! runtime, so this type must not be constructed or used from an async (Tokio)
//! context. Both [`FoundryLocalProvider::new`] and every trait method detect an
//! ambient runtime via [`tokio::runtime::Handle::try_current`] and return
//! [`ModelError::ServiceUnavailable`] instead of panicking.
//!
//! ## Why this is feature-gated and off by default
//!
//! The older `foundry-local` 0.1 REST SDK pins `url =2.4.1`, which conflicts
//! with `gpui`'s transitive `url ^2.5` requirement and makes the whole
//! workspace unresolvable. This crate therefore targets `foundry-local-sdk`
//! 1.2.x and keeps it optional so the default build stays hermetic and offline.

use std::future::Future;
use std::path::PathBuf;
use std::sync::Arc;

use foundry_local_sdk::{
    FoundryLocalConfig, FoundryLocalError, FoundryLocalManager, Model, ModelInfo,
};
use tokio::runtime::Runtime;

use crate::error::{ModelError, ModelResult};
use crate::provider::{
    ModelClass, ModelDescriptor, ModelId, ModelLocation, ModelProvider, ModelStatus,
    ProgressCallback, ServiceStatus,
};

/// Stable provider identifier for the Foundry Local backend.
pub const FOUNDRY_PROVIDER_ID: &str = "foundry-local";

/// [`ModelProvider`] backed by the Foundry Local Core engine.
pub struct FoundryLocalProvider {
    runtime: Runtime,
    // `FoundryLocalManager::create` returns a `&'static Self`: the SDK keeps the
    // native engine in a process-global singleton (`OnceLock`) it owns for the
    // life of the process. We borrow that singleton rather than owning it, so
    // there is no `Box::leak` here and nothing for us to drop.
    manager: &'static FoundryLocalManager,
}

impl std::fmt::Debug for FoundryLocalProvider {
    fn fmt(
        &self,
        formatter: &mut std::fmt::Formatter<'_>,
    ) -> std::fmt::Result {
        formatter
            .debug_struct("FoundryLocalProvider")
            .field("provider_id", &FOUNDRY_PROVIDER_ID)
            .finish_non_exhaustive()
    }
}

impl FoundryLocalProvider {
    /// Initialise the Foundry Local engine for `app_name`.
    ///
    /// The embedded OpenAI-compatible web service is pinned to a loopback
    /// address so enabling it never exposes unauthenticated local inference to
    /// other hosts. Note that `FoundryLocalManager::create` returns a
    /// process-global singleton: if the manager was already initialised earlier
    /// in the process, that existing instance is returned and this `app_name` /
    /// bind configuration is ignored. [`ModelProvider::start_service`]
    /// therefore re-checks the effective bind and refuses non-loopback
    /// endpoints defensively.
    ///
    /// # Errors
    /// Returns [`ModelError::ServiceUnavailable`] when called from within a
    /// Tokio runtime, when a runtime cannot be created, or when the native
    /// engine cannot be initialised.
    pub fn new(app_name: impl Into<String>) -> ModelResult<Self> {
        if tokio::runtime::Handle::try_current().is_ok() {
            return Err(ModelError::ServiceUnavailable(
                "FoundryLocalProvider cannot be created from within a Tokio runtime".to_owned(),
            ));
        }
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .map_err(|error| ModelError::ServiceUnavailable(error.to_string()))?;
        // Pin the embedded OpenAI-compatible web service to loopback with an
        // ephemeral port so enabling it never exposes unauthenticated local
        // inference to other hosts on the network.
        let config = FoundryLocalConfig::new(app_name).web_service_urls("http://127.0.0.1:0");
        let manager = FoundryLocalManager::create(config)
            .map_err(|error| ModelError::ServiceUnavailable(error.to_string()))?;
        Ok(Self { runtime, manager })
    }

    /// Drive a future to completion on the owned runtime, refusing to run when
    /// an ambient Tokio runtime would make `block_on` panic.
    fn block_on<F: Future>(
        &self,
        future: F,
    ) -> ModelResult<F::Output> {
        if tokio::runtime::Handle::try_current().is_ok() {
            return Err(ModelError::ServiceUnavailable(
                "FoundryLocalProvider must not be called from within a Tokio runtime".to_owned(),
            ));
        }
        Ok(self.runtime.block_on(future))
    }

    fn model(
        &self,
        id: &ModelId,
    ) -> ModelResult<Arc<Model>> {
        self.block_on(self.manager.catalog().get_model(id.as_str()))?
            .map_err(|error| map_lookup_error(id, &error))
    }

    fn descriptor_of(model: &Model) -> ModelDescriptor {
        let info = model.info();
        ModelDescriptor {
            // Lookups (`status`/`resolve`/`ensure`/`evict`) all go through
            // `catalog().get_model(alias)`, so the descriptor id must be the
            // alias for a list -> resolve round trip to succeed.
            id: ModelId::new(model.alias().to_owned()),
            display_name: info
                .display_name
                .clone()
                .unwrap_or_else(|| model.alias().to_owned()),
            class: model_class_of(info),
            size_bytes: info.file_size_mb.map(|mb| mb.saturating_mul(1_024 * 1_024)),
            downloaded: info.cached,
        }
    }
}

/// Map a `get_model` lookup failure onto the right [`ModelError`].
///
/// The SDK reports a genuinely unknown/invalid alias via `ModelOperation` or
/// `Validation`; every other variant (native load, catalog refresh, HTTP,
/// serialization, I/O, internal) means the engine/service is unreachable rather
/// than the model being absent, so callers can distinguish the two.
fn map_lookup_error(
    id: &ModelId,
    error: &FoundryLocalError,
) -> ModelError {
    match error {
        FoundryLocalError::ModelOperation { .. } | FoundryLocalError::Validation { .. } => {
            ModelError::NotFound(id.clone())
        },
        other => ModelError::ServiceUnavailable(other.to_string()),
    }
}

/// Map Foundry model metadata onto a coarse [`ModelClass`].
fn model_class_of(info: &ModelInfo) -> ModelClass {
    let task = info
        .task
        .as_deref()
        .unwrap_or_default()
        .to_ascii_lowercase();
    let outputs = info
        .output_modalities
        .as_deref()
        .unwrap_or_default()
        .to_ascii_lowercase();
    if task.contains("transcription")
        || task.contains("speech")
        || task.contains("audio")
        || outputs.contains("audio")
    {
        ModelClass::Transcription
    } else if task.contains("embed") {
        ModelClass::Embedding
    } else if task.contains("chat")
        || task.contains("text-generation")
        || task.contains("completion")
    {
        ModelClass::Chat
    } else {
        ModelClass::Other
    }
}

/// Whether an endpoint URL points at the loopback interface.
fn endpoint_is_loopback(url: &str) -> bool {
    let after_scheme = url.split_once("://").map_or(url, |(_, rest)| rest);
    let host = after_scheme
        .split(['/', '?', '#'])
        .next()
        .unwrap_or(after_scheme);
    host.starts_with("127.")
        || host.starts_with("localhost")
        || host.starts_with("[::1]")
        || host.starts_with("::1")
}

impl ModelProvider for FoundryLocalProvider {
    fn provider_id(&self) -> &str {
        FOUNDRY_PROVIDER_ID
    }

    fn service_status(&self) -> ServiceStatus {
        match self.manager.urls() {
            Ok(urls) if !urls.is_empty() => ServiceStatus::Running {
                endpoint: urls.into_iter().next(),
            },
            Ok(_) => ServiceStatus::Stopped,
            Err(error) => ServiceStatus::Unavailable(error.to_string()),
        }
    }

    fn start_service(&self) -> ModelResult<ServiceStatus> {
        self.block_on(self.manager.start_web_service())?
            .map_err(|error| ModelError::ServiceUnavailable(error.to_string()))?;
        let status = self.service_status();
        // The loopback bind requested in `new` only takes effect on first
        // singleton init; if some earlier initialiser bound a non-loopback
        // address, refuse rather than silently exposing local inference.
        if let ServiceStatus::Running {
            endpoint: Some(url),
        } = &status
            && !endpoint_is_loopback(url)
        {
            return Err(ModelError::ServiceUnavailable(format!(
                "Foundry Local web service bound to non-loopback endpoint {url}; \
                 refusing to expose local inference"
            )));
        }
        Ok(status)
    }

    fn stop_service(&self) -> ModelResult<()> {
        self.block_on(self.manager.stop_web_service())?
            .map_err(|error| ModelError::ServiceUnavailable(error.to_string()))
    }

    fn list_available(&self) -> ModelResult<Vec<ModelDescriptor>> {
        let models = self
            .block_on(self.manager.catalog().get_models())?
            .map_err(|error| ModelError::Backend(error.to_string()))?;
        Ok(models
            .iter()
            .map(|model| Self::descriptor_of(model))
            .collect())
    }

    fn list_downloaded(&self) -> ModelResult<Vec<ModelDescriptor>> {
        let models = self
            .block_on(self.manager.catalog().get_cached_models())?
            .map_err(|error| ModelError::Backend(error.to_string()))?;
        Ok(models
            .iter()
            .map(|model| Self::descriptor_of(model))
            .collect())
    }

    fn status(
        &self,
        id: &ModelId,
    ) -> ModelResult<ModelStatus> {
        let model = self.model(id)?;
        let loaded = self
            .block_on(model.is_loaded())?
            .map_err(|error| ModelError::Backend(error.to_string()))?;
        if loaded {
            return Ok(ModelStatus::Loaded);
        }
        let cached = self
            .block_on(model.is_cached())?
            .map_err(|error| ModelError::Backend(error.to_string()))?;
        Ok(if cached {
            ModelStatus::Downloaded
        } else {
            ModelStatus::NotDownloaded
        })
    }

    fn ensure(
        &self,
        id: &ModelId,
        progress: ProgressCallback<'_>,
    ) -> ModelResult<ModelLocation> {
        let model = self.model(id)?;
        // The SDK's progress callback must be `'static + Send`, which a
        // borrowed `ProgressCallback` cannot satisfy without a channel/thread
        // bridge; report coarse start/finish ticks around the blocking
        // download instead of streaming per-chunk percentages.
        progress(0, 1);
        self.block_on(model.download(None::<fn(f64)>))?
            .map_err(|error| ModelError::Download {
                id: id.clone(),
                message: error.to_string(),
            })?;
        progress(1, 1);
        let path = self
            .block_on(model.path())?
            .map_err(|error| ModelError::Backend(error.to_string()))?;
        Ok(ModelLocation::Artifact(path))
    }

    fn resolve(
        &self,
        id: &ModelId,
    ) -> ModelResult<ModelLocation> {
        let model = self.model(id)?;
        let cached = self
            .block_on(model.is_cached())?
            .map_err(|error| ModelError::Backend(error.to_string()))?;
        if !cached {
            return Err(ModelError::NotDownloaded { id: id.clone() });
        }
        let path: PathBuf = self
            .block_on(model.path())?
            .map_err(|error| ModelError::Backend(error.to_string()))?;
        Ok(ModelLocation::Artifact(path))
    }

    fn evict(
        &self,
        id: &ModelId,
    ) -> ModelResult<()> {
        let model = self.model(id)?;
        self.block_on(model.remove_from_cache())?
            .map(|_| ())
            .map_err(|error| ModelError::Backend(error.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use foundry_local_sdk::ModelInfo;

    use super::{ModelClass, endpoint_is_loopback, model_class_of};

    /// Build a `ModelInfo` with only the fields `model_class_of` inspects set;
    /// everything else takes a benign default so the mapping can be unit-tested
    /// without a running engine.
    fn info(
        task: Option<&str>,
        output_modalities: Option<&str>,
    ) -> ModelInfo {
        ModelInfo {
            id: "id".to_owned(),
            name: "name".to_owned(),
            version: 1,
            alias: "alias".to_owned(),
            display_name: None,
            provider_type: "AzureFoundry".to_owned(),
            uri: "azureml://x".to_owned(),
            model_type: "onnx".to_owned(),
            prompt_template: None,
            publisher: None,
            model_settings: None,
            license: None,
            license_description: None,
            cached: false,
            task: task.map(str::to_owned),
            runtime: None,
            file_size_mb: None,
            supports_tool_calling: None,
            max_output_tokens: None,
            min_fl_version: None,
            created_at_unix: 0,
            context_length: None,
            input_modalities: None,
            output_modalities: output_modalities.map(str::to_owned),
            capabilities: None,
        }
    }

    #[test]
    fn model_class_is_derived_from_task_and_modalities() {
        assert_eq!(
            model_class_of(&info(Some("automatic-speech-recognition"), None)),
            ModelClass::Transcription
        );
        assert_eq!(
            model_class_of(&info(Some("generic"), Some("audio"))),
            ModelClass::Transcription
        );
        assert_eq!(
            model_class_of(&info(Some("text-embedding"), None)),
            ModelClass::Embedding
        );
        assert_eq!(
            model_class_of(&info(Some("chat-completion"), None)),
            ModelClass::Chat
        );
        assert_eq!(model_class_of(&info(None, None)), ModelClass::Other);
    }

    #[test]
    fn loopback_endpoints_are_recognised() {
        assert!(endpoint_is_loopback("http://127.0.0.1:5273"));
        assert!(endpoint_is_loopback("http://localhost:5273/v1"));
        assert!(endpoint_is_loopback("http://[::1]:8080"));
        assert!(!endpoint_is_loopback("http://0.0.0.0:5273"));
        assert!(!endpoint_is_loopback("http://192.168.1.10:5273"));
    }
}
