//! Filesystem-backed [`ModelProvider`] over Wisp's local model cache.
//!
//! This adapter exposes the existing Nemotron download/verify/status routines
//! (`local_model_*` / `download_local_model_*`) through the workspace-wide
//! [`wisp_models::ModelProvider`] boundary. It performs no new I/O of its own —
//! every operation delegates to the same functions the desktop app already
//! calls — so routing the local-model selection path through this abstraction
//! preserves current behavior while giving Wisp a unified way to list, acquire,
//! resolve, and evict local models.
//!
//! The optional Foundry Local backend lives in `wisp-models` behind the
//! `foundry` feature; both implement the same trait, so a caller can swap one
//! for the other (or fall back gracefully) without touching the session path.

use std::path::{Path, PathBuf};

use wisp_models::{
    ModelClass, ModelDescriptor, ModelError, ModelId, ModelLocation, ModelProvider, ModelResult,
    ModelStatus, ProgressCallback, ServiceStatus,
};

use crate::{
    LocalModelId, NemotronTranscriberFactory, TranscriberBackend,
    download_local_model_for_with_progress, local_model_path_for, local_model_spec_for,
    local_model_specs,
};

/// [`ModelId`] value used for the bundled Nemotron model.
pub const NEMOTRON_MODEL_ID: &str = "nemotron";

/// Stable identifier reported by [`FilesystemModelProvider`].
pub const FILESYSTEM_PROVIDER_ID: &str = "wisp-filesystem";

/// The [`ModelId`] for the bundled Nemotron model.
#[must_use]
pub fn nemotron_model_id() -> ModelId {
    ModelId::new(NEMOTRON_MODEL_ID)
}

fn model_id_of(id: LocalModelId) -> ModelId {
    match id {
        LocalModelId::Nemotron => nemotron_model_id(),
    }
}

fn local_model_id_from(id: &ModelId) -> Option<LocalModelId> {
    match id.as_str() {
        NEMOTRON_MODEL_ID => Some(LocalModelId::Nemotron),
        _ => None,
    }
}

fn require_local_id(id: &ModelId) -> ModelResult<LocalModelId> {
    local_model_id_from(id).ok_or_else(|| ModelError::NotFound(id.clone()))
}

/// Cheap presence check: every artifact exists with its expected byte length.
///
/// This is deliberately not a full integrity check. The readiness notions in
/// play are:
///
/// * SHA-256 verification — enforced only when acquiring a bundle (`ensure` ->
///   the verifying downloader) and by the desktop setup UI's
///   `local_model_status_for`. This is the one place the ~682 MB bundle is
///   hashed.
/// * Presence + exact byte size — used here for listing/`status`/`resolve` and,
///   by extension, the session-start path that resolves through this provider,
///   so surfacing state or starting a session never hashes the bundle.
/// * Presence + non-zero length — the transcriber load path's own
///   `model_bundle_ready` gate at `OnlineRecognizer` construction.
///
/// Accepted trust boundary: the model cache lives under the user's own data
/// directory, so local write access to it is trusted; the byte-size check
/// guards against truncated/interrupted downloads rather than a malicious local
/// tamperer (which the acquisition-time SHA-256 already covers).
fn bundle_present(
    data_dir: &Path,
    id: LocalModelId,
) -> bool {
    let path = local_model_path_for(data_dir, id);
    local_model_spec_for(id).artifacts.iter().all(|artifact| {
        std::fs::metadata(path.join(artifact.filename))
            .is_ok_and(|meta| meta.is_file() && meta.len() == artifact.bytes)
    })
}

fn descriptor_for(
    data_dir: &Path,
    id: LocalModelId,
) -> ModelDescriptor {
    let spec = local_model_spec_for(id);
    ModelDescriptor {
        id: model_id_of(id),
        display_name: spec.name.to_owned(),
        class: ModelClass::Transcription,
        size_bytes: Some(spec.bytes),
        downloaded: bundle_present(data_dir, id),
    }
}

