//! Backend-neutral capture/transcription contracts and orchestration.
//!
//! Platform adapters own OS handles; this module owns the stable Rust
//! boundary between capture, recording, transcription, and the desktop shell.

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
};
use std::time::{Duration, Instant};

use crossbeam_channel as channel;
use wisp_core::{AudioFrame, CaptureEvent, TrackDescriptor, TrackId, TranscriptEvent};

/// Stable identifier advertised by a backend implementation.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct BackendId(String);

impl BackendId {
    #[must_use]
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for BackendId {
    fn fmt(
        &self,
        formatter: &mut std::fmt::Formatter<'_>,
    ) -> std::fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// Why a backend cannot currently be selected.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum UnavailableReason {
    UnsupportedPlatform,
    PermissionDenied(String),
    MissingModel(String),
    UnsupportedLocale(String),
    InsufficientCompute(String),
    InitializationFailed(String),
}

/// Result of probing a backend without starting a session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Availability {
    Available,
    Unavailable(UnavailableReason),
}

impl Availability {
    #[must_use]
    pub const fn is_available(&self) -> bool {
        matches!(self, Self::Available)
    }
}

/// Capture features guaranteed by a backend when its probe is available.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CaptureCapabilities {
    pub tracks: Vec<TrackDescriptor>,
    pub simultaneous_tracks: bool,
    pub monotonic_timestamps: bool,
    pub device_change_notifications: bool,
}

/// Capture capability probe.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CaptureProbe {
    pub backend_id: BackendId,
    pub availability: Availability,
    pub capabilities: CaptureCapabilities,
}

/// Whether recognition can leave the machine.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RecognitionPrivacy {
    Offline,
    Online,
}

/// Logical class used for user preference and fallback ordering.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TranscriberClass {
    Platform,
    LocalModel,
}

/// Transcription features guaranteed by an available backend.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TranscriberCapabilities {
    pub privacy: RecognitionPrivacy,
    pub features: Vec<TranscriberFeature>,
}

impl TranscriberCapabilities {
    #[must_use]
    pub fn supports(
        &self,
        feature: TranscriberFeature,
    ) -> bool {
        self.features.contains(&feature)
    }
}

/// Independently probeable transcription feature.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum TranscriberFeature {
    Streaming,
    PartialResults,
    SegmentTimestamps,
    WordTimestamps,
    Diarization,
}

/// Transcription capability probe used by selection before session start.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TranscriberProbe {
    pub backend_id: BackendId,
    pub class: TranscriberClass,
    pub availability: Availability,
    pub capabilities: TranscriberCapabilities,
}

/// Privacy guarantee requested for a session.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PrivacyRequirement {
    OfflineRequired,
    OnlineAllowed,
}

/// Explicit fallback behavior for transcription selection.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct TranscriptionPolicy {
    pub privacy: PrivacyRequirement,
    pub preferred: TranscriberClass,
    pub allow_backend_fallback: bool,
    pub allow_record_only: bool,
}

impl TranscriptionPolicy {
    #[must_use]
    pub const fn platform_default() -> Self {
        Self {
            privacy: PrivacyRequirement::OnlineAllowed,
            preferred: TranscriberClass::Platform,
            allow_backend_fallback: false,
            allow_record_only: true,
        }
    }

    #[must_use]
    pub const fn offline_local_model() -> Self {
        Self {
            privacy: PrivacyRequirement::OfflineRequired,
            preferred: TranscriberClass::LocalModel,
            allow_backend_fallback: true,
            allow_record_only: true,
        }
    }
}

/// Outcome of applying [`TranscriptionPolicy`] to capability probes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TranscriptionSelection {
    Backend(BackendId),
    RecordOnly { reason: String },
    Unavailable { reason: String },
}

/// Choose a transcription backend without weakening the privacy requirement.
#[must_use]
pub fn select_transcriber(
    policy: TranscriptionPolicy,
    candidates: &[TranscriberProbe],
) -> TranscriptionSelection {
    if let Some(preferred) = candidates
        .iter()
        .filter(|candidate| is_eligible(policy, candidate))
        .find(|candidate| candidate.class == policy.preferred)
    {
        return TranscriptionSelection::Backend(preferred.backend_id.clone());
    }
    if policy.allow_backend_fallback
        && let Some(fallback) = candidates
            .iter()
            .find(|candidate| is_eligible(policy, candidate))
    {
        return TranscriptionSelection::Backend(fallback.backend_id.clone());
    }

    no_transcriber_selection(policy, candidates, None)
}

/// Reselect after a chosen backend fails during initialization.
///
/// The failed backend is excluded. A remaining backend is considered only
/// when `allow_backend_fallback` is enabled; privacy is never weakened.
#[must_use]
pub fn select_transcriber_after_failure(
    policy: TranscriptionPolicy,
    candidates: &[TranscriberProbe],
    failed_backend: &BackendId,
) -> TranscriptionSelection {
    if policy.allow_backend_fallback
        && let Some(fallback) = candidates.iter().find(|candidate| {
            candidate.backend_id != *failed_backend && is_eligible(policy, candidate)
        })
    {
        return TranscriptionSelection::Backend(fallback.backend_id.clone());
    }

    no_transcriber_selection(policy, candidates, Some(failed_backend))
}

fn is_eligible(
    policy: TranscriptionPolicy,
    candidate: &TranscriberProbe,
) -> bool {
    candidate.availability.is_available()
        && (policy.privacy != PrivacyRequirement::OfflineRequired
            || candidate.capabilities.privacy == RecognitionPrivacy::Offline)
}

fn no_transcriber_selection(
    policy: TranscriptionPolicy,
    candidates: &[TranscriberProbe],
    excluded: Option<&BackendId>,
) -> TranscriptionSelection {
    let reason = if policy.privacy == PrivacyRequirement::OfflineRequired
        && candidates.iter().any(|candidate| {
            excluded != Some(&candidate.backend_id)
                && candidate.availability.is_available()
                && candidate.capabilities.privacy == RecognitionPrivacy::Online
        }) {
        "no available offline transcription backend".to_owned()
    } else {
        "no transcription backend satisfies the session policy".to_owned()
    };
    if policy.allow_record_only {
        TranscriptionSelection::RecordOnly { reason }
    } else {
        TranscriptionSelection::Unavailable { reason }
    }
}

/// Whether a running pipeline should flush or stop immediately.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ShutdownMode {
    Graceful,
    Abort,
}

/// Error category shared across platform adapters.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum BackendErrorKind {
    InvalidState,
    PermissionDenied,
    DeviceUnavailable,
    UnsupportedFormat,
    MissingModel,
    Internal,
}

/// Error returned through a backend-neutral trait boundary.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{backend}: {message}")]
pub struct BackendError {
    pub backend: BackendId,
    pub kind: BackendErrorKind,
    pub message: String,
}

impl BackendError {
    #[must_use]
    pub fn new(
        backend: BackendId,
        kind: BackendErrorKind,
        message: impl Into<String>,
    ) -> Self {
        Self {
            backend,
            kind,
            message: message.into(),
        }
    }
}

pub type BackendResult<T> = std::result::Result<T, BackendError>;

/// Boundary implemented by an OS capture adapter.
pub trait CaptureBackend: Send {
    fn probe(&self) -> CaptureProbe;
    /// Start producing capture events.
    ///
    /// # Errors
    /// Returns an error if permissions, devices, or backend initialization
    /// prevent capture from starting.
    fn start(&mut self) -> BackendResult<()>;
    /// Wait for the next capture event.
    ///
    /// # Errors
    /// Returns an error when the active capture stream fails.
    fn next_event(
        &mut self,
        timeout: Duration,
    ) -> BackendResult<Option<CaptureEvent>>;
    /// Stop capture using the requested shutdown mode.
    ///
    /// Graceful stop must stop producers while preserving buffered events for
    /// subsequent zero-timeout [`Self::next_event`] calls. Abort may discard
    /// buffered events.
    ///
    /// # Errors
    /// Returns an error when the backend cannot release or finalize capture.
    fn stop(
        &mut self,
        mode: ShutdownMode,
    ) -> BackendResult<()>;
}

/// Boundary implemented by an OS recognizer or local model.
pub trait TranscriberBackend: Send {
    fn probe(&self) -> TranscriberProbe;
    /// Start recognition for the supplied tracks.
    ///
    /// # Errors
    /// Returns an error if the model/service cannot be initialized.
    fn start(
        &mut self,
        tracks: &[TrackDescriptor],
    ) -> BackendResult<()>;
    /// Accept one captured frame.
    ///
    /// # Errors
    /// Returns an error if the format is unsupported or recognition fails.
    fn push(
        &mut self,
        frame: &AudioFrame,
    ) -> BackendResult<()>;
    /// Preserve the recognition timeline when capture reports PCM that could
    /// not cross the bounded capture boundary.
    ///
    /// Backends whose recognizer derives timestamps from the submitted sample
    /// count should insert an equivalent interval of silence. Backends with an
    /// external clock may keep the default no-op.
    ///
    /// # Errors
    /// Returns an error when the backend cannot preserve the gap.
    fn push_gap(
        &mut self,
        _track_id: TrackId,
        _dropped_frames: u64,
    ) -> BackendResult<()> {
        Ok(())
    }
    /// Wait for the next transcript update.
    ///
    /// # Errors
    /// Returns an error when recognition fails.
    fn next_event(
        &mut self,
        timeout: Duration,
    ) -> BackendResult<Option<TranscriptEvent>>;
    /// Flush input and finalize pending transcript results.
    ///
    /// # Errors
    /// Returns an error when finalization fails.
    fn finish(&mut self) -> BackendResult<()>;
    /// Cancel recognition and discard buffered results.
    ///
    /// # Errors
    /// Returns an error when the native recognizer could not be disabled.
    fn abort(&mut self) -> BackendResult<()>;
}

/// Provider-neutral construction input for local, platform, or plugin
/// transcribers. `model_artifact` may identify either one model file or a
/// provider-owned bundle directory. Provider-specific runtimes may consume
/// named options without changing capture or session orchestration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TranscriberConfig {
    pub backend_id: BackendId,
    pub locale: String,
    pub model_artifact: Option<PathBuf>,
    pub options: BTreeMap<String, String>,
}

/// Registration boundary for built-in ONNX and plugin providers.
pub trait TranscriberFactory: Send + Sync {
    fn backend_id(&self) -> BackendId;

    /// Construct an unstarted provider.
    ///
    /// # Errors
    /// Returns an error when required provider configuration is absent.
    fn create(
        &self,
        config: &TranscriberConfig,
    ) -> BackendResult<Box<dyn TranscriberBackend>>;
}

impl<T> TranscriberBackend for Box<T>
where
    T: TranscriberBackend + ?Sized,
{
    fn probe(&self) -> TranscriberProbe {
        (**self).probe()
    }

    fn start(
        &mut self,
        tracks: &[TrackDescriptor],
    ) -> BackendResult<()> {
        (**self).start(tracks)
    }

    fn push(
        &mut self,
        frame: &AudioFrame,
    ) -> BackendResult<()> {
        (**self).push(frame)
    }

    fn push_gap(
        &mut self,
        track_id: TrackId,
        dropped_frames: u64,
    ) -> BackendResult<()> {
        (**self).push_gap(track_id, dropped_frames)
    }

    fn next_event(
        &mut self,
        timeout: Duration,
    ) -> BackendResult<Option<TranscriptEvent>> {
        (**self).next_event(timeout)
    }

    fn finish(&mut self) -> BackendResult<()> {
        (**self).finish()
    }

    fn abort(&mut self) -> BackendResult<()> {
        (**self).abort()
    }
}

/// Fan-out adapter used by platform recorders that already own the capture
/// consumer. It keeps recording and transcription on the same ordered PCM
/// stream while publishing backend-neutral transcript events independently.
#[cfg(any(test, target_os = "linux", target_os = "windows"))]
pub(crate) struct RecordingTranscriber {
    backend: Box<dyn TranscriberBackend>,
    events: channel::Sender<TranscriptEvent>,
}

