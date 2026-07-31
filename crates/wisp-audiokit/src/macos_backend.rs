//! Production macOS adapters for the backend-neutral session architecture.
//!
//! The Swift framework still owns Core Audio capture, `SpeechAnalyzer`, and
//! Ogg/Opus recording as one lifecycle transaction. These adapters split its
//! bounded callback stream at the Rust boundary so the application can use the
//! shared [`SessionOrchestrator`] without copying real-time PCM across FFI.

use std::collections::VecDeque;
use std::path::Path;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use wisp_core::{
    AudioFrame, CaptureEvent, MonotonicTimestamp, SourceKind, SourceLabel, TrackDescriptor,
    TrackId, TranscriptEvent,
};

#[cfg(test)]
use crate::SessionResult;
use crate::{
    Availability, BackendError, BackendErrorKind, BackendId, BackendResult, CaptureBackend,
    CaptureCapabilities, CaptureProbe, Event, NativeSession, OrchestratorEvent, Permission,
    PermissionStatus, RecognitionPrivacy, SessionConfig, SessionError, SessionOptions,
    SessionOrchestrator, ShutdownMode, TranscriberBackend, TranscriberCapabilities,
    TranscriberClass, TranscriberFeature, TranscriberProbe, TranscriptionSelection,
    UnavailableReason, check_permission, select_transcriber,
};

const CAPTURE_BACKEND_ID: &str = "macos-core-audio-process-tap";
const TRANSCRIBER_BACKEND_ID: &str = "macos-speech-analyzer";
const CLEANUP_RETRY_BUDGET: usize = 3;

trait SwiftSessionBridge: Send {
    fn start_capture(&mut self) -> crate::Result<()>;
    fn start_transcription(&self) -> crate::Result<()>;
    fn has_started_capture(&self) -> bool;
    fn set_microphone_muted(
        &self,
        muted: bool,
    );
    fn stop_capture(&self) -> crate::Result<()>;
    fn finish_transcription(&self) -> crate::Result<()>;
    fn abort(&self);
    fn push_transcriber_frame(
        &self,
        frame: &AudioFrame,
    ) -> crate::Result<()>;
    fn disable_transcription(&self) -> crate::Result<()>;
    fn try_recv(&self) -> Option<Event>;
    fn try_recv_audio(&self) -> Option<CaptureEvent>;
    fn try_recv_transcriber_failure(&self) -> Option<crate::MacosTranscriberFailure>;
    fn try_recv_capture_failure(&self) -> Option<crate::MacosCaptureFailure>;
    fn recv_audio_timeout(
        &self,
        timeout: Duration,
    ) -> Option<CaptureEvent>;
    fn first_audio_timestamp(
        &self,
        _track_id: TrackId,
    ) -> Option<MonotonicTimestamp> {
        None
    }
}

impl SwiftSessionBridge for NativeSession {
    fn start_capture(&mut self) -> crate::Result<()> {
        Self::start_capture(self)
    }

    fn start_transcription(&self) -> crate::Result<()> {
        Self::start_transcription(self)
    }

    fn has_started_capture(&self) -> bool {
        Self::has_started_capture(self)
    }

    fn set_microphone_muted(
        &self,
        muted: bool,
    ) {
        Self::set_microphone_muted(self, muted);
    }

    fn stop_capture(&self) -> crate::Result<()> {
        Self::stop_capture(self)
    }

    fn finish_transcription(&self) -> crate::Result<()> {
        Self::finish_transcription(self)
    }

    fn abort(&self) {
        Self::abort(self);
    }

    fn push_transcriber_frame(
        &self,
        frame: &AudioFrame,
    ) -> crate::Result<()> {
        Self::push_transcriber_frame(self, frame)
    }

    fn disable_transcription(&self) -> crate::Result<()> {
        Self::disable_transcription(self)
    }

    fn try_recv(&self) -> Option<Event> {
        Self::try_recv(self)
    }

    fn try_recv_audio(&self) -> Option<CaptureEvent> {
        Self::try_recv_audio(self)
    }

    fn try_recv_transcriber_failure(&self) -> Option<crate::MacosTranscriberFailure> {
        Self::try_recv_transcriber_failure(self)
    }

    fn try_recv_capture_failure(&self) -> Option<crate::MacosCaptureFailure> {
        Self::try_recv_capture_failure(self)
    }

    fn recv_audio_timeout(
        &self,
        timeout: Duration,
    ) -> Option<CaptureEvent> {
        Self::recv_audio_timeout(self, timeout)
    }

    fn first_audio_timestamp(
        &self,
        track_id: TrackId,
    ) -> Option<MonotonicTimestamp> {
        Self::first_audio_timestamp(self, track_id)
    }
}

struct SharedState {
    bridge: Box<dyn SwiftSessionBridge>,
    capture_events: VecDeque<CaptureEvent>,
    transcript_events: VecDeque<TranscriptEvent>,
    compatibility_events: VecDeque<Event>,
    microphone_permission: PermissionStatus,
    speech_permission: PermissionStatus,
    bridge_transcription: BridgeTranscription,
    capture_started: bool,
    transcriber_started: bool,
    transcriber_tracks: [TranscriberTrackState; 2],
    stopped: bool,
}

#[derive(Default)]
struct TranscriberTrackState {
    format: Option<(u32, u16, SourceKind)>,
    pending_gap: u64,
    silence_sequence: u64,
}