/// A [`ModelProvider`] backed by Wisp's on-disk model cache under a data
/// directory.
#[derive(Debug, Clone)]
pub struct FilesystemModelProvider {
    data_dir: PathBuf,
}

impl FilesystemModelProvider {
    /// Build a provider rooted at Wisp's data directory (models live under
    /// `<data_dir>/models`).
    #[must_use]
    pub fn new(data_dir: impl Into<PathBuf>) -> Self {
        Self {
            data_dir: data_dir.into(),
        }
    }
}

impl ModelProvider for FilesystemModelProvider {
    fn provider_id(&self) -> &str {
        FILESYSTEM_PROVIDER_ID
    }

    fn service_status(&self) -> ServiceStatus {
        // A filesystem cache has no service to manage; report it as always
        // "running" with no endpoint so callers can treat every provider
        // uniformly.
        ServiceStatus::Running { endpoint: None }
    }

    fn start_service(&self) -> ModelResult<ServiceStatus> {
        Ok(self.service_status())
    }

    fn stop_service(&self) -> ModelResult<()> {
        Ok(())
    }

    fn list_available(&self) -> ModelResult<Vec<ModelDescriptor>> {
        Ok(local_model_specs()
            .iter()
            .map(|spec| descriptor_for(&self.data_dir, spec.id))
            .collect())
    }

    fn list_downloaded(&self) -> ModelResult<Vec<ModelDescriptor>> {
        Ok(self
            .list_available()?
            .into_iter()
            .filter(|descriptor| descriptor.downloaded)
            .collect())
    }