#[cfg(any(test, target_os = "linux", target_os = "windows"))]
impl RecordingTranscriber {
    pub(crate) fn start(
        mut backend: Box<dyn TranscriberBackend>,
        tracks: &[TrackDescriptor],
    ) -> BackendResult<(Self, channel::Receiver<TranscriptEvent>)> {
        backend.start(tracks)?;
        // Transcript finals are not replaceable telemetry. Keep this handoff
        // lossless; capture-side backpressure is handled by each backend's
        // bounded PCM queue instead.
        let (events, receiver) = channel::unbounded();
        Ok((Self { backend, events }, receiver))
    }

    pub(crate) fn push_capture(
        &mut self,
        event: &CaptureEvent,
    ) -> BackendResult<()> {
        match event {
            CaptureEvent::Samples(frame) => self.backend.push(frame)?,
            CaptureEvent::Overflow {
                track_id,
                dropped_frames,
            } => self.backend.push_gap(*track_id, *dropped_frames)?,
            _ => {},
        }
        self.drain()
    }

    pub(crate) fn finish(&mut self) -> BackendResult<()> {
        self.backend.finish()?;
        self.drain()
    }

    fn drain(&mut self) -> BackendResult<()> {
        while let Some(event) = self.backend.next_event(Duration::ZERO)? {
            self.events.send(event).map_err(|_| {
                BackendError::new(
                    BackendId::new("recording-transcriber"),
                    BackendErrorKind::Internal,
                    "transcript event receiver disconnected",
                )
            })?;
        }
        Ok(())
    }
}

/// Unified event surfaced by [`SessionOrchestrator`].
#[derive(Debug, Clone, PartialEq)]
pub enum OrchestratorEvent {
    Capture(CaptureEvent),
    Transcript(TranscriptEvent),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OrchestratorState {
    Ready,
    Running,
    CleanupPending,
    Stopped,
}

#[derive(Debug, Clone, Copy)]
struct CleanupProgress {
    capture: CaptureCleanupProgress,
    transcriber: TranscriberCleanupState,
}

#[derive(Debug, Clone, Copy, Default)]
struct CaptureCleanupProgress {
    stopped: bool,
    drained: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TranscriberCleanupState {
    Active,
    NeedsAbort,
    FinishedNeedsDrain,
    Cleaned,
}

impl CleanupProgress {
    const fn running(has_transcriber: bool) -> Self {
        Self {
            capture: CaptureCleanupProgress {
                stopped: false,
                drained: false,
            },
            transcriber: if has_transcriber {
                TranscriberCleanupState::Active
            } else {
                TranscriberCleanupState::Cleaned
            },
        }
    }

    const fn failed_transcriber_start() -> Self {
        Self {
            capture: CaptureCleanupProgress {
                stopped: false,
                drained: false,
            },
            transcriber: TranscriberCleanupState::NeedsAbort,
        }
    }

    const fn is_complete(self) -> bool {
        self.capture.stopped
            && self.capture.drained
            && matches!(self.transcriber, TranscriberCleanupState::Cleaned)
    }
}

/// Minimal synchronous coordinator shared by desktop worker implementations.
///
/// macOS uses this coordinator through production adapters around its
/// C/Swift bridge; Rust-native backends implement the same traits directly.
pub struct SessionOrchestrator<C, T> {
    capture: C,
    transcriber: Option<T>,
    pending: VecDeque<OrchestratorEvent>,
    pending_error: Option<BackendError>,
    state: OrchestratorState,
    cleanup: Option<CleanupProgress>,
}

impl<C, T> SessionOrchestrator<C, T>
where
    C: CaptureBackend,
    T: TranscriberBackend,
{
    #[must_use]
    pub fn new(
        capture: C,
        transcriber: Option<T>,
    ) -> Self {
        Self {
            capture,
            transcriber,
            pending: VecDeque::new(),
            pending_error: None,
            state: OrchestratorState::Ready,
            cleanup: None,
        }
    }

    /// Start capture and the optional transcription consumer transactionally.
    ///
    /// An orchestrator is one-shot once capture has started. Double-start and
    /// restart after shutdown return [`BackendErrorKind::InvalidState`].
    ///
    /// # Errors
    /// Returns a backend start error. If transcription fails after capture
    /// starts, capture abort is attempted immediately. A failed abort is
    /// included in the returned error and remains retryable through
    /// [`Self::shutdown`] with [`ShutdownMode::Abort`].
    pub fn start(&mut self) -> BackendResult<()> {
        self.start_inner(false).map(|_| ())
    }

    /// Start capture and retain it in record-only mode if the optional
    /// transcriber cannot start and can be cleanly disabled.
    ///
    /// The returned error is the transcriber startup failure that caused the
    /// fallback. `Ok(None)` means capture and transcription both started.
    /// If disabling the partially-started transcriber fails, the entire
    /// transaction is aborted and an aggregated error is returned.
    ///
    /// # Errors
    /// Returns capture startup failures and failures that prevent a safe
    /// record-only fallback.
    pub fn start_allowing_record_only(&mut self) -> BackendResult<Option<BackendError>> {
        self.start_inner(true)
    }

    fn start_inner(
        &mut self,
        retain_capture_after_transcriber_failure: bool,
    ) -> BackendResult<Option<BackendError>> {
        if self.state != OrchestratorState::Ready {
            return Err(self.invalid_state_error("session orchestrator has already been started"));
        }
        self.capture.start()?;
        if let Some(transcriber) = &mut self.transcriber {
            let tracks = self.capture.probe().capabilities.tracks;
            if let Err(start_error) = transcriber.start(&tracks) {
                if retain_capture_after_transcriber_failure {
                    match transcriber.abort() {
                        Ok(()) => {
                            self.transcriber = None;
                            self.state = OrchestratorState::Running;
                            return Ok(Some(start_error));
                        },
                        Err(disable_error) => {
                            self.state = OrchestratorState::CleanupPending;
                            // The fallback admission already attempted the
                            // transcriber abort once. Abort capture now, but do
                            // not silently retry the same failed transcriber
                            // phase in this call: both failures must remain
                            // visible and only unfinished phases are retried by
                            // a later shutdown.
                            let mut progress = CleanupProgress::failed_transcriber_start();
                            let capture_abort = self.capture.stop(ShutdownMode::Abort);
                            if capture_abort.is_ok() {
                                progress.capture.stopped = true;
                                progress.capture.drained = true;
                            }
                            self.cleanup = Some(progress);
                            return match capture_abort {
                                Ok(()) => Err(BackendError::new(
                                    disable_error.backend.clone(),
                                    disable_error.kind,
                                    format!(
                                        "transcriber start failed ({start_error}); record-only fallback failed ({disable_error})"
                                    ),
                                )),
                                Err(capture_error) => Err(BackendError::new(
                                    capture_error.backend,
                                    capture_error.kind,
                                    format!(
                                        "transcriber start failed ({start_error}); record-only fallback failed ({disable_error}); capture abort also failed: {}",
                                        capture_error.message
                                    ),
                                )),
                            };
                        },
                    }
                }
                self.state = OrchestratorState::CleanupPending;
                self.cleanup = Some(CleanupProgress::failed_transcriber_start());
                return match self.continue_cleanup(ShutdownMode::Abort) {
                    Ok(()) => Err(start_error),
                    Err(cleanup_error) => Err(BackendError::new(
                        cleanup_error.backend,
                        cleanup_error.kind,
                        format!(
                            "transcriber start failed ({start_error}); capture abort also failed: {}",
                            cleanup_error.message
                        ),
                    )),
                };
            }
        }
        self.state = OrchestratorState::Running;
        Ok(None)
    }

    /// Pump one queued or newly captured event through the pipeline.
    ///
    /// Audio remains observable to recording consumers while the transcriber
    /// borrows the same frame, avoiding a second PCM allocation.
    ///
    /// # Errors
    /// Returns [`BackendErrorKind::InvalidState`] before start/after shutdown,
    /// or a backend error from capture/transcription.
    pub fn pump_once(
        &mut self,
        timeout: Duration,
    ) -> BackendResult<Option<OrchestratorEvent>> {
        if let Some(event) = self.pending.pop_front() {
            return Ok(Some(event));
        }
        if let Some(error) = self.pending_error.take() {
            return Err(error);
        }
        if self.state != OrchestratorState::Running {
            return Err(self.invalid_state_error("session orchestrator is not running"));
        }
        let Some(capture_event) = self.capture.next_event(timeout)? else {
            return self.poll_transcript(Duration::ZERO);
        };

        let push_result = match (&capture_event, &mut self.transcriber) {
            (CaptureEvent::Samples(frame), Some(transcriber)) => transcriber.push(frame),
            (
                CaptureEvent::Overflow {
                    track_id,
                    dropped_frames,
                },
                Some(transcriber),
            ) => transcriber.push_gap(*track_id, *dropped_frames),
            _ => Ok(()),
        };
        self.pending
            .push_back(OrchestratorEvent::Capture(capture_event));

        if let Err(error) = push_result {
            self.pending_error = Some(error);
        } else {
            match self.poll_transcript(Duration::ZERO) {
                Ok(Some(event)) => self.pending.push_back(event),
                Ok(None) => {},
                Err(error) => self.pending_error = Some(error),
            }
        }
        Ok(self.pending.pop_front())
    }

    /// Stop capture first, drain every buffered capture event through
    /// recording/transcription consumers, then flush transcript finals.
    ///
    /// # Errors
    /// Returns the first shutdown error. Completed cleanup steps are retained,
    /// so a later graceful or abort shutdown retries only unfinished work.
    /// Graceful finalization waits until capture has stopped and drained.
    pub fn shutdown(
        &mut self,
        mode: ShutdownMode,
    ) -> BackendResult<()> {
        match self.state {
            OrchestratorState::Ready | OrchestratorState::Stopped => return Ok(()),
            OrchestratorState::Running => {
                self.state = OrchestratorState::CleanupPending;
                self.cleanup = Some(CleanupProgress::running(self.transcriber.is_some()));
            },
            OrchestratorState::CleanupPending => {},
        }
        self.continue_cleanup(mode)
    }

    /// Stop forwarding capture frames to the current transcriber while
    /// leaving capture and recording running.
    ///
    /// Platform adapters use this after a terminal recognizer failure when
    /// policy explicitly permits a record-only fallback. Buffered transcript
    /// events and a pending transcriber error are discarded.
    ///
    /// # Errors
    /// Returns [`BackendErrorKind::InvalidState`] unless the orchestrator is
    /// currently running.
    pub fn disable_transcriber(&mut self) -> BackendResult<()> {
        if self.state != OrchestratorState::Running {
            return Err(self.invalid_state_error(
                "transcriber can only be disabled while the session is running",
            ));
        }
        if let Some(transcriber) = &mut self.transcriber {
            transcriber.abort()?;
        }
        self.transcriber = None;
        self.pending
            .retain(|event| !matches!(event, OrchestratorEvent::Transcript(_)));
        self.pending_error = None;
        Ok(())
    }

    #[must_use]
    pub fn into_parts(self) -> (C, Option<T>) {
        (self.capture, self.transcriber)
    }

    fn poll_transcript(
        &mut self,
        timeout: Duration,
    ) -> BackendResult<Option<OrchestratorEvent>> {
        let Some(transcriber) = &mut self.transcriber else {
            return Ok(None);
        };
        transcriber
            .next_event(timeout)
            .map(|event| event.map(OrchestratorEvent::Transcript))
    }

    fn queue_capture_event(
        &mut self,
        event: CaptureEvent,
        transcriber_accepts_audio: bool,
    ) -> BackendResult<()> {
        let push_result = match (transcriber_accepts_audio, &event, &mut self.transcriber) {
            (true, CaptureEvent::Samples(frame), Some(transcriber)) => transcriber.push(frame),
            (
                true,
                CaptureEvent::Overflow {
                    track_id,
                    dropped_frames,
                },
                Some(transcriber),
            ) => transcriber.push_gap(*track_id, *dropped_frames),
            _ => Ok(()),
        };
        self.pending.push_back(OrchestratorEvent::Capture(event));
        push_result
    }

    fn invalid_state_error(
        &self,
        message: &'static str,
    ) -> BackendError {
        BackendError::new(
            self.capture.probe().backend_id,
            BackendErrorKind::InvalidState,
            message,
        )
    }

    fn continue_cleanup(
        &mut self,
        mode: ShutdownMode,
    ) -> BackendResult<()> {
        let mut progress = self
            .cleanup
            .unwrap_or_else(|| CleanupProgress::running(self.transcriber.is_some()));
        let mut first_error = None;

        if !progress.capture.stopped {
            match self.capture.stop(mode) {
                Ok(()) => progress.capture.stopped = true,
                Err(error) => first_error = Some(error),
            }
        }

        match mode {
            ShutdownMode::Graceful => {
                if progress.capture.stopped && !progress.capture.drained {
                    loop {
                        match self.capture.next_event(Duration::ZERO) {
                            Ok(Some(event)) => {
                                let transcriber_accepts_audio =
                                    progress.transcriber == TranscriberCleanupState::Active;
                                if let Err(error) =
                                    self.queue_capture_event(event, transcriber_accepts_audio)
                                    && first_error.is_none()
                                {
                                    first_error = Some(error);
                                }
                            },
                            Ok(None) => {
                                progress.capture.drained = true;
                                break;
                            },
                            Err(error) => {
                                if first_error.is_none() {
                                    first_error = Some(error);
                                }
                                break;
                            },
                        }
                    }
                }

                if progress.capture.drained
                    && progress.transcriber == TranscriberCleanupState::Active
                    && let Some(transcriber) = &mut self.transcriber
                {
                    match transcriber.finish() {
                        Ok(()) => {
                            progress.transcriber = TranscriberCleanupState::FinishedNeedsDrain;
                        },
                        Err(error) if first_error.is_none() => first_error = Some(error),
                        Err(_) => {},
                    }
                }

                if progress.transcriber == TranscriberCleanupState::FinishedNeedsDrain
                    && let Some(transcriber) = &mut self.transcriber
                {
                    loop {
                        match transcriber.next_event(Duration::ZERO) {
                            Ok(Some(event)) => {
                                self.pending.push_back(OrchestratorEvent::Transcript(event));
                            },
                            Ok(None) => {
                                progress.transcriber = TranscriberCleanupState::Cleaned;
                                break;
                            },
                            Err(error) => {
                                if first_error.is_none() {
                                    first_error = Some(error);
                                }
                                break;
                            },
                        }
                    }
                }
            },
            ShutdownMode::Abort => {
                if progress.capture.stopped {
                    progress.capture.drained = true;
                }
                if progress.transcriber != TranscriberCleanupState::Cleaned {
                    if let Some(transcriber) = &mut self.transcriber {
                        match transcriber.abort() {
                            Ok(()) => {
                                progress.transcriber = TranscriberCleanupState::Cleaned;
                            },
                            Err(error) if first_error.is_none() => first_error = Some(error),
                            Err(_) => {},
                        }
                    } else {
                        progress.transcriber = TranscriberCleanupState::Cleaned;
                    }
                }
            },
        }

        self.cleanup = Some(progress);
        if progress.is_complete() {
            self.state = OrchestratorState::Stopped;
        }

        match first_error {
            Some(error) => Err(error),
            None if progress.is_complete() => Ok(()),
            None => Err(self.invalid_state_error("session cleanup remains incomplete")),
        }
    }
}

/// Result of a non-blocking real-time frame enqueue.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameEnqueue {
    Enqueued,
    Dropped,
}

/// Non-audio notifications accepted by the control side of a capture queue.
///
/// Samples intentionally have no representation here, so PCM can only enter
/// through the bounded real-time frame queue.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum CaptureControlEvent {
    Error {
        track_id: Option<TrackId>,
        message: String,
        recoverable: bool,
    },
}