fn duration_to_frames(
    duration: Duration,
    sample_rate: u32,
) -> u64 {
    const NANOS_PER_SECOND: u128 = 1_000_000_000;
    let scaled = duration.as_nanos().saturating_mul(u128::from(sample_rate));
    let rounded = scaled.saturating_add(NANOS_PER_SECOND / 2) / NANOS_PER_SECOND;
    u64::try_from(rounded).unwrap_or(u64::MAX)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BridgeTranscription {
    Enabled,
    Disabled,
}

type Shared = Arc<Mutex<SharedState>>;

fn lock_shared(shared: &Shared) -> MutexGuard<'_, SharedState> {
    shared
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn tracks() -> Vec<TrackDescriptor> {
    vec![
        SourceLabel::Mic.track_descriptor(),
        SourceLabel::System.track_descriptor(),
    ]
}

fn permission_availability(
    permission: Permission,
    status: PermissionStatus,
) -> Availability {
    match status {
        PermissionStatus::Denied | PermissionStatus::Restricted => {
            Availability::Unavailable(UnavailableReason::PermissionDenied(format!(
                "{} permission is {status:?}",
                match permission {
                    Permission::Microphone => "microphone",
                    Permission::SpeechRecognition => "speech recognition",
                }
            )))
        },
        PermissionStatus::Undetermined | PermissionStatus::Granted => Availability::Available,
    }
}

fn backend_error(
    backend: &'static str,
    kind: BackendErrorKind,
    message: impl Into<String>,
) -> BackendError {
    BackendError::new(BackendId::new(backend), kind, message)
}

fn session_start_error(
    state: &SharedState,
    error: &SessionError,
) -> BackendError {
    let detail = match error {
        SessionError::Start(detail) => detail.clone(),
        error => error.to_string(),
    };
    let normalized = detail.to_ascii_lowercase();
    let kind = if matches!(
        state.microphone_permission,
        PermissionStatus::Denied | PermissionStatus::Restricted
    ) || (normalized.contains("microphone")
        && normalized.contains("permission")
        && normalized.contains("denied"))
    {
        BackendErrorKind::PermissionDenied
    } else {
        BackendErrorKind::Internal
    };
    backend_error(CAPTURE_BACKEND_ID, kind, detail)
}

fn transcriber_start_error(
    state: &SharedState,
    error: &SessionError,
) -> BackendError {
    let detail = match error {
        SessionError::Start(detail) => detail.clone(),
        error => error.to_string(),
    };
    let normalized = detail.to_ascii_lowercase();
    let kind = if matches!(
        state.speech_permission,
        PermissionStatus::Denied | PermissionStatus::Restricted
    ) || (normalized.contains("speech")
        && normalized.contains("permission")
        && normalized.contains("denied"))
    {
        BackendErrorKind::PermissionDenied
    } else if normalized.contains("model") || normalized.contains("asset") {
        BackendErrorKind::MissingModel
    } else {
        BackendErrorKind::Internal
    };
    backend_error(TRANSCRIBER_BACKEND_ID, kind, detail)
}

fn route_bridge_event(
    state: &mut SharedState,
    event: Event,
) -> Option<CaptureEvent> {
    match event {
        Event::Result(result) => {
            if state.bridge_transcription == BridgeTranscription::Disabled {
                return None;
            }
            state
                .compatibility_events
                .push_back(Event::Result(result.clone()));
            state.transcript_events.push_back(result.transcript_event());
            None
        },
        Event::Log(message) => {
            state
                .compatibility_events
                .push_back(Event::Log(message.clone()));
            None
        },
    }
}

fn drain_bridge(state: &mut SharedState) {
    while let Some(failure) = state.bridge.try_recv_capture_failure() {
        state.capture_events.push_back(CaptureEvent::Error {
            track_id: failure.track_id,
            message: failure.message,
            recoverable: false,
        });
    }
    while let Some(event) = state.bridge.try_recv() {
        if let Some(event) = route_bridge_event(state, event) {
            state.capture_events.push_back(event);
        }
    }
    while let Some(event) = state.bridge.try_recv_audio() {
        state.capture_events.push_back(event);
    }
}

/// Core Audio microphone + Process Tap capture adapter.
///
/// Swift retains the recording writer and paired `SpeechAnalyzer`, while its
/// callbacks copy native microphone/system PCM into the bounded nonblocking
/// capture queue consumed here. This makes frames observable to the common
/// orchestrator without performing file I/O or waiting in an audio callback.
pub struct MacosCaptureBackend {
    shared: Shared,
}

impl MacosCaptureBackend {
    fn new(shared: Shared) -> Self {
        Self { shared }
    }

    /// Construct independent macOS capture with Ogg/Opus recording and PCM
    /// events, without requesting speech permission or starting
    /// `SpeechAnalyzer`.
    ///
    /// This entry point can be paired with any [`TranscriberBackend`] through
    /// [`SessionOrchestrator`].
    ///
    /// # Errors
    /// Returns path, locale, or Swift construction errors.
    pub fn new_recording_only(
        output_dir: impl AsRef<Path>,
        locale: &str,
    ) -> crate::Result<Self> {
        let config = SessionConfig::platform_default(locale);
        let bridge = NativeSession::new_for_backend(output_dir, config, false, true, true)?;
        Ok(Self::new(make_shared_state(
            Box::new(bridge),
            check_permission(Permission::Microphone),
            check_permission(Permission::SpeechRecognition),
            false,
        )))
    }

    /// Replace microphone PCM with silence while preserving both timelines.
    pub fn set_microphone_muted(
        &self,
        muted: bool,
    ) {
        lock_shared(&self.shared).bridge.set_microphone_muted(muted);
    }
}

impl CaptureBackend for MacosCaptureBackend {
    fn probe(&self) -> CaptureProbe {
        let state = lock_shared(&self.shared);
        CaptureProbe {
            backend_id: BackendId::new(CAPTURE_BACKEND_ID),
            availability: permission_availability(
                Permission::Microphone,
                state.microphone_permission,
            ),
            capabilities: CaptureCapabilities {
                tracks: tracks(),
                simultaneous_tracks: true,
                monotonic_timestamps: true,
                device_change_notifications: true,
            },
        }
    }

    fn start(&mut self) -> BackendResult<()> {
        let mut state = lock_shared(&self.shared);
        if !permission_availability(Permission::Microphone, state.microphone_permission)
            .is_available()
        {
            return Err(backend_error(
                CAPTURE_BACKEND_ID,
                BackendErrorKind::PermissionDenied,
                "microphone permission is denied or restricted",
            ));
        }
        if state.capture_started || state.stopped {
            return Err(backend_error(
                CAPTURE_BACKEND_ID,
                BackendErrorKind::InvalidState,
                "macOS capture session has already been consumed",
            ));
        }

        match state.bridge.start_capture() {
            Ok(()) => {
                state.capture_started = true;
                Ok(())
            },
            Err(error) => {
                // WispSession already performs transactional cleanup on a
                // failed start. Calling stop is an idempotent barrier and also
                // makes this adapter safe if the bridge implementation changes.
                state.capture_started |= state.bridge.has_started_capture();
                let _ = state.bridge.stop_capture();
                state.stopped = true;
                drain_bridge(&mut state);
                Err(session_start_error(&state, &error))
            },
        }
    }

    fn next_event(
        &mut self,
        timeout: Duration,
    ) -> BackendResult<Option<CaptureEvent>> {
        let mut state = lock_shared(&self.shared);
        if let Some(event) = state.capture_events.pop_front() {
            return Ok(Some(event));
        }
        if let Some(failure) = state.bridge.try_recv_capture_failure() {
            return Ok(Some(CaptureEvent::Error {
                track_id: failure.track_id,
                message: failure.message,
                recoverable: false,
            }));
        }
        if let Some(event) = state.bridge.try_recv()
            && let Some(event) = route_bridge_event(&mut state, event)
        {
            return Ok(Some(event));
        }
        if let Some(event) = if timeout.is_zero() {
            state.bridge.try_recv_audio()
        } else {
            state.bridge.recv_audio_timeout(timeout)
        } {
            return Ok(Some(event));
        }
        Ok(state
            .bridge
            .try_recv()
            .and_then(|event| route_bridge_event(&mut state, event)))
    }

    fn stop(
        &mut self,
        mode: ShutdownMode,
    ) -> BackendResult<()> {
        let mut state = lock_shared(&self.shared);
        if !state.stopped {
            match mode {
                ShutdownMode::Graceful => state.bridge.stop_capture().map_err(|error| {
                    backend_error(
                        CAPTURE_BACKEND_ID,
                        BackendErrorKind::Internal,
                        error.to_string(),
                    )
                })?,
                ShutdownMode::Abort => state.bridge.abort(),
            }
            state.stopped = true;
        }
        match mode {
            ShutdownMode::Graceful => drain_bridge(&mut state),
            ShutdownMode::Abort => {
                while state.bridge.try_recv().is_some() {}
                while state.bridge.try_recv_audio().is_some() {}
                state.capture_events.clear();
                state.transcript_events.clear();
                state.compatibility_events.clear();
            },
        }
        Ok(())
    }
}

/// Apple `SpeechAnalyzer` adapter paired with [`MacosCaptureBackend`].
///
/// Swift capture records and sends each buffer into Rust's bounded PCM queue.
/// [`Self::push`] is the only path back into `SpeechAnalyzer`, guaranteeing
/// that dropped capture frames cannot bypass the orchestrator. Results arrive
/// through the independently bounded transcript callback queue.
/// A future non-platform transcriber can consume these same exposed frames by
/// constructing capture in recording-only mode.
pub struct MacosTranscriberBackend {
    shared: Shared,
}

impl MacosTranscriberBackend {
    fn new(shared: Shared) -> Self {
        Self { shared }
    }
}

fn transcriber_track_index(track_id: TrackId) -> Option<usize> {
    match track_id {
        TrackId::MICROPHONE => Some(0),
        TrackId::SYSTEM => Some(1),
        _ => None,
    }
}

fn flush_transcriber_gap(
    state: &mut SharedState,
    track_index: usize,
    track_id: TrackId,
) -> BackendResult<()> {
    const SILENCE_CHUNK_FRAMES: u64 = 4_096;
    while state.transcriber_tracks[track_index].pending_gap > 0 {
        let chunk_frames = state.transcriber_tracks[track_index]
            .pending_gap
            .min(SILENCE_CHUNK_FRAMES);
        let Some((sample_rate, channels, source)) =
            state.transcriber_tracks[track_index].format.clone()
        else {
            return Ok(());
        };
        let sample_count = usize::try_from(chunk_frames)
            .ok()
            .and_then(|frames| frames.checked_mul(usize::from(channels)))
            .ok_or_else(|| {
                backend_error(
                    TRANSCRIBER_BACKEND_ID,
                    BackendErrorKind::Internal,
                    "macOS SpeechAnalyzer silence gap size overflow",
                )
            })?;
        let sequence = state.transcriber_tracks[track_index].silence_sequence;
        let frame = AudioFrame::from_f32(
            track_id,
            source,
            sequence,
            MonotonicTimestamp::default(),
            sample_rate,
            channels,
            vec![0.0; sample_count],
        )
        .map_err(|error| {
            backend_error(
                TRANSCRIBER_BACKEND_ID,
                BackendErrorKind::Internal,
                format!("could not build SpeechAnalyzer silence gap: {error}"),
            )
        })?;
        state
            .bridge
            .push_transcriber_frame(&frame)
            .map_err(|error| {
                backend_error(
                    TRANSCRIBER_BACKEND_ID,
                    BackendErrorKind::Internal,
                    error.to_string(),
                )
            })?;
        state.transcriber_tracks[track_index].pending_gap -= chunk_frames;
        state.transcriber_tracks[track_index].silence_sequence = state.transcriber_tracks
            [track_index]
            .silence_sequence
            .wrapping_add(1);
    }
    Ok(())
}

impl TranscriberBackend for MacosTranscriberBackend {
    fn probe(&self) -> TranscriberProbe {
        let state = lock_shared(&self.shared);
        macos_transcriber_probe(state.speech_permission)
    }

    fn start(
        &mut self,
        supplied_tracks: &[TrackDescriptor],
    ) -> BackendResult<()> {
        let mut state = lock_shared(&self.shared);
        if !permission_availability(Permission::SpeechRecognition, state.speech_permission)
            .is_available()
        {
            return Err(backend_error(
                TRANSCRIBER_BACKEND_ID,
                BackendErrorKind::PermissionDenied,
                "speech recognition permission is denied or restricted",
            ));
        }
        if !state.capture_started || state.stopped || state.transcriber_started {
            return Err(backend_error(
                TRANSCRIBER_BACKEND_ID,
                BackendErrorKind::InvalidState,
                "SpeechAnalyzer requires one running macOS capture session",
            ));
        }
        let has_microphone = supplied_tracks
            .iter()
            .any(|track| track.id == TrackId::MICROPHONE);
        let has_system = supplied_tracks
            .iter()
            .any(|track| track.id == TrackId::SYSTEM);
        if !has_microphone || !has_system {
            return Err(backend_error(
                TRANSCRIBER_BACKEND_ID,
                BackendErrorKind::UnsupportedFormat,
                "macOS SpeechAnalyzer requires microphone and system tracks",
            ));
        }
        state
            .bridge
            .start_transcription()
            .map_err(|error| transcriber_start_error(&state, &error))?;
        state.transcriber_started = true;
        Ok(())
    }

    fn push(
        &mut self,
        frame: &AudioFrame,
    ) -> BackendResult<()> {
        let mut state = lock_shared(&self.shared);
        if !state.transcriber_started {
            return Err(backend_error(
                TRANSCRIBER_BACKEND_ID,
                BackendErrorKind::InvalidState,
                "SpeechAnalyzer is not running",
            ));
        }
        if !matches!(
            (frame.track_id(), frame.source()),
            (TrackId::MICROPHONE, SourceKind::Microphone)
                | (TrackId::SYSTEM, SourceKind::SystemAudio)
        ) {
            return Err(backend_error(
                TRANSCRIBER_BACKEND_ID,
                BackendErrorKind::UnsupportedFormat,
                "frame does not belong to a macOS microphone or system track",
            ));
        }
        let Some(track_index) = transcriber_track_index(frame.track_id()) else {
            return Err(backend_error(
                TRANSCRIBER_BACKEND_ID,
                BackendErrorKind::UnsupportedFormat,
                "frame track is not supported by macOS SpeechAnalyzer",
            ));
        };
        let is_first_frame = state.transcriber_tracks[track_index].format.is_none();
        state.transcriber_tracks[track_index].format = Some((
            frame.format().sample_rate,
            frame.format().channels,
            frame.source().clone(),
        ));
        if is_first_frame {
            let reported_gap = state.transcriber_tracks[track_index].pending_gap;
            if reported_gap > 0 {
                // Swift's track start is the first PCM buffer that crossed its
                // native handoff, even when Rust's bounded queue rejected that
                // buffer. A native handoff overflow precedes this anchor and is
                // already represented by it; a Rust rejection advances the
                // native clock after the anchor and must be reinserted into the
                // analyzer stream. The absolute first callback timestamp makes
                // that distinction without racing a second drop counter.
                let leading_silence = state.bridge.first_audio_timestamp(frame.track_id()).map_or(
                    0,
                    |first_timestamp| {
                        duration_to_frames(
                            frame
                                .timestamp()
                                .as_duration()
                                .saturating_sub(first_timestamp.as_duration()),
                            frame.format().sample_rate,
                        )
                        .min(reported_gap)
                    },
                );
                state.transcriber_tracks[track_index].pending_gap = leading_silence;
                let message = if leading_silence == 0 {
                    format!(
                        "[ASR] represented {reported_gap} leading native capture-gap frame(s) through the track start on track {}",
                        frame.track_id().get()
                    )
                } else {
                    format!(
                        "[ASR] inserted {leading_silence} leading Rust capture-queue gap frame(s) and represented {} pre-anchor frame(s) through the track start on track {}",
                        reported_gap.saturating_sub(leading_silence),
                        frame.track_id().get()
                    )
                };
                state.compatibility_events.push_back(Event::Log(message));
            }
        }
        flush_transcriber_gap(&mut state, track_index, frame.track_id())?;
        state.bridge.push_transcriber_frame(frame).map_err(|error| {
            backend_error(
                TRANSCRIBER_BACKEND_ID,
                BackendErrorKind::Internal,
                error.to_string(),
            )
        })
    }

    fn push_gap(
        &mut self,
        track_id: TrackId,
        dropped_frames: u64,
    ) -> BackendResult<()> {
        let Some(track_index) = transcriber_track_index(track_id) else {
            return Ok(());
        };
        let mut state = lock_shared(&self.shared);
        state.transcriber_tracks[track_index].pending_gap = state.transcriber_tracks[track_index]
            .pending_gap
            .saturating_add(dropped_frames);
        let has_started = state.transcriber_tracks[track_index].format.is_some();
        if has_started {
            flush_transcriber_gap(&mut state, track_index, track_id)?;
        }
        if has_started {
            state.compatibility_events.push_back(Event::Log(format!(
                "[ASR] inserted silence for {dropped_frames} capture-gap frame(s) on track {}",
                track_id.get()
            )));
        }
        Ok(())
    }

    fn next_event(
        &mut self,
        _timeout: Duration,
    ) -> BackendResult<Option<TranscriptEvent>> {
        let mut state = lock_shared(&self.shared);
        if let Some(failure) = state.bridge.try_recv_transcriber_failure() {
            if !failure.terminal {
                state.compatibility_events.push_back(Event::Log(format!(
                    "[ASR] recoverable transcription gap: {}",
                    failure.message
                )));
                return Ok(None);
            }
            return Err(backend_error(
                TRANSCRIBER_BACKEND_ID,
                BackendErrorKind::Internal,
                failure.message,
            ));
        }
        Ok(state.transcript_events.pop_front())
    }

    fn finish(&mut self) -> BackendResult<()> {
        let mut state = lock_shared(&self.shared);
        let finish_result = state.bridge.finish_transcription();
        // SpeechAnalyzer may publish a valid final before its terminal
        // finishing task reports an error. Preserve those callbacks even
        // though the orchestrator must retain this cleanup phase for retry.
        drain_bridge(&mut state);
        finish_result.map_err(|error| {
            backend_error(
                TRANSCRIBER_BACKEND_ID,
                BackendErrorKind::Internal,
                error.to_string(),
            )
        })?;
        state.transcriber_started = false;
        Ok(())
    }

    fn abort(&mut self) -> BackendResult<()> {
        let mut state = lock_shared(&self.shared);
        state.bridge.disable_transcription().map_err(|error| {
            backend_error(
                TRANSCRIBER_BACKEND_ID,
                BackendErrorKind::Internal,
                format!("could not disable SpeechAnalyzer session-wide: {error}"),
            )
        })?;
        state.transcriber_started = false;
        state.transcript_events.clear();
        Ok(())
    }
}

/// Production macOS session driven through the backend-neutral orchestrator.
///
/// The facade intentionally returns compatibility [`Event`] values so the
/// desktop's persistence, UI, exports, and MCP observers do not need a second
/// migration to retain their current behavior.
pub struct MacosSession {
    orchestrator: SessionOrchestrator<MacosCaptureBackend, MacosTranscriberBackend>,
    shared: Shared,
    startup_events: VecDeque<Event>,
    policy: crate::TranscriptionPolicy,
    record_only: bool,
    lifecycle: MacosLifecycle,
    runtime_failure: Option<SessionError>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MacosLifecycle {
    Ready,
    Running,
    CleanupPending,
    Stopped,
}

impl MacosSession {
    /// Construct a platform-default `SpeechAnalyzer` session.
    ///
    /// # Errors
    /// Returns the same path, locale, and Swift construction errors as the
    /// compatibility [`Session`] API.
    pub fn new(
        output_dir: impl AsRef<Path>,
        locale: &str,
    ) -> crate::Result<Self> {
        Self::new_with_config(output_dir, SessionConfig::platform_default(locale))
    }

    /// Construct a session with a desktop-compatible configuration.
    ///
    /// # Errors
    /// Returns the same construction errors as [`Session::new_with_config`].
    pub fn new_with_config(
        output_dir: impl AsRef<Path>,
        config: SessionConfig,
    ) -> crate::Result<Self> {
        Self::new_with_options(output_dir, config.into())
    }

    /// Construct a session with explicit selection policy.
    ///
    /// # Errors
    /// Returns the same construction errors as [`Session::new_with_options`].
    pub fn new_with_options(
        output_dir: impl AsRef<Path>,
        options: SessionOptions,
    ) -> crate::Result<Self> {
        let (config, policy) = options.into_parts();
        let microphone_permission = check_permission(Permission::Microphone);
        let speech_permission = check_permission(Permission::SpeechRecognition);
        let transcription_enabled = match select_macos_transcriber(policy, speech_permission) {
            TranscriptionSelection::Backend(backend)
                if backend == BackendId::new(TRANSCRIBER_BACKEND_ID) =>
            {
                true
            },
            TranscriptionSelection::RecordOnly { .. } => false,
            TranscriptionSelection::Unavailable { reason } => {
                return Err(SessionError::Start(reason));
            },
            TranscriptionSelection::Backend(backend) => {
                return Err(SessionError::Start(format!(
                    "unsupported macOS transcription backend selected: {backend}"
                )));
            },
        };
        let bridge = NativeSession::new_for_backend(
            output_dir,
            config,
            transcription_enabled,
            true,
            policy.allow_record_only,
        )?;
        Ok(Self::from_bridge(
            Box::new(bridge),
            microphone_permission,
            speech_permission,
            transcription_enabled,
            policy,
        ))
    }

    fn from_bridge(
        bridge: Box<dyn SwiftSessionBridge>,
        microphone_permission: PermissionStatus,
        speech_permission: PermissionStatus,
        transcription_enabled: bool,
        policy: crate::TranscriptionPolicy,
    ) -> Self {
        let shared = make_shared_state(
            bridge,
            microphone_permission,
            speech_permission,
            transcription_enabled,
        );
        let capture = MacosCaptureBackend::new(Arc::clone(&shared));
        let transcriber =
            transcription_enabled.then(|| MacosTranscriberBackend::new(Arc::clone(&shared)));
        Self {
            orchestrator: SessionOrchestrator::new(capture, transcriber),
            shared,
            startup_events: VecDeque::new(),
            policy,
            record_only: !transcription_enabled,
            lifecycle: MacosLifecycle::Ready,
            runtime_failure: None,
        }
    }

    /// Start capture and transcription transactionally.
    ///
    /// # Errors
    /// Returns a compatibility start error while preserving any final results
    /// flushed by Swift's failed-start rollback for [`Self::try_recv`].
    pub fn start(&mut self) -> crate::Result<()> {
        if self.lifecycle == MacosLifecycle::Running {
            return Err(SessionError::Start("session is already started".into()));
        }
        if self.lifecycle == MacosLifecycle::Stopped {
            return Err(SessionError::Start("session has already stopped".into()));
        }
        if self.lifecycle == MacosLifecycle::CleanupPending {
            return Err(SessionError::Start(
                "session cleanup remains incomplete".into(),
            ));
        }
        let start_result = if self.policy.allow_record_only {
            self.orchestrator.start_allowing_record_only()
        } else {
            self.orchestrator.start().map(|()| None)
        };
        match start_result {
            Ok(start_failure) => {
                self.lifecycle = MacosLifecycle::Running;
                if let Some(error) = start_failure {
                    let mut state = lock_shared(&self.shared);
                    state.bridge_transcription = BridgeTranscription::Disabled;
                    state.transcriber_started = false;
                    state.transcript_events.clear();
                    self.record_only = true;
                    self.startup_events.push_back(Event::Log(format!(
                        "[ASR] {error}; continuing record-only after startup failure"
                    )));
                }
                Ok(())
            },
            Err(error) => {
                let mut state = lock_shared(&self.shared);
                drain_bridge(&mut state);
                self.startup_events
                    .extend(state.compatibility_events.drain(..));
                self.lifecycle = MacosLifecycle::Stopped;
                Err(SessionError::Start(error.message))
            },
        }
    }

    /// Whether the underlying Swift transaction reached capture before a
    /// successful start or rollback.
    #[must_use]
    pub fn has_started_capture(&self) -> bool {
        lock_shared(&self.shared).capture_started
    }

    /// Whether transcription is disabled and the session is retaining audio
    /// only, either from initial policy selection or runtime fallback.
    #[must_use]
    pub const fn is_record_only(&self) -> bool {
        self.record_only
    }

    /// Whether a terminal backend event has completed session cleanup.
    #[must_use]
    pub const fn is_stopped(&self) -> bool {
        matches!(self.lifecycle, MacosLifecycle::Stopped)
    }

    /// Take a terminal runtime failure observed before or during graceful
    /// shutdown. Ordinary user-requested stops leave this empty.
    pub fn take_runtime_failure(&mut self) -> Option<SessionError> {
        self.runtime_failure.take()
    }

    /// Replace microphone PCM with silence while retaining both timelines.
    pub fn set_microphone_muted(
        &self,
        muted: bool,
    ) {
        lock_shared(&self.shared).bridge.set_microphone_muted(muted);
    }

    /// Gracefully stop capture, Ogg writers, and `SpeechAnalyzer`, retaining all
    /// flushed final results for subsequent event drains.
    pub fn stop(&mut self) {
        if self.lifecycle == MacosLifecycle::Stopped {
            return;
        }
        if let Err(error) = self.shutdown_gracefully_bounded() {
            let message = format!("[FATAL] {error}");
            self.runtime_failure = Some(SessionError::Start(message.clone()));
            self.startup_events.push_back(Event::Log(message));
        }
    }

    /// Abort the session and discard buffered callback results.
    pub fn abort(&mut self) {
        if self.lifecycle == MacosLifecycle::Stopped {
            return;
        }
        self.startup_events.clear();
        self.lifecycle = MacosLifecycle::CleanupPending;
        if let Err(error) = self.shutdown_abort_bounded() {
            self.startup_events
                .push_back(Event::Log(format!("[FATAL] abort failed: {error}")));
        }
        let mut state = lock_shared(&self.shared);
        state.capture_events.clear();
        state.transcript_events.clear();
        state.compatibility_events.clear();
    }

    fn shutdown_abort_bounded(&mut self) -> BackendResult<()> {
        self.lifecycle = MacosLifecycle::CleanupPending;
        let mut last_error = None;
        for _ in 0..CLEANUP_RETRY_BUDGET {
            match self.orchestrator.shutdown(ShutdownMode::Abort) {
                Ok(()) => {
                    self.lifecycle = MacosLifecycle::Stopped;
                    return Ok(());
                },
                Err(error) => last_error = Some(error),
            }
        }
        Err(last_error.unwrap_or_else(|| {
            backend_error(
                CAPTURE_BACKEND_ID,
                BackendErrorKind::InvalidState,
                "abort cleanup remained incomplete",
            )
        }))
    }

    fn shutdown_gracefully_bounded(&mut self) -> Result<(), String> {
        self.lifecycle = MacosLifecycle::CleanupPending;
        let mut last_error = None;
        for _ in 0..CLEANUP_RETRY_BUDGET {
            match self.orchestrator.shutdown(ShutdownMode::Graceful) {
                Ok(()) => {
                    self.lifecycle = MacosLifecycle::Stopped;
                    return Ok(());
                },
                Err(error) => last_error = Some(error),
            }
        }

        let graceful_error = last_error.map_or_else(
            || "session cleanup remained incomplete".to_owned(),
            |error| error.to_string(),
        );
        match self.orchestrator.shutdown(ShutdownMode::Abort) {
            Ok(()) => {
                self.lifecycle = MacosLifecycle::Stopped;
                Err(format!(
                    "graceful shutdown exhausted {CLEANUP_RETRY_BUDGET} attempts: {graceful_error}; cleanup was aborted"
                ))
            },
            Err(abort_error) => Err(format!(
                "graceful shutdown exhausted {CLEANUP_RETRY_BUDGET} attempts: {graceful_error}; abort also failed: {abort_error}"
            )),
        }
    }

    /// Non-blocking event poll.
    #[must_use]
    pub fn try_recv(&mut self) -> Option<Event> {
        self.recv_timeout(Duration::ZERO)
    }

    /// Poll for a compatibility event through [`SessionOrchestrator`].
    #[must_use]
    #[allow(clippy::too_many_lines)]
    pub fn recv_timeout(
        &mut self,
        timeout: Duration,
    ) -> Option<Event> {
        if let Some(event) = self.startup_events.pop_front() {
            return Some(event);
        }
        if let Some(event) = lock_shared(&self.shared).compatibility_events.pop_front() {
            return Some(event);
        }
        if matches!(
            self.lifecycle,
            MacosLifecycle::Ready | MacosLifecycle::CleanupPending
        ) {
            return None;
        }

        let started_at = Instant::now();
        let mut first_poll = true;
        loop {
            let remaining = timeout.saturating_sub(started_at.elapsed());
            if !first_poll && remaining.is_zero() {
                return lock_shared(&self.shared).compatibility_events.pop_front();
            }
            first_poll = false;
            match self.orchestrator.pump_once(remaining) {
                Ok(Some(OrchestratorEvent::Capture(CaptureEvent::Samples(_)))) => {},
                Ok(Some(OrchestratorEvent::Capture(event))) => {
                    let terminal = matches!(
                        event,
                        CaptureEvent::Error {
                            recoverable: false,
                            ..
                        }
                    );
                    let output = capture_event_to_compat(event);
                    if terminal {
                        let _ = self.shutdown_abort_bounded();
                        self.runtime_failure = Some(SessionError::Start(match &output {
                            Event::Log(message) => message.clone(),
                            Event::Result(_) => "macOS capture failed".into(),
                        }));
                    }
                    return Some(output);
                },
                Ok(Some(OrchestratorEvent::Transcript(_))) => {
                    // The exact Swift callback is the compatibility facade's
                    // authoritative event. The correlated neutral transcript
                    // has already passed through the orchestrator and must not
                    // be remapped into a duplicate result.
                    if let Some(compatibility) =
                        lock_shared(&self.shared).compatibility_events.pop_front()
                    {
                        return Some(compatibility);
                    }
                },
                Ok(None) => {
                    return lock_shared(&self.shared).compatibility_events.pop_front();
                },
                Err(error)
                    if self.lifecycle == MacosLifecycle::Stopped
                        && error.kind == BackendErrorKind::InvalidState =>
                {
                    return None;
                },
                Err(error) if error.backend == BackendId::new(TRANSCRIBER_BACKEND_ID) => {
                    let failed_backend = BackendId::new(TRANSCRIBER_BACKEND_ID);
                    let speech_permission = { lock_shared(&self.shared).speech_permission };
                    let selection = crate::select_transcriber_after_failure(
                        self.policy,
                        &[macos_transcriber_probe(speech_permission)],
                        &failed_backend,
                    );
                    match selection {
                        TranscriptionSelection::RecordOnly { reason } => {
                            if let Err(disable_error) = self.orchestrator.disable_transcriber() {
                                let _ = self.shutdown_abort_bounded();
                                let message = format!(
                                    "[FATAL] could not enter record-only mode after {error}: {disable_error}"
                                );
                                self.runtime_failure = Some(SessionError::Start(message.clone()));
                                return Some(Event::Log(message));
                            }
                            let mut state = lock_shared(&self.shared);
                            state.bridge_transcription = BridgeTranscription::Disabled;
                            state.transcriber_started = false;
                            state.transcript_events.clear();
                            self.record_only = true;
                            return Some(Event::Log(format!(
                                "[ASR] {error}; continuing record-only ({reason})"
                            )));
                        },
                        TranscriptionSelection::Unavailable { reason } => {
                            let cleanup_error = self.shutdown_gracefully_bounded().err();
                            let mut message =
                                format!("[FATAL] transcription failed: {error}; {reason}");
                            if let Some(cleanup_error) = cleanup_error {
                                use std::fmt::Write as _;
                                let _ = write!(
                                    message,
                                    "; graceful cleanup remains incomplete: {cleanup_error}"
                                );
                            }
                            self.runtime_failure = Some(SessionError::Start(message.clone()));
                            return Some(Event::Log(message));
                        },
                        TranscriptionSelection::Backend(backend) => {
                            let cleanup_error = self.shutdown_gracefully_bounded().err();
                            let mut message = format!(
                                "[FATAL] runtime fallback backend is not connected: {backend}"
                            );
                            if let Some(cleanup_error) = cleanup_error {
                                use std::fmt::Write as _;
                                let _ = write!(
                                    message,
                                    "; graceful cleanup remains incomplete: {cleanup_error}"
                                );
                            }
                            self.runtime_failure = Some(SessionError::Start(message.clone()));
                            return Some(Event::Log(message));
                        },
                    }
                },
                Err(error) => {
                    let _ = self.shutdown_abort_bounded();
                    let message = format!("[FATAL] backend error: {error}");
                    self.runtime_failure = Some(SessionError::Start(message.clone()));
                    return Some(Event::Log(message));
                },
            }
        }
    }
}

fn make_shared_state(
    bridge: Box<dyn SwiftSessionBridge>,
    microphone_permission: PermissionStatus,
    speech_permission: PermissionStatus,
    speech_enabled_in_bridge: bool,
) -> Shared {
    Arc::new(Mutex::new(SharedState {
        bridge,
        capture_events: VecDeque::new(),
        transcript_events: VecDeque::new(),
        compatibility_events: VecDeque::new(),
        microphone_permission,
        speech_permission,
        bridge_transcription: if speech_enabled_in_bridge {
            BridgeTranscription::Enabled
        } else {
            BridgeTranscription::Disabled
        },
        capture_started: false,
        transcriber_started: false,
        transcriber_tracks: Default::default(),
        stopped: false,
    }))
}

fn macos_transcriber_probe(speech_permission: PermissionStatus) -> TranscriberProbe {
    TranscriberProbe {
        backend_id: BackendId::new(TRANSCRIBER_BACKEND_ID),
        class: TranscriberClass::Platform,
        availability: permission_availability(Permission::SpeechRecognition, speech_permission),
        capabilities: TranscriberCapabilities {
            privacy: RecognitionPrivacy::Offline,
            features: vec![
                TranscriberFeature::Streaming,
                TranscriberFeature::PartialResults,
                TranscriberFeature::SegmentTimestamps,
            ],
        },
    }
}

fn select_macos_transcriber(
    policy: crate::TranscriptionPolicy,
    speech_permission: PermissionStatus,
) -> TranscriptionSelection {
    select_transcriber(policy, &[macos_transcriber_probe(speech_permission)])
}

impl Drop for MacosSession {
    fn drop(&mut self) {
        // Cleanup progress is transactional, so each retry performs only the
        // unfinished native steps. Keep Drop bounded even for a persistently
        // failing bridge.
        for _ in 0..CLEANUP_RETRY_BUDGET {
            if self.orchestrator.shutdown(ShutdownMode::Abort).is_ok() {
                break;
            }
        }
    }
}

fn capture_event_to_compat(event: CaptureEvent) -> Event {
    match event {
        CaptureEvent::Samples(frame) => Event::Log(format!(
            "[AUDIO] captured {} frame(s) for track {}",
            frame.frame_count(),
            frame.track_id().get()
        )),
        CaptureEvent::Overflow {
            track_id,
            dropped_frames,
        } => Event::Log(format!(
            "[AUDIO] recoverable capture gap: dropped {dropped_frames} frame(s) for track {}",
            track_id.get()
        )),
        CaptureEvent::Error {
            message,
            recoverable,
            ..
        } => {
            if recoverable || message.starts_with("[FATAL]") {
                Event::Log(message)
            } else {
                Event::Log(format!("[FATAL] {message}"))
            }
        },
        _ => Event::Log("[AUDIO] unsupported capture event".into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wisp_core::MonotonicTimestamp;

    struct FakeBridge {
        start_result: crate::Result<()>,
        start_transcription_result: crate::Result<()>,
        disable_result: crate::Result<()>,
        stop_capture_results: Mutex<VecDeque<crate::Result<()>>>,
        finish_transcription_results: Mutex<VecDeque<crate::Result<()>>>,
        started_capture: bool,
        events: Mutex<VecDeque<Event>>,
        audio_events: Mutex<VecDeque<CaptureEvent>>,
        transcriber_failures: Mutex<VecDeque<crate::MacosTranscriberFailure>>,
        capture_failures: Mutex<VecDeque<crate::MacosCaptureFailure>>,
        stop_events: Mutex<VecDeque<Event>>,
        calls: Arc<Mutex<Vec<&'static str>>>,
        pushed_frames: Arc<Mutex<Vec<AudioFrame>>>,
        first_audio_timestamps: [Option<MonotonicTimestamp>; 2],
        block_audio_for_timeout: bool,
    }

    impl SwiftSessionBridge for FakeBridge {
        fn start_capture(&mut self) -> crate::Result<()> {
            lock_calls(&self.calls).push("start-capture");
            self.start_result.clone()
        }

        fn start_transcription(&self) -> crate::Result<()> {
            lock_calls(&self.calls).push("start-transcription");
            self.start_transcription_result.clone()
        }

        fn has_started_capture(&self) -> bool {
            self.started_capture
        }

        fn set_microphone_muted(
            &self,
            muted: bool,
        ) {
            lock_calls(&self.calls).push(if muted { "mute" } else { "unmute" });
        }

        fn stop_capture(&self) -> crate::Result<()> {
            lock_calls(&self.calls).push("stop-capture");
            self.stop_capture_results
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .pop_front()
                .unwrap_or(Ok(()))
        }

        fn finish_transcription(&self) -> crate::Result<()> {
            lock_calls(&self.calls).push("finish-transcription");
            let mut events = self
                .events
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let mut stop_events = self
                .stop_events
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            events.extend(stop_events.drain(..));
            self.finish_transcription_results
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .pop_front()
                .unwrap_or(Ok(()))
        }

        fn abort(&self) {
            lock_calls(&self.calls).push("abort");
        }

        fn push_transcriber_frame(
            &self,
            frame: &AudioFrame,
        ) -> crate::Result<()> {
            lock_calls(&self.calls).push("push-transcriber-frame");
            self.pushed_frames
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(frame.clone());
            Ok(())
        }

        fn disable_transcription(&self) -> crate::Result<()> {
            lock_calls(&self.calls).push("disable-transcription");
            self.disable_result.clone()
        }

        fn try_recv(&self) -> Option<Event> {
            self.events
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .pop_front()
        }

        fn try_recv_audio(&self) -> Option<CaptureEvent> {
            self.audio_events
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .pop_front()
        }

        fn try_recv_transcriber_failure(&self) -> Option<crate::MacosTranscriberFailure> {
            self.transcriber_failures
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .pop_front()
        }

        fn try_recv_capture_failure(&self) -> Option<crate::MacosCaptureFailure> {
            self.capture_failures
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .pop_front()
        }

        fn recv_audio_timeout(
            &self,
            timeout: Duration,
        ) -> Option<CaptureEvent> {
            let event = self.try_recv_audio();
            if event.is_none() && self.block_audio_for_timeout {
                lock_calls(&self.calls).push("recv-audio-wait");
                std::thread::sleep(timeout);
            }
            event
        }

        fn first_audio_timestamp(
            &self,
            track_id: TrackId,
        ) -> Option<MonotonicTimestamp> {
            transcriber_track_index(track_id).and_then(|index| self.first_audio_timestamps[index])
        }
    }

    fn lock_calls<'a>(
        calls: &'a Arc<Mutex<Vec<&'static str>>>
    ) -> MutexGuard<'a, Vec<&'static str>> {
        calls
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn fake_session(
        start_result: crate::Result<()>,
        started_capture: bool,
        events: impl IntoIterator<Item = Event>,
        stop_events: impl IntoIterator<Item = Event>,
        microphone_permission: PermissionStatus,
        speech_permission: PermissionStatus,
    ) -> (MacosSession, Arc<Mutex<Vec<&'static str>>>) {
        fake_session_with_audio(
            start_result,
            started_capture,
            events,
            [],
            stop_events,
            microphone_permission,
            speech_permission,
            true,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn fake_session_with_audio(
        start_result: crate::Result<()>,
        started_capture: bool,
        events: impl IntoIterator<Item = Event>,
        audio_events: impl IntoIterator<Item = CaptureEvent>,
        stop_events: impl IntoIterator<Item = Event>,
        microphone_permission: PermissionStatus,
        speech_permission: PermissionStatus,
        transcription_enabled: bool,
    ) -> (MacosSession, Arc<Mutex<Vec<&'static str>>>) {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let bridge = FakeBridge {
            start_result,
            start_transcription_result: Ok(()),
            disable_result: Ok(()),
            stop_capture_results: Mutex::new(VecDeque::new()),
            finish_transcription_results: Mutex::new(VecDeque::new()),
            started_capture,
            events: Mutex::new(events.into_iter().collect()),
            audio_events: Mutex::new(audio_events.into_iter().collect()),
            transcriber_failures: Mutex::new(VecDeque::new()),
            capture_failures: Mutex::new(VecDeque::new()),
            stop_events: Mutex::new(stop_events.into_iter().collect()),
            calls: Arc::clone(&calls),
            pushed_frames: Arc::new(Mutex::new(Vec::new())),
            first_audio_timestamps: [None; 2],
            block_audio_for_timeout: false,
        };
        (
            MacosSession::from_bridge(
                Box::new(bridge),
                microphone_permission,
                speech_permission,
                transcription_enabled,
                crate::TranscriptionPolicy::platform_default(),
            ),
            calls,
        )
    }

    fn fake_session_with_failures(
        failures: impl IntoIterator<Item = crate::MacosTranscriberFailure>,
        policy: crate::TranscriptionPolicy,
    ) -> (MacosSession, Arc<Mutex<Vec<&'static str>>>) {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let bridge = FakeBridge {
            start_result: Ok(()),
            start_transcription_result: Ok(()),
            disable_result: Ok(()),
            stop_capture_results: Mutex::new(VecDeque::new()),
            finish_transcription_results: Mutex::new(VecDeque::new()),
            started_capture: false,
            events: Mutex::new(VecDeque::new()),
            audio_events: Mutex::new(VecDeque::new()),
            transcriber_failures: Mutex::new(failures.into_iter().collect()),
            capture_failures: Mutex::new(VecDeque::new()),
            stop_events: Mutex::new(VecDeque::new()),
            calls: Arc::clone(&calls),
            pushed_frames: Arc::new(Mutex::new(Vec::new())),
            first_audio_timestamps: [None; 2],
            block_audio_for_timeout: false,
        };
        (
            MacosSession::from_bridge(
                Box::new(bridge),
                PermissionStatus::Granted,
                PermissionStatus::Granted,
                true,
                policy,
            ),
            calls,
        )
    }

    fn result(
        source: SourceLabel,
        is_final: bool,
        text: &str,
    ) -> Event {
        Event::Result(SessionResult {
            source,
            segment_id: 7,
            is_final,
            text: text.into(),
            start_seconds: 1.25,
            end_seconds: 2.75,
            confidence_mean: Some(0.8),
            confidence_min: Some(0.5),
        })
    }

    fn audio_frame(
        source: SourceLabel,
        sequence: u64,
    ) -> AudioFrame {
        AudioFrame::from_f32(
            source.track_id(),
            source.source_kind(),
            sequence,
            MonotonicTimestamp::from_duration(Duration::from_millis(sequence * 10)),
            48_000,
            1,
            vec![0.25; 480],
        )
        .expect("valid fake audio")
    }

    #[test]
    fn permission_denial_prevents_bridge_start() {
        let (mut session, calls) = fake_session(
            Ok(()),
            false,
            [],
            [],
            PermissionStatus::Denied,
            PermissionStatus::Granted,
        );

        let error = session.start().unwrap_err();

        assert!(error.to_string().contains("microphone permission"));
        assert!(lock_calls(&calls).is_empty());
    }

    #[test]
    fn failed_capture_start_rolls_back_without_fabricating_transcriber_final() {
        let (mut session, calls) = fake_session(
            Err(SessionError::Start("system tap unavailable".into())),
            true,
            [],
            [result(SourceLabel::Mic, true, "flushed")],
            PermissionStatus::Granted,
            PermissionStatus::Granted,
        );

        assert!(session.start().is_err());
        assert!(session.has_started_capture());
        assert_eq!(*lock_calls(&calls), ["start-capture", "stop-capture"]);
        assert!(session.try_recv().is_none());
    }

    #[test]
    fn failed_start_preserves_original_detail_without_duplicate_wrapper() {
        let (mut session, _calls) = fake_session(
            Err(SessionError::Start("system tap unavailable".into())),
            false,
            [],
            [],
            PermissionStatus::Granted,
            PermissionStatus::Granted,
        );

        let SessionError::Start(detail) = session.start().unwrap_err() else {
            panic!("expected start error");
        };
        assert_eq!(detail, "system tap unavailable");
    }

    #[test]
    fn live_results_retain_callback_order_identity_and_metadata() {
        let events = [
            result(SourceLabel::Mic, false, "mic draft"),
            result(SourceLabel::System, false, "system draft"),
            result(SourceLabel::Mic, true, "mic final"),
            result(SourceLabel::System, true, "system final"),
        ];
        let (mut session, _calls) = fake_session(
            Ok(()),
            false,
            events,
            [],
            PermissionStatus::Granted,
            PermissionStatus::Granted,
        );
        session.start().unwrap();

        let received = (0..4)
            .map(|_| {
                session
                    .recv_timeout(Duration::from_millis(10))
                    .expect("result")
            })
            .collect::<Vec<_>>();
        let results = received
            .iter()
            .map(|event| match event {
                Event::Result(result) => result,
                Event::Log(message) => panic!("unexpected log: {message}"),
            })
            .collect::<Vec<_>>();
        assert_eq!(
            results
                .iter()
                .map(|result| result.text.as_str())
                .collect::<Vec<_>>(),
            ["mic draft", "system draft", "mic final", "system final"]
        );
        assert_eq!(results[0].source.track_id(), TrackId::MICROPHONE);
        assert_eq!(results[1].source.track_id(), TrackId::SYSTEM);
        assert!(!results[0].is_final);
        assert!(results[2].is_final);
        assert!((results[3].start_seconds - 1.25).abs() < f64::EPSILON);
        assert!((results[3].end_seconds - 2.75).abs() < f64::EPSILON);
        assert_eq!(results[3].confidence_mean, Some(0.8));
        assert_eq!(results[3].confidence_min, Some(0.5));
        assert!(
            session.try_recv().is_none(),
            "each live Swift result must be delivered exactly once"
        );
    }

    #[test]
    fn capture_frames_are_observable_and_forwarded_through_orchestrator() {
        let frame = audio_frame(SourceLabel::Mic, 3);
        let (mut session, calls) = fake_session_with_audio(
            Ok(()),
            false,
            [],
            [CaptureEvent::Samples(frame.clone())],
            [],
            PermissionStatus::Granted,
            PermissionStatus::Granted,
            true,
        );
        session.start().unwrap();

        assert_eq!(
            session.orchestrator.pump_once(Duration::ZERO).unwrap(),
            Some(OrchestratorEvent::Capture(CaptureEvent::Samples(frame)))
        );
        assert_eq!(
            *lock_calls(&calls),
            [
                "start-capture",
                "start-transcription",
                "push-transcriber-frame"
            ],
            "CaptureBackend -> orchestrator -> TranscriberBackend::push must reach Swift"
        );
    }

    #[test]
    fn leading_overflow_uses_nonzero_native_track_anchor_without_silence() {
        let frame = audio_frame(SourceLabel::Mic, 42);
        assert!(frame.timestamp().as_duration() > Duration::ZERO);
        let pushed_frames = Arc::new(Mutex::new(Vec::new()));
        let calls = Arc::new(Mutex::new(Vec::new()));
        let bridge = FakeBridge {
            start_result: Ok(()),
            start_transcription_result: Ok(()),
            disable_result: Ok(()),
            stop_capture_results: Mutex::new(VecDeque::new()),
            finish_transcription_results: Mutex::new(VecDeque::new()),
            started_capture: true,
            events: Mutex::new(VecDeque::new()),
            audio_events: Mutex::new(VecDeque::from([
                CaptureEvent::Overflow {
                    track_id: TrackId::MICROPHONE,
                    dropped_frames: 9_600,
                },
                CaptureEvent::Samples(frame.clone()),
            ])),
            transcriber_failures: Mutex::new(VecDeque::new()),
            capture_failures: Mutex::new(VecDeque::new()),
            stop_events: Mutex::new(VecDeque::new()),
            calls,
            pushed_frames: Arc::clone(&pushed_frames),
            first_audio_timestamps: [Some(frame.timestamp()), None],
            block_audio_for_timeout: false,
        };
        let mut session = MacosSession::from_bridge(
            Box::new(bridge),
            PermissionStatus::Granted,
            PermissionStatus::Granted,
            true,
            crate::TranscriptionPolicy::platform_default(),
        );
        session.start().unwrap();

        assert!(matches!(
            session.orchestrator.pump_once(Duration::ZERO).unwrap(),
            Some(OrchestratorEvent::Capture(CaptureEvent::Overflow {
                dropped_frames: 9_600,
                ..
            }))
        ));
        assert_eq!(
            session.orchestrator.pump_once(Duration::ZERO).unwrap(),
            Some(OrchestratorEvent::Capture(CaptureEvent::Samples(
                frame.clone()
            )))
        );
        assert_eq!(
            *pushed_frames.lock().unwrap(),
            [frame],
            "leading elapsed time is already represented by trackStartSeconds"
        );
    }

    #[test]
    fn rust_queue_rejection_before_first_pcm_inserts_only_post_anchor_silence() {
        let frame = audio_frame(SourceLabel::Mic, 42);
        let first_rust_callback = MonotonicTimestamp::from_duration(
            frame
                .timestamp()
                .as_duration()
                .checked_sub(Duration::from_millis(100))
                .expect("test frame starts after the simulated leading rejection"),
        );
        let pushed_frames = Arc::new(Mutex::new(Vec::new()));
        let calls = Arc::new(Mutex::new(Vec::new()));
        let bridge = FakeBridge {
            start_result: Ok(()),
            start_transcription_result: Ok(()),
            disable_result: Ok(()),
            stop_capture_results: Mutex::new(VecDeque::new()),
            finish_transcription_results: Mutex::new(VecDeque::new()),
            started_capture: true,
            events: Mutex::new(VecDeque::new()),
            audio_events: Mutex::new(VecDeque::from([
                CaptureEvent::Overflow {
                    track_id: TrackId::MICROPHONE,
                    dropped_frames: 4_800,
                },
                CaptureEvent::Samples(frame.clone()),
            ])),
            transcriber_failures: Mutex::new(VecDeque::new()),
            capture_failures: Mutex::new(VecDeque::new()),
            stop_events: Mutex::new(VecDeque::new()),
            calls,
            pushed_frames: Arc::clone(&pushed_frames),
            first_audio_timestamps: [Some(first_rust_callback), None],
            block_audio_for_timeout: false,
        };
        let mut session = MacosSession::from_bridge(
            Box::new(bridge),
            PermissionStatus::Granted,
            PermissionStatus::Granted,
            true,
            crate::TranscriptionPolicy::platform_default(),
        );
        session.start().unwrap();

        let _ = session.orchestrator.pump_once(Duration::ZERO).unwrap();
        let _ = session.orchestrator.pump_once(Duration::ZERO).unwrap();

        let pushed = pushed_frames
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        assert_eq!(pushed.len(), 3);
        assert_eq!(
            pushed[..2]
                .iter()
                .map(|silence| u64::from(silence.frame_count()))
                .sum::<u64>(),
            4_800
        );
        assert!(pushed[..2].iter().all(|silence| {
            silence
                .samples()
                .as_f32()
                .is_some_and(|samples| samples.iter().all(|sample| *sample == 0.0))
        }));
        assert_eq!(pushed[2], frame);
    }

    #[test]
    fn speech_denial_after_capture_start_falls_back_and_discards_results() {
        let (mut session, calls) = fake_session(
            Ok(()),
            false,
            [result(SourceLabel::Mic, true, "discarded")],
            [],
            PermissionStatus::Granted,
            PermissionStatus::Denied,
        );

        session.start().unwrap();
        assert!(session.has_started_capture());
        assert!(session.is_record_only());
        assert!(matches!(
            session.try_recv(),
            Some(Event::Log(message)) if message.contains("continuing record-only")
        ));
        assert!(session.try_recv().is_none());
        session.stop();
        drop(session);
        assert_eq!(
            *lock_calls(&calls),
            ["start-capture", "disable-transcription", "stop-capture"]
        );
    }

    #[test]
    fn record_only_starts_without_speech_permission() {
        let (mut session, calls) = fake_session_with_audio(
            Ok(()),
            false,
            [],
            [],
            [],
            PermissionStatus::Granted,
            PermissionStatus::Denied,
            false,
        );

        session.start().unwrap();
        session.stop();
        assert_eq!(*lock_calls(&calls), ["start-capture", "stop-capture"]);
    }

    #[test]
    fn recording_only_capture_failure_is_not_misclassified_by_speech_denial() {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let bridge = FakeBridge {
            start_result: Err(SessionError::Start("system tap unavailable".into())),
            start_transcription_result: Ok(()),
            disable_result: Ok(()),
            stop_capture_results: Mutex::new(VecDeque::new()),
            finish_transcription_results: Mutex::new(VecDeque::new()),
            started_capture: false,
            events: Mutex::new(VecDeque::new()),
            audio_events: Mutex::new(VecDeque::new()),
            transcriber_failures: Mutex::new(VecDeque::new()),
            capture_failures: Mutex::new(VecDeque::new()),
            stop_events: Mutex::new(VecDeque::new()),
            calls,
            pushed_frames: Arc::new(Mutex::new(Vec::new())),
            first_audio_timestamps: [None; 2],
            block_audio_for_timeout: false,
        };
        let shared = make_shared_state(
            Box::new(bridge),
            PermissionStatus::Granted,
            PermissionStatus::Denied,
            false,
        );
        let mut capture = MacosCaptureBackend::new(shared);

        let error = capture.start().unwrap_err();

        assert_eq!(error.kind, BackendErrorKind::Internal);
        assert_eq!(error.message, "system tap unavailable");
    }

    #[test]
    fn terminal_runtime_failure_falls_back_to_record_only_when_allowed() {
        let failure = crate::MacosTranscriberFailure {
            terminal: true,
            message: "analyzer terminated".into(),
        };
        let (mut session, calls) =
            fake_session_with_failures([failure], crate::TranscriptionPolicy::platform_default());
        session.start().unwrap();

        let event = session
            .recv_timeout(Duration::from_millis(10))
            .expect("record-only notice");

        assert!(matches!(event, Event::Log(message) if message.contains("continuing record-only")));
        assert!(session.is_record_only());
        session.stop();
        assert_eq!(
            *lock_calls(&calls),
            [
                "start-capture",
                "start-transcription",
                "disable-transcription",
                "stop-capture"
            ]
        );
    }

    #[test]
    fn terminal_runtime_failure_is_fatal_when_record_only_is_forbidden() {
        let failure = crate::MacosTranscriberFailure {
            terminal: true,
            message: "analyzer terminated".into(),
        };
        let policy = crate::TranscriptionPolicy {
            allow_record_only: false,
            ..crate::TranscriptionPolicy::platform_default()
        };
        let (mut session, calls) = fake_session_with_failures([failure], policy);
        session.start().unwrap();

        let event = session
            .recv_timeout(Duration::from_millis(10))
            .expect("fatal notice");

        assert!(matches!(event, Event::Log(message) if message.starts_with("[FATAL]")));
        assert!(!session.is_record_only());
        assert_eq!(
            *lock_calls(&calls),
            [
                "start-capture",
                "start-transcription",
                "stop-capture",
                "finish-transcription"
            ]
        );
    }

    #[test]
    fn recoverable_transcriber_gap_does_not_disable_transcription() {
        let failure = crate::MacosTranscriberFailure {
            terminal: false,
            message: "input queue overflow".into(),
        };
        let (mut session, _calls) =
            fake_session_with_failures([failure], crate::TranscriptionPolicy::platform_default());
        session.start().unwrap();

        let event = session
            .recv_timeout(Duration::from_millis(10))
            .expect("recoverable notice");

        assert!(matches!(event, Event::Log(message) if message.contains("recoverable")));
        assert!(!session.is_record_only());
    }

    #[test]
    fn policy_selects_platform_record_only_or_unavailable_explicitly() {
        assert!(matches!(
            select_macos_transcriber(
                crate::TranscriptionPolicy::platform_default(),
                PermissionStatus::Granted
            ),
            TranscriptionSelection::Backend(backend)
                if backend == BackendId::new(TRANSCRIBER_BACKEND_ID)
        ));
        assert!(matches!(
            select_macos_transcriber(
                crate::TranscriptionPolicy::platform_default(),
                PermissionStatus::Denied
            ),
            TranscriptionSelection::RecordOnly { .. }
        ));
        let strict = crate::TranscriptionPolicy {
            allow_record_only: false,
            ..crate::TranscriptionPolicy::platform_default()
        };
        assert!(matches!(
            select_macos_transcriber(strict, PermissionStatus::Denied),
            TranscriptionSelection::Unavailable { .. }
        ));
        let no_fallback_local = crate::TranscriptionPolicy {
            preferred: TranscriberClass::LocalModel,
            allow_backend_fallback: false,
            ..crate::TranscriptionPolicy::offline_local_model()
        };
        assert!(matches!(
            select_macos_transcriber(no_fallback_local, PermissionStatus::Granted),
            TranscriptionSelection::RecordOnly { .. }
        ));
    }

    #[test]
    fn duplicate_start_keeps_live_session_stoppable() {
        let (mut session, calls) = fake_session(
            Ok(()),
            false,
            [],
            [],
            PermissionStatus::Granted,
            PermissionStatus::Granted,
        );
        session.start().unwrap();

        assert!(
            session
                .start()
                .unwrap_err()
                .to_string()
                .contains("already started")
        );
        session.stop();
        assert_eq!(
            *lock_calls(&calls),
            [
                "start-capture",
                "start-transcription",
                "stop-capture",
                "finish-transcription"
            ]
        );
        assert!(session.try_recv().is_none());
    }

    #[test]
    fn graceful_drain_preserves_mixed_result_and_log_order() {
        let final_gap = "[FATAL] transcription gap: dropped 2 final result(s)";
        let (mut session, _calls) = fake_session(
            Ok(()),
            false,
            [],
            [
                result(SourceLabel::Mic, true, "first"),
                Event::Log("between".into()),
                Event::Log(final_gap.into()),
                result(SourceLabel::System, true, "last"),
            ],
            PermissionStatus::Granted,
            PermissionStatus::Granted,
        );
        session.start().unwrap();
        session.stop();

        let events = std::iter::from_fn(|| session.try_recv()).collect::<Vec<_>>();
        assert_eq!(events.len(), 4, "graceful events must be delivered once");
        assert!(matches!(&events[0], Event::Result(result) if result.text == "first"));
        assert_eq!(events[1], Event::Log("between".into()));
        assert_eq!(events[2], Event::Log(final_gap.into()));
        assert!(matches!(&events[3], Event::Result(result) if result.text == "last"));
    }

    #[test]
    fn graceful_stop_drains_final_with_stable_track_identity_and_timestamps() {
        let (mut session, calls) = fake_session(
            Ok(()),
            false,
            [],
            [
                result(SourceLabel::System, false, "draft"),
                result(SourceLabel::System, true, "final"),
            ],
            PermissionStatus::Granted,
            PermissionStatus::Granted,
        );
        session.start().unwrap();
        session.stop();

        let partial = session.try_recv().expect("partial");
        let final_event = session.try_recv().expect("final");
        let (Event::Result(partial), Event::Result(final_event)) = (partial, final_event) else {
            panic!("expected transcript results");
        };
        assert_eq!(partial.source.track_id(), TrackId::SYSTEM);
        assert_eq!(partial.segment_id, final_event.segment_id);
        assert!((partial.start_seconds - 1.25).abs() < f64::EPSILON);
        assert!((final_event.end_seconds - 2.75).abs() < f64::EPSILON);
        assert!(!partial.is_final);
        assert!(final_event.is_final);
        assert_eq!(
            *lock_calls(&calls),
            [
                "start-capture",
                "start-transcription",
                "stop-capture",
                "finish-transcription"
            ]
        );
        assert!(
            session.try_recv().is_none(),
            "graceful final results must be exhausted after exact delivery"
        );
    }

    #[test]
    fn abort_discards_buffered_results_and_mute_reaches_swift_bridge() {
        let (mut session, calls) = fake_session(
            Ok(()),
            false,
            [],
            [result(SourceLabel::Mic, true, "discard me")],
            PermissionStatus::Granted,
            PermissionStatus::Granted,
        );
        session.start().unwrap();
        session.set_microphone_muted(true);
        session.abort();

        assert!(session.try_recv().is_none());
        assert_eq!(
            *lock_calls(&calls),
            [
                "start-capture",
                "start-transcription",
                "mute",
                "abort",
                "disable-transcription"
            ]
        );
    }

    #[test]
    fn callback_overflow_gap_remains_fatal_through_orchestrator() {
        let message = "[FATAL] transcription gap: dropped 3 final result(s)";
        let (mut session, _calls) = fake_session(
            Ok(()),
            false,
            [Event::Log(message.into())],
            [],
            PermissionStatus::Granted,
            PermissionStatus::Granted,
        );
        session.start().unwrap();

        assert_eq!(session.try_recv(), Some(Event::Log(message.into())));
    }

    #[test]
    fn terminal_capture_error_aborts_lifecycle_and_surfaces_once() {
        let (mut session, calls) = fake_session_with_audio(
            Ok(()),
            false,
            [],
            [CaptureEvent::Error {
                track_id: Some(TrackId::SYSTEM),
                message: "process tap disconnected".into(),
                recoverable: false,
            }],
            [],
            PermissionStatus::Granted,
            PermissionStatus::Granted,
            true,
        );
        session.start().unwrap();

        assert_eq!(
            session.try_recv(),
            Some(Event::Log("[FATAL] process tap disconnected".into()))
        );
        assert!(session.is_stopped());
        assert!(session.try_recv().is_none());
        assert_eq!(
            *lock_calls(&calls),
            [
                "start-capture",
                "start-transcription",
                "abort",
                "disable-transcription"
            ]
        );
    }

    #[test]
    fn ordinary_status_log_is_not_misreported_as_capture_failure() {
        let (session, _calls) = fake_session_with_audio(
            Ok(()),
            false,
            [Event::Log("[MIC] engine started".into())],
            [],
            [],
            PermissionStatus::Granted,
            PermissionStatus::Granted,
            false,
        );
        let mut capture = MacosCaptureBackend::new(Arc::clone(&session.shared));
        capture.start().unwrap();

        assert_eq!(capture.next_event(Duration::ZERO).unwrap(), None);
        assert_eq!(
            lock_shared(&session.shared)
                .compatibility_events
                .pop_front(),
            Some(Event::Log("[MIC] engine started".into()))
        );
    }

    #[test]
    fn probes_publish_separate_microphone_and_system_tracks() {
        let (session, _calls) = fake_session(
            Ok(()),
            false,
            [],
            [],
            PermissionStatus::Granted,
            PermissionStatus::Granted,
        );
        let capture = MacosCaptureBackend::new(Arc::clone(&session.shared));
        let transcriber = MacosTranscriberBackend::new(Arc::clone(&session.shared));

        let capture_probe = capture.probe();
        assert_eq!(capture_probe.capabilities.tracks, tracks());
        assert!(capture_probe.capabilities.simultaneous_tracks);
        let transcriber_probe = transcriber.probe();
        assert_eq!(transcriber_probe.class, TranscriberClass::Platform);
        assert_eq!(
            transcriber_probe.capabilities.privacy,
            RecognitionPrivacy::Offline
        );
    }

    #[test]
    fn native_transcriber_start_failure_retains_capture_when_record_only_is_allowed() {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let bridge = FakeBridge {
            start_result: Ok(()),
            start_transcription_result: Err(SessionError::Start(
                "SpeechAnalyzer model setup failed".into(),
            )),
            disable_result: Ok(()),
            stop_capture_results: Mutex::new(VecDeque::new()),
            finish_transcription_results: Mutex::new(VecDeque::new()),
            started_capture: true,
            events: Mutex::new(VecDeque::new()),
            audio_events: Mutex::new(VecDeque::new()),
            transcriber_failures: Mutex::new(VecDeque::new()),
            capture_failures: Mutex::new(VecDeque::new()),
            stop_events: Mutex::new(VecDeque::new()),
            calls: Arc::clone(&calls),
            pushed_frames: Arc::new(Mutex::new(Vec::new())),
            first_audio_timestamps: [None; 2],
            block_audio_for_timeout: false,
        };
        let mut session = MacosSession::from_bridge(
            Box::new(bridge),
            PermissionStatus::Granted,
            PermissionStatus::Granted,
            true,
            crate::TranscriptionPolicy::platform_default(),
        );

        session.start().unwrap();
        assert!(session.is_record_only());
        assert!(session.has_started_capture());
        assert!(matches!(
            session.try_recv(),
            Some(Event::Log(message)) if message.contains("continuing record-only")
        ));
        session.stop();
        assert_eq!(
            *lock_calls(&calls),
            [
                "start-capture",
                "start-transcription",
                "disable-transcription",
                "stop-capture"
            ]
        );
    }

    #[test]
    fn native_transcriber_start_failure_aborts_capture_when_record_only_is_forbidden() {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let bridge = FakeBridge {
            start_result: Ok(()),
            start_transcription_result: Err(SessionError::Start(
                "SpeechAnalyzer model setup failed".into(),
            )),
            disable_result: Ok(()),
            stop_capture_results: Mutex::new(VecDeque::new()),
            finish_transcription_results: Mutex::new(VecDeque::new()),
            started_capture: true,
            events: Mutex::new(VecDeque::new()),
            audio_events: Mutex::new(VecDeque::new()),
            transcriber_failures: Mutex::new(VecDeque::new()),
            capture_failures: Mutex::new(VecDeque::new()),
            stop_events: Mutex::new(VecDeque::new()),
            calls: Arc::clone(&calls),
            pushed_frames: Arc::new(Mutex::new(Vec::new())),
            first_audio_timestamps: [None; 2],
            block_audio_for_timeout: false,
        };
        let strict = crate::TranscriptionPolicy {
            allow_record_only: false,
            ..crate::TranscriptionPolicy::platform_default()
        };
        let mut session = MacosSession::from_bridge(
            Box::new(bridge),
            PermissionStatus::Granted,
            PermissionStatus::Granted,
            true,
            strict,
        );

        assert!(session.start().is_err());
        assert!(!session.is_record_only());
        assert_eq!(
            *lock_calls(&calls),
            [
                "start-capture",
                "start-transcription",
                "abort",
                "disable-transcription"
            ]
        );
    }

    #[test]
    fn graceful_shutdown_pushes_tail_pcm_before_finishing_transcriber() {
        let frame = audio_frame(SourceLabel::Mic, 42);
        let (mut session, calls) = fake_session_with_audio(
            Ok(()),
            false,
            [],
            [CaptureEvent::Samples(frame)],
            [],
            PermissionStatus::Granted,
            PermissionStatus::Granted,
            true,
        );
        session.start().unwrap();
        session.stop();

        assert_eq!(
            *lock_calls(&calls),
            [
                "start-capture",
                "start-transcription",
                "stop-capture",
                "push-transcriber-frame",
                "finish-transcription"
            ]
        );
    }

    #[test]
    fn terminal_capture_error_discovered_during_graceful_drain_is_runtime_failure() {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let bridge = FakeBridge {
            start_result: Ok(()),
            start_transcription_result: Ok(()),
            disable_result: Ok(()),
            stop_capture_results: Mutex::new(VecDeque::new()),
            finish_transcription_results: Mutex::new(VecDeque::new()),
            started_capture: true,
            events: Mutex::new(VecDeque::new()),
            audio_events: Mutex::new(VecDeque::new()),
            transcriber_failures: Mutex::new(VecDeque::new()),
            capture_failures: Mutex::new(VecDeque::from([crate::MacosCaptureFailure {
                track_id: Some(TrackId::SYSTEM),
                message: "recorder failed while draining".into(),
            }])),
            stop_events: Mutex::new(VecDeque::new()),
            calls,
            pushed_frames: Arc::new(Mutex::new(Vec::new())),
            first_audio_timestamps: [None; 2],
            block_audio_for_timeout: false,
        };
        let mut session = MacosSession::from_bridge(
            Box::new(bridge),
            PermissionStatus::Granted,
            PermissionStatus::Granted,
            true,
            crate::TranscriptionPolicy::platform_default(),
        );
        session.start().unwrap();

        session.stop();
        assert_eq!(
            session.try_recv(),
            Some(Event::Log("[FATAL] recorder failed while draining".into()))
        );
        let failure = session
            .take_runtime_failure()
            .expect("graceful drain failure must not become an ordinary stop");
        assert!(
            failure
                .to_string()
                .contains("recorder failed while draining")
        );
    }

    #[test]
    fn strict_runtime_transcriber_failure_flushes_final_and_retries_only_finish_phase() {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let final_event = result(SourceLabel::Mic, true, "final from failed cleanup");
        let bridge = FakeBridge {
            start_result: Ok(()),
            start_transcription_result: Ok(()),
            disable_result: Ok(()),
            stop_capture_results: Mutex::new(VecDeque::new()),
            finish_transcription_results: Mutex::new(VecDeque::from([
                Err(SessionError::Start("native finish failed".into())),
                Ok(()),
            ])),
            started_capture: true,
            events: Mutex::new(VecDeque::new()),
            audio_events: Mutex::new(VecDeque::new()),
            transcriber_failures: Mutex::new(VecDeque::from([crate::MacosTranscriberFailure {
                terminal: true,
                message: "strict recognizer failure".into(),
            }])),
            capture_failures: Mutex::new(VecDeque::new()),
            stop_events: Mutex::new(VecDeque::from([final_event.clone()])),
            calls: Arc::clone(&calls),
            pushed_frames: Arc::new(Mutex::new(Vec::new())),
            first_audio_timestamps: [None; 2],
            block_audio_for_timeout: false,
        };
        let strict = crate::TranscriptionPolicy {
            allow_record_only: false,
            ..crate::TranscriptionPolicy::platform_default()
        };
        let mut session = MacosSession::from_bridge(
            Box::new(bridge),
            PermissionStatus::Granted,
            PermissionStatus::Granted,
            true,
            strict,
        );
        session.start().unwrap();

        assert!(matches!(
            session.recv_timeout(Duration::ZERO),
            Some(Event::Log(message)) if message.contains("strict recognizer failure")
        ));
        assert!(session.is_stopped());
        let runtime_failure = session
            .take_runtime_failure()
            .expect("strict transcriber failure must be terminal");
        assert!(
            runtime_failure
                .to_string()
                .contains("strict recognizer failure")
        );
        assert!(
            !runtime_failure.to_string().contains("native finish failed"),
            "a transient cleanup error resolved inside the retry budget is not terminal"
        );
        assert_eq!(session.try_recv(), Some(final_event));
        assert_eq!(
            *lock_calls(&calls),
            [
                "start-capture",
                "start-transcription",
                "stop-capture",
                "finish-transcription",
                "finish-transcription",
            ]
        );

        session
            .orchestrator
            .shutdown(ShutdownMode::Graceful)
            .unwrap();
        assert_eq!(
            *lock_calls(&calls),
            [
                "start-capture",
                "start-transcription",
                "stop-capture",
                "finish-transcription",
                "finish-transcription",
            ],
            "completed capture stop/drain must not be repeated when finish retries"
        );
    }

    #[test]
    fn failed_capture_stop_retries_before_finish_without_repeating_completed_work() {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let bridge = FakeBridge {
            start_result: Ok(()),
            start_transcription_result: Ok(()),
            disable_result: Ok(()),
            stop_capture_results: Mutex::new(VecDeque::from([
                Err(SessionError::Start("native capture stop failed".into())),
                Ok(()),
            ])),
            finish_transcription_results: Mutex::new(VecDeque::new()),
            started_capture: true,
            events: Mutex::new(VecDeque::new()),
            audio_events: Mutex::new(VecDeque::new()),
            transcriber_failures: Mutex::new(VecDeque::new()),
            capture_failures: Mutex::new(VecDeque::new()),
            stop_events: Mutex::new(VecDeque::new()),
            calls: Arc::clone(&calls),
            pushed_frames: Arc::new(Mutex::new(Vec::new())),
            first_audio_timestamps: [None; 2],
            block_audio_for_timeout: false,
        };
        let mut session = MacosSession::from_bridge(
            Box::new(bridge),
            PermissionStatus::Granted,
            PermissionStatus::Granted,
            true,
            crate::TranscriptionPolicy::platform_default(),
        );
        session.start().unwrap();

        session.stop();
        assert!(session.is_stopped());
        assert!(session.take_runtime_failure().is_none());
        assert_eq!(
            *lock_calls(&calls),
            [
                "start-capture",
                "start-transcription",
                "stop-capture",
                "stop-capture",
                "finish-transcription",
            ]
        );

        session
            .orchestrator
            .shutdown(ShutdownMode::Graceful)
            .unwrap();
        assert_eq!(
            *lock_calls(&calls),
            [
                "start-capture",
                "start-transcription",
                "stop-capture",
                "stop-capture",
                "finish-transcription",
            ]
        );
    }

    #[test]
    fn persistent_graceful_finish_aborts_only_after_budget_and_preserves_final() {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let final_event = result(SourceLabel::Mic, true, "final before bounded abort");
        let bridge = FakeBridge {
            start_result: Ok(()),
            start_transcription_result: Ok(()),
            disable_result: Ok(()),
            stop_capture_results: Mutex::new(VecDeque::new()),
            finish_transcription_results: Mutex::new(VecDeque::from([
                Err(SessionError::Start("finish failed once".into())),
                Err(SessionError::Start("finish failed twice".into())),
                Err(SessionError::Start("finish failed persistently".into())),
            ])),
            started_capture: true,
            events: Mutex::new(VecDeque::new()),
            audio_events: Mutex::new(VecDeque::new()),
            transcriber_failures: Mutex::new(VecDeque::new()),
            capture_failures: Mutex::new(VecDeque::new()),
            stop_events: Mutex::new(VecDeque::from([final_event.clone()])),
            calls: Arc::clone(&calls),
            pushed_frames: Arc::new(Mutex::new(Vec::new())),
            first_audio_timestamps: [None; 2],
            block_audio_for_timeout: false,
        };
        let mut session = MacosSession::from_bridge(
            Box::new(bridge),
            PermissionStatus::Granted,
            PermissionStatus::Granted,
            true,
            crate::TranscriptionPolicy::platform_default(),
        );
        session.start().unwrap();

        session.stop();

        assert!(session.is_stopped());
        assert!(matches!(
            session.try_recv(),
            Some(Event::Log(message))
                if message.contains("exhausted 3 attempts")
                    && message.contains("cleanup was aborted")
        ));
        assert_eq!(session.try_recv(), Some(final_event));
        assert_eq!(
            *lock_calls(&calls),
            [
                "start-capture",
                "start-transcription",
                "stop-capture",
                "finish-transcription",
                "finish-transcription",
                "finish-transcription",
                "disable-transcription",
            ]
        );
    }

    #[test]
    fn legacy_blocking_receive_allows_prompt_stop_and_keeps_terminal_final() {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let final_event = result(SourceLabel::Mic, true, "final from concurrent stop");
        let bridge = FakeBridge {
            start_result: Ok(()),
            start_transcription_result: Ok(()),
            disable_result: Ok(()),
            stop_capture_results: Mutex::new(VecDeque::new()),
            finish_transcription_results: Mutex::new(VecDeque::new()),
            started_capture: true,
            events: Mutex::new(VecDeque::new()),
            audio_events: Mutex::new(VecDeque::new()),
            transcriber_failures: Mutex::new(VecDeque::new()),
            capture_failures: Mutex::new(VecDeque::new()),
            stop_events: Mutex::new(VecDeque::from([final_event.clone()])),
            calls: Arc::clone(&calls),
            pushed_frames: Arc::new(Mutex::new(Vec::new())),
            first_audio_timestamps: [None; 2],
            block_audio_for_timeout: true,
        };
        let macos = MacosSession::from_bridge(
            Box::new(bridge),
            PermissionStatus::Granted,
            PermissionStatus::Granted,
            true,
            crate::TranscriptionPolicy::platform_default(),
        );
        let mut legacy = crate::Session::from_macos(macos);
        legacy.start().unwrap();
        let legacy = Arc::new(legacy);
        let waiting_session = Arc::clone(&legacy);
        let waiting =
            std::thread::spawn(move || waiting_session.recv_timeout(Duration::from_secs(1)));

        let wait_deadline = Instant::now() + Duration::from_secs(1);
        while !lock_calls(&calls).contains(&"recv-audio-wait") {
            assert!(
                Instant::now() < wait_deadline,
                "receiver did not enter its blocking poll"
            );
            std::thread::yield_now();
        }

        let stop_started = Instant::now();
        legacy.stop();
        assert!(
            stop_started.elapsed() < Duration::from_millis(250),
            "stop must not wait for the caller's one-second receive timeout"
        );
        assert_eq!(
            waiting.join().unwrap(),
            Some(final_event),
            "interrupting the waiter must not consume or discard the flushed final"
        );
        assert_eq!(legacy.try_recv(), None);
    }

    #[test]
    fn terminal_capture_payload_is_not_correlated_with_an_ordinary_log() {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let bridge = FakeBridge {
            start_result: Ok(()),
            start_transcription_result: Ok(()),
            disable_result: Ok(()),
            stop_capture_results: Mutex::new(VecDeque::new()),
            finish_transcription_results: Mutex::new(VecDeque::new()),
            started_capture: true,
            events: Mutex::new(VecDeque::from([Event::Log("ordinary".into())])),
            audio_events: Mutex::new(VecDeque::new()),
            transcriber_failures: Mutex::new(VecDeque::new()),
            capture_failures: Mutex::new(VecDeque::from([crate::MacosCaptureFailure {
                track_id: Some(TrackId::SYSTEM),
                message: "exact terminal payload".into(),
            }])),
            stop_events: Mutex::new(VecDeque::new()),
            calls,
            pushed_frames: Arc::new(Mutex::new(Vec::new())),
            first_audio_timestamps: [None; 2],
            block_audio_for_timeout: false,
        };
        let mut session = MacosSession::from_bridge(
            Box::new(bridge),
            PermissionStatus::Granted,
            PermissionStatus::Granted,
            true,
            crate::TranscriptionPolicy::platform_default(),
        );
        session.start().unwrap();

        assert_eq!(
            session.try_recv(),
            Some(Event::Log("[FATAL] exact terminal payload".into()))
        );
        assert!(session.is_stopped());
        assert_ne!(
            session.try_recv(),
            Some(Event::Log("[FATAL] ordinary".into())),
            "an unrelated log must never be substituted for the typed terminal payload"
        );
    }

    #[test]
    fn disable_transcription_failure_prevents_false_record_only_state() {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let bridge = FakeBridge {
            start_result: Ok(()),
            start_transcription_result: Ok(()),
            disable_result: Err(SessionError::Start("native cancel failed".into())),
            stop_capture_results: Mutex::new(VecDeque::new()),
            finish_transcription_results: Mutex::new(VecDeque::new()),
            started_capture: true,
            events: Mutex::new(VecDeque::new()),
            audio_events: Mutex::new(VecDeque::new()),
            transcriber_failures: Mutex::new(VecDeque::from([crate::MacosTranscriberFailure {
                terminal: true,
                message: "recognizer failed".into(),
            }])),
            capture_failures: Mutex::new(VecDeque::new()),
            stop_events: Mutex::new(VecDeque::new()),
            calls,
            pushed_frames: Arc::new(Mutex::new(Vec::new())),
            first_audio_timestamps: [None; 2],
            block_audio_for_timeout: false,
        };
        let mut session = MacosSession::from_bridge(
            Box::new(bridge),
            PermissionStatus::Granted,
            PermissionStatus::Granted,
            true,
            crate::TranscriptionPolicy::platform_default(),
        );
        session.start().unwrap();

        let event = session.try_recv().expect("terminal disable failure");
        assert!(
            matches!(event, Event::Log(message) if message.contains("could not enter record-only"))
        );
        assert!(!session.is_record_only());
        assert!(
            !session.is_stopped(),
            "persistent abort failure must remain cleanup-pending for a later retry"
        );
    }

    #[test]
    fn capture_gap_after_track_start_inserts_silence_before_next_pcm() {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let pushed_frames = Arc::new(Mutex::new(Vec::new()));
        let first_frame = audio_frame(SourceLabel::Mic, 0);
        let frame = audio_frame(SourceLabel::Mic, 9);
        let bridge = FakeBridge {
            start_result: Ok(()),
            start_transcription_result: Ok(()),
            disable_result: Ok(()),
            stop_capture_results: Mutex::new(VecDeque::new()),
            finish_transcription_results: Mutex::new(VecDeque::new()),
            started_capture: true,
            events: Mutex::new(VecDeque::new()),
            audio_events: Mutex::new(VecDeque::from([
                CaptureEvent::Samples(first_frame),
                CaptureEvent::Overflow {
                    track_id: TrackId::MICROPHONE,
                    dropped_frames: 7,
                },
                CaptureEvent::Samples(frame),
            ])),
            transcriber_failures: Mutex::new(VecDeque::new()),
            capture_failures: Mutex::new(VecDeque::new()),
            stop_events: Mutex::new(VecDeque::new()),
            calls,
            pushed_frames: Arc::clone(&pushed_frames),
            first_audio_timestamps: [None; 2],
            block_audio_for_timeout: false,
        };
        let mut session = MacosSession::from_bridge(
            Box::new(bridge),
            PermissionStatus::Granted,
            PermissionStatus::Granted,
            true,
            crate::TranscriptionPolicy::platform_default(),
        );
        session.start().unwrap();
        let _ = session.orchestrator.pump_once(Duration::ZERO).unwrap();
        let _ = session.orchestrator.pump_once(Duration::ZERO).unwrap();
        let _ = session.orchestrator.pump_once(Duration::ZERO).unwrap();

        let pushed = pushed_frames
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        assert_eq!(pushed.len(), 3);
        assert_eq!(pushed[1].frame_count(), 7);
        assert!(
            pushed[1]
                .samples()
                .as_f32()
                .unwrap()
                .iter()
                .all(|sample| *sample == 0.0)
        );
        assert_eq!(pushed[2].sequence(), 9);
    }
}