    fn status(
        &self,
        id: &ModelId,
    ) -> ModelResult<ModelStatus> {
        let local = require_local_id(id)?;
        Ok(if bundle_present(&self.data_dir, local) {
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
        let local = require_local_id(id)?;
        let status =
            download_local_model_for_with_progress(&self.data_dir, local, |downloaded, total| {
                progress(downloaded, total);
            })
            .map_err(|error| ModelError::Download {
                id: id.clone(),
                message: error.to_string(),
            })?;
        Ok(ModelLocation::Artifact(status.path().to_path_buf()))
    }

    fn resolve(
        &self,
        id: &ModelId,
    ) -> ModelResult<ModelLocation> {
        let local = require_local_id(id)?;
        if bundle_present(&self.data_dir, local) {
            Ok(ModelLocation::Artifact(local_model_path_for(
                &self.data_dir,
                local,
            )))
        } else {
            Err(ModelError::NotDownloaded { id: id.clone() })
        }
    }

    fn evict(
        &self,
        id: &ModelId,
    ) -> ModelResult<()> {
        let local = require_local_id(id)?;
        let path = local_model_path_for(&self.data_dir, local);
        match std::fs::remove_dir_all(&path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(ModelError::Backend(format!(
                "failed to evict {}: {error}",
                path.display()
            ))),
        }
    }
}

/// Build a Nemotron transcriber by resolving its artifact through a
/// [`ModelProvider`].
///
/// This is the seam that lets the existing local-model selection path acquire
/// its artifact through the lifecycle abstraction instead of a hand-supplied
/// path: resolve the (already cached) model, then hand the artifact to the
/// existing [`NemotronTranscriberFactory`].
///
/// # Errors
/// Returns [`ModelError::NotDownloaded`] when the model is not cached,
/// [`ModelError::NotFound`] for an unknown id, or [`ModelError::Unsupported`]
/// when the provider only offers a remote endpoint (Nemotron needs a local
/// bundle).
pub fn nemotron_transcriber_from_provider(
    provider: &dyn ModelProvider,
    id: &ModelId,
    locale: &str,
) -> ModelResult<Box<dyn TranscriberBackend>> {
    match provider.resolve(id)? {
        ModelLocation::Artifact(path) => {
            Ok(NemotronTranscriberFactory::from_artifact(path, locale))
        },
        ModelLocation::Endpoint { .. } => Err(ModelError::Unsupported {
            provider: provider.provider_id().to_owned(),
            operation: "Nemotron requires a local artifact, not a remote endpoint".to_owned(),
        }),
    }
}

/// Build a Nemotron transcriber for an already-resolved bundle path, routing
/// the resolution through [`FilesystemModelProvider`].
///
/// This is the production seam used by the platform session builders: they hold
/// the bundle path (`<data_dir>/models/<bundle>`), so this derives the data
/// directory, resolves the model through the [`ModelProvider`] abstraction
/// (cheap presence check, no hashing), and only falls back to the direct
/// [`NemotronTranscriberFactory`] when the provider cannot resolve it — keeping
/// behavior identical to the legacy path in every case.
#[must_use]
pub fn nemotron_transcriber_via_filesystem_provider(
    bundle_path: PathBuf,
    locale: &str,
) -> Box<dyn TranscriberBackend> {
    if let Some(data_dir) = bundle_path.parent().and_then(Path::parent) {
        let provider = FilesystemModelProvider::new(data_dir);
        if let Ok(backend) =
            nemotron_transcriber_from_provider(&provider, &nemotron_model_id(), locale)
        {
            return backend;
        }
    }
    NemotronTranscriberFactory::from_artifact(bundle_path, locale)
}

#[cfg(test)]
mod tests {
    use wisp_models::{
        InMemoryModelProvider, ModelClass, ModelDescriptor, ModelError, ModelId, ModelLocation,
        ModelProvider, ModelResult, ModelStatus, ProgressCallback, ServiceStatus,
    };

    use super::{
        FILESYSTEM_PROVIDER_ID, FilesystemModelProvider, NEMOTRON_MODEL_ID, nemotron_model_id,
        nemotron_transcriber_from_provider, nemotron_transcriber_via_filesystem_provider,
    };
    use crate::{
        LocalModelId, NEMOTRON_BACKEND_ID, TranscriberBackend, local_model_path_for,
        local_model_spec_for,
    };

    /// Materialise the canonical Nemotron bundle with correctly sized, sparse
    /// artifacts (no bytes are actually written), returning the bundle path.
    fn materialize_bundle(data_dir: &std::path::Path) -> std::path::PathBuf {
        let path = local_model_path_for(data_dir, LocalModelId::Nemotron);
        std::fs::create_dir_all(&path).expect("create bundle dir");
        for artifact in local_model_spec_for(LocalModelId::Nemotron).artifacts {
            let file =
                std::fs::File::create(path.join(artifact.filename)).expect("create artifact");
            file.set_len(artifact.bytes).expect("size artifact");
        }
        path
    }

    const ENDPOINT_FAKE_ID: &str = "endpoint-fake";

    /// Minimal endpoint-only provider used to exercise the `Endpoint` branch of
    /// [`nemotron_transcriber_from_provider`].
    struct EndpointProvider;

    impl ModelProvider for EndpointProvider {
        fn provider_id(&self) -> &str {
            ENDPOINT_FAKE_ID
        }
        fn service_status(&self) -> ServiceStatus {
            ServiceStatus::Running {
                endpoint: Some("http://127.0.0.1:0".to_owned()),
            }
        }
        fn start_service(&self) -> ModelResult<ServiceStatus> {
            Ok(self.service_status())
        }
        fn stop_service(&self) -> ModelResult<()> {
            Ok(())
        }
        fn list_available(&self) -> ModelResult<Vec<ModelDescriptor>> {
            Ok(Vec::new())
        }
        fn list_downloaded(&self) -> ModelResult<Vec<ModelDescriptor>> {
            Ok(Vec::new())
        }
        fn status(
            &self,
            _id: &ModelId,
        ) -> ModelResult<ModelStatus> {
            Ok(ModelStatus::Downloaded)
        }
        fn ensure(
            &self,
            id: &ModelId,
            _progress: ProgressCallback<'_>,
        ) -> ModelResult<ModelLocation> {
            self.resolve(id)
        }
        fn resolve(
            &self,
            id: &ModelId,
        ) -> ModelResult<ModelLocation> {
            Ok(ModelLocation::Endpoint {
                url: "http://127.0.0.1:0".to_owned(),
                model: id.clone(),
            })
        }
        fn evict(
            &self,
            _id: &ModelId,
        ) -> ModelResult<()> {
            Ok(())
        }
    }

    #[test]
    fn provider_reports_identity_and_service() {
        let dir = tempfile::tempdir().expect("temp dir");
        let provider = FilesystemModelProvider::new(dir.path());
        assert_eq!(provider.provider_id(), FILESYSTEM_PROVIDER_ID);
        assert_eq!(
            provider.service_status(),
            ServiceStatus::Running { endpoint: None }
        );
        // Filesystem service controls are graceful no-ops.
        assert!(provider.start_service().is_ok());
        assert!(provider.stop_service().is_ok());
    }

    #[test]
    fn nemotron_model_id_is_stable() {
        assert_eq!(NEMOTRON_MODEL_ID, "nemotron");
        assert_eq!(nemotron_model_id().as_str(), "nemotron");
    }

    #[test]
    fn lists_nemotron_as_available_but_not_downloaded() {
        let dir = tempfile::tempdir().expect("temp dir");
        let provider = FilesystemModelProvider::new(dir.path());
        let available = provider.list_available().expect("available");
        assert_eq!(available.len(), 1);
        let descriptor = &available[0];
        assert_eq!(descriptor.id, nemotron_model_id());
        assert_eq!(descriptor.class, ModelClass::Transcription);
        assert_eq!(
            descriptor.size_bytes,
            Some(local_model_spec_for(LocalModelId::Nemotron).bytes)
        );
        assert!(!descriptor.display_name.is_empty());
        assert!(!descriptor.downloaded);
        assert!(provider.list_downloaded().expect("downloaded").is_empty());
    }

    #[test]
    fn status_and_resolve_reflect_missing_bundle() {
        let dir = tempfile::tempdir().expect("temp dir");
        let provider = FilesystemModelProvider::new(dir.path());
        let id = nemotron_model_id();
        assert_eq!(provider.status(&id), Ok(ModelStatus::NotDownloaded));
        assert_eq!(
            provider.resolve(&id),
            Err(ModelError::NotDownloaded { id: id.clone() })
        );
    }

    #[test]
    fn unknown_model_is_not_found_across_operations() {
        let dir = tempfile::tempdir().expect("temp dir");
        let provider = FilesystemModelProvider::new(dir.path());
        let unknown = ModelId::new("does-not-exist");
        assert_eq!(
            provider.status(&unknown),
            Err(ModelError::NotFound(unknown.clone()))
        );
        assert_eq!(
            provider.resolve(&unknown),
            Err(ModelError::NotFound(unknown.clone()))
        );
        assert_eq!(
            provider.ensure(&unknown, &mut |_, _| {}),
            Err(ModelError::NotFound(unknown.clone()))
        );
        assert_eq!(provider.evict(&unknown), Err(ModelError::NotFound(unknown)));
    }

    #[test]
    fn evict_is_idempotent_when_bundle_absent() {
        let dir = tempfile::tempdir().expect("temp dir");
        let provider = FilesystemModelProvider::new(dir.path());
        assert!(provider.evict(&nemotron_model_id()).is_ok());
    }

    #[test]
    fn evict_removes_an_existing_bundle_directory() {
        let dir = tempfile::tempdir().expect("temp dir");
        let provider = FilesystemModelProvider::new(dir.path());
        let bundle = dir
            .path()
            .join("models")
            .join(local_model_spec_for(LocalModelId::Nemotron).filename);
        std::fs::create_dir_all(&bundle).expect("create bundle");
        std::fs::write(bundle.join("encoder.int8.onnx"), b"stub").expect("write stub");
        assert!(bundle.exists());
        provider.evict(&nemotron_model_id()).expect("evict");
        assert!(!bundle.exists());
    }

    #[test]
    fn transcriber_from_provider_requires_cached_artifact() {
        let dir = tempfile::tempdir().expect("temp dir");
        let provider = FilesystemModelProvider::new(dir.path());
        let result = nemotron_transcriber_from_provider(&provider, &nemotron_model_id(), "ja-JP");
        assert!(matches!(result, Err(ModelError::NotDownloaded { .. })));
    }

    #[test]
    fn transcriber_from_provider_builds_backend_for_artifact() {
        let id = nemotron_model_id();
        let fake = InMemoryModelProvider::new("fake").with_model(
            id.clone(),
            "Nemotron",
            ModelClass::Transcription,
            None,
            true,
            "/tmp/nemotron-bundle",
        );
        let backend = nemotron_transcriber_from_provider(&fake, &id, "ja-JP")
            .expect("artifact resolves to a backend");
        assert_eq!(backend.probe().backend_id.as_str(), NEMOTRON_BACKEND_ID);
    }

    #[test]
    fn transcriber_from_provider_rejects_remote_endpoint() {
        let result =
            nemotron_transcriber_from_provider(&EndpointProvider, &nemotron_model_id(), "ja-JP");
        assert!(matches!(result, Err(ModelError::Unsupported { .. })));
    }

    #[test]
    fn via_filesystem_provider_falls_back_to_direct_artifact() {
        // No bundle exists at the derived data dir, so this exercises the
        // fallback branch and must still yield a Nemotron backend.
        let backend = nemotron_transcriber_via_filesystem_provider(
            std::path::PathBuf::from("/tmp/models/nemotron-bundle"),
            "ja-JP",
        );
        assert_eq!(backend.probe().backend_id.as_str(), NEMOTRON_BACKEND_ID);
    }

    #[test]
    fn present_bundle_resolves_to_canonical_path() {
        let dir = tempfile::tempdir().expect("temp dir");
        let provider = FilesystemModelProvider::new(dir.path());
        let bundle = materialize_bundle(dir.path());
        let id = nemotron_model_id();

        assert_eq!(provider.status(&id), Ok(ModelStatus::Downloaded));
        assert_eq!(
            provider.resolve(&id),
            Ok(ModelLocation::Artifact(bundle.clone()))
        );
        assert_eq!(
            bundle,
            local_model_path_for(dir.path(), LocalModelId::Nemotron)
        );

        let downloaded = provider.list_downloaded().expect("downloaded");
        assert_eq!(downloaded.len(), 1);
        assert_eq!(downloaded[0].id, id);
        assert!(downloaded[0].downloaded);
    }

    #[test]
    fn wrong_size_artifact_is_not_downloaded() {
        let dir = tempfile::tempdir().expect("temp dir");
        let provider = FilesystemModelProvider::new(dir.path());
        let bundle = materialize_bundle(dir.path());
        let first = local_model_spec_for(LocalModelId::Nemotron).artifacts[0];
        let corrupt = std::fs::File::create(bundle.join(first.filename)).expect("recreate");
        corrupt
            .set_len(first.bytes.saturating_sub(1))
            .expect("shrink artifact");

        let id = nemotron_model_id();
        assert_eq!(provider.status(&id), Ok(ModelStatus::NotDownloaded));
        assert!(matches!(
            provider.resolve(&id),
            Err(ModelError::NotDownloaded { .. })
        ));
        assert!(provider.list_downloaded().expect("downloaded").is_empty());
    }

    #[test]
    fn via_filesystem_provider_uses_resolved_bundle_when_present() {
        let dir = tempfile::tempdir().expect("temp dir");
        let bundle = materialize_bundle(dir.path());
        // The bundle resolves through the provider (success branch), yielding a
        // Nemotron backend rooted at the canonical path.
        let backend = nemotron_transcriber_via_filesystem_provider(bundle, "ja-JP");
        assert_eq!(backend.probe().backend_id.as_str(), NEMOTRON_BACKEND_ID);
    }
}