impl From<CaptureControlEvent> for CaptureEvent {
    fn from(event: CaptureControlEvent) -> Self {
        match event {
            CaptureControlEvent::Error {
                track_id,
                message,
                recoverable,
            } => Self::Error {
                track_id,
                message,
                recoverable,
            },
        }
    }
}

/// Result of a non-blocking control-event enqueue.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ControlEnqueue {
    Enqueued,
    Dropped,
}

#[cfg(any(test, target_os = "windows"))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WorkerFailureRoute {
    Startup,
    Runtime,
}

#[cfg(any(test, target_os = "windows"))]
pub(crate) struct StartupCoordinator {
    expected_workers: usize,
    ready_workers: AtomicUsize,
}

#[cfg(any(test, target_os = "windows"))]
impl StartupCoordinator {
    pub(crate) const fn new(expected_workers: usize) -> Self {
        Self {
            expected_workers,
            ready_workers: AtomicUsize::new(0),
        }
    }

    pub(crate) fn observe_ready(&self) {
        self.ready_workers.fetch_add(1, Ordering::SeqCst);
    }

    pub(crate) fn is_complete(&self) -> bool {
        self.ready_workers.load(Ordering::SeqCst) >= self.expected_workers
    }
}

#[cfg(any(test, target_os = "windows"))]
#[derive(Default)]
pub(crate) struct WorkerStartupPhase {
    ready_published: AtomicBool,
}

#[cfg(any(test, target_os = "windows"))]
impl WorkerStartupPhase {
    pub(crate) fn mark_ready_published(&self) {
        self.ready_published.store(true, Ordering::SeqCst);
    }

    pub(crate) fn failure_route(&self) -> WorkerFailureRoute {
        if self.ready_published.load(Ordering::SeqCst) {
            WorkerFailureRoute::Runtime
        } else {
            WorkerFailureRoute::Startup
        }
    }
}

#[cfg(any(test, target_os = "windows"))]
pub(crate) fn publish_ready_and_wait<T>(
    sender: &channel::Sender<T>,
    ready: T,
    coordinator: &StartupCoordinator,
    phase: &WorkerStartupPhase,
    stop_requested: &AtomicBool,
) -> Result<(), channel::SendError<T>> {
    sender.send(ready)?;
    phase.mark_ready_published();
    while !coordinator.is_complete() && !stop_requested.load(Ordering::SeqCst) {
        std::thread::sleep(Duration::from_millis(1));
    }
    Ok(())
}

struct DropCounter {
    track_id: TrackId,
    frames: AtomicU64,
}

struct ProducerGate {
    closed: AtomicBool,
    frozen: AtomicBool,
    active: AtomicUsize,
    quiesced_sender: channel::Sender<()>,
    quiesced_receiver: channel::Receiver<()>,
    #[cfg(test)]
    enter_hook: std::sync::Mutex<Option<Arc<ProducerEnterHook>>>,
}

impl Default for ProducerGate {
    fn default() -> Self {
        let (quiesced_sender, quiesced_receiver) = channel::bounded(1);
        Self {
            closed: AtomicBool::new(false),
            frozen: AtomicBool::new(false),
            active: AtomicUsize::new(0),
            quiesced_sender,
            quiesced_receiver,
            #[cfg(test)]
            enter_hook: std::sync::Mutex::new(None),
        }
    }
}

impl ProducerGate {
    fn try_enter(&self) -> ProducerEntry<'_> {
        if self.closed.load(Ordering::Acquire) {
            return ProducerEntry::ClosedBeforeIncrement;
        }
        if self.frozen.load(Ordering::Acquire) {
            return ProducerEntry::RejectedBeforeIncrement;
        }
        #[cfg(test)]
        let hook = self
            .enter_hook
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        #[cfg(test)]
        if let Some(hook) = &hook {
            hook.after_first_check.wait();
            hook.allow_increment.wait();
        }
        self.active.fetch_add(1, Ordering::AcqRel);
        #[cfg(test)]
        if let Some(hook) = &hook {
            hook.after_increment.wait();
            hook.allow_second_check.wait();
        }
        if self.closed.load(Ordering::Acquire) {
            return ProducerEntry::ClosedAfterIncrement(ActiveProducer(self));
        }
        if self.frozen.load(Ordering::Acquire) {
            return ProducerEntry::RejectedAfterIncrement(ActiveProducer(self));
        }
        ProducerEntry::Entered(ActiveProducer(self))
    }

    fn try_freeze(&self) -> Option<FrozenProducerGate<'_>> {
        if self.closed.load(Ordering::Acquire) {
            return None;
        }
        self.frozen.store(true, Ordering::Release);
        if self.closed.load(Ordering::Acquire) {
            self.frozen.store(false, Ordering::Release);
            return None;
        }
        if self.active.load(Ordering::Acquire) == 0 {
            Some(FrozenProducerGate(self))
        } else {
            None
        }
    }

    fn leave_active(&self) {
        if self.active.fetch_sub(1, Ordering::AcqRel) == 1 {
            let _ = self.quiesced_sender.try_send(());
        }
    }

    fn close(&self) {
        self.closed.store(true, Ordering::Release);
        self.frozen.store(false, Ordering::Release);
        let _ = self.quiesced_sender.try_send(());
    }

    fn is_closed(&self) -> bool {
        self.closed.load(Ordering::Acquire)
    }
}

enum ProducerEntry<'a> {
    Entered(ActiveProducer<'a>),
    RejectedBeforeIncrement,
    RejectedAfterIncrement(ActiveProducer<'a>),
    ClosedBeforeIncrement,
    ClosedAfterIncrement(ActiveProducer<'a>),
}

struct ActiveProducer<'a>(&'a ProducerGate);

impl Drop for ActiveProducer<'_> {
    fn drop(&mut self) {
        self.0.leave_active();
    }
}

struct FrozenProducerGate<'a>(&'a ProducerGate);

impl Drop for FrozenProducerGate<'_> {
    fn drop(&mut self) {
        self.0.frozen.store(false, Ordering::Release);
    }
}

/// Producer handle tied to exactly one capture track.
///
/// `try_send` never waits. Control/error events use a separate channel and
/// must only be sent outside the real-time sample callback.
#[derive(Clone)]
pub struct RealtimeCaptureSender {
    track_id: TrackId,
    frame_sender: channel::Sender<AudioFrame>,
    control_sender: channel::Sender<CaptureControlEvent>,
    dropped: Arc<DropCounter>,
    gate: Arc<ProducerGate>,
    #[cfg(test)]
    critical_section_hook: Option<Arc<ProducerCriticalSectionHook>>,
}

#[cfg(test)]
struct ProducerCriticalSectionHook {
    entered: std::sync::Barrier,
    release: std::sync::Barrier,
}

#[cfg(test)]
struct ProducerEnterHook {
    after_first_check: std::sync::Barrier,
    allow_increment: std::sync::Barrier,
    after_increment: std::sync::Barrier,
    allow_second_check: std::sync::Barrier,
}

impl RealtimeCaptureSender {
    /// Account for frames dropped before an [`AudioFrame`] could be
    /// constructed (for example, platform callback-pool exhaustion).
    ///
    /// This is non-blocking and allocation-free, so platform real-time
    /// callbacks can preserve overflow accounting without fabricating PCM.
    ///
    /// # Errors
    /// Returns [`BackendErrorKind::InvalidState`] after the receiver closes.
    pub fn report_dropped_frames(
        &self,
        frame_count: u64,
    ) -> BackendResult<()> {
        if self.gate.is_closed() {
            return Err(capture_receiver_disconnected_error());
        }
        self.dropped
            .frames
            .fetch_add(frame_count, Ordering::Relaxed);
        Ok(())
    }

    /// Enqueue an audio frame without waiting for capacity.
    ///
    /// # Errors
    /// Returns [`BackendErrorKind::Internal`] for the wrong track and
    /// [`BackendErrorKind::InvalidState`] after the receiver disconnects.
    pub fn try_send(
        &self,
        frame: AudioFrame,
    ) -> BackendResult<FrameEnqueue> {
        if frame.track_id() != self.track_id {
            return Err(BackendError::new(
                BackendId::new("capture-queue"),
                BackendErrorKind::Internal,
                "frame track does not match its real-time sender",
            ));
        }
        let frame_count = u64::from(frame.frame_count());
        let _active = match self.gate.try_enter() {
            ProducerEntry::Entered(active) => active,
            ProducerEntry::RejectedBeforeIncrement => {
                if self.gate.is_closed() {
                    return Err(capture_receiver_disconnected_error());
                }
                self.dropped
                    .frames
                    .fetch_add(frame_count, Ordering::Relaxed);
                return Ok(FrameEnqueue::Dropped);
            },
            ProducerEntry::RejectedAfterIncrement(active) => {
                self.dropped
                    .frames
                    .fetch_add(frame_count, Ordering::Relaxed);
                drop(active);
                return Ok(FrameEnqueue::Dropped);
            },
            ProducerEntry::ClosedBeforeIncrement => {
                return Err(capture_receiver_disconnected_error());
            },
            ProducerEntry::ClosedAfterIncrement(active) => {
                drop(active);
                return Err(capture_receiver_disconnected_error());
            },
        };
        #[cfg(test)]
        if let Some(hook) = &self.critical_section_hook {
            hook.entered.wait();
            hook.release.wait();
        }
        if self.gate.is_closed() {
            return Err(capture_receiver_disconnected_error());
        }
        if self.dropped.frames.load(Ordering::Relaxed) > 0 {
            self.dropped
                .frames
                .fetch_add(frame_count, Ordering::Relaxed);
            return Ok(FrameEnqueue::Dropped);
        }
        match self.frame_sender.try_send(frame) {
            Ok(()) => Ok(FrameEnqueue::Enqueued),
            Err(channel::TrySendError::Full(_)) => {
                self.dropped
                    .frames
                    .fetch_add(frame_count, Ordering::Relaxed);
                Ok(FrameEnqueue::Dropped)
            },
            Err(channel::TrySendError::Disconnected(_)) => {
                Err(capture_receiver_disconnected_error())
            },
        }
    }

    /// Publish a control event without waiting for queue capacity.
    ///
    /// The control queue is bounded like the frame queue. Callers can log or
    /// otherwise account for [`ControlEnqueue::Dropped`].
    ///
    /// # Errors
    /// Returns [`BackendErrorKind::InvalidState`] after the receiver
    /// disconnects.
    pub fn send_control(
        &self,
        event: CaptureControlEvent,
    ) -> BackendResult<ControlEnqueue> {
        match self.control_sender.try_send(event) {
            Ok(()) => Ok(ControlEnqueue::Enqueued),
            Err(channel::TrySendError::Full(_)) => Ok(ControlEnqueue::Dropped),
            Err(channel::TrySendError::Disconnected(_)) => Err(BackendError::new(
                BackendId::new("capture-queue"),
                BackendErrorKind::InvalidState,
                "capture receiver is disconnected",
            )),
        }
    }
}

fn capture_receiver_disconnected_error() -> BackendError {
    BackendError::new(
        BackendId::new("capture-queue"),
        BackendErrorKind::InvalidState,
        "capture receiver is disconnected",
    )
}

#[derive(Default)]
struct CaptureDeliveryState {
    frames_before_boundary: usize,
    boundary_events: VecDeque<CaptureEvent>,
    pending_controls: VecDeque<CaptureControlEvent>,
    boundary_deferred: bool,
}

/// Consumer side of the bounded real-time capture channel.
///
/// Overflow and control notifications establish a bounded delivery boundary:
/// PCM already accepted into the frame queue is drained first, followed by all
/// overflow reports captured at that boundary and then the control event.
/// Frames accepted later cannot extend the snapshot, so a continuously active
/// track cannot starve overflow or fatal delivery.
///
/// This is intentionally a single-consumer handle:
///
/// ```compile_fail
/// fn require_clone<T: Clone>() {}
/// require_clone::<wisp_audiokit::CaptureEventReceiver>();
/// ```
///
/// ```compile_fail
/// fn require_sync<T: Sync>() {}
/// require_sync::<wisp_audiokit::CaptureEventReceiver>();
/// ```
pub struct CaptureEventReceiver {
    frame_receiver: channel::Receiver<AudioFrame>,
    control_receiver: channel::Receiver<CaptureControlEvent>,
    dropped: Arc<Vec<Arc<DropCounter>>>,
    gate: Arc<ProducerGate>,
    delivery: RefCell<CaptureDeliveryState>,
}

impl CaptureEventReceiver {
    #[must_use]
    pub fn try_recv(&self) -> Option<CaptureEvent> {
        let mut delivery = self.delivery.borrow_mut();
        self.try_recv_locked(&mut delivery)
    }

    #[must_use]
    pub fn recv_timeout(
        &self,
        timeout: Duration,
    ) -> Option<CaptureEvent> {
        let started = Instant::now();
        let mut frame_open = true;
        let mut control_open = true;
        loop {
            if let Some(event) = self.try_recv() {
                return Some(event);
            }
            if self.boundary_deferred() {
                let remaining = timeout.saturating_sub(started.elapsed());
                if remaining.is_zero() {
                    return self.try_recv();
                }
                match self.gate.quiesced_receiver.recv_timeout(remaining) {
                    Ok(()) => continue,
                    Err(channel::RecvTimeoutError::Disconnected) => {},
                    Err(channel::RecvTimeoutError::Timeout) => return self.try_recv(),
                }
            }
            if !frame_open && !control_open {
                return None;
            }

            let remaining = timeout.saturating_sub(started.elapsed());
            if remaining.is_zero() {
                return self.try_recv();
            }
            match (frame_open, control_open) {
                (true, true) => {
                    channel::select! {
                        recv(self.frame_receiver) -> frame => match frame {
                            Ok(frame) => return Some(CaptureEvent::Samples(frame)),
                            Err(_) => frame_open = false,
                        },
                        recv(self.control_receiver) -> event => match event {
                            Ok(event) => self.stage_control(event),
                            Err(_) => control_open = false,
                        },
                        default(remaining) => return self.try_recv(),
                    }
                },
                (true, false) => match self.frame_receiver.recv_timeout(remaining) {
                    Ok(frame) => return Some(CaptureEvent::Samples(frame)),
                    Err(channel::RecvTimeoutError::Disconnected) => frame_open = false,
                    Err(channel::RecvTimeoutError::Timeout) => return self.try_recv(),
                },
                (false, true) => match self.control_receiver.recv_timeout(remaining) {
                    Ok(event) => self.stage_control(event),
                    Err(channel::RecvTimeoutError::Disconnected) => control_open = false,
                    Err(channel::RecvTimeoutError::Timeout) => return self.try_recv(),
                },
                (false, false) => return self.try_recv(),
            }
        }
    }

    #[must_use]
    pub fn recv(&self) -> Option<CaptureEvent> {
        let mut frame_open = true;
        let mut control_open = true;
        loop {
            if let Some(event) = self.try_recv() {
                return Some(event);
            }
            if self.boundary_deferred() {
                let _ = self.gate.quiesced_receiver.recv();
                continue;
            }
            match (frame_open, control_open) {
                (true, true) => {
                    channel::select! {
                        recv(self.frame_receiver) -> frame => match frame {
                            Ok(frame) => return Some(CaptureEvent::Samples(frame)),
                            Err(_) => frame_open = false,
                        },
                        recv(self.control_receiver) -> event => match event {
                            Ok(event) => self.stage_control(event),
                            Err(_) => control_open = false,
                        },
                    }
                },
                (true, false) => match self.frame_receiver.recv() {
                    Ok(frame) => return Some(CaptureEvent::Samples(frame)),
                    Err(_) => frame_open = false,
                },
                (false, true) => match self.control_receiver.recv() {
                    Ok(event) => self.stage_control(event),
                    Err(_) => control_open = false,
                },
                (false, false) => return self.try_recv(),
            }
        }
    }

    fn try_recv_locked(
        &self,
        delivery: &mut CaptureDeliveryState,
    ) -> Option<CaptureEvent> {
        if delivery.boundary_events.is_empty()
            && self.has_pending_boundary(delivery)
            && !self.stage_boundary_locked(delivery)
        {
            return None;
        }
        if !delivery.boundary_events.is_empty() {
            if delivery.frames_before_boundary > 0 {
                match self.frame_receiver.try_recv() {
                    Ok(frame) => {
                        delivery.frames_before_boundary -= 1;
                        return Some(CaptureEvent::Samples(frame));
                    },
                    Err(channel::TryRecvError::Empty | channel::TryRecvError::Disconnected) => {
                        delivery.frames_before_boundary = 0;
                    },
                }
            }
            return delivery.boundary_events.pop_front();
        }
        self.frame_receiver
            .try_recv()
            .ok()
            .map(CaptureEvent::Samples)
    }

    fn stage_control(
        &self,
        control: CaptureControlEvent,
    ) {
        let mut delivery = self.delivery.borrow_mut();
        delivery.pending_controls.push_back(control);
        self.stage_boundary_locked(&mut delivery);
    }

    fn stage_boundary_locked(
        &self,
        delivery: &mut CaptureDeliveryState,
    ) -> bool {
        let Some(_frozen) = self.gate.try_freeze() else {
            delivery.boundary_deferred = true;
            return false;
        };
        delivery.boundary_deferred = false;
        let boundary_was_empty = delivery.boundary_events.is_empty();
        let frames_before_boundary = self.frame_receiver.len();
        let control = delivery
            .pending_controls
            .pop_front()
            .or_else(|| self.control_receiver.try_recv().ok());
        let overflows = self.take_all_overflows();
        delivery.boundary_events.extend(overflows);
        delivery
            .boundary_events
            .extend(control.map(CaptureEvent::from));
        if boundary_was_empty && !delivery.boundary_events.is_empty() {
            delivery.frames_before_boundary = frames_before_boundary;
        }
        true
    }

    fn take_all_overflows(&self) -> Vec<CaptureEvent> {
        self.dropped
            .iter()
            .filter_map(|counter| {
                let dropped_frames = counter.frames.swap(0, Ordering::Relaxed);
                (dropped_frames > 0).then_some(CaptureEvent::Overflow {
                    track_id: counter.track_id,
                    dropped_frames,
                })
            })
            .collect()
    }

    fn has_pending_boundary(
        &self,
        delivery: &CaptureDeliveryState,
    ) -> bool {
        !delivery.pending_controls.is_empty()
            || !self.control_receiver.is_empty()
            || self
                .dropped
                .iter()
                .any(|counter| counter.frames.load(Ordering::Relaxed) > 0)
    }

    fn boundary_deferred(&self) -> bool {
        self.delivery.borrow().boundary_deferred
    }
}

impl Drop for CaptureEventReceiver {
    fn drop(&mut self) {
        // Close the protocol before the channel fields are dropped. A producer
        // racing a deferred boundary must observe disconnection rather than
        // remaining behind the sticky freeze and reporting a normal overflow.
        self.gate.close();
    }
}

/// Construct one non-blocking producer per track and a merged consumer.
///
/// # Panics
/// Panics when `capacity` is zero.
#[must_use]
pub fn realtime_capture_channel(
    capacity: usize,
    tracks: &[TrackId],
) -> (Vec<RealtimeCaptureSender>, CaptureEventReceiver) {
    assert!(capacity > 0, "capture queue capacity must be non-zero");
    let (frame_sender, frame_receiver) = channel::bounded(capacity);
    let (control_sender, control_receiver) = channel::bounded(capacity);
    let gate = Arc::new(ProducerGate::default());
    let counters = tracks
        .iter()
        .map(|track_id| {
            Arc::new(DropCounter {
                track_id: *track_id,
                frames: AtomicU64::new(0),
            })
        })
        .collect::<Vec<_>>();
    let senders = counters
        .iter()
        .map(|counter| RealtimeCaptureSender {
            track_id: counter.track_id,
            frame_sender: frame_sender.clone(),
            control_sender: control_sender.clone(),
            dropped: Arc::clone(counter),
            gate: Arc::clone(&gate),
            #[cfg(test)]
            critical_section_hook: None,
        })
        .collect();
    (
        senders,
        CaptureEventReceiver {
            frame_receiver,
            control_receiver,
            dropped: Arc::new(counters),
            gate,
            delivery: RefCell::new(CaptureDeliveryState::default()),
        },
    )
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::collections::VecDeque;
    use std::sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    };
    use std::time::{Duration, Instant};

    use crossbeam_channel as channel;
    use wisp_core::{
        AudioFrame, CaptureEvent, MonotonicTimestamp, SourceKind, TrackId, TranscriptEvent,
        TranscriptSegment, TranscriptSegmentId,
    };

    use super::{
        Availability, BackendError, BackendErrorKind, BackendId, BackendResult, CaptureBackend,
        CaptureCapabilities, CaptureControlEvent, CaptureEventReceiver, CaptureProbe,
        ControlEnqueue, FrameEnqueue, OrchestratorEvent, PrivacyRequirement, RecognitionPrivacy,
        RecordingTranscriber, SessionOrchestrator, ShutdownMode, StartupCoordinator,
        TranscriberBackend, TranscriberCapabilities, TranscriberClass, TranscriberFeature,
        TranscriberProbe, TranscriptionPolicy, TranscriptionSelection, WorkerFailureRoute,
        WorkerStartupPhase, publish_ready_and_wait, realtime_capture_channel, select_transcriber,
        select_transcriber_after_failure,
    };

    fn transcriber_probe(
        name: &str,
        class: TranscriberClass,
        privacy: RecognitionPrivacy,
        available: bool,
    ) -> TranscriberProbe {
        TranscriberProbe {
            backend_id: BackendId::new(name),
            class,
            availability: if available {
                Availability::Available
            } else {
                Availability::Unavailable(super::UnavailableReason::UnsupportedPlatform)
            },
            capabilities: TranscriberCapabilities {
                privacy,
                features: vec![
                    TranscriberFeature::Streaming,
                    TranscriberFeature::PartialResults,
                    TranscriberFeature::SegmentTimestamps,
                ],
            },
        }
    }

    #[test]
    fn selects_available_preferred_backend() {
        let candidates = [
            transcriber_probe(
                "platform",
                TranscriberClass::Platform,
                RecognitionPrivacy::Online,
                true,
            ),
            transcriber_probe(
                "local",
                TranscriberClass::LocalModel,
                RecognitionPrivacy::Offline,
                true,
            ),
        ];

        assert_eq!(
            select_transcriber(TranscriptionPolicy::platform_default(), &candidates),
            TranscriptionSelection::Backend(BackendId::new("platform"))
        );
    }

    #[test]
    fn offline_requirement_falls_back_only_to_offline_backend() {
        let candidates = [
            transcriber_probe(
                "platform-online",
                TranscriberClass::Platform,
                RecognitionPrivacy::Online,
                true,
            ),
            transcriber_probe(
                "local-offline",
                TranscriberClass::LocalModel,
                RecognitionPrivacy::Offline,
                true,
            ),
        ];
        let policy = TranscriptionPolicy {
            preferred: TranscriberClass::Platform,
            ..TranscriptionPolicy::offline_local_model()
        };

        assert_eq!(
            select_transcriber(policy, &candidates),
            TranscriptionSelection::Backend(BackendId::new("local-offline"))
        );
    }

    #[test]
    fn offline_requirement_never_promotes_to_online_and_can_record_only() {
        let candidates = [transcriber_probe(
            "platform-online",
            TranscriberClass::Platform,
            RecognitionPrivacy::Online,
            true,
        )];

        assert!(matches!(
            select_transcriber(
                TranscriptionPolicy {
                    privacy: PrivacyRequirement::OfflineRequired,
                    preferred: TranscriberClass::LocalModel,
                    allow_backend_fallback: true,
                    allow_record_only: true,
                },
                &candidates,
            ),
            TranscriptionSelection::RecordOnly { .. }
        ));
    }

    #[test]
    fn unavailable_is_explicit_when_record_only_is_forbidden() {
        assert!(matches!(
            select_transcriber(
                TranscriptionPolicy {
                    allow_record_only: false,
                    ..TranscriptionPolicy::offline_local_model()
                },
                &[],
            ),
            TranscriptionSelection::Unavailable { .. }
        ));
    }

    #[test]
    fn runtime_initialization_failure_honors_backend_fallback_policy() {
        let candidates = [
            transcriber_probe(
                "platform",
                TranscriberClass::Platform,
                RecognitionPrivacy::Online,
                true,
            ),
            transcriber_probe(
                "local",
                TranscriberClass::LocalModel,
                RecognitionPrivacy::Offline,
                true,
            ),
        ];
        let failed = BackendId::new("platform");

        assert_eq!(
            select_transcriber_after_failure(
                TranscriptionPolicy {
                    allow_backend_fallback: true,
                    ..TranscriptionPolicy::platform_default()
                },
                &candidates,
                &failed,
            ),
            TranscriptionSelection::Backend(BackendId::new("local"))
        );
        assert!(matches!(
            select_transcriber_after_failure(
                TranscriptionPolicy::platform_default(),
                &candidates,
                &failed,
            ),
            TranscriptionSelection::RecordOnly { .. }
        ));
    }

    #[test]
    fn runtime_fallback_never_weakens_offline_requirement() {
        let candidates = [
            transcriber_probe(
                "local",
                TranscriberClass::LocalModel,
                RecognitionPrivacy::Offline,
                true,
            ),
            transcriber_probe(
                "platform-online",
                TranscriberClass::Platform,
                RecognitionPrivacy::Online,
                true,
            ),
        ];

        assert!(matches!(
            select_transcriber_after_failure(
                TranscriptionPolicy::offline_local_model(),
                &candidates,
                &BackendId::new("local"),
            ),
            TranscriptionSelection::RecordOnly { .. }
        ));
    }

    #[test]
    fn bounded_capture_queue_reports_drops_without_blocking() {
        let (senders, receiver) = realtime_capture_channel(1, &[TrackId::MICROPHONE]);
        let sender = &senders[0];
        let make_frame = |sequence| {
            AudioFrame::from_f32(
                TrackId::MICROPHONE,
                SourceKind::Microphone,
                sequence,
                MonotonicTimestamp::default(),
                16_000,
                1,
                vec![0.0; 160],
            )
            .unwrap()
        };

        assert_eq!(
            sender.try_send(make_frame(0)).unwrap(),
            FrameEnqueue::Enqueued
        );
        assert_eq!(
            sender.try_send(make_frame(1)).unwrap(),
            FrameEnqueue::Dropped
        );
        assert_eq!(
            sender.try_send(make_frame(2)).unwrap(),
            FrameEnqueue::Dropped
        );
        sender.report_dropped_frames(80).unwrap();
        assert!(matches!(
            receiver.try_recv(),
            Some(CaptureEvent::Samples(frame)) if frame.sequence() == 0
        ));
        assert!(matches!(
            receiver.try_recv(),
            Some(CaptureEvent::Overflow {
                track_id: TrackId::MICROPHONE,
                dropped_frames: 400,
            })
        ));
    }

    #[test]
    fn observed_ready_routes_later_worker_failure_only_to_runtime() {
        let coordinator = StartupCoordinator::new(2);
        let microphone = WorkerStartupPhase::default();
        let system = WorkerStartupPhase::default();
        assert_eq!(microphone.failure_route(), WorkerFailureRoute::Startup);

        microphone.mark_ready_published();
        coordinator.observe_ready();
        assert!(!coordinator.is_complete());
        assert_eq!(microphone.failure_route(), WorkerFailureRoute::Runtime);

        system.mark_ready_published();
        coordinator.observe_ready();
        assert!(coordinator.is_complete());
        assert_eq!(system.failure_route(), WorkerFailureRoute::Runtime);
    }

    #[test]
    fn production_ready_helper_publishes_then_waits_for_observation() {
        let coordinator = Arc::new(StartupCoordinator::new(1));
        let phase = Arc::new(WorkerStartupPhase::default());
        let stop_requested = Arc::new(AtomicBool::new(false));
        let (sender, receiver) = channel::bounded(1);
        let worker_coordinator = Arc::clone(&coordinator);
        let worker_phase = Arc::clone(&phase);
        let worker_stop = Arc::clone(&stop_requested);
        let worker = std::thread::spawn(move || {
            publish_ready_and_wait(
                &sender,
                "ready",
                &worker_coordinator,
                &worker_phase,
                &worker_stop,
            )
        });

        assert_eq!(receiver.recv().unwrap(), "ready");
        let deadline = Instant::now() + Duration::from_secs(1);
        while phase.failure_route() == WorkerFailureRoute::Startup {
            assert!(Instant::now() < deadline, "worker did not publish Ready");
            std::thread::yield_now();
        }
        assert_eq!(phase.failure_route(), WorkerFailureRoute::Runtime);
        assert!(!worker.is_finished());
        coordinator.observe_ready();
        worker.join().unwrap().unwrap();
        assert!(coordinator.is_complete());
    }

    #[test]
    fn queued_control_event_cannot_be_starved_by_replenished_frames() {
        let (senders, receiver) = realtime_capture_channel(1, &[TrackId::MICROPHONE]);
        let sender = &senders[0];
        let make_frame = |sequence| {
            AudioFrame::from_f32(
                TrackId::MICROPHONE,
                SourceKind::Microphone,
                sequence,
                MonotonicTimestamp::default(),
                16_000,
                1,
                vec![0.0; 160],
            )
            .unwrap()
        };
        sender.try_send(make_frame(0)).unwrap();
        sender
            .send_control(CaptureControlEvent::Error {
                track_id: Some(TrackId::SYSTEM),
                message: "system stream failed".into(),
                recoverable: false,
            })
            .unwrap();

        let mut delivered_control = false;
        for sequence in 1..=8 {
            match receiver.try_recv() {
                Some(CaptureEvent::Error { message, .. }) => {
                    assert_eq!(message, "system stream failed");
                    delivered_control = true;
                    break;
                },
                Some(CaptureEvent::Samples(_)) => {
                    sender.try_send(make_frame(sequence)).unwrap();
                },
                _ => {},
            }
        }
        assert!(
            delivered_control,
            "control event was starved by audio frames"
        );
    }

    #[test]
    fn fatal_boundary_drains_prequeued_pcm_and_overflow_before_control() {
        let (senders, receiver) =
            realtime_capture_channel(2, &[TrackId::MICROPHONE, TrackId::SYSTEM]);
        let make_frame = |track_id, sequence| {
            AudioFrame::from_f32(
                track_id,
                if track_id == TrackId::MICROPHONE {
                    SourceKind::Microphone
                } else {
                    SourceKind::SystemAudio
                },
                sequence,
                MonotonicTimestamp::default(),
                16_000,
                1,
                vec![0.0; 160],
            )
            .unwrap()
        };
        senders[0]
            .try_send(make_frame(TrackId::MICROPHONE, 0))
            .unwrap();
        senders[1].try_send(make_frame(TrackId::SYSTEM, 0)).unwrap();
        assert_eq!(
            senders[0]
                .try_send(make_frame(TrackId::MICROPHONE, 1))
                .unwrap(),
            FrameEnqueue::Dropped
        );
        senders[1]
            .send_control(CaptureControlEvent::Error {
                track_id: Some(TrackId::SYSTEM),
                message: "fatal".into(),
                recoverable: false,
            })
            .unwrap();

        assert!(matches!(
            receiver.try_recv(),
            Some(CaptureEvent::Samples(frame)) if frame.track_id() == TrackId::MICROPHONE
        ));
        // This frame was accepted after the fatal boundary snapshot and must
        // not extend it.
        senders[1].try_send(make_frame(TrackId::SYSTEM, 1)).unwrap();
        assert!(matches!(
            receiver.try_recv(),
            Some(CaptureEvent::Samples(frame))
                if frame.track_id() == TrackId::SYSTEM && frame.sequence() == 0
        ));
        assert!(matches!(
            receiver.try_recv(),
            Some(CaptureEvent::Overflow {
                track_id: TrackId::MICROPHONE,
                dropped_frames: 160,
            })
        ));
        assert!(matches!(
            receiver.try_recv(),
            Some(CaptureEvent::Error { message, .. }) if message == "fatal"
        ));
        assert!(matches!(
            receiver.try_recv(),
            Some(CaptureEvent::Samples(frame)) if frame.sequence() == 1
        ));
    }

    #[test]
    fn boundary_snapshot_defers_without_spinning_while_real_sender_is_active() {
        let (mut senders, receiver) = realtime_capture_channel(2, &[TrackId::MICROPHONE]);
        let make_frame = |sequence| {
            AudioFrame::from_f32(
                TrackId::MICROPHONE,
                SourceKind::Microphone,
                sequence,
                MonotonicTimestamp::default(),
                16_000,
                1,
                vec![0.0; 160],
            )
            .unwrap()
        };
        senders[0].try_send(make_frame(0)).unwrap();
        senders[0]
            .send_control(CaptureControlEvent::Error {
                track_id: Some(TrackId::MICROPHONE),
                message: "fatal".into(),
                recoverable: false,
            })
            .unwrap();

        let hook = Arc::new(super::ProducerCriticalSectionHook {
            entered: std::sync::Barrier::new(2),
            release: std::sync::Barrier::new(2),
        });
        senders[0].critical_section_hook = Some(Arc::clone(&hook));
        let sender = senders[0].clone();
        let producer = std::thread::spawn(move || sender.try_send(make_frame(1)).unwrap());
        hook.entered.wait();

        // A nominal non-blocking receive must defer the boundary immediately
        // instead of spinning until this real sender leaves its critical
        // section.
        assert!(receiver.try_recv().is_none());
        assert!(receiver.gate.frozen.load(Ordering::Acquire));
        senders[0].critical_section_hook = None;
        let overlapping = Arc::new(std::sync::Barrier::new(9));
        let mut overlapping_producers = Vec::new();
        for sequence in 2..10 {
            let sender = senders[0].clone();
            let overlapping = Arc::clone(&overlapping);
            overlapping_producers.push(std::thread::spawn(move || {
                overlapping.wait();
                sender.try_send(make_frame(sequence)).unwrap()
            }));
        }
        overlapping.wait();
        for overlapping_producer in overlapping_producers {
            assert_eq!(overlapping_producer.join().unwrap(), FrameEnqueue::Dropped);
        }

        hook.release.wait();
        assert_eq!(producer.join().unwrap(), FrameEnqueue::Dropped);
        assert!(matches!(
            receiver.try_recv(),
            Some(CaptureEvent::Samples(frame)) if frame.sequence() == 0
        ));
        assert_eq!(
            senders[0].try_send(make_frame(4)).unwrap(),
            FrameEnqueue::Enqueued
        );
        assert!(matches!(
            receiver.try_recv(),
            Some(CaptureEvent::Overflow {
                track_id: TrackId::MICROPHONE,
                dropped_frames: 1_440,
            })
        ));
        assert!(matches!(
            receiver.try_recv(),
            Some(CaptureEvent::Error { message, .. }) if message == "fatal"
        ));
        assert!(matches!(
            receiver.try_recv(),
            Some(CaptureEvent::Samples(frame)) if frame.sequence() == 4
        ));
    }

    #[test]
    fn dropping_receiver_releases_deferred_boundary_and_closes_surviving_senders() {
        let (mut senders, receiver) = realtime_capture_channel(1, &[TrackId::MICROPHONE]);
        senders[0]
            .send_control(CaptureControlEvent::Error {
                track_id: Some(TrackId::MICROPHONE),
                message: "fatal".into(),
                recoverable: false,
            })
            .unwrap();
        let hook = Arc::new(super::ProducerCriticalSectionHook {
            entered: std::sync::Barrier::new(2),
            release: std::sync::Barrier::new(2),
        });
        senders[0].critical_section_hook = Some(Arc::clone(&hook));
        let sender = senders[0].clone();
        let producer = std::thread::spawn(move || {
            sender.try_send(
                AudioFrame::from_f32(
                    TrackId::MICROPHONE,
                    SourceKind::Microphone,
                    0,
                    MonotonicTimestamp::default(),
                    16_000,
                    1,
                    vec![0.0; 160],
                )
                .unwrap(),
            )
        });
        hook.entered.wait();

        assert!(receiver.try_recv().is_none());
        assert!(receiver.gate.frozen.load(Ordering::Acquire));
        drop(receiver);
        assert!(senders[0].gate.is_closed());
        assert!(!senders[0].gate.frozen.load(Ordering::Acquire));

        hook.release.wait();
        assert_eq!(
            producer.join().unwrap().unwrap_err().kind,
            BackendErrorKind::InvalidState
        );
        senders[0].critical_section_hook = None;
        let error = senders[0]
            .try_send(
                AudioFrame::from_f32(
                    TrackId::MICROPHONE,
                    SourceKind::Microphone,
                    1,
                    MonotonicTimestamp::default(),
                    16_000,
                    1,
                    vec![0.0; 160],
                )
                .unwrap(),
            )
            .unwrap_err();
        assert_eq!(error.kind, BackendErrorKind::InvalidState);
    }

    #[test]
    fn second_check_rollback_notifies_blocking_boundary_receiver() {
        let (senders, receiver) = realtime_capture_channel(1, &[TrackId::MICROPHONE]);
        senders[0]
            .send_control(CaptureControlEvent::Error {
                track_id: Some(TrackId::MICROPHONE),
                message: "fatal".into(),
                recoverable: false,
            })
            .unwrap();
        let hook = Arc::new(super::ProducerEnterHook {
            after_first_check: std::sync::Barrier::new(2),
            allow_increment: std::sync::Barrier::new(2),
            after_increment: std::sync::Barrier::new(2),
            allow_second_check: std::sync::Barrier::new(2),
        });
        *senders[0]
            .gate
            .enter_hook
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(Arc::clone(&hook));
        let sender = senders[0].clone();
        let producer = std::thread::spawn(move || {
            sender
                .try_send(
                    AudioFrame::from_f32(
                        TrackId::MICROPHONE,
                        SourceKind::Microphone,
                        0,
                        MonotonicTimestamp::default(),
                        16_000,
                        1,
                        vec![0.0; 160],
                    )
                    .unwrap(),
                )
                .unwrap()
        });

        hook.after_first_check.wait();
        hook.allow_increment.wait();
        hook.after_increment.wait();
        let gate = Arc::clone(&senders[0].gate);
        let consumer = std::thread::spawn(move || {
            let first = receiver.recv_timeout(Duration::from_secs(1));
            (receiver, first)
        });
        let deadline = Instant::now() + Duration::from_secs(1);
        while !gate.frozen.load(Ordering::Acquire) {
            assert!(Instant::now() < deadline, "boundary was not requested");
            std::thread::yield_now();
        }
        hook.allow_second_check.wait();

        assert_eq!(producer.join().unwrap(), FrameEnqueue::Dropped);
        let (receiver, first) = consumer.join().unwrap();
        assert!(matches!(
            first,
            Some(CaptureEvent::Overflow {
                track_id: TrackId::MICROPHONE,
                dropped_frames: 160,
            })
        ));
        assert!(matches!(
            receiver.try_recv(),
            Some(CaptureEvent::Error { message, .. }) if message == "fatal"
        ));
    }

    #[test]
    fn all_track_overflows_precede_frames_accepted_after_boundary() {
        let third_track = TrackId::new(3);
        let (senders, receiver) =
            realtime_capture_channel(1, &[TrackId::MICROPHONE, TrackId::SYSTEM, third_track]);
        let make_frame = |track_id, source, sequence| {
            AudioFrame::from_f32(
                track_id,
                source,
                sequence,
                MonotonicTimestamp::default(),
                16_000,
                1,
                vec![0.0; 160],
            )
            .unwrap()
        };
        senders[0]
            .try_send(make_frame(TrackId::MICROPHONE, SourceKind::Microphone, 0))
            .unwrap();
        senders[0]
            .try_send(make_frame(TrackId::MICROPHONE, SourceKind::Microphone, 1))
            .unwrap();
        senders[1]
            .try_send(make_frame(TrackId::SYSTEM, SourceKind::SystemAudio, 0))
            .unwrap();

        assert!(matches!(
            receiver.try_recv(),
            Some(CaptureEvent::Samples(_))
        ));
        senders[2]
            .try_send(make_frame(
                third_track,
                SourceKind::Other("third".into()),
                0,
            ))
            .unwrap();
        assert!(matches!(
            receiver.try_recv(),
            Some(CaptureEvent::Overflow {
                track_id: TrackId::MICROPHONE,
                ..
            })
        ));
        assert!(matches!(
            receiver.try_recv(),
            Some(CaptureEvent::Overflow {
                track_id: TrackId::SYSTEM,
                ..
            })
        ));
        assert!(matches!(
            receiver.try_recv(),
            Some(CaptureEvent::Samples(frame)) if frame.track_id() == third_track
        ));
    }

    #[test]
    fn recv_timeout_drains_frame_when_control_is_disconnected() {
        let (frame_sender, frame_receiver) = channel::bounded(1);
        let (control_sender, control_receiver) = channel::bounded(1);
        drop(control_sender);
        frame_sender
            .send(
                AudioFrame::from_f32(
                    TrackId::MICROPHONE,
                    SourceKind::Microphone,
                    7,
                    MonotonicTimestamp::default(),
                    16_000,
                    1,
                    vec![0.0; 160],
                )
                .unwrap(),
            )
            .unwrap();
        drop(frame_sender);
        let receiver = CaptureEventReceiver {
            frame_receiver,
            control_receiver,
            dropped: Arc::new(Vec::new()),
            gate: Arc::new(super::ProducerGate::default()),
            delivery: RefCell::new(super::CaptureDeliveryState::default()),
        };

        assert!(matches!(
            receiver.recv_timeout(Duration::from_millis(10)),
            Some(CaptureEvent::Samples(frame)) if frame.sequence() == 7
        ));
    }

    #[test]
    fn recv_timeout_drains_control_when_frames_are_disconnected() {
        let (frame_sender, frame_receiver) = channel::bounded(1);
        let (control_sender, control_receiver) = channel::bounded(1);
        drop(frame_sender);
        control_sender
            .send(CaptureControlEvent::Error {
                track_id: Some(TrackId::SYSTEM),
                message: "final error".into(),
                recoverable: false,
            })
            .unwrap();
        drop(control_sender);
        let receiver = CaptureEventReceiver {
            frame_receiver,
            control_receiver,
            dropped: Arc::new(Vec::new()),
            gate: Arc::new(super::ProducerGate::default()),
            delivery: RefCell::new(super::CaptureDeliveryState::default()),
        };

        assert!(matches!(
            receiver.recv_timeout(Duration::from_millis(10)),
            Some(CaptureEvent::Error { message, .. }) if message == "final error"
        ));
    }

    #[test]
    fn control_queue_is_bounded_and_cannot_carry_samples() {
        let (senders, receiver) = realtime_capture_channel(1, &[TrackId::MICROPHONE]);
        let sender = &senders[0];
        let error = |message: &str| CaptureControlEvent::Error {
            track_id: Some(TrackId::MICROPHONE),
            message: message.into(),
            recoverable: true,
        };

        assert_eq!(
            sender.send_control(error("first")).unwrap(),
            ControlEnqueue::Enqueued
        );
        assert_eq!(
            sender.send_control(error("second")).unwrap(),
            ControlEnqueue::Dropped
        );
        assert!(matches!(
            receiver.try_recv(),
            Some(CaptureEvent::Error { message, .. }) if message == "first"
        ));
    }

    #[derive(Clone)]
    struct FakeCapture {
        events: Arc<Mutex<VecDeque<CaptureEvent>>>,
        calls: Arc<Mutex<Vec<&'static str>>>,
        stop_failures_remaining: usize,
    }

    impl CaptureBackend for FakeCapture {
        fn probe(&self) -> CaptureProbe {
            CaptureProbe {
                backend_id: BackendId::new("fake-capture"),
                availability: Availability::Available,
                capabilities: CaptureCapabilities {
                    tracks: vec![wisp_core::SourceLabel::Mic.track_descriptor()],
                    simultaneous_tracks: true,
                    monotonic_timestamps: true,
                    device_change_notifications: true,
                },
            }
        }

        fn start(&mut self) -> BackendResult<()> {
            self.calls.lock().unwrap().push("capture-start");
            Ok(())
        }

        fn next_event(
            &mut self,
            _timeout: Duration,
        ) -> BackendResult<Option<CaptureEvent>> {
            Ok(self.events.lock().unwrap().pop_front())
        }

        fn stop(
            &mut self,
            mode: ShutdownMode,
        ) -> BackendResult<()> {
            self.calls.lock().unwrap().push(match mode {
                ShutdownMode::Graceful => "capture-stop-graceful",
                ShutdownMode::Abort => "capture-stop-abort",
            });
            if self.stop_failures_remaining > 0 {
                self.stop_failures_remaining -= 1;
                Err(BackendError::new(
                    BackendId::new("fake-capture"),
                    BackendErrorKind::Internal,
                    "capture stop failed",
                ))
            } else {
                Ok(())
            }
        }
    }

    struct FakeTranscriber {
        events: VecDeque<TranscriptEvent>,
        finish_events: VecDeque<TranscriptEvent>,
        push_errors_remaining: usize,
        next_event_errors_remaining: usize,
        calls: Arc<Mutex<Vec<&'static str>>>,
    }

    impl TranscriberBackend for FakeTranscriber {
        fn probe(&self) -> TranscriberProbe {
            transcriber_probe(
                "fake-transcriber",
                TranscriberClass::LocalModel,
                RecognitionPrivacy::Offline,
                true,
            )
        }

        fn start(
            &mut self,
            _tracks: &[wisp_core::TrackDescriptor],
        ) -> BackendResult<()> {
            self.calls.lock().unwrap().push("transcriber-start");
            Ok(())
        }

        fn push(
            &mut self,
            _frame: &AudioFrame,
        ) -> BackendResult<()> {
            self.calls.lock().unwrap().push("transcriber-push");
            if self.push_errors_remaining > 0 {
                self.push_errors_remaining -= 1;
                Err(BackendError::new(
                    BackendId::new("fake-transcriber"),
                    BackendErrorKind::Internal,
                    "push failed",
                ))
            } else {
                Ok(())
            }
        }

        fn push_gap(
            &mut self,
            _track_id: TrackId,
            _dropped_frames: u64,
        ) -> BackendResult<()> {
            self.calls.lock().unwrap().push("transcriber-gap");
            Ok(())
        }

        fn next_event(
            &mut self,
            _timeout: Duration,
        ) -> BackendResult<Option<TranscriptEvent>> {
            if self.next_event_errors_remaining > 0 {
                self.next_event_errors_remaining -= 1;
                Err(BackendError::new(
                    BackendId::new("fake-transcriber"),
                    BackendErrorKind::Internal,
                    "next event failed",
                ))
            } else {
                Ok(self.events.pop_front())
            }
        }

        fn finish(&mut self) -> BackendResult<()> {
            self.calls.lock().unwrap().push("transcriber-finish");
            self.events.append(&mut self.finish_events);
            Ok(())
        }

        fn abort(&mut self) -> BackendResult<()> {
            self.calls.lock().unwrap().push("transcriber-abort");
            Ok(())
        }
    }

    #[test]
    fn recording_transcriber_fans_out_pcm_gap_events_and_finish() {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let segment = TranscriptSegment {
            track_id: TrackId::MICROPHONE,
            segment_id: TranscriptSegmentId::new(4),
            text: "final".into(),
            start_seconds: 0.0,
            end_seconds: 1.0,
            confidence_mean: None,
            confidence_min: None,
        };
        let backend = FakeTranscriber {
            events: VecDeque::new(),
            finish_events: VecDeque::from([TranscriptEvent::Final(segment.clone())]),
            push_errors_remaining: 0,
            next_event_errors_remaining: 0,
            calls: Arc::clone(&calls),
        };
        let tracks = [wisp_core::SourceLabel::Mic.track_descriptor()];
        let (mut tap, events) = RecordingTranscriber::start(Box::new(backend), &tracks).unwrap();
        let frame = AudioFrame::from_f32(
            TrackId::MICROPHONE,
            SourceKind::Microphone,
            0,
            MonotonicTimestamp::default(),
            16_000,
            1,
            vec![0.0; 160],
        )
        .unwrap();
        tap.push_capture(&CaptureEvent::Samples(frame)).unwrap();
        tap.push_capture(&CaptureEvent::Overflow {
            track_id: TrackId::MICROPHONE,
            dropped_frames: 80,
        })
        .unwrap();
        tap.finish().unwrap();
        assert_eq!(events.recv().unwrap(), TranscriptEvent::Final(segment));
        assert_eq!(
            calls.lock().unwrap().as_slice(),
            [
                "transcriber-start",
                "transcriber-push",
                "transcriber-gap",
                "transcriber-finish"
            ]
        );
    }

    #[test]
    fn recording_transcriber_preserves_final_bursts_larger_than_old_queue_capacity() {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let events = (0..256)
            .map(|id| {
                TranscriptEvent::Final(TranscriptSegment {
                    track_id: TrackId::MICROPHONE,
                    segment_id: TranscriptSegmentId::new(id),
                    text: format!("final {id}"),
                    start_seconds: 0.0,
                    end_seconds: 0.5,
                    confidence_mean: None,
                    confidence_min: None,
                })
            })
            .collect();
        let backend = FakeTranscriber {
            events,
            finish_events: VecDeque::new(),
            push_errors_remaining: 0,
            next_event_errors_remaining: 0,
            calls,
        };
        let tracks = [wisp_core::SourceLabel::Mic.track_descriptor()];
        let (mut tap, receiver) = RecordingTranscriber::start(Box::new(backend), &tracks).unwrap();
        tap.push_capture(&CaptureEvent::Error {
            track_id: None,
            message: "test notification".into(),
            recoverable: true,
        })
        .unwrap();
        assert_eq!(receiver.try_iter().count(), 256);
    }

    fn fake_orchestrator(
        transcript_events: VecDeque<TranscriptEvent>
    ) -> (
        SessionOrchestrator<FakeCapture, FakeTranscriber>,
        Arc<Mutex<Vec<&'static str>>>,
    ) {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let frame = AudioFrame::from_f32(
            TrackId::MICROPHONE,
            SourceKind::Microphone,
            0,
            MonotonicTimestamp::default(),
            16_000,
            1,
            vec![0.0; 160],
        )
        .unwrap();
        let capture = FakeCapture {
            events: Arc::new(Mutex::new(VecDeque::from([CaptureEvent::Samples(frame)]))),
            calls: Arc::clone(&calls),
            stop_failures_remaining: 0,
        };
        let transcriber = FakeTranscriber {
            events: transcript_events,
            finish_events: VecDeque::new(),
            push_errors_remaining: 0,
            next_event_errors_remaining: 0,
            calls: Arc::clone(&calls),
        };
        (SessionOrchestrator::new(capture, Some(transcriber)), calls)
    }

    #[test]
    fn fake_backend_keeps_partial_to_final_identity() {
        let segment = TranscriptSegment {
            track_id: TrackId::MICROPHONE,
            segment_id: TranscriptSegmentId::new(9),
            text: "partial".into(),
            start_seconds: 0.0,
            end_seconds: 1.0,
            confidence_mean: None,
            confidence_min: None,
        };
        let events = VecDeque::from([
            TranscriptEvent::Partial(segment.clone()),
            TranscriptEvent::Final(TranscriptSegment {
                text: "final".into(),
                ..segment
            }),
        ]);
        let (mut orchestrator, _calls) = fake_orchestrator(events);
        orchestrator.start().unwrap();

        assert!(matches!(
            orchestrator.pump_once(Duration::ZERO).unwrap(),
            Some(OrchestratorEvent::Capture(CaptureEvent::Samples(_)))
        ));
        let partial = orchestrator.pump_once(Duration::ZERO).unwrap().unwrap();
        let final_event = orchestrator.pump_once(Duration::ZERO).unwrap().unwrap();
        let (
            OrchestratorEvent::Transcript(TranscriptEvent::Partial(partial)),
            OrchestratorEvent::Transcript(TranscriptEvent::Final(final_event)),
        ) = (partial, final_event)
        else {
            panic!("expected partial then final transcript events");
        };
        assert_eq!(partial.segment_id, final_event.segment_id);
    }

    #[test]
    fn graceful_shutdown_flushes_while_abort_does_not() {
        let (mut graceful, graceful_calls) = fake_orchestrator(VecDeque::new());
        graceful.start().unwrap();
        graceful.shutdown(ShutdownMode::Graceful).unwrap();
        assert_eq!(
            *graceful_calls.lock().unwrap(),
            [
                "capture-start",
                "transcriber-start",
                "capture-stop-graceful",
                "transcriber-push",
                "transcriber-finish"
            ]
        );

        let (mut aborted, abort_calls) = fake_orchestrator(VecDeque::new());
        aborted.start().unwrap();
        aborted.shutdown(ShutdownMode::Abort).unwrap();
        assert_eq!(
            *abort_calls.lock().unwrap(),
            [
                "capture-start",
                "transcriber-start",
                "capture-stop-abort",
                "transcriber-abort"
            ]
        );
    }

    #[test]
    fn disabling_transcriber_keeps_capture_running_and_discards_transcripts() {
        let segment = TranscriptSegment {
            track_id: TrackId::MICROPHONE,
            segment_id: TranscriptSegmentId::new(3),
            text: "must not escape".into(),
            start_seconds: 0.0,
            end_seconds: 1.0,
            confidence_mean: None,
            confidence_min: None,
        };
        let (mut orchestrator, calls) =
            fake_orchestrator(VecDeque::from([TranscriptEvent::Final(segment)]));
        orchestrator.start().unwrap();

        orchestrator.disable_transcriber().unwrap();
        assert!(matches!(
            orchestrator.pump_once(Duration::ZERO).unwrap(),
            Some(OrchestratorEvent::Capture(CaptureEvent::Samples(_)))
        ));
        assert_eq!(orchestrator.pump_once(Duration::ZERO).unwrap(), None);
        orchestrator.shutdown(ShutdownMode::Graceful).unwrap();

        assert_eq!(
            *calls.lock().unwrap(),
            [
                "capture-start",
                "transcriber-start",
                "transcriber-abort",
                "capture-stop-graceful",
            ]
        );
    }

    #[test]
    fn transcription_push_failure_does_not_hide_capture_event() {
        let (orchestrator, _calls) = fake_orchestrator(VecDeque::new());
        let (capture, transcriber) = orchestrator.into_parts();
        let mut transcriber = transcriber.unwrap();
        transcriber.push_errors_remaining = 1;
        capture
            .events
            .lock()
            .unwrap()
            .push_back(CaptureEvent::Samples(
                AudioFrame::from_f32(
                    TrackId::MICROPHONE,
                    SourceKind::Microphone,
                    1,
                    MonotonicTimestamp::default(),
                    16_000,
                    1,
                    vec![0.0; 160],
                )
                .unwrap(),
            ));
        let mut orchestrator = SessionOrchestrator::new(capture, Some(transcriber));
        orchestrator.start().unwrap();

        assert!(matches!(
            orchestrator.pump_once(Duration::ZERO).unwrap(),
            Some(OrchestratorEvent::Capture(CaptureEvent::Samples(frame)))
                if frame.sequence() == 0
        ));
        assert!(matches!(
            orchestrator.pump_once(Duration::ZERO),
            Err(BackendError {
                kind: BackendErrorKind::Internal,
                ..
            })
        ));
        assert!(matches!(
            orchestrator.pump_once(Duration::ZERO).unwrap(),
            Some(OrchestratorEvent::Capture(CaptureEvent::Samples(frame)))
                if frame.sequence() == 1
        ));
    }

    #[test]
    fn transcription_next_event_failure_does_not_stop_capture() {
        let (orchestrator, _calls) = fake_orchestrator(VecDeque::new());
        let (capture, transcriber) = orchestrator.into_parts();
        let mut transcriber = transcriber.unwrap();
        transcriber.next_event_errors_remaining = 1;
        capture
            .events
            .lock()
            .unwrap()
            .push_back(CaptureEvent::Samples(
                AudioFrame::from_f32(
                    TrackId::MICROPHONE,
                    SourceKind::Microphone,
                    1,
                    MonotonicTimestamp::default(),
                    16_000,
                    1,
                    vec![0.0; 160],
                )
                .unwrap(),
            ));
        let mut orchestrator = SessionOrchestrator::new(capture, Some(transcriber));
        orchestrator.start().unwrap();

        assert!(matches!(
            orchestrator.pump_once(Duration::ZERO).unwrap(),
            Some(OrchestratorEvent::Capture(CaptureEvent::Samples(frame)))
                if frame.sequence() == 0
        ));
        assert!(orchestrator.pump_once(Duration::ZERO).is_err());
        assert!(matches!(
            orchestrator.pump_once(Duration::ZERO).unwrap(),
            Some(OrchestratorEvent::Capture(CaptureEvent::Samples(frame)))
                if frame.sequence() == 1
        ));
    }

    #[test]
    fn graceful_shutdown_drains_final_emitted_by_finish() {
        let segment = TranscriptSegment {
            track_id: TrackId::MICROPHONE,
            segment_id: TranscriptSegmentId::new(12),
            text: "flushed final".into(),
            start_seconds: 0.0,
            end_seconds: 1.0,
            confidence_mean: None,
            confidence_min: None,
        };
        let (orchestrator, calls) = fake_orchestrator(VecDeque::new());
        let (capture, transcriber) = orchestrator.into_parts();
        let mut transcriber = transcriber.unwrap();
        capture
            .events
            .lock()
            .unwrap()
            .push_back(CaptureEvent::Samples(
                AudioFrame::from_f32(
                    TrackId::MICROPHONE,
                    SourceKind::Microphone,
                    1,
                    MonotonicTimestamp::default(),
                    16_000,
                    1,
                    vec![0.0; 160],
                )
                .unwrap(),
            ));
        transcriber
            .finish_events
            .push_back(TranscriptEvent::Final(segment));
        let mut orchestrator = SessionOrchestrator::new(capture, Some(transcriber));
        orchestrator.start().unwrap();
        orchestrator.shutdown(ShutdownMode::Graceful).unwrap();
        assert_eq!(
            *calls.lock().unwrap(),
            [
                "capture-start",
                "transcriber-start",
                "capture-stop-graceful",
                "transcriber-push",
                "transcriber-push",
                "transcriber-finish",
            ]
        );

        assert!(matches!(
            orchestrator.pump_once(Duration::ZERO).unwrap(),
            Some(OrchestratorEvent::Capture(CaptureEvent::Samples(frame)))
                if frame.sequence() == 0
        ));
        assert!(matches!(
            orchestrator.pump_once(Duration::ZERO).unwrap(),
            Some(OrchestratorEvent::Capture(CaptureEvent::Samples(frame)))
                if frame.sequence() == 1
        ));
        assert!(matches!(
            orchestrator.pump_once(Duration::ZERO).unwrap(),
            Some(OrchestratorEvent::Transcript(TranscriptEvent::Final(segment)))
                if segment.text == "flushed final"
        ));
    }

    #[test]
    fn orchestrator_rejects_double_start_and_restart() {
        let (mut orchestrator, calls) = fake_orchestrator(VecDeque::new());
        orchestrator.start().unwrap();
        assert!(matches!(
            orchestrator.start(),
            Err(BackendError {
                kind: BackendErrorKind::InvalidState,
                ..
            })
        ));
        orchestrator.shutdown(ShutdownMode::Abort).unwrap();
        assert!(matches!(
            orchestrator.start(),
            Err(BackendError {
                kind: BackendErrorKind::InvalidState,
                ..
            })
        ));
        assert_eq!(
            calls
                .lock()
                .unwrap()
                .iter()
                .filter(|call| **call == "capture-start")
                .count(),
            1
        );
    }

    #[test]
    fn failed_graceful_stop_can_be_retried_as_abort() {
        let (orchestrator, calls) = fake_orchestrator(VecDeque::new());
        let (mut capture, transcriber) = orchestrator.into_parts();
        capture.stop_failures_remaining = 1;
        let mut orchestrator = SessionOrchestrator::new(capture, transcriber);
        orchestrator.start().unwrap();

        assert!(orchestrator.shutdown(ShutdownMode::Graceful).is_err());
        orchestrator.shutdown(ShutdownMode::Abort).unwrap();
        assert_eq!(
            *calls.lock().unwrap(),
            [
                "capture-start",
                "transcriber-start",
                "capture-stop-graceful",
                "capture-stop-abort",
                "transcriber-abort",
            ]
        );
    }

    #[test]
    fn aborted_transcriber_is_not_pushed_during_later_graceful_retry() {
        let (orchestrator, calls) = fake_orchestrator(VecDeque::new());
        let (mut capture, transcriber) = orchestrator.into_parts();
        capture.stop_failures_remaining = 1;
        let mut orchestrator = SessionOrchestrator::new(capture, transcriber);
        orchestrator.start().unwrap();

        assert!(orchestrator.shutdown(ShutdownMode::Abort).is_err());
        orchestrator.shutdown(ShutdownMode::Graceful).unwrap();
        assert_eq!(
            *calls.lock().unwrap(),
            [
                "capture-start",
                "transcriber-start",
                "capture-stop-abort",
                "transcriber-abort",
                "capture-stop-graceful",
            ]
        );
        assert!(matches!(
            orchestrator.pump_once(Duration::ZERO).unwrap(),
            Some(OrchestratorEvent::Capture(CaptureEvent::Samples(_)))
        ));
    }

    #[test]
    fn start_failure_aborts_capture_transactionally() {
        struct FailingTranscriber {
            calls: Arc<Mutex<Vec<&'static str>>>,
        }

        impl TranscriberBackend for FailingTranscriber {
            fn probe(&self) -> TranscriberProbe {
                transcriber_probe(
                    "failing",
                    TranscriberClass::LocalModel,
                    RecognitionPrivacy::Offline,
                    true,
                )
            }

            fn start(
                &mut self,
                _tracks: &[wisp_core::TrackDescriptor],
            ) -> BackendResult<()> {
                self.calls.lock().unwrap().push("transcriber-start");
                Err(BackendError::new(
                    BackendId::new("failing"),
                    BackendErrorKind::MissingModel,
                    "missing model",
                ))
            }

            fn push(
                &mut self,
                _frame: &AudioFrame,
            ) -> BackendResult<()> {
                Ok(())
            }

            fn next_event(
                &mut self,
                _timeout: Duration,
            ) -> BackendResult<Option<TranscriptEvent>> {
                Ok(None)
            }

            fn finish(&mut self) -> BackendResult<()> {
                Ok(())
            }

            fn abort(&mut self) -> BackendResult<()> {
                self.calls.lock().unwrap().push("transcriber-abort");
                Ok(())
            }
        }

        let calls = Arc::new(Mutex::new(Vec::new()));
        let capture = FakeCapture {
            events: Arc::new(Mutex::new(VecDeque::new())),
            calls: Arc::clone(&calls),
            stop_failures_remaining: 0,
        };
        let transcriber = FailingTranscriber {
            calls: Arc::clone(&calls),
        };
        let mut orchestrator = SessionOrchestrator::new(capture, Some(transcriber));

        assert!(orchestrator.start().is_err());
        assert_eq!(
            *calls.lock().unwrap(),
            [
                "capture-start",
                "transcriber-start",
                "capture-stop-abort",
                "transcriber-abort",
            ]
        );
        assert!(matches!(
            orchestrator.start(),
            Err(BackendError {
                kind: BackendErrorKind::InvalidState,
                ..
            })
        ));

        let retry_calls = Arc::new(Mutex::new(Vec::new()));
        let capture = FakeCapture {
            events: Arc::new(Mutex::new(VecDeque::new())),
            calls: Arc::clone(&retry_calls),
            stop_failures_remaining: 1,
        };
        let transcriber = FailingTranscriber {
            calls: Arc::clone(&retry_calls),
        };
        let mut orchestrator = SessionOrchestrator::new(capture, Some(transcriber));
        let error = orchestrator.start().unwrap_err();
        assert!(error.message.contains("capture abort also failed"));
        orchestrator.shutdown(ShutdownMode::Abort).unwrap();
        assert_eq!(
            *retry_calls.lock().unwrap(),
            [
                "capture-start",
                "transcriber-start",
                "capture-stop-abort",
                "transcriber-abort",
                "capture-stop-abort",
            ]
        );
    }

    struct RecordOnlyFallbackFailingTranscriber {
        calls: Arc<Mutex<Vec<&'static str>>>,
        abort_failures_remaining: usize,
    }

    impl TranscriberBackend for RecordOnlyFallbackFailingTranscriber {
        fn probe(&self) -> TranscriberProbe {
            transcriber_probe(
                "record-only-failure",
                TranscriberClass::Platform,
                RecognitionPrivacy::Offline,
                true,
            )
        }

        fn start(
            &mut self,
            _tracks: &[wisp_core::TrackDescriptor],
        ) -> BackendResult<()> {
            self.calls.lock().unwrap().push("transcriber-start");
            Err(BackendError::new(
                BackendId::new("record-only-failure"),
                BackendErrorKind::MissingModel,
                "startup failed",
            ))
        }

        fn push(
            &mut self,
            _frame: &AudioFrame,
        ) -> BackendResult<()> {
            Ok(())
        }

        fn next_event(
            &mut self,
            _timeout: Duration,
        ) -> BackendResult<Option<TranscriptEvent>> {
            Ok(None)
        }

        fn finish(&mut self) -> BackendResult<()> {
            Ok(())
        }

        fn abort(&mut self) -> BackendResult<()> {
            self.calls.lock().unwrap().push("transcriber-abort");
            if self.abort_failures_remaining > 0 {
                self.abort_failures_remaining -= 1;
                Err(BackendError::new(
                    BackendId::new("record-only-failure"),
                    BackendErrorKind::Internal,
                    "disable failed",
                ))
            } else {
                Ok(())
            }
        }
    }

    #[test]
    fn record_only_disable_failure_aborts_capture_and_retries_only_transcriber() {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let capture = FakeCapture {
            events: Arc::new(Mutex::new(VecDeque::new())),
            calls: Arc::clone(&calls),
            stop_failures_remaining: 0,
        };
        let transcriber = RecordOnlyFallbackFailingTranscriber {
            calls: Arc::clone(&calls),
            abort_failures_remaining: 1,
        };
        let mut orchestrator = SessionOrchestrator::new(capture, Some(transcriber));

        let error = orchestrator.start_allowing_record_only().unwrap_err();
        assert!(error.message.contains("record-only fallback failed"));
        assert!(!error.message.contains("capture abort also failed"));
        assert_eq!(
            *calls.lock().unwrap(),
            [
                "capture-start",
                "transcriber-start",
                "transcriber-abort",
                "capture-stop-abort",
            ]
        );

        orchestrator.shutdown(ShutdownMode::Abort).unwrap();
        assert_eq!(
            *calls.lock().unwrap(),
            [
                "capture-start",
                "transcriber-start",
                "transcriber-abort",
                "capture-stop-abort",
                "transcriber-abort",
            ],
            "successful capture abort must not be repeated"
        );
    }

    #[test]
    fn record_only_disable_and_capture_abort_failures_are_aggregated_and_retried() {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let capture = FakeCapture {
            events: Arc::new(Mutex::new(VecDeque::new())),
            calls: Arc::clone(&calls),
            stop_failures_remaining: 1,
        };
        let transcriber = RecordOnlyFallbackFailingTranscriber {
            calls: Arc::clone(&calls),
            abort_failures_remaining: 1,
        };
        let mut orchestrator = SessionOrchestrator::new(capture, Some(transcriber));

        let error = orchestrator.start_allowing_record_only().unwrap_err();
        assert!(error.message.contains("startup failed"));
        assert!(error.message.contains("disable failed"));
        assert!(error.message.contains("capture abort also failed"));

        orchestrator.shutdown(ShutdownMode::Abort).unwrap();
        assert_eq!(
            *calls.lock().unwrap(),
            [
                "capture-start",
                "transcriber-start",
                "transcriber-abort",
                "capture-stop-abort",
                "capture-stop-abort",
                "transcriber-abort",
            ]
        );
    }
}
