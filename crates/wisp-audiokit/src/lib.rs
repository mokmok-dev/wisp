//! Safe Rust wrapper over Wisp's platform audio/transcription backends.
//!
//! macOS is backed by the Swift `WispAudioKit` framework. Windows records
//! WASAPI microphone and system-loopback audio as Ogg/Opus. Linux records
//! `PipeWire` microphone and, when available, sink-monitor audio as Ogg/Opus.
//! Other platforms keep a stub so the workspace stays buildable.

mod backend;
mod error;
#[cfg(target_os = "macos")]
mod macos_backend;
#[cfg(any(test, target_os = "linux", target_os = "windows"))]
mod ogg_opus_recorder;
#[cfg(target_os = "linux")]
mod pipewire_capture;
#[cfg(target_os = "windows")]
mod wasapi_capture;

pub use backend::{
    Availability, BackendError, BackendErrorKind, BackendId, BackendResult, CaptureBackend,
    CaptureCapabilities, CaptureControlEvent, CaptureEventReceiver, CaptureProbe, ControlEnqueue,
    FrameEnqueue, OrchestratorEvent, PrivacyRequirement, RealtimeCaptureSender, RecognitionPrivacy,
    SessionOrchestrator, ShutdownMode, TranscriberBackend, TranscriberCapabilities,
    TranscriberClass, TranscriberFeature, TranscriberProbe, TranscriptionPolicy,
    TranscriptionSelection, UnavailableReason, realtime_capture_channel, select_transcriber,
    select_transcriber_after_failure,
};
pub use error::{Result, SessionError, SetupError, SetupResult};
#[cfg(target_os = "macos")]
pub use macos_backend::{MacosCaptureBackend, MacosSession, MacosTranscriberBackend};
#[cfg(target_os = "linux")]
pub use pipewire_capture::{
    PIPEWIRE_CAPTURE_QUEUE_CAPACITY, PIPEWIRE_CHANNELS, PIPEWIRE_SAMPLE_RATE, PipewireCapture,
    PipewireCaptureBackend, PipewireRecording,
};
#[cfg(target_os = "windows")]
pub use wasapi_capture::{
    WASAPI_CAPTURE_QUEUE_CAPACITY, WASAPI_CHANNELS, WASAPI_SAMPLE_RATE, WasapiCapture,
    WasapiCaptureEvent, WasapiPcmChunk, WasapiRecording,
};
pub use wisp_core::{
    AudioFormat, AudioFrame, AudioFrameError, AudioSamples, CaptureEvent, MonotonicTimestamp,
    SampleFormat, SourceKind, TrackDescriptor, TrackId, TranscriptEvent, TranscriptSegment,
    TranscriptSegmentId,
};

use std::path::{Path, PathBuf};

#[cfg(target_os = "macos")]
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct MacosTranscriberFailure {
    pub(crate) terminal: bool,
    pub(crate) message: String,
}

#[cfg(target_os = "macos")]
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct MacosCaptureFailure {
    pub(crate) track_id: Option<TrackId>,
    pub(crate) message: String,
}

/// TCC-style OS permission gated by Wisp at startup.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Permission {
    /// Microphone access. Required for the mic capture path.
    Microphone,
    /// On-device speech recognition. Required for both pipelines.
    SpeechRecognition,
}

/// Current state of a single [`Permission`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PermissionStatus {
    /// The user has not been asked yet; calling [`request_permission`] will
    /// trigger the OS dialog.
    Undetermined,
    /// The user explicitly denied this permission. Re-requesting won't show
    /// a dialog — the user has to flip it in System Settings.
    Denied,
    /// Granted; the corresponding capture path can be used.
    Granted,
    /// Restricted by a system policy (e.g. parental controls). Only
    /// reachable for `SpeechRecognition`.
    Restricted,
}

impl PermissionStatus {
    /// Convenience: true iff the underlying capability is usable.
    #[must_use]
    pub fn is_granted(self) -> bool {
        matches!(self, Self::Granted)
    }
}

/// Transcription engine selected for a session.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RecognizerBackend {
    /// Use the OS-provided speech recognizer for the current platform.
    ///
    /// macOS maps this to `SpeechAnalyzer`. Windows maps this to the online
    /// dictation grammar in `Windows.Media.SpeechRecognition`, which uses the
    /// OS microphone path while WASAPI records both local audio sources.
    Platform,
    /// Use a downloaded local model. On Windows this is the path intended
    /// for WASAPI mic + loopback PCM so both sides of the call can be
    /// transcribed by the same offline engine.
    LocalModel,
}

impl RecognizerBackend {
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Platform => platform_recognizer_label(),
            Self::LocalModel => "Local model",
        }
    }
}

/// Configuration used when constructing a [`Session`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionConfig {
    pub locale: String,
    pub recognizer: RecognizerBackend,
    pub local_model_path: Option<PathBuf>,
}

impl SessionConfig {
    #[must_use]
    pub fn platform_default(locale: impl Into<String>) -> Self {
        Self {
            locale: locale.into(),
            recognizer: RecognizerBackend::Platform,
            local_model_path: None,
        }
    }

    #[must_use]
    pub fn local_model(
        locale: impl Into<String>,
        path: impl Into<PathBuf>,
    ) -> Self {
        Self {
            locale: locale.into(),
            recognizer: RecognizerBackend::LocalModel,
            local_model_path: Some(path.into()),
        }
    }

    #[must_use]
    pub const fn with_transcription_policy(
        mut self,
        policy: TranscriptionPolicy,
    ) -> SessionOptions {
        self.recognizer = match policy.preferred {
            TranscriberClass::Platform => RecognizerBackend::Platform,
            TranscriberClass::LocalModel => RecognizerBackend::LocalModel,
        };
        SessionOptions::new(self, policy)
    }
}

/// Extended session configuration carrying backend-selection policy.
///
/// Fields are private so this options layer can evolve without breaking
/// callers that use the source-compatible public [`SessionConfig`] fields.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionOptions {
    config: SessionConfig,
    transcription_policy: TranscriptionPolicy,
}

impl SessionOptions {
    #[must_use]
    pub const fn new(
        config: SessionConfig,
        transcription_policy: TranscriptionPolicy,
    ) -> Self {
        Self {
            config,
            transcription_policy,
        }
    }

    #[must_use]
    pub const fn config(&self) -> &SessionConfig {
        &self.config
    }

    #[must_use]
    pub const fn transcription_policy(&self) -> TranscriptionPolicy {
        self.transcription_policy
    }

    #[must_use]
    pub fn into_parts(self) -> (SessionConfig, TranscriptionPolicy) {
        (self.config, self.transcription_policy)
    }
}

impl From<SessionConfig> for SessionOptions {
    fn from(config: SessionConfig) -> Self {
        let policy = match config.recognizer {
            RecognizerBackend::Platform => TranscriptionPolicy::platform_default(),
            RecognizerBackend::LocalModel => TranscriptionPolicy::offline_local_model(),
        };
        Self::new(config, policy)
    }
}

#[cfg(any(test, target_os = "windows"))]
#[derive(Debug)]
struct OneShotSessionLifecycle {
    state: std::sync::atomic::AtomicU8,
    transition: std::sync::Mutex<()>,
    #[cfg(test)]
    completion_claim_hook: std::sync::Mutex<Option<std::sync::Arc<WindowsCompletionClaimHook>>>,
    #[cfg(test)]
    stop_transition_hook: std::sync::Mutex<Option<std::sync::Arc<WindowsStopTransitionHook>>>,
}

#[cfg(test)]
#[derive(Debug)]
struct WindowsCompletionClaimHook {
    observed: crossbeam_channel::Sender<WindowsCompletionClaim>,
    release: crossbeam_channel::Receiver<()>,
}

#[cfg(test)]
#[derive(Debug)]
struct WindowsStopTransitionHook {
    contended: crossbeam_channel::Sender<()>,
    acquired: crossbeam_channel::Sender<()>,
}

#[cfg(any(test, target_os = "windows"))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WindowsCompletionClaim {
    Suppressed,
    AbortStartup,
    ContinueRecording,
    Fatal,
}

#[cfg(any(test, target_os = "windows"))]
impl OneShotSessionLifecycle {
    const READY: u8 = 0;
    const STARTING: u8 = 1;
    const RUNNING: u8 = 2;
    const RECORDING_ONLY: u8 = 3;
    const STOP_CLAIMED: u8 = 4;
    const FATAL_CLAIMED: u8 = 5;
    const START_ABORTED: u8 = 6;

    const fn new() -> Self {
        Self {
            state: std::sync::atomic::AtomicU8::new(Self::READY),
            transition: std::sync::Mutex::new(()),
            #[cfg(test)]
            completion_claim_hook: std::sync::Mutex::new(None),
            #[cfg(test)]
            stop_transition_hook: std::sync::Mutex::new(None),
        }
    }

    fn begin_start(&self) -> bool {
        let _transition = self
            .transition
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        self.state
            .compare_exchange(
                Self::READY,
                Self::STARTING,
                std::sync::atomic::Ordering::SeqCst,
                std::sync::atomic::Ordering::SeqCst,
            )
            .is_ok()
    }

    fn complete_start(&self) -> bool {
        let _transition = self
            .transition
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        self.state
            .compare_exchange(
                Self::STARTING,
                Self::RUNNING,
                std::sync::atomic::Ordering::SeqCst,
                std::sync::atomic::Ordering::SeqCst,
            )
            .is_ok()
    }

    fn claim_stop(&self) -> bool {
        #[cfg(test)]
        let hook = self
            .stop_transition_hook
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
        #[cfg(test)]
        let _transition = if let Some(hook) = hook {
            let transition = match self.transition.try_lock() {
                Ok(transition) => transition,
                Err(std::sync::TryLockError::Poisoned(error)) => error.into_inner(),
                Err(std::sync::TryLockError::WouldBlock) => {
                    let _ = hook.contended.send(());
                    self.transition
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                },
            };
            let _ = hook.acquired.send(());
            transition
        } else {
            self.transition
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
        };
        #[cfg(not(test))]
        let _transition = self
            .transition
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        self.state
            .fetch_update(
                std::sync::atomic::Ordering::SeqCst,
                std::sync::atomic::Ordering::SeqCst,
                |current| {
                    matches!(
                        current,
                        Self::STARTING | Self::RUNNING | Self::RECORDING_ONLY
                    )
                    .then_some(Self::STOP_CLAIMED)
                },
            )
            .is_ok()
    }

    fn claim_completion(
        &self,
        continue_recording: bool,
        publish: impl FnOnce(WindowsCompletionClaim),
    ) -> WindowsCompletionClaim {
        let _transition = self
            .transition
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let (next, claim) = match self.state.load(std::sync::atomic::Ordering::SeqCst) {
            Self::STARTING => (Self::START_ABORTED, WindowsCompletionClaim::AbortStartup),
            Self::RUNNING if continue_recording => (
                Self::RECORDING_ONLY,
                WindowsCompletionClaim::ContinueRecording,
            ),
            Self::RUNNING => (Self::FATAL_CLAIMED, WindowsCompletionClaim::Fatal),
            _ => return WindowsCompletionClaim::Suppressed,
        };
        self.state.store(next, std::sync::atomic::Ordering::SeqCst);
        #[cfg(test)]
        if claim == WindowsCompletionClaim::Fatal
            && let Some(hook) = self
                .completion_claim_hook
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .take()
        {
            let _ = hook.observed.send(claim);
            let _ = hook.release.recv();
        }
        publish(claim);
        claim
    }

    #[cfg(test)]
    fn set_completion_claim_hook(
        &self,
        hook: std::sync::Arc<WindowsCompletionClaimHook>,
    ) {
        *self
            .completion_claim_hook
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(hook);
    }

    #[cfg(test)]
    fn set_stop_transition_hook(
        &self,
        hook: std::sync::Arc<WindowsStopTransitionHook>,
    ) {
        *self
            .stop_transition_hook
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(hook);
    }

    fn capture_is_running(&self) -> bool {
        matches!(
            self.state.load(std::sync::atomic::Ordering::SeqCst),
            Self::RUNNING | Self::RECORDING_ONLY
        )
    }

    fn speech_is_running(&self) -> bool {
        self.state.load(std::sync::atomic::Ordering::SeqCst) == Self::RUNNING
    }

    fn startup_was_aborted(&self) -> bool {
        self.state.load(std::sync::atomic::Ordering::SeqCst) == Self::START_ABORTED
    }

    fn all_cleanup_is_claimed(&self) -> bool {
        matches!(
            self.state.load(std::sync::atomic::Ordering::SeqCst),
            Self::STOP_CLAIMED | Self::FATAL_CLAIMED | Self::START_ABORTED
        )
    }
}

#[cfg(any(test, target_os = "windows"))]
struct WindowsStartTransactionGuard<F>
where
    F: FnOnce(),
{
    unwind: Option<F>,
}

#[cfg(any(test, target_os = "windows"))]
impl<F> WindowsStartTransactionGuard<F>
where
    F: FnOnce(),
{
    fn new(unwind: F) -> Self {
        Self {
            unwind: Some(unwind),
        }
    }

    fn finish(mut self) {
        self.unwind = None;
    }
}

#[cfg(any(test, target_os = "windows"))]
impl<F> Drop for WindowsStartTransactionGuard<F>
where
    F: FnOnce(),
{
    fn drop(&mut self) {
        if let Some(unwind) = self.unwind.take() {
            unwind();
        }
    }
}

#[cfg(any(test, target_os = "macos", target_os = "windows"))]
const CALLBACK_FINAL_CAPACITY: usize = 64;
#[cfg(any(test, target_os = "macos", target_os = "windows"))]
const CALLBACK_PARTIAL_CAPACITY: usize = 64;
#[cfg(any(test, target_os = "macos", target_os = "windows"))]
const CALLBACK_LOG_CAPACITY: usize = 16;
#[cfg(any(test, target_os = "macos", target_os = "windows"))]
const CALLBACK_SOURCE_SLOTS: usize = 2;

#[cfg(any(test, target_os = "macos", target_os = "windows"))]
trait CallbackEventKey: Clone + PartialEq {
    fn source_slot(&self) -> usize;
    fn sequence(&self) -> u64;
}

#[cfg(any(test, target_os = "macos", target_os = "windows"))]
impl CallbackEventKey for u64 {
    fn source_slot(&self) -> usize {
        0
    }

    fn sequence(&self) -> u64 {
        *self
    }
}

#[cfg(any(test, target_os = "macos", target_os = "windows"))]
impl CallbackEventKey for (wisp_core::SourceLabel, u64) {
    fn source_slot(&self) -> usize {
        match self.0 {
            wisp_core::SourceLabel::Mic => 0,
            wisp_core::SourceLabel::System => 1,
        }
    }

    fn sequence(&self) -> u64 {
        self.1
    }
}

#[cfg(any(test, target_os = "macos", target_os = "windows"))]
#[derive(Debug, Clone, PartialEq, Eq)]
enum CallbackEventClass<K> {
    Final(Option<K>),
    Partial(K),
    Log,
}

#[cfg(any(test, target_os = "macos", target_os = "windows"))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CallbackEnqueue {
    Enqueued,
    Replaced,
    DroppedFull,
    DroppedBusy,
}

#[cfg(any(test, target_os = "macos", target_os = "windows"))]
struct CallbackQueueState<T, K> {
    partials: std::collections::VecDeque<(K, T)>,
    logs: std::collections::VecDeque<T>,
    finalized_high_watermarks: [Option<u64>; CALLBACK_SOURCE_SLOTS],
    lossy_wake_armed: bool,
}

#[cfg(any(test, target_os = "macos", target_os = "windows"))]
struct CallbackTerminalWatermarks {
    initialized: [std::sync::atomic::AtomicBool; CALLBACK_SOURCE_SLOTS],
    sequences: [std::sync::atomic::AtomicU64; CALLBACK_SOURCE_SLOTS],
}

#[cfg(any(test, target_os = "macos", target_os = "windows"))]
impl Default for CallbackTerminalWatermarks {
    fn default() -> Self {
        Self {
            initialized: std::array::from_fn(|_| std::sync::atomic::AtomicBool::new(false)),
            sequences: std::array::from_fn(|_| std::sync::atomic::AtomicU64::new(0)),
        }
    }
}

#[cfg(any(test, target_os = "macos", target_os = "windows"))]
impl<T, K> Default for CallbackQueueState<T, K> {
    fn default() -> Self {
        Self {
            partials: std::collections::VecDeque::new(),
            logs: std::collections::VecDeque::new(),
            finalized_high_watermarks: [None; CALLBACK_SOURCE_SLOTS],
            lossy_wake_armed: false,
        }
    }
}

#[cfg(any(test, target_os = "macos", target_os = "windows"))]
#[derive(Clone)]
struct CallbackEventSender<T, K> {
    state: std::sync::Arc<std::sync::Mutex<CallbackQueueState<T, K>>>,
    lossy_ready: crossbeam_channel::Sender<()>,
    lossy_ready_cleanup: crossbeam_channel::Receiver<()>,
    finals: crossbeam_channel::Sender<(Option<K>, T)>,
    final_gap_ready: crossbeam_channel::Sender<()>,
    dropped_finals: std::sync::Arc<std::sync::atomic::AtomicU64>,
    terminal_high_watermarks: std::sync::Arc<CallbackTerminalWatermarks>,
    report_final_gaps: bool,
    #[cfg(test)]
    final_cleanup_hook: Option<std::sync::Arc<CallbackFinalCleanupHook>>,
    #[cfg(test)]
    final_overflow_hook: Option<std::sync::Arc<CallbackFinalOverflowHook>>,
}

#[cfg(any(test, target_os = "macos", target_os = "windows"))]
struct CallbackEventReceiver<T, K> {
    state: std::sync::Arc<std::sync::Mutex<CallbackQueueState<T, K>>>,
    lossy_ready: crossbeam_channel::Receiver<()>,
    finals: crossbeam_channel::Receiver<(Option<K>, T)>,
    final_gap_ready: crossbeam_channel::Receiver<()>,
    dropped_finals: std::sync::Arc<std::sync::atomic::AtomicU64>,
    terminal_high_watermarks: std::sync::Arc<CallbackTerminalWatermarks>,
    final_gap_factory: Option<fn(u64) -> T>,
    #[cfg(test)]
    wait_hook: Option<std::sync::Arc<CallbackWaitHook>>,
}

#[cfg(any(test, target_os = "macos", target_os = "windows"))]
enum CallbackSweep<T> {
    Event(T),
    Empty,
    Busy,
}

#[cfg(test)]
struct CallbackWaitHook {
    fired: std::sync::atomic::AtomicBool,
    entered: std::sync::Barrier,
    release: std::sync::Barrier,
}

#[cfg(test)]
struct CallbackFinalCleanupHook {
    entered: std::sync::Barrier,
    release: std::sync::Barrier,
}

#[cfg(test)]
struct CallbackFinalOverflowHook {
    entered: std::sync::Barrier,
    release: std::sync::Barrier,
}

#[cfg(test)]
fn callback_event_channel<T, K>() -> (CallbackEventSender<T, K>, CallbackEventReceiver<T, K>) {
    callback_event_channel_inner(None)
}

#[cfg(any(test, target_os = "macos", target_os = "windows"))]
fn callback_event_channel_with_final_gap<T, K>(
    final_gap_factory: fn(u64) -> T
) -> (CallbackEventSender<T, K>, CallbackEventReceiver<T, K>) {
    callback_event_channel_inner(Some(final_gap_factory))
}

#[cfg(any(test, target_os = "macos", target_os = "windows"))]
fn callback_event_channel_inner<T, K>(
    final_gap_factory: Option<fn(u64) -> T>
) -> (CallbackEventSender<T, K>, CallbackEventReceiver<T, K>) {
    let (lossy_ready, lossy_ready_receiver) = crossbeam_channel::bounded(1);
    let (finals, final_receiver) = crossbeam_channel::bounded(CALLBACK_FINAL_CAPACITY);
    let (final_gap_ready, final_gap_ready_receiver) = crossbeam_channel::bounded(1);
    let state = std::sync::Arc::new(std::sync::Mutex::new(CallbackQueueState::default()));
    let dropped_finals = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
    let terminal_high_watermarks = std::sync::Arc::new(CallbackTerminalWatermarks::default());
    (
        CallbackEventSender {
            state: std::sync::Arc::clone(&state),
            lossy_ready,
            lossy_ready_cleanup: lossy_ready_receiver.clone(),
            finals,
            final_gap_ready,
            dropped_finals: std::sync::Arc::clone(&dropped_finals),
            terminal_high_watermarks: std::sync::Arc::clone(&terminal_high_watermarks),
            report_final_gaps: final_gap_factory.is_some(),
            #[cfg(test)]
            final_cleanup_hook: None,
            #[cfg(test)]
            final_overflow_hook: None,
        },
        CallbackEventReceiver {
            state,
            lossy_ready: lossy_ready_receiver,
            finals: final_receiver,
            final_gap_ready: final_gap_ready_receiver,
            dropped_finals,
            terminal_high_watermarks,
            final_gap_factory,
            #[cfg(test)]
            wait_hook: None,
        },
    )
}

#[cfg(any(test, target_os = "macos", target_os = "windows"))]
impl<T, K> CallbackEventSender<T, K>
where
    K: CallbackEventKey,
{
    fn try_send(
        &self,
        class: CallbackEventClass<K>,
        event: T,
    ) -> CallbackEnqueue {
        if let CallbackEventClass::Final(key) = class {
            let terminal_key = key.clone();
            return match self.finals.try_send((key, event)) {
                Ok(()) => {
                    if let Some(key) = &terminal_key {
                        self.publish_terminal(key);
                        self.try_finish_final_acceptance(key);
                    }
                    CallbackEnqueue::Enqueued
                },
                Err(crossbeam_channel::TrySendError::Full((key, _event))) => {
                    if let Some(key) = &key {
                        self.publish_terminal(key);
                        self.try_finish_final_acceptance(key);
                    }
                    if self.report_final_gaps {
                        self.run_final_overflow_hook();
                        saturating_atomic_increment(&self.dropped_finals);
                        let _ = self.final_gap_ready.try_send(());
                    }
                    CallbackEnqueue::DroppedFull
                },
                Err(crossbeam_channel::TrySendError::Disconnected(_)) => {
                    CallbackEnqueue::DroppedFull
                },
            };
        }
        let Ok(mut state) = self.state.try_lock() else {
            return CallbackEnqueue::DroppedBusy;
        };
        reconcile_terminal_high_watermarks(
            &mut state,
            &self.terminal_high_watermarks,
            &self.lossy_ready_cleanup,
        );
        let outcome = match class {
            CallbackEventClass::Final(_) => unreachable!("final events use their reserved lane"),
            CallbackEventClass::Partial(key) => {
                if key_is_finalized(&state, &key) {
                    CallbackEnqueue::DroppedFull
                } else if let Some((_, queued)) = state
                    .partials
                    .iter_mut()
                    .find(|(queued_key, _)| *queued_key == key)
                {
                    *queued = event;
                    CallbackEnqueue::Replaced
                } else if state.partials.len() >= CALLBACK_PARTIAL_CAPACITY {
                    CallbackEnqueue::DroppedFull
                } else {
                    state.partials.push_back((key, event));
                    CallbackEnqueue::Enqueued
                }
            },
            CallbackEventClass::Log => {
                if state.logs.len() >= CALLBACK_LOG_CAPACITY {
                    CallbackEnqueue::DroppedFull
                } else {
                    state.logs.push_back(event);
                    CallbackEnqueue::Enqueued
                }
            },
        };
        if outcome == CallbackEnqueue::Enqueued {
            arm_lossy_wake(&mut state, &self.lossy_ready);
        }
        drop(state);
        outcome
    }

    fn try_finish_final_acceptance(
        &self,
        key: &K,
    ) {
        let Ok(mut state) = self.state.try_lock() else {
            return;
        };
        finish_final_acceptance(&mut state, key, &self.lossy_ready_cleanup);
        drop(state);
        #[cfg(test)]
        if let Some(hook) = &self.final_cleanup_hook {
            hook.entered.wait();
            hook.release.wait();
        }
    }

    fn publish_terminal(
        &self,
        key: &K,
    ) {
        let slot = key.source_slot();
        self.terminal_high_watermarks.sequences[slot]
            .fetch_max(key.sequence(), std::sync::atomic::Ordering::AcqRel);
        self.terminal_high_watermarks.initialized[slot]
            .store(true, std::sync::atomic::Ordering::Release);
    }

    fn run_final_overflow_hook(&self) {
        #[cfg(not(test))]
        let _ = self;
        #[cfg(test)]
        if let Some(hook) = &self.final_overflow_hook {
            hook.entered.wait();
            hook.release.wait();
        }
    }
}

#[cfg(any(test, target_os = "macos", target_os = "windows"))]
impl<T, K> CallbackEventReceiver<T, K>
where
    K: CallbackEventKey,
{
    fn try_recv(&self) -> Option<T> {
        self.try_recv_result().ok()
    }

    #[cfg(target_os = "windows")]
    fn recv(&self) -> Option<T> {
        self.recv_result().ok()
    }

    #[cfg(any(test, target_os = "windows"))]
    fn recv_timeout(
        &self,
        timeout: std::time::Duration,
    ) -> Option<T> {
        self.recv_timeout_result(timeout).ok()
    }

    fn try_recv_result(&self) -> std::result::Result<T, crossbeam_channel::TryRecvError> {
        let mut finals_open = true;
        let mut final_gap_open = true;
        let mut lossy_open = true;
        match self.try_recv_from_open_lanes(&mut finals_open, &mut final_gap_open, &mut lossy_open)
        {
            CallbackSweep::Event(event) => return Ok(event),
            CallbackSweep::Busy => return Err(crossbeam_channel::TryRecvError::Empty),
            CallbackSweep::Empty => {},
        }
        if !finals_open && !final_gap_open && !lossy_open {
            Err(crossbeam_channel::TryRecvError::Disconnected)
        } else {
            Err(crossbeam_channel::TryRecvError::Empty)
        }
    }

    #[cfg(target_os = "windows")]
    fn recv_result(&self) -> std::result::Result<T, crossbeam_channel::RecvError> {
        let mut finals_open = true;
        let mut final_gap_open = true;
        let mut lossy_open = true;
        loop {
            match self.try_recv_from_open_lanes(
                &mut finals_open,
                &mut final_gap_open,
                &mut lossy_open,
            ) {
                CallbackSweep::Event(event) => return Ok(event),
                CallbackSweep::Busy => {
                    std::thread::sleep(std::time::Duration::from_millis(1));
                    continue;
                },
                CallbackSweep::Empty => {},
            }
            if !finals_open && !final_gap_open && !lossy_open {
                return Err(crossbeam_channel::RecvError);
            }

            let mut selector = crossbeam_channel::Select::new();
            // Selection only wakes this loop. The next sweep consumes final
            // before lossy even when both became ready while blocked.
            if finals_open {
                selector.recv(&self.finals);
            }
            if final_gap_open {
                selector.recv(&self.final_gap_ready);
            }
            if lossy_open {
                selector.recv(&self.lossy_ready);
            }
            self.run_wait_hook();
            selector.ready();
        }
    }

    #[cfg(any(test, target_os = "windows"))]
    fn recv_timeout_result(
        &self,
        timeout: std::time::Duration,
    ) -> std::result::Result<T, crossbeam_channel::RecvTimeoutError> {
        let started = std::time::Instant::now();
        let mut finals_open = true;
        let mut final_gap_open = true;
        let mut lossy_open = true;
        loop {
            match self.try_recv_from_open_lanes(
                &mut finals_open,
                &mut final_gap_open,
                &mut lossy_open,
            ) {
                CallbackSweep::Event(event) => return Ok(event),
                CallbackSweep::Busy => {
                    let remaining = timeout.saturating_sub(started.elapsed());
                    if remaining.is_zero() {
                        return Err(crossbeam_channel::RecvTimeoutError::Timeout);
                    }
                    std::thread::sleep(remaining.min(std::time::Duration::from_millis(1)));
                    continue;
                },
                CallbackSweep::Empty => {},
            }
            if !finals_open && !final_gap_open && !lossy_open {
                return Err(crossbeam_channel::RecvTimeoutError::Disconnected);
            }

            let remaining = timeout.saturating_sub(started.elapsed());
            if remaining.is_zero() {
                return Err(crossbeam_channel::RecvTimeoutError::Timeout);
            }
            let mut selector = crossbeam_channel::Select::new();
            // Selection only wakes this loop. The next sweep consumes final
            // before lossy even when both became ready while blocked.
            if finals_open {
                selector.recv(&self.finals);
            }
            if final_gap_open {
                selector.recv(&self.final_gap_ready);
            }
            if lossy_open {
                selector.recv(&self.lossy_ready);
            }
            self.run_wait_hook();
            selector
                .ready_timeout(remaining)
                .map_err(|_| crossbeam_channel::RecvTimeoutError::Timeout)?;
        }
    }

    #[cfg(any(test, target_os = "windows"))]
    fn final_receiver(&self) -> &crossbeam_channel::Receiver<(Option<K>, T)> {
        &self.finals
    }

    #[cfg(any(test, target_os = "windows"))]
    fn final_gap_ready_receiver(&self) -> &crossbeam_channel::Receiver<()> {
        &self.final_gap_ready
    }

    #[cfg(any(test, target_os = "windows"))]
    fn lossy_ready_receiver(&self) -> &crossbeam_channel::Receiver<()> {
        &self.lossy_ready
    }

    fn accept_final_event_locked(
        &self,
        state: &mut CallbackQueueState<T, K>,
        (key, event): (Option<K>, T),
    ) -> T {
        if let Some(key) = key {
            finish_final_acceptance(state, &key, &self.lossy_ready);
        }
        event
    }

    fn try_recv_lossy_locked(
        &self,
        state: &mut CallbackQueueState<T, K>,
    ) -> std::result::Result<T, crossbeam_channel::TryRecvError> {
        let wake = self.lossy_ready.try_recv();
        while let Some((key, event)) = state.partials.pop_front() {
            if !key_is_finalized(state, &key) {
                if state.partials.is_empty() && state.logs.is_empty() {
                    reconcile_empty_lossy_wake(state, &self.lossy_ready);
                }
                return Ok(event);
            }
        }
        if let Some(event) = state.logs.pop_front() {
            if state.logs.is_empty() {
                reconcile_empty_lossy_wake(state, &self.lossy_ready);
            }
            return Ok(event);
        }
        reconcile_empty_lossy_wake(state, &self.lossy_ready);
        match wake {
            Err(crossbeam_channel::TryRecvError::Disconnected) => {
                Err(crossbeam_channel::TryRecvError::Disconnected)
            },
            Ok(()) | Err(crossbeam_channel::TryRecvError::Empty) => {
                Err(crossbeam_channel::TryRecvError::Empty)
            },
        }
    }

    fn try_recv_from_open_lanes(
        &self,
        finals_open: &mut bool,
        final_gap_open: &mut bool,
        lossy_open: &mut bool,
    ) -> CallbackSweep<T> {
        let mut state = match self.state.try_lock() {
            Ok(state) => state,
            Err(std::sync::TryLockError::Poisoned(error)) => error.into_inner(),
            Err(std::sync::TryLockError::WouldBlock) => return CallbackSweep::Busy,
        };
        reconcile_terminal_high_watermarks(
            &mut state,
            &self.terminal_high_watermarks,
            &self.lossy_ready,
        );
        if *finals_open {
            match self.finals.try_recv() {
                Ok(final_event) => {
                    return CallbackSweep::Event(
                        self.accept_final_event_locked(&mut state, final_event),
                    );
                },
                Err(crossbeam_channel::TryRecvError::Disconnected) => *finals_open = false,
                Err(crossbeam_channel::TryRecvError::Empty) => {},
            }
        }
        if *final_gap_open
            && matches!(
                self.final_gap_ready.try_recv(),
                Err(crossbeam_channel::TryRecvError::Disconnected)
            )
        {
            *final_gap_open = false;
        }
        if let Some(factory) = self.final_gap_factory {
            let dropped = self
                .dropped_finals
                .swap(0, std::sync::atomic::Ordering::AcqRel);
            if dropped > 0 {
                return CallbackSweep::Event(factory(dropped));
            }
        }
        while *lossy_open {
            match self.try_recv_lossy_locked(&mut state) {
                Ok(event) => return CallbackSweep::Event(event),
                Err(crossbeam_channel::TryRecvError::Disconnected) => {
                    *lossy_open = false;
                },
                Err(crossbeam_channel::TryRecvError::Empty) => break,
            }
        }
        CallbackSweep::Empty
    }

    #[cfg(any(test, target_os = "windows"))]
    fn run_wait_hook(&self) {
        #[cfg(not(test))]
        let _ = self;
        #[cfg(test)]
        if let Some(hook) = &self.wait_hook
            && !hook.fired.swap(true, std::sync::atomic::Ordering::SeqCst)
        {
            hook.entered.wait();
            hook.release.wait();
        }
    }
}

#[cfg(any(test, target_os = "macos", target_os = "windows"))]
fn finish_final_acceptance<T, K>(
    state: &mut CallbackQueueState<T, K>,
    key: &K,
    lossy_ready: &crossbeam_channel::Receiver<()>,
) where
    K: CallbackEventKey,
{
    let slot = key.source_slot();
    state.finalized_high_watermarks[slot] = Some(
        state.finalized_high_watermarks[slot]
            .map_or_else(|| key.sequence(), |current| current.max(key.sequence())),
    );
    if let Some(position) = state
        .partials
        .iter()
        .position(|(queued_key, _)| queued_key == key)
    {
        state.partials.remove(position);
    }
    if state.partials.is_empty() && state.logs.is_empty() {
        reconcile_empty_lossy_wake(state, lossy_ready);
    }
}

#[cfg(any(test, target_os = "macos", target_os = "windows"))]
fn reconcile_terminal_high_watermarks<T, K>(
    state: &mut CallbackQueueState<T, K>,
    published: &CallbackTerminalWatermarks,
    lossy_ready: &crossbeam_channel::Receiver<()>,
) where
    K: CallbackEventKey,
{
    let mut changed = false;
    for slot in 0..CALLBACK_SOURCE_SLOTS {
        if !published.initialized[slot].load(std::sync::atomic::Ordering::Acquire) {
            continue;
        }
        let sequence = published.sequences[slot].load(std::sync::atomic::Ordering::Acquire);
        if state.finalized_high_watermarks[slot].is_none_or(|current| sequence > current) {
            state.finalized_high_watermarks[slot] = Some(sequence);
            changed = true;
        }
    }
    if !changed {
        return;
    }
    let finalized = state.finalized_high_watermarks;
    state.partials.retain(|(key, _)| {
        finalized[key.source_slot()].is_none_or(|sequence| key.sequence() > sequence)
    });
    if state.partials.is_empty() && state.logs.is_empty() {
        reconcile_empty_lossy_wake(state, lossy_ready);
    }
}

#[cfg(any(test, target_os = "macos", target_os = "windows"))]
fn reconcile_empty_lossy_wake<T, K>(
    state: &mut CallbackQueueState<T, K>,
    lossy_ready: &crossbeam_channel::Receiver<()>,
) {
    while lossy_ready.try_recv().is_ok() {}
    state.lossy_wake_armed = false;
}

#[cfg(any(test, target_os = "macos", target_os = "windows"))]
fn key_is_finalized<T, K>(
    state: &CallbackQueueState<T, K>,
    key: &K,
) -> bool
where
    K: CallbackEventKey,
{
    state.finalized_high_watermarks[key.source_slot()]
        .is_some_and(|finalized| key.sequence() <= finalized)
}

#[cfg(any(test, target_os = "macos", target_os = "windows"))]
fn arm_lossy_wake<T, K>(
    state: &mut CallbackQueueState<T, K>,
    wake: &crossbeam_channel::Sender<()>,
) {
    if !state.partials.is_empty() || !state.logs.is_empty() {
        match wake.try_send(()) {
            Ok(()) | Err(crossbeam_channel::TrySendError::Full(())) => {
                state.lossy_wake_armed = true;
            },
            Err(crossbeam_channel::TrySendError::Disconnected(())) => {},
        }
    }
}

#[cfg(any(test, target_os = "macos", target_os = "windows"))]
fn saturating_atomic_increment(value: &std::sync::atomic::AtomicU64) {
    let _ = value.fetch_update(
        std::sync::atomic::Ordering::Relaxed,
        std::sync::atomic::Ordering::Relaxed,
        |current| Some(current.saturating_add(1)),
    );
}

#[cfg(any(test, target_os = "windows"))]
enum MergedSessionReceive<T> {
    Main(T),
    Notification(String),
    RuntimeControl(WindowsRuntimeNotification),
}

#[cfg(test)]
fn try_recv_callback_session_channels<T, K>(
    main: &CallbackEventReceiver<T, K>,
    fatal: &crossbeam_channel::Receiver<String>,
    warning: &crossbeam_channel::Receiver<String>,
) -> Option<MergedSessionReceive<T>>
where
    K: CallbackEventKey,
{
    try_recv_callback_session_channels_with_control(main, fatal, None, warning)
}

#[cfg(any(test, target_os = "windows"))]
fn try_recv_callback_session_channels_with_control<T, K>(
    main: &CallbackEventReceiver<T, K>,
    fatal: &crossbeam_channel::Receiver<String>,
    runtime_control: Option<&crossbeam_channel::Receiver<WindowsRuntimeNotification>>,
    warning: &crossbeam_channel::Receiver<String>,
) -> Option<MergedSessionReceive<T>>
where
    K: CallbackEventKey,
{
    if let Ok(message) = fatal.try_recv() {
        return Some(MergedSessionReceive::Notification(message));
    }
    if let Some(runtime_control) = runtime_control
        && let Ok(control) = runtime_control.try_recv()
    {
        return Some(MergedSessionReceive::RuntimeControl(control));
    }
    let mut finals_open = true;
    let mut final_gap_open = true;
    let mut lossy_open = true;
    match main.try_recv_from_open_lanes(&mut finals_open, &mut final_gap_open, &mut lossy_open) {
        CallbackSweep::Event(event) => Some(MergedSessionReceive::Main(event)),
        CallbackSweep::Busy => None,
        CallbackSweep::Empty => warning
            .try_recv()
            .ok()
            .map(MergedSessionReceive::Notification),
    }
}

#[cfg(test)]
fn recv_session_channels<T>(
    main: &crossbeam_channel::Receiver<T>,
    fatal: &crossbeam_channel::Receiver<String>,
    warning: &crossbeam_channel::Receiver<String>,
    timeout: Option<std::time::Duration>,
) -> Option<MergedSessionReceive<T>> {
    let started = std::time::Instant::now();
    let mut main_open = true;
    let mut fatal_open = true;
    let mut warning_open = true;

    loop {
        if fatal_open {
            match fatal.try_recv() {
                Ok(message) => return Some(MergedSessionReceive::Notification(message)),
                Err(crossbeam_channel::TryRecvError::Disconnected) => fatal_open = false,
                Err(crossbeam_channel::TryRecvError::Empty) => {},
            }
        }
        if main_open {
            match main.try_recv() {
                Ok(event) => return Some(MergedSessionReceive::Main(event)),
                Err(crossbeam_channel::TryRecvError::Disconnected) => main_open = false,
                Err(crossbeam_channel::TryRecvError::Empty) => {},
            }
        }
        if warning_open {
            match warning.try_recv() {
                Ok(message) => return Some(MergedSessionReceive::Notification(message)),
                Err(crossbeam_channel::TryRecvError::Disconnected) => warning_open = false,
                Err(crossbeam_channel::TryRecvError::Empty) => {},
            }
        }
        if !main_open && !fatal_open && !warning_open {
            return None;
        }

        let mut selector = crossbeam_channel::Select::new();
        // Registration documents the priority contract; `ready` only wakes
        // this loop and the next sweep consumes fatal, final, lossy, warning.
        let main_index = main_open.then(|| selector.recv(main));
        let fatal_index = fatal_open.then(|| selector.recv(fatal));
        let warning_index = warning_open.then(|| selector.recv(warning));
        let operation = if let Some(timeout) = timeout {
            let remaining = timeout.saturating_sub(started.elapsed());
            if remaining.is_zero() {
                return None;
            }
            match selector.select_timeout(remaining) {
                Ok(operation) => operation,
                Err(_) => return None,
            }
        } else {
            selector.select()
        };
        let index = operation.index();
        if Some(index) == main_index {
            match operation.recv(main) {
                Ok(event) => return Some(MergedSessionReceive::Main(event)),
                Err(_) => main_open = false,
            }
        } else if Some(index) == fatal_index {
            match operation.recv(fatal) {
                Ok(message) => return Some(MergedSessionReceive::Notification(message)),
                Err(_) => fatal_open = false,
            }
        } else if Some(index) == warning_index {
            match operation.recv(warning) {
                Ok(message) => return Some(MergedSessionReceive::Notification(message)),
                Err(_) => warning_open = false,
            }
        }
    }
}

#[cfg(test)]
fn recv_callback_session_channels<T, K>(
    main: &CallbackEventReceiver<T, K>,
    fatal: &crossbeam_channel::Receiver<String>,
    warning: &crossbeam_channel::Receiver<String>,
    timeout: Option<std::time::Duration>,
) -> Option<MergedSessionReceive<T>>
where
    K: CallbackEventKey,
{
    recv_callback_session_channels_with_control(main, fatal, None, warning, timeout)
}

#[cfg(any(test, target_os = "windows"))]
fn recv_callback_session_channels_with_control<T, K>(
    main: &CallbackEventReceiver<T, K>,
    fatal: &crossbeam_channel::Receiver<String>,
    runtime_control: Option<&crossbeam_channel::Receiver<WindowsRuntimeNotification>>,
    warning: &crossbeam_channel::Receiver<String>,
    timeout: Option<std::time::Duration>,
) -> Option<MergedSessionReceive<T>>
where
    K: CallbackEventKey,
{
    let started = std::time::Instant::now();
    let mut main_final_open = true;
    let mut main_final_gap_open = true;
    let mut main_lossy_open = true;
    let mut fatal_open = true;
    let mut runtime_control_open = runtime_control.is_some();
    let mut warning_open = true;

    loop {
        if fatal_open {
            match fatal.try_recv() {
                Ok(message) => return Some(MergedSessionReceive::Notification(message)),
                Err(crossbeam_channel::TryRecvError::Disconnected) => fatal_open = false,
                Err(crossbeam_channel::TryRecvError::Empty) => {},
            }
        }
        if let Some(runtime_control) = runtime_control.filter(|_| runtime_control_open) {
            match runtime_control.try_recv() {
                Ok(control) => return Some(MergedSessionReceive::RuntimeControl(control)),
                Err(crossbeam_channel::TryRecvError::Disconnected) => {
                    runtime_control_open = false;
                },
                Err(crossbeam_channel::TryRecvError::Empty) => {},
            }
        }
        match main.try_recv_from_open_lanes(
            &mut main_final_open,
            &mut main_final_gap_open,
            &mut main_lossy_open,
        ) {
            CallbackSweep::Event(event) => return Some(MergedSessionReceive::Main(event)),
            CallbackSweep::Busy => {
                if let Some(timeout) = timeout {
                    let remaining = timeout.saturating_sub(started.elapsed());
                    if remaining.is_zero() {
                        return None;
                    }
                    std::thread::sleep(remaining.min(std::time::Duration::from_millis(1)));
                } else {
                    std::thread::sleep(std::time::Duration::from_millis(1));
                }
                continue;
            },
            CallbackSweep::Empty => {},
        }
        if warning_open {
            match warning.try_recv() {
                Ok(message) => return Some(MergedSessionReceive::Notification(message)),
                Err(crossbeam_channel::TryRecvError::Disconnected) => warning_open = false,
                Err(crossbeam_channel::TryRecvError::Empty) => {},
            }
        }
        if !main_final_open
            && !main_final_gap_open
            && !main_lossy_open
            && !fatal_open
            && !runtime_control_open
            && !warning_open
        {
            return None;
        }

        let mut selector = crossbeam_channel::Select::new();
        if fatal_open {
            selector.recv(fatal);
        }
        if let Some(runtime_control) = runtime_control.filter(|_| runtime_control_open) {
            selector.recv(runtime_control);
        }
        if main_final_open {
            selector.recv(main.final_receiver());
        }
        if main_final_gap_open {
            selector.recv(main.final_gap_ready_receiver());
        }
        if main_lossy_open {
            selector.recv(main.lossy_ready_receiver());
        }
        if warning_open {
            selector.recv(warning);
        }
        main.run_wait_hook();
        if let Some(timeout) = timeout {
            let remaining = timeout.saturating_sub(started.elapsed());
            if remaining.is_zero() {
                return None;
            }
            if selector.ready_timeout(remaining).is_err() {
                return None;
            }
        } else {
            selector.ready();
        }
    }
}

#[cfg(any(test, target_os = "windows"))]
#[derive(Debug, Clone, PartialEq, Eq)]
enum WindowsTranscriptionMode {
    PlatformOnline,
    RecordOnly { reason: String },
}

#[cfg(any(test, target_os = "windows"))]
#[derive(Debug, Clone, PartialEq, Eq)]
enum WindowsRuntimeFailureAction {
    ContinueRecording { reason: String },
    FailStart { reason: String },
}

#[cfg(any(test, target_os = "windows"))]
#[derive(Debug, Clone, PartialEq, Eq)]
enum WindowsRuntimeNotification {
    ContinueRecording(String),
    Fatal(String),
}

#[cfg(any(test, target_os = "windows"))]
#[derive(Debug)]
enum WindowsPlatformStart<R, S> {
    Platform {
        recording: R,
        speech: S,
    },
    RecordOnly {
        recording: R,
        speech_error: String,
        reason: String,
    },
}

#[cfg(any(test, target_os = "windows"))]
#[derive(Debug)]
enum WindowsPlatformStartError<E> {
    Recording(E),
    Speech(String),
}

#[cfg(any(test, target_os = "windows"))]
fn select_windows_transcription_mode(
    recognizer: RecognizerBackend,
    policy: TranscriptionPolicy,
) -> std::result::Result<WindowsTranscriptionMode, String> {
    let recognizer_class = match recognizer {
        RecognizerBackend::Platform => TranscriberClass::Platform,
        RecognizerBackend::LocalModel => TranscriberClass::LocalModel,
    };
    if recognizer_class != policy.preferred {
        return Err(format!(
            "recognizer {:?} conflicts with transcription policy preference {:?}",
            recognizer, policy.preferred
        ));
    }
    windows_mode_from_selection(select_transcriber(policy, &windows_transcriber_probes()))
}

#[cfg(any(test, target_os = "windows"))]
fn select_windows_transcription_after_failure(
    policy: TranscriptionPolicy,
    failed_backend: &BackendId,
) -> std::result::Result<WindowsTranscriptionMode, String> {
    windows_mode_from_selection(select_transcriber_after_failure(
        policy,
        &windows_transcriber_probes(),
        failed_backend,
    ))
}

#[cfg(any(test, target_os = "windows"))]
fn windows_runtime_failure_action(policy: TranscriptionPolicy) -> WindowsRuntimeFailureAction {
    let failed = BackendId::new("windows-platform-online");
    match select_windows_transcription_after_failure(policy, &failed) {
        Ok(WindowsTranscriptionMode::RecordOnly { reason }) => {
            WindowsRuntimeFailureAction::ContinueRecording { reason }
        },
        Ok(WindowsTranscriptionMode::PlatformOnline) => WindowsRuntimeFailureAction::FailStart {
            reason: "runtime fallback reselected the failed Windows platform recognizer".into(),
        },
        Err(reason) => WindowsRuntimeFailureAction::FailStart { reason },
    }
}

#[cfg(any(test, target_os = "windows"))]
fn windows_runtime_completion_notification(
    policy: TranscriptionPolicy,
    status: &str,
) -> WindowsRuntimeNotification {
    match windows_runtime_failure_action(policy) {
        WindowsRuntimeFailureAction::ContinueRecording { reason } => {
            WindowsRuntimeNotification::ContinueRecording(format!(
                "platform dictation stopped ({status}); {reason}; continuing with local WASAPI recording"
            ))
        },
        WindowsRuntimeFailureAction::FailStart { reason } => WindowsRuntimeNotification::Fatal(
            format!("platform dictation failed at runtime ({status}); {reason}"),
        ),
    }
}

#[cfg(any(test, target_os = "windows"))]
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum WindowsCleanupLevel {
    SpeechOnly = 1,
    All = 2,
}

#[cfg(any(test, target_os = "windows"))]
const WINDOWS_CLEANUP_MAX_ATTEMPTS: usize = 3;

#[cfg(any(test, target_os = "windows"))]
struct BoundedCleanupInner {
    requested: std::sync::atomic::AtomicU8,
    wake: crossbeam_channel::Sender<()>,
    shutdown: std::sync::atomic::AtomicBool,
    all_requested_generation: std::sync::atomic::AtomicU64,
    all_state: std::sync::Mutex<WindowsAllCleanupState>,
    all_generation_changed: std::sync::Condvar,
    #[cfg(test)]
    all_wait_hook: std::sync::Mutex<Option<std::sync::Arc<WindowsAllWaitHook>>>,
}

#[cfg(any(test, target_os = "windows"))]
#[derive(Debug, Default)]
struct WindowsAllCleanupState {
    generation: u64,
    completed_generation: u64,
}

#[cfg(test)]
struct WindowsAllWaitHook {
    observed: crossbeam_channel::Sender<u64>,
    release: crossbeam_channel::Receiver<()>,
}

#[cfg(any(test, target_os = "windows"))]
#[derive(Clone)]
struct BoundedCleanupHandle(std::sync::Arc<BoundedCleanupInner>);

#[cfg(any(test, target_os = "windows"))]
impl BoundedCleanupHandle {
    fn spawn(
        mut cleanup: impl FnMut(WindowsCleanupLevel) -> Option<String> + Send + 'static,
        mut finish: impl FnMut(WindowsCleanupLevel, u64, Option<String>) + Send + 'static,
    ) -> std::io::Result<(Self, std::thread::JoinHandle<()>)> {
        let (wake, receiver) = crossbeam_channel::bounded(1);
        let inner = std::sync::Arc::new(BoundedCleanupInner {
            requested: std::sync::atomic::AtomicU8::new(0),
            wake,
            shutdown: std::sync::atomic::AtomicBool::new(false),
            all_requested_generation: std::sync::atomic::AtomicU64::new(0),
            all_state: std::sync::Mutex::new(WindowsAllCleanupState::default()),
            all_generation_changed: std::sync::Condvar::new(),
            #[cfg(test)]
            all_wait_hook: std::sync::Mutex::new(None),
        });
        let worker_inner = std::sync::Arc::clone(&inner);
        let worker = std::thread::Builder::new()
            .name("wisp-windows-session-cleanup".into())
            .spawn(move || {
                while receiver.recv().is_ok() {
                    let requested = worker_inner
                        .requested
                        .swap(0, std::sync::atomic::Ordering::AcqRel);
                    let level = match requested {
                        value if value == WindowsCleanupLevel::SpeechOnly as u8 => {
                            Some(WindowsCleanupLevel::SpeechOnly)
                        },
                        value if value == WindowsCleanupLevel::All as u8 => {
                            Some(WindowsCleanupLevel::All)
                        },
                        _ => None,
                    };
                    if let Some(level) = level {
                        let generation = if level == WindowsCleanupLevel::All {
                            worker_inner
                                .all_requested_generation
                                .load(std::sync::atomic::Ordering::Acquire)
                        } else {
                            0
                        };
                        let mut error = None;
                        for attempt in 0..WINDOWS_CLEANUP_MAX_ATTEMPTS {
                            error = cleanup(level);
                            if error.is_none() {
                                break;
                            }
                            if attempt + 1 < WINDOWS_CLEANUP_MAX_ATTEMPTS {
                                std::thread::park_timeout(std::time::Duration::from_millis(
                                    1 << attempt,
                                ));
                            }
                        }
                        finish(level, generation, error);
                        if level == WindowsCleanupLevel::All {
                            let mut state = worker_inner
                                .all_state
                                .lock()
                                .unwrap_or_else(std::sync::PoisonError::into_inner);
                            state.completed_generation = generation;
                            worker_inner.all_generation_changed.notify_all();
                        }
                    }
                    if worker_inner
                        .shutdown
                        .load(std::sync::atomic::Ordering::Acquire)
                        && worker_inner
                            .requested
                            .load(std::sync::atomic::Ordering::Acquire)
                            == 0
                    {
                        break;
                    }
                }
            })?;
        Ok((Self(inner), worker))
    }

    fn request(
        &self,
        level: WindowsCleanupLevel,
    ) -> (u64, bool) {
        if level == WindowsCleanupLevel::All {
            return self.request_all_generation();
        }
        self.0
            .requested
            .fetch_max(level as u8, std::sync::atomic::Ordering::AcqRel);
        let _ = self.0.wake.try_send(());
        (0, true)
    }

    fn request_all_generation(&self) -> (u64, bool) {
        let mut state = self
            .0
            .all_state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if state.generation > 0 {
            return (state.generation, false);
        }
        state.generation = 1;
        self.0
            .all_requested_generation
            .store(state.generation, std::sync::atomic::Ordering::Release);
        self.0.requested.fetch_max(
            WindowsCleanupLevel::All as u8,
            std::sync::atomic::Ordering::AcqRel,
        );
        self.0.all_generation_changed.notify_all();
        drop(state);
        let _ = self.0.wake.try_send(());
        (1, true)
    }

    fn wait_for_all_generation(&self) -> u64 {
        let mut state = self
            .0
            .all_state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        while state.generation == 0 {
            state = self
                .0
                .all_generation_changed
                .wait(state)
                .unwrap_or_else(std::sync::PoisonError::into_inner);
        }
        state.generation
    }

    fn wait_for_all_completion(
        &self,
        generation: u64,
    ) {
        let mut state = self
            .0
            .all_state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        while state.completed_generation < generation {
            #[cfg(test)]
            let hook = {
                self.0
                    .all_wait_hook
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .take()
            };
            #[cfg(test)]
            if let Some(hook) = hook {
                let _ = hook.observed.send(generation);
                let _ = hook.release.recv();
            }
            state = self
                .0
                .all_generation_changed
                .wait(state)
                .unwrap_or_else(std::sync::PoisonError::into_inner);
        }
    }

    #[cfg(test)]
    fn set_all_wait_hook(
        &self,
        hook: std::sync::Arc<WindowsAllWaitHook>,
    ) {
        *self
            .0
            .all_wait_hook
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(hook);
    }

    fn shutdown(&self) {
        self.0
            .shutdown
            .store(true, std::sync::atomic::Ordering::Release);
        let _ = self.0.wake.try_send(());
    }
}

#[cfg(any(test, target_os = "windows"))]
#[derive(Debug, Clone)]
struct WindowsCleanupIntent {
    notification: WindowsRuntimeNotification,
    lifecycle: std::sync::Arc<OneShotSessionLifecycle>,
}

#[cfg(any(test, target_os = "windows"))]
#[derive(Debug, Clone, Default)]
struct WindowsCleanupReport {
    error: Option<String>,
    notification: Option<WindowsRuntimeNotification>,
}

#[cfg(any(test, target_os = "windows"))]
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
struct WindowsSpeechCleanupProgress(u8);

#[cfg(any(test, target_os = "windows"))]
impl WindowsSpeechCleanupProgress {
    const STOP_ASYNC: u8 = 1 << 0;
    const COMPLETED_REMOVED: u8 = 1 << 1;
    const RESULT_REMOVED: u8 = 1 << 2;
    const CLOSED: u8 = 1 << 3;

    const fn contains(
        self,
        step: u8,
    ) -> bool {
        self.0 & step != 0
    }

    fn mark(
        &mut self,
        step: u8,
    ) {
        self.0 |= step;
    }
}

#[cfg(any(test, target_os = "windows"))]
const WINDOWS_SPEECH_RUNTIME_CLEANUP: u8 = WindowsSpeechCleanupProgress::STOP_ASYNC
    | WindowsSpeechCleanupProgress::COMPLETED_REMOVED
    | WindowsSpeechCleanupProgress::RESULT_REMOVED
    | WindowsSpeechCleanupProgress::CLOSED;

#[cfg(any(test, target_os = "windows"))]
fn run_windows_speech_cleanup(
    progress: &mut WindowsSpeechCleanupProgress,
    required: u8,
    mut stop_async: impl FnMut() -> std::result::Result<(), String>,
    mut remove_completed: impl FnMut() -> std::result::Result<(), String>,
    mut remove_result: impl FnMut() -> std::result::Result<(), String>,
    mut close: impl FnMut() -> std::result::Result<(), String>,
) -> std::result::Result<(), String> {
    let mut errors = Vec::new();
    if required & WindowsSpeechCleanupProgress::STOP_ASYNC != 0
        && !progress.contains(WindowsSpeechCleanupProgress::STOP_ASYNC)
    {
        match stop_async() {
            Ok(()) => progress.mark(WindowsSpeechCleanupProgress::STOP_ASYNC),
            Err(error) => errors.push(format!("StopAsync: {error}")),
        }
    }
    if required & WindowsSpeechCleanupProgress::COMPLETED_REMOVED != 0
        && !progress.contains(WindowsSpeechCleanupProgress::COMPLETED_REMOVED)
    {
        match remove_completed() {
            Ok(()) => progress.mark(WindowsSpeechCleanupProgress::COMPLETED_REMOVED),
            Err(error) => errors.push(format!("RemoveCompleted: {error}")),
        }
    }
    if required & WindowsSpeechCleanupProgress::RESULT_REMOVED != 0
        && !progress.contains(WindowsSpeechCleanupProgress::RESULT_REMOVED)
    {
        match remove_result() {
            Ok(()) => progress.mark(WindowsSpeechCleanupProgress::RESULT_REMOVED),
            Err(error) => errors.push(format!("RemoveResultGenerated: {error}")),
        }
    }
    if required & WindowsSpeechCleanupProgress::CLOSED != 0
        && !progress.contains(WindowsSpeechCleanupProgress::CLOSED)
    {
        match close() {
            Ok(()) => progress.mark(required),
            Err(error) => errors.push(format!("Close: {error}")),
        }
    }
    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors.join("; "))
    }
}

#[cfg(any(test, target_os = "windows"))]
#[derive(Debug, Clone, Copy)]
struct WindowsSpeechStartupCleanupState {
    start_attempted: bool,
    completed_token: bool,
    result_token: bool,
}

#[cfg(any(test, target_os = "windows"))]
fn run_windows_speech_startup_cleanup(
    progress: &mut WindowsSpeechCleanupProgress,
    state: WindowsSpeechStartupCleanupState,
    stop_async: impl FnMut() -> std::result::Result<(), String>,
    remove_completed: impl FnMut() -> std::result::Result<(), String>,
    remove_result: impl FnMut() -> std::result::Result<(), String>,
    close: impl FnMut() -> std::result::Result<(), String>,
) -> std::result::Result<(), String> {
    let mut required = WindowsSpeechCleanupProgress::CLOSED;
    if state.start_attempted {
        required |= WindowsSpeechCleanupProgress::STOP_ASYNC;
    }
    if state.completed_token {
        required |= WindowsSpeechCleanupProgress::COMPLETED_REMOVED;
    }
    if state.result_token {
        required |= WindowsSpeechCleanupProgress::RESULT_REMOVED;
    }
    run_windows_speech_cleanup(
        progress,
        required,
        stop_async,
        remove_completed,
        remove_result,
        close,
    )
}

#[cfg(any(test, target_os = "windows"))]
fn run_windows_speech_startup_cleanup_bounded(
    progress: &mut WindowsSpeechCleanupProgress,
    state: WindowsSpeechStartupCleanupState,
    mut stop_async: impl FnMut() -> std::result::Result<(), String>,
    mut remove_completed: impl FnMut() -> std::result::Result<(), String>,
    mut remove_result: impl FnMut() -> std::result::Result<(), String>,
    mut close: impl FnMut() -> std::result::Result<(), String>,
) -> std::result::Result<(), String> {
    for attempt in 0..WINDOWS_CLEANUP_MAX_ATTEMPTS {
        let result = run_windows_speech_startup_cleanup(
            progress,
            state,
            &mut stop_async,
            &mut remove_completed,
            &mut remove_result,
            &mut close,
        );
        if result.is_ok() || attempt + 1 == WINDOWS_CLEANUP_MAX_ATTEMPTS {
            return result;
        }
        std::thread::park_timeout(std::time::Duration::from_millis(1 << attempt));
    }
    unreachable!("Windows startup cleanup retry budget is nonzero")
}

#[cfg(any(test, target_os = "windows"))]
fn windows_speech_start_error(
    primary: impl std::fmt::Display,
    cleanup: std::result::Result<(), String>,
) -> crate::error::SessionError {
    let mut message = primary.to_string();
    if let Err(error) = cleanup {
        let _ = std::fmt::Write::write_fmt(
            &mut message,
            format_args!("; Windows speech startup cleanup failed: {error}"),
        );
    }
    crate::error::SessionError::Start(message)
}

#[cfg(any(test, target_os = "windows"))]
fn aggregate_windows_cleanup_errors(
    speech: std::result::Result<(), String>,
    recording: Option<std::result::Result<(), String>>,
) -> Option<String> {
    let mut errors = Vec::new();
    if let Err(error) = speech {
        errors.push(format!("Windows speech cleanup failed: {error}"));
    }
    if let Some(Err(error)) = recording {
        errors.push(format!("WASAPI recording cleanup failed: {error}"));
    }
    (!errors.is_empty()).then(|| errors.join("; "))
}

#[cfg(any(test, target_os = "windows"))]
fn run_windows_session_cleanup(
    level: WindowsCleanupLevel,
    recording_complete: &mut bool,
    stop_speech: impl FnOnce() -> std::result::Result<(), String>,
    stop_recording: impl FnOnce() -> std::result::Result<(), String>,
) -> Option<String> {
    let speech = stop_speech();
    let recording = (level == WindowsCleanupLevel::All && !*recording_complete).then(|| {
        let result = stop_recording();
        if result.is_ok() {
            *recording_complete = true;
        }
        result
    });
    aggregate_windows_cleanup_errors(speech, recording)
}

#[cfg(any(test, target_os = "windows"))]
fn finalize_windows_cleanup_notification(
    intent: Option<WindowsCleanupIntent>,
    level: WindowsCleanupLevel,
    cleanup_error: Option<&str>,
) -> Option<WindowsRuntimeNotification> {
    let intent = intent?;
    match intent.notification {
        WindowsRuntimeNotification::ContinueRecording(_) if level == WindowsCleanupLevel::All => {
            None
        },
        WindowsRuntimeNotification::ContinueRecording(_)
            if intent.lifecycle.all_cleanup_is_claimed() =>
        {
            None
        },
        WindowsRuntimeNotification::ContinueRecording(mut message) => {
            if let Some(error) = cleanup_error {
                let _ = std::fmt::Write::write_fmt(
                    &mut message,
                    format_args!("; speech cleanup failed: {error}"),
                );
            }
            Some(WindowsRuntimeNotification::ContinueRecording(message))
        },
        WindowsRuntimeNotification::Fatal(mut message) => {
            if let Some(error) = cleanup_error {
                let _ = std::fmt::Write::write_fmt(
                    &mut message,
                    format_args!("; session cleanup failed: {error}"),
                );
            }
            Some(WindowsRuntimeNotification::Fatal(message))
        },
    }
}

#[cfg(any(test, target_os = "windows"))]
struct WindowsRuntimeControlPublisher {
    lifecycle: std::sync::Arc<OneShotSessionLifecycle>,
    cleanup_intents: crossbeam_channel::Sender<WindowsCleanupIntent>,
    cleanup: BoundedCleanupHandle,
    policy: TranscriptionPolicy,
}

#[cfg(any(test, target_os = "windows"))]
impl WindowsRuntimeControlPublisher {
    fn new(
        lifecycle: std::sync::Arc<OneShotSessionLifecycle>,
        cleanup_intents: crossbeam_channel::Sender<WindowsCleanupIntent>,
        cleanup: BoundedCleanupHandle,
        policy: TranscriptionPolicy,
    ) -> Self {
        Self {
            lifecycle,
            cleanup_intents,
            cleanup,
            policy,
        }
    }

    fn publish(
        &self,
        status: &str,
    ) -> bool {
        let notification = windows_runtime_completion_notification(self.policy, status);
        let continue_recording = matches!(
            notification,
            WindowsRuntimeNotification::ContinueRecording(_)
        );
        let lifecycle = std::sync::Arc::clone(&self.lifecycle);
        let claim = lifecycle.claim_completion(continue_recording, |claim| {
            let notification = if claim == WindowsCompletionClaim::AbortStartup {
                WindowsRuntimeNotification::Fatal(format!(
                    "platform dictation completed before Windows session startup committed ({status})"
                ))
            } else {
                notification
            };
            let _ = self.cleanup_intents.try_send(WindowsCleanupIntent {
                notification,
                lifecycle: std::sync::Arc::clone(&lifecycle),
            });
            self.cleanup
                .request(if claim == WindowsCompletionClaim::ContinueRecording {
                    WindowsCleanupLevel::SpeechOnly
                } else {
                    WindowsCleanupLevel::All
                });
        });
        claim != WindowsCompletionClaim::Suppressed
    }
}

#[cfg(any(test, target_os = "windows"))]
fn format_windows_runtime_notification(control: WindowsRuntimeNotification) -> String {
    match control {
        WindowsRuntimeNotification::ContinueRecording(message) => format!("[WIN] {message}"),
        WindowsRuntimeNotification::Fatal(message) => format!("[FATAL] {message}"),
    }
}

#[cfg(any(test, target_os = "windows"))]
fn complete_windows_start_transaction(
    lifecycle: &OneShotSessionLifecycle,
    wait_for_cleanup: impl FnOnce() -> WindowsCleanupReport,
) -> crate::error::Result<()> {
    if lifecycle.complete_start() {
        return Ok(());
    }
    let WindowsCleanupReport {
        error,
        notification,
    } = wait_for_cleanup();
    let notification_present = notification.is_some();
    let mut reason = match notification {
        Some(
            WindowsRuntimeNotification::Fatal(message)
            | WindowsRuntimeNotification::ContinueRecording(message),
        ) => message,
        None if lifecycle.startup_was_aborted() => {
            "Windows speech completed before session startup committed".into()
        },
        None => "Windows session startup lost lifecycle ownership".into(),
    };
    if !notification_present && let Some(error) = error {
        let _ = std::fmt::Write::write_fmt(
            &mut reason,
            format_args!("; session cleanup failed: {error}"),
        );
    }
    Err(crate::error::SessionError::Start(reason))
}

#[cfg(any(test, target_os = "windows"))]
fn claim_and_wait_windows_stop(
    lifecycle: &OneShotSessionLifecycle,
    request_all_cleanup: impl FnOnce() -> (u64, bool),
    wait_for_all_cleanup: impl FnOnce(u64) -> WindowsCleanupReport,
) -> (bool, bool, Option<WindowsCleanupReport>) {
    let claimed = lifecycle.claim_stop();
    let Some((generation, started_retry)) =
        lifecycle.all_cleanup_is_claimed().then(request_all_cleanup)
    else {
        return (claimed, false, None);
    };
    (
        claimed,
        started_retry,
        Some(wait_for_all_cleanup(generation)),
    )
}

#[cfg(any(test, target_os = "windows"))]
fn finish_unwound_windows_start(
    lifecycle: &OneShotSessionLifecycle,
    finish_startup: impl FnOnce(),
    request_all_cleanup: impl FnOnce() -> (u64, bool),
    wait_for_all_cleanup: impl FnOnce(u64) -> WindowsCleanupReport,
) {
    finish_startup();
    let _ = claim_and_wait_windows_stop(lifecycle, request_all_cleanup, wait_for_all_cleanup);
}

#[cfg(any(test, target_os = "windows"))]
fn start_windows_platform<R, S, E, SpeechError, CleanupError>(
    policy: TranscriptionPolicy,
    start_recording: impl FnOnce() -> std::result::Result<R, E>,
    start_speech: impl FnOnce() -> std::result::Result<S, SpeechError>,
    stop_recording: impl FnOnce(R) -> std::result::Result<(), CleanupError>,
) -> std::result::Result<WindowsPlatformStart<R, S>, WindowsPlatformStartError<E>>
where
    SpeechError: std::fmt::Display,
    CleanupError: std::fmt::Display,
{
    let recording = start_recording().map_err(WindowsPlatformStartError::Recording)?;
    match start_speech() {
        Ok(speech) => Ok(WindowsPlatformStart::Platform { recording, speech }),
        Err(error) => {
            let speech_error = error.to_string();
            match windows_runtime_failure_action(policy) {
                WindowsRuntimeFailureAction::ContinueRecording { reason } => {
                    Ok(WindowsPlatformStart::RecordOnly {
                        recording,
                        speech_error,
                        reason,
                    })
                },
                WindowsRuntimeFailureAction::FailStart { reason } => {
                    let failure =
                        format!("platform dictation unavailable ({speech_error}); {reason}");
                    let failure = match stop_recording(recording) {
                        Ok(()) => failure,
                        Err(error) => {
                            format!("{failure}; recording cleanup failed: {error}")
                        },
                    };
                    Err(WindowsPlatformStartError::Speech(failure))
                },
            }
        },
    }
}

#[cfg(any(test, target_os = "windows"))]
fn windows_transcriber_probes() -> [TranscriberProbe; 2] {
    [
        TranscriberProbe {
            backend_id: BackendId::new("windows-platform-online"),
            class: TranscriberClass::Platform,
            availability: Availability::Available,
            capabilities: TranscriberCapabilities {
                privacy: RecognitionPrivacy::Online,
                features: vec![
                    TranscriberFeature::Streaming,
                    TranscriberFeature::SegmentTimestamps,
                ],
            },
        },
        TranscriberProbe {
            backend_id: BackendId::new("windows-local-model"),
            class: TranscriberClass::LocalModel,
            availability: Availability::Unavailable(UnavailableReason::InitializationFailed(
                "Windows local-model inference is not connected".into(),
            )),
            capabilities: TranscriberCapabilities {
                privacy: RecognitionPrivacy::Offline,
                features: vec![TranscriberFeature::SegmentTimestamps],
            },
        },
    ]
}

#[cfg(any(test, target_os = "windows"))]
fn windows_mode_from_selection(
    selection: TranscriptionSelection
) -> std::result::Result<WindowsTranscriptionMode, String> {
    match selection {
        TranscriptionSelection::Backend(id) if id.as_str() == "windows-platform-online" => {
            Ok(WindowsTranscriptionMode::PlatformOnline)
        },
        TranscriptionSelection::Backend(id) => Err(format!(
            "Windows transcription backend is not connected: {id}"
        )),
        TranscriptionSelection::RecordOnly { reason } => {
            Ok(WindowsTranscriptionMode::RecordOnly { reason })
        },
        TranscriptionSelection::Unavailable { reason } => Err(reason),
    }
}

/// Metadata for the local model offered by the setup screen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LocalModelSpec {
    pub name: &'static str,
    pub filename: &'static str,
    pub url: &'static str,
    pub approx_bytes: u64,
}

/// Current filesystem state for the local model.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LocalModelStatus {
    Missing {
        spec: LocalModelSpec,
        path: PathBuf,
    },
    Ready {
        spec: LocalModelSpec,
        path: PathBuf,
        bytes: u64,
    },
}

impl LocalModelStatus {
    #[must_use]
    pub fn is_ready(&self) -> bool {
        matches!(self, Self::Ready { .. })
    }

    #[must_use]
    pub fn path(&self) -> &Path {
        match self {
            Self::Missing { path, .. } | Self::Ready { path, .. } => path,
        }
    }
}

const LOCAL_MODEL_SPEC: LocalModelSpec = LocalModelSpec {
    name: "Whisper base",
    filename: "ggml-base.bin",
    url: "https://huggingface.co/ggerganov/whisper.cpp/resolve/main/ggml-base.bin",
    approx_bytes: 142 * 1024 * 1024,
};

#[must_use]
pub const fn local_model_spec() -> LocalModelSpec {
    LOCAL_MODEL_SPEC
}

#[must_use]
pub fn local_model_path(data_dir: impl AsRef<Path>) -> PathBuf {
    data_dir
        .as_ref()
        .join("models")
        .join(local_model_spec().filename)
}

#[must_use]
pub fn local_model_status(data_dir: impl AsRef<Path>) -> LocalModelStatus {
    let spec = local_model_spec();
    let path = local_model_path(data_dir);
    match std::fs::metadata(&path) {
        Ok(meta) if meta.is_file() && meta.len() > 0 => LocalModelStatus::Ready {
            spec,
            path,
            bytes: meta.len(),
        },
        _ => LocalModelStatus::Missing { spec, path },
    }
}

/// Download the default local model into Wisp's data directory.
///
/// This is intentionally blocking; UI callers should run it on a background
/// executor and then refresh [`local_model_status`].
///
/// # Errors
/// Returns [`SetupError`] if the model directory cannot be created, the
/// download command fails, or the temporary file cannot be moved into place.
pub fn download_local_model(data_dir: impl AsRef<Path>) -> SetupResult<LocalModelStatus> {
    let data_dir = data_dir.as_ref();
    let final_path = local_model_path(data_dir);
    let Some(model_dir) = final_path.parent() else {
        return Err(SetupError::Install(format!(
            "invalid model path: {}",
            final_path.display()
        )));
    };
    std::fs::create_dir_all(model_dir).map_err(|err| SetupError::CreateModelDirectory {
        path: model_dir.to_path_buf(),
        message: err.to_string(),
    })?;

    let part_path = final_path.with_extension("bin.part");
    let _ = std::fs::remove_file(&part_path);
    download_url(local_model_spec().url, &part_path)?;
    std::fs::rename(&part_path, &final_path)
        .or_else(|_| {
            std::fs::copy(&part_path, &final_path)?;
            std::fs::remove_file(&part_path)
        })
        .map_err(|err| SetupError::Install(err.to_string()))?;
    Ok(local_model_status(data_dir))
}

#[must_use]
pub const fn requires_recognizer_setup() -> bool {
    cfg!(target_os = "windows")
}

#[must_use]
pub const fn platform_recognizer_label() -> &'static str {
    if cfg!(target_os = "windows") {
        "Windows.Media.SpeechRecognition"
    } else if cfg!(target_os = "macos") {
        "Apple SpeechAnalyzer"
    } else {
        "Platform recognizer"
    }
}

fn download_url(
    url: &str,
    destination: &Path,
) -> SetupResult<()> {
    if let Some(path) = url.strip_prefix("file://") {
        std::fs::copy(path, destination)
            .map(|_| ())
            .map_err(|err| SetupError::Download(err.to_string()))?;
        return Ok(());
    }
    #[cfg(target_os = "windows")]
    let mut cmd = {
        let mut cmd = std::process::Command::new("powershell.exe");
        cmd.args([
            "-NoProfile",
            "-ExecutionPolicy",
            "Bypass",
            "-Command",
            "$ProgressPreference='SilentlyContinue'; Invoke-WebRequest -Uri $args[0] -OutFile $args[1]",
            url,
        ]);
        cmd.arg(destination);
        cmd
    };
    #[cfg(not(target_os = "windows"))]
    let mut cmd = {
        let mut cmd = std::process::Command::new("curl");
        cmd.args(["--fail", "--location", "--show-error", "--output"]);
        cmd.arg(destination);
        cmd.arg(url);
        cmd
    };
    let output = cmd
        .output()
        .map_err(|err| SetupError::Download(err.to_string()))?;
    if output.status.success() {
        return Ok(());
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    Err(SetupError::Download(stderr.trim().to_string()))
}

#[cfg(target_os = "macos")]
mod imp {
    use std::ffi::{CStr, CString};
    use std::path::Path;
    use std::ptr::NonNull;
    use std::time::Duration;

    use wisp_audiokit_sys as sys;
    use wisp_core::{
        AudioFrame, CaptureEvent, MonotonicTimestamp, SourceKind, SourceLabel, TrackId,
    };

    use crate::error::{Result, SessionError};
    use crate::{
        CallbackEnqueue, CallbackEventClass, CallbackEventReceiver, CallbackEventSender,
        CaptureEventReceiver, MacosCaptureFailure, MacosTranscriberFailure, Permission,
        PermissionStatus, RealtimeCaptureSender, SessionConfig,
        callback_event_channel_with_final_gap, realtime_capture_channel,
    };

    const MACOS_CAPTURE_QUEUE_CAPACITY: usize = 64;

    fn permission_to_raw(perm: Permission) -> i32 {
        match perm {
            Permission::Microphone => sys::WISP_PERMISSION_MICROPHONE,
            Permission::SpeechRecognition => sys::WISP_PERMISSION_SPEECH_RECOGNITION,
        }
    }

    fn status_from_raw(raw: i32) -> PermissionStatus {
        match raw {
            sys::WISP_PERMISSION_STATUS_GRANTED => PermissionStatus::Granted,
            sys::WISP_PERMISSION_STATUS_DENIED => PermissionStatus::Denied,
            sys::WISP_PERMISSION_STATUS_RESTRICTED => PermissionStatus::Restricted,
            // Treat negative ("invalid permission id") as undetermined too —
            // we never pass an invalid id from safe Rust, and conflating the
            // two keeps the surface tidy.
            _ => PermissionStatus::Undetermined,
        }
    }

    /// Read the current status of `permission` from the OS. Never prompts.
    #[must_use]
    pub fn check_permission(permission: Permission) -> PermissionStatus {
        // SAFETY: simple value-in, value-out call into Swift; no pointers.
        let raw = unsafe { sys::wisp_permission_status(permission_to_raw(permission)) };
        status_from_raw(raw)
    }

    /// Show the OS permission prompt for `permission` (only if the user has
    /// not been asked yet) and block until they respond. Returns the
    /// resulting status. If the status is already determined, returns it
    /// immediately without prompting.
    ///
    /// Safe to call from any thread; the macOS APIs marshal the dialog to
    /// the main thread internally. Callers from a UI event loop should run
    /// this on a worker thread to keep the UI responsive while the user
    /// reads the prompt.
    #[must_use]
    pub fn request_permission(permission: Permission) -> PermissionStatus {
        // SAFETY: simple value-in, value-out call into Swift; no pointers.
        let raw = unsafe { sys::wisp_permission_request(permission_to_raw(permission)) };
        status_from_raw(raw)
    }

    /// `WispAudioKit` library version (e.g. `"0.1.0"`).
    ///
    /// # Panics
    /// Panics if the Swift side's version string is not valid UTF-8. It
    /// ships as a static ASCII constant, so this only fires on build-time
    /// binary corruption.
    #[must_use]
    #[allow(clippy::expect_used)]
    pub fn version() -> &'static str {
        // SAFETY: returns a static UTF-8 C string that lives forever.
        unsafe {
            let ptr = sys::wisp_audiokit_version();
            CStr::from_ptr(ptr)
                .to_str()
                .expect("`WispAudioKit` version is valid UTF-8")
        }
    }

    // ---- Types ---------------------------------------------------------

    /// One transcription update from a running macOS native session.
    #[derive(Debug, Clone, PartialEq)]
    pub struct SessionResult {
        pub source: SourceLabel,
        pub segment_id: u64,
        pub is_final: bool,
        pub text: String,
        pub start_seconds: f64,
        pub end_seconds: f64,
        pub confidence_mean: Option<f64>,
        pub confidence_min: Option<f64>,
    }

    /// Either a transcription result or a log line emitted by the session.
    #[derive(Debug, Clone, PartialEq)]
    pub enum Event {
        Result(SessionResult),
        Log(String),
    }

    fn final_gap_event(dropped_finals: u64) -> Event {
        Event::Log(format!(
            "[FATAL] transcription gap: dropped {dropped_finals} final result(s)"
        ))
    }

    fn transcriber_failure_gap_event(dropped_terminal_failures: u64) -> MacosTranscriberFailure {
        MacosTranscriberFailure {
            terminal: true,
            message: format!(
                "transcriber failure queue overflowed; dropped {dropped_terminal_failures} terminal failure(s)"
            ),
        }
    }

    // ---- Session -------------------------------------------------------

    /// Low-level native handle used by the production macOS backends.
    ///
    /// This type is crate-private so the public compatibility [`crate::Session`]
    /// cannot bypass [`crate::MacosSession`] and its backend orchestrator.
    pub(crate) struct NativeSession {
        handle: NonNull<sys::WispSession>,
        receiver: CallbackEventReceiver<Event, (SourceLabel, u64)>,
        transcriber_failure_receiver: CallbackEventReceiver<MacosTranscriberFailure, u64>,
        capture_failure_receiver: CallbackEventReceiver<MacosCaptureFailure, u64>,
        audio_receiver: Option<CaptureEventReceiver>,
        // Kept alive so the callbacks' user_data pointer stays valid for
        // as long as the Swift side might call them.
        ctx: Box<CallbackContext>,
    }

    // SAFETY: Session owns the C handle and the receiver. The handle is
    // an opaque pointer we never deref ourselves; the C side serializes
    // access internally, so it is sound to move the handle across threads.
    // (`Session` stays `!Sync` overall because the `NonNull` field is
    // `!Sync` — only `Send` needs the manual impl.)
    unsafe impl Send for NativeSession {}

    // Swift may invoke `on_result_thunk` / `on_log_thunk` from different
    // threads. The thunks form `&CallbackContext` from a raw `user_data`
    // pointer, so `CallbackContext` must be `Sync`. The bounded callback
    // sender is `Sync`, which lets those callbacks fire concurrently without
    // UB while retaining nonblocking backpressure.
    struct CallbackContext {
        sender: CallbackEventSender<Event, (SourceLabel, u64)>,
        transcriber_failure_sender: CallbackEventSender<MacosTranscriberFailure, u64>,
        capture_failure_sender: CallbackEventSender<MacosCaptureFailure, u64>,
        audio_senders: Option<[RealtimeCaptureSender; 2]>,
        first_audio_timestamps: [std::sync::atomic::AtomicU64; 2],
    }

    const UNSET_AUDIO_TIMESTAMP_BITS: u64 = f64::NAN.to_bits();

    impl CallbackContext {
        fn audio_track_index(track_id: TrackId) -> Option<usize> {
            match track_id {
                TrackId::MICROPHONE => Some(0),
                TrackId::SYSTEM => Some(1),
                _ => None,
            }
        }

        fn remember_first_audio_timestamp(
            &self,
            track_id: TrackId,
            timestamp_seconds: f64,
        ) {
            let Some(index) = Self::audio_track_index(track_id) else {
                return;
            };
            let _ = self.first_audio_timestamps[index].compare_exchange(
                UNSET_AUDIO_TIMESTAMP_BITS,
                timestamp_seconds.to_bits(),
                std::sync::atomic::Ordering::AcqRel,
                std::sync::atomic::Ordering::Acquire,
            );
        }

        fn first_audio_timestamp(
            &self,
            track_id: TrackId,
        ) -> Option<MonotonicTimestamp> {
            let index = Self::audio_track_index(track_id)?;
            let bits =
                self.first_audio_timestamps[index].load(std::sync::atomic::Ordering::Acquire);
            (bits != UNSET_AUDIO_TIMESTAMP_BITS).then(|| {
                MonotonicTimestamp::from_duration(Duration::from_secs_f64(f64::from_bits(bits)))
            })
        }
    }

    impl NativeSession {
        pub(crate) fn new_for_backend(
            output_dir: impl AsRef<Path>,
            config: SessionConfig,
            transcription_enabled: bool,
            emit_audio: bool,
            allow_record_only: bool,
        ) -> Result<Self> {
            let output_dir = output_dir.as_ref();
            let path_str = output_dir
                .to_str()
                .ok_or_else(|| SessionError::InvalidPath(output_dir.to_path_buf()))?;
            let path_c = CString::new(path_str)
                .map_err(|_| SessionError::InvalidPath(output_dir.to_path_buf()))?;
            let locale_c = CString::new(config.locale.clone())
                .map_err(|_| SessionError::InvalidLocale(config.locale))?;

            let (sender, receiver) = callback_event_channel_with_final_gap(final_gap_event);
            let (transcriber_failure_sender, transcriber_failure_receiver) =
                callback_event_channel_with_final_gap(transcriber_failure_gap_event);
            let (capture_failure_sender, capture_failure_receiver) =
                callback_event_channel_with_final_gap(|dropped| MacosCaptureFailure {
                    track_id: None,
                    message: format!(
                        "terminal capture failure queue overflowed; dropped {dropped} failure(s)"
                    ),
                });
            let (audio_senders, audio_receiver) = if emit_audio {
                let (senders, receiver) = realtime_capture_channel(
                    MACOS_CAPTURE_QUEUE_CAPACITY,
                    &[TrackId::MICROPHONE, TrackId::SYSTEM],
                );
                (
                    Some([senders[0].clone(), senders[1].clone()]),
                    Some(receiver),
                )
            } else {
                (None, None)
            };
            let ctx = Box::new(CallbackContext {
                sender,
                transcriber_failure_sender,
                capture_failure_sender,
                audio_senders,
                first_audio_timestamps: std::array::from_fn(|_| {
                    std::sync::atomic::AtomicU64::new(UNSET_AUDIO_TIMESTAMP_BITS)
                }),
            });
            let user_data = std::ptr::from_ref::<CallbackContext>(ctx.as_ref()) as *mut _;

            // SAFETY: pointers are valid for the duration of the call and
            // `user_data` is kept alive by holding `ctx` in `Session`.
            let raw = unsafe {
                sys::wisp_session_new_v2(
                    path_c.as_ptr(),
                    locale_c.as_ptr(),
                    i32::from(transcription_enabled),
                    i32::from(allow_record_only),
                    Some(on_result_thunk),
                    emit_audio.then_some(on_audio_thunk),
                    emit_audio.then_some(on_audio_overflow_thunk),
                    Some(on_transcriber_error_thunk),
                    Some(on_terminal_error_thunk),
                    Some(on_log_thunk),
                    user_data,
                )
            };
            let handle = NonNull::new(raw).ok_or(SessionError::Construction)?;
            Ok(Self {
                handle,
                receiver,
                transcriber_failure_receiver,
                capture_failure_receiver,
                audio_receiver,
                ctx,
            })
        }

        /// Whether microphone capture reached the running state. This is
        /// useful after `start()` fails to distinguish an unstarted session
        /// from one containing partial output that should be preserved.
        #[must_use]
        pub fn has_started_capture(&self) -> bool {
            // SAFETY: handle is non-null and the getter does not mutate the
            // Swift session.
            unsafe { sys::wisp_session_has_started_capture(self.handle.as_ptr()) != 0 }
        }

        /// Replace microphone samples with silence while system capture keeps
        /// running. The audio timelines remain continuous and aligned.
        pub fn set_microphone_muted(
            &self,
            muted: bool,
        ) {
            // SAFETY: handle is non-null and the Swift side stores this flag
            // behind a lock shared with the microphone callback.
            unsafe {
                sys::wisp_session_set_microphone_muted(self.handle.as_ptr(), i32::from(muted));
            }
        }

        pub(crate) fn push_transcriber_frame(
            &self,
            frame: &AudioFrame,
        ) -> Result<()> {
            let source = match (frame.track_id(), frame.source()) {
                (TrackId::MICROPHONE, SourceKind::Microphone) => sys::WISP_SOURCE_MIC,
                (TrackId::SYSTEM, SourceKind::SystemAudio) => sys::WISP_SOURCE_SYSTEM,
                _ => {
                    return Err(SessionError::Start(
                        "frame is not a macOS microphone/system track".into(),
                    ));
                },
            };
            let Some(samples) = frame.samples().as_f32() else {
                return Err(SessionError::Start(
                    "macOS SpeechAnalyzer requires Float32 PCM".into(),
                ));
            };
            let rc = unsafe {
                sys::wisp_session_push_transcriber_audio(
                    self.handle.as_ptr(),
                    source,
                    frame.format().sample_rate,
                    u32::from(frame.format().channels),
                    samples.as_ptr(),
                    samples.len(),
                )
            };
            if rc == 0 {
                Ok(())
            } else {
                Err(SessionError::Start(self.last_error_detail(rc)))
            }
        }

        pub(crate) fn start_capture(&mut self) -> Result<()> {
            let rc = unsafe { sys::wisp_session_start_capture(self.handle.as_ptr()) };
            if rc == 0 {
                Ok(())
            } else {
                Err(SessionError::Start(self.last_error_detail(rc)))
            }
        }

        pub(crate) fn start_transcription(&self) -> Result<()> {
            let rc = unsafe { sys::wisp_session_start_transcription(self.handle.as_ptr()) };
            if rc == 0 {
                Ok(())
            } else {
                Err(SessionError::Start(self.last_error_detail(rc)))
            }
        }

        pub(crate) fn disable_transcription(&self) -> Result<()> {
            let rc = unsafe { sys::wisp_session_disable_transcription(self.handle.as_ptr()) };
            if rc == 0 {
                Ok(())
            } else {
                Err(SessionError::Start(self.last_error_detail(rc)))
            }
        }

        pub(crate) fn abort(&self) {
            unsafe { sys::wisp_session_abort(self.handle.as_ptr()) };
        }

        pub(crate) fn stop_capture(&self) -> Result<()> {
            let rc = unsafe { sys::wisp_session_stop_capture(self.handle.as_ptr()) };
            if rc == 0 {
                Ok(())
            } else {
                Err(SessionError::Start(self.last_error_detail(rc)))
            }
        }

        pub(crate) fn finish_transcription(&self) -> Result<()> {
            let rc = unsafe { sys::wisp_session_finish_transcription(self.handle.as_ptr()) };
            if rc == 0 {
                Ok(())
            } else {
                Err(SessionError::Start(self.last_error_detail(rc)))
            }
        }

        fn last_error_detail(
            &self,
            rc: i32,
        ) -> String {
            let message = unsafe { sys::wisp_session_last_error_message(self.handle.as_ptr()) };
            if message.is_null() {
                format!("unknown error (rc={rc})")
            } else {
                unsafe { CStr::from_ptr(message) }
                    .to_string_lossy()
                    .into_owned()
            }
        }

        /// Non-blocking event poll.
        #[must_use]
        pub fn try_recv(&self) -> Option<Event> {
            self.receiver.try_recv()
        }

        pub(crate) fn try_recv_audio(&self) -> Option<CaptureEvent> {
            self.audio_receiver
                .as_ref()
                .and_then(CaptureEventReceiver::try_recv)
        }

        pub(crate) fn try_recv_transcriber_failure(&self) -> Option<MacosTranscriberFailure> {
            self.transcriber_failure_receiver.try_recv()
        }

        pub(crate) fn try_recv_capture_failure(&self) -> Option<MacosCaptureFailure> {
            self.capture_failure_receiver.try_recv()
        }

        pub(crate) fn recv_audio_timeout(
            &self,
            timeout: Duration,
        ) -> Option<CaptureEvent> {
            self.audio_receiver
                .as_ref()
                .and_then(|receiver| receiver.recv_timeout(timeout))
        }

        pub(crate) fn first_audio_timestamp(
            &self,
            track_id: TrackId,
        ) -> Option<MonotonicTimestamp> {
            self.ctx.first_audio_timestamp(track_id)
        }
    }

    impl Drop for NativeSession {
        fn drop(&mut self) {
            // SAFETY: handle is non-null and we own it. Stop is a no-op if
            // the session was never started or has already stopped.
            unsafe {
                sys::wisp_session_stop(self.handle.as_ptr());
                sys::wisp_session_free(self.handle.as_ptr());
            }
        }
    }

    // ---- Callback thunks ----------------------------------------------

    unsafe extern "C" fn on_result_thunk(
        source: i32,
        segment_id: u64,
        is_final: i32,
        text_utf8: *const std::os::raw::c_char,
        text_len: usize,
        start_seconds: f64,
        end_seconds: f64,
        confidence_mean: f64,
        confidence_min: f64,
        user_data: *mut std::os::raw::c_void,
    ) {
        if user_data.is_null() {
            return;
        }
        // SAFETY: user_data was set by Session::new to point at a
        // CallbackContext kept alive by Session.
        let ctx = unsafe { &*(user_data.cast::<CallbackContext>()) };
        let text = if text_utf8.is_null() || text_len == 0 {
            String::new()
        } else {
            // SAFETY: Swift guarantees (ptr, len) is a valid UTF-8 slice
            // for the duration of the call.
            let bytes = unsafe { std::slice::from_raw_parts(text_utf8.cast::<u8>(), text_len) };
            String::from_utf8_lossy(bytes).into_owned()
        };
        let label = match source {
            sys::WISP_SOURCE_MIC => SourceLabel::Mic,
            sys::WISP_SOURCE_SYSTEM => SourceLabel::System,
            _ => return,
        };
        let result = SessionResult {
            source: label,
            segment_id,
            is_final: is_final != 0,
            text,
            start_seconds,
            end_seconds,
            confidence_mean: confidence_mean.is_finite().then_some(confidence_mean),
            confidence_min: confidence_min.is_finite().then_some(confidence_min),
        };
        let key = (result.source, result.segment_id);
        let class = if result.is_final {
            CallbackEventClass::Final(Some(key))
        } else {
            CallbackEventClass::Partial(key)
        };
        let was_final = result.is_final;
        let outcome = ctx.sender.try_send(class, Event::Result(result));
        if was_final && outcome == CallbackEnqueue::DroppedFull {
            let _ = ctx.transcriber_failure_sender.try_send(
                CallbackEventClass::Final(Some(segment_id)),
                MacosTranscriberFailure {
                    terminal: true,
                    message: format!(
                        "transcription final callback overflowed for segment {segment_id}"
                    ),
                },
            );
        }
    }

    unsafe extern "C" fn on_log_thunk(
        message_utf8: *const std::os::raw::c_char,
        message_len: usize,
        user_data: *mut std::os::raw::c_void,
    ) {
        if user_data.is_null() {
            return;
        }
        let ctx = unsafe { &*(user_data.cast::<CallbackContext>()) };
        let text = if message_utf8.is_null() || message_len == 0 {
            String::new()
        } else {
            let bytes =
                unsafe { std::slice::from_raw_parts(message_utf8.cast::<u8>(), message_len) };
            String::from_utf8_lossy(bytes).into_owned()
        };
        let _ = ctx
            .sender
            .try_send(CallbackEventClass::Log, Event::Log(text));
    }

    unsafe extern "C" fn on_transcriber_error_thunk(
        terminal: i32,
        message_utf8: *const std::os::raw::c_char,
        message_len: usize,
        user_data: *mut std::os::raw::c_void,
    ) {
        if user_data.is_null() {
            return;
        }
        let ctx = unsafe { &*(user_data.cast::<CallbackContext>()) };
        let message = if message_utf8.is_null() || message_len == 0 {
            "unknown SpeechAnalyzer failure".to_owned()
        } else {
            let bytes =
                unsafe { std::slice::from_raw_parts(message_utf8.cast::<u8>(), message_len) };
            String::from_utf8_lossy(bytes).into_owned()
        };
        let failure = MacosTranscriberFailure {
            terminal: terminal != 0,
            message,
        };
        let class = if failure.terminal {
            CallbackEventClass::Final(Some(0))
        } else {
            CallbackEventClass::Log
        };
        let _ = ctx.transcriber_failure_sender.try_send(class, failure);
    }

    unsafe extern "C" fn on_terminal_error_thunk(
        source: i32,
        message_utf8: *const std::os::raw::c_char,
        message_len: usize,
        user_data: *mut std::os::raw::c_void,
    ) {
        if user_data.is_null() {
            return;
        }
        let ctx = unsafe { &*(user_data.cast::<CallbackContext>()) };
        let message = if message_utf8.is_null() || message_len == 0 {
            "unknown terminal capture failure".to_owned()
        } else {
            let bytes =
                unsafe { std::slice::from_raw_parts(message_utf8.cast::<u8>(), message_len) };
            String::from_utf8_lossy(bytes).into_owned()
        };
        let (track_id, key) = match source {
            sys::WISP_SOURCE_MIC => (Some(TrackId::MICROPHONE), Some(1)),
            sys::WISP_SOURCE_SYSTEM => (Some(TrackId::SYSTEM), Some(2)),
            -1 => (None, Some(0)),
            _ => return,
        };
        let _ = ctx.capture_failure_sender.try_send(
            CallbackEventClass::Final(key),
            MacosCaptureFailure { track_id, message },
        );
    }

    unsafe extern "C" fn on_audio_thunk(
        source: i32,
        sequence: u64,
        timestamp_seconds: f64,
        sample_rate: u32,
        channels: u32,
        samples: *const f32,
        sample_count: usize,
        user_data: *mut std::os::raw::c_void,
    ) {
        if user_data.is_null()
            || samples.is_null()
            || sample_count == 0
            || channels == 0
            || !timestamp_seconds.is_finite()
            || timestamp_seconds < 0.0
            || sample_rate == 0
        {
            return;
        }
        let ctx = unsafe { &*(user_data.cast::<CallbackContext>()) };
        let Some(senders) = &ctx.audio_senders else {
            return;
        };
        let (sender, track_id, source_kind) = match source {
            sys::WISP_SOURCE_MIC => (&senders[0], TrackId::MICROPHONE, SourceKind::Microphone),
            sys::WISP_SOURCE_SYSTEM => (&senders[1], TrackId::SYSTEM, SourceKind::SystemAudio),
            _ => return,
        };
        let Ok(channels) = u16::try_from(channels) else {
            return;
        };
        let timestamp =
            MonotonicTimestamp::from_duration(Duration::from_secs_f64(timestamp_seconds));
        ctx.remember_first_audio_timestamp(track_id, timestamp_seconds);
        let values = unsafe { std::slice::from_raw_parts(samples, sample_count) }.to_vec();
        let Ok(frame) = AudioFrame::from_f32(
            track_id,
            source_kind,
            sequence,
            timestamp,
            sample_rate,
            channels,
            values,
        ) else {
            return;
        };
        let _ = sender.try_send(frame);
    }

    unsafe extern "C" fn on_audio_overflow_thunk(
        source: i32,
        dropped_frames: u64,
        user_data: *mut std::os::raw::c_void,
    ) {
        if user_data.is_null() || dropped_frames == 0 {
            return;
        }
        let ctx = unsafe { &*(user_data.cast::<CallbackContext>()) };
        let Some(senders) = &ctx.audio_senders else {
            return;
        };
        let sender = match source {
            sys::WISP_SOURCE_MIC => &senders[0],
            sys::WISP_SOURCE_SYSTEM => &senders[1],
            _ => return,
        };
        let _ = sender.report_dropped_frames(dropped_frames);
    }

    #[cfg(test)]
    mod callback_tests {
        use super::*;
        use crate::CALLBACK_FINAL_CAPACITY;

        struct CallbackHarness {
            context: Box<CallbackContext>,
            _event_receiver: CallbackEventReceiver<Event, (SourceLabel, u64)>,
            transcriber_receiver: CallbackEventReceiver<MacosTranscriberFailure, u64>,
            capture_receiver: CallbackEventReceiver<MacosCaptureFailure, u64>,
            audio_receiver: Option<CaptureEventReceiver>,
        }

        fn callback_context(with_audio: bool) -> CallbackHarness {
            callback_context_with_capacity(with_audio, 8)
        }

        fn callback_context_with_capacity(
            with_audio: bool,
            audio_capacity: usize,
        ) -> CallbackHarness {
            let (sender, receiver) = callback_event_channel_with_final_gap(final_gap_event);
            let (transcriber_failure_sender, transcriber_failure_receiver) =
                callback_event_channel_with_final_gap(transcriber_failure_gap_event);
            let (capture_failure_sender, capture_failure_receiver) =
                callback_event_channel_with_final_gap(|dropped| MacosCaptureFailure {
                    track_id: None,
                    message: format!("dropped {dropped}"),
                });
            let (audio_senders, audio_receiver) = if with_audio {
                let (senders, receiver) = realtime_capture_channel(
                    audio_capacity,
                    &[TrackId::MICROPHONE, TrackId::SYSTEM],
                );
                (
                    Some([senders[0].clone(), senders[1].clone()]),
                    Some(receiver),
                )
            } else {
                (None, None)
            };
            CallbackHarness {
                context: Box::new(CallbackContext {
                    sender,
                    transcriber_failure_sender,
                    capture_failure_sender,
                    audio_senders,
                    first_audio_timestamps: std::array::from_fn(|_| {
                        std::sync::atomic::AtomicU64::new(UNSET_AUDIO_TIMESTAMP_BITS)
                    }),
                }),
                _event_receiver: receiver,
                transcriber_receiver: transcriber_failure_receiver,
                capture_receiver: capture_failure_receiver,
                audio_receiver,
            }
        }

        #[test]
        fn first_audio_timestamp_survives_rust_queue_rejection_before_delivery() {
            let harness = callback_context_with_capacity(true, 1);
            let receiver = harness.audio_receiver.as_ref().unwrap();
            let user_data = std::ptr::from_ref(harness.context.as_ref())
                .cast_mut()
                .cast();
            let samples = [0.1_f32; 480];

            unsafe {
                on_audio_thunk(
                    sys::WISP_SOURCE_SYSTEM,
                    0,
                    0.2,
                    48_000,
                    1,
                    samples.as_ptr(),
                    samples.len(),
                    user_data,
                );
                on_audio_thunk(
                    sys::WISP_SOURCE_MIC,
                    0,
                    0.25,
                    48_000,
                    1,
                    samples.as_ptr(),
                    samples.len(),
                    user_data,
                );
            }

            assert!(matches!(
                receiver.try_recv(),
                Some(CaptureEvent::Samples(frame)) if frame.track_id() == TrackId::SYSTEM
            ));
            assert!(matches!(
                receiver.try_recv(),
                Some(CaptureEvent::Overflow {
                    track_id: TrackId::MICROPHONE,
                    dropped_frames: 480,
                })
            ));

            unsafe {
                on_audio_thunk(
                    sys::WISP_SOURCE_MIC,
                    1,
                    0.26,
                    48_000,
                    1,
                    samples.as_ptr(),
                    samples.len(),
                    user_data,
                );
            }
            assert!(matches!(
                receiver.try_recv(),
                Some(CaptureEvent::Samples(frame)) if frame.track_id() == TrackId::MICROPHONE
            ));
            assert_eq!(
                harness
                    .context
                    .first_audio_timestamp(TrackId::MICROPHONE)
                    .map(MonotonicTimestamp::as_duration),
                Some(Duration::from_secs_f64(0.25))
            );
        }

        #[test]
        fn direct_final_result_overflow_publishes_terminal_transcriber_failure() {
            let harness = callback_context(false);
            let user_data = std::ptr::from_ref(harness.context.as_ref())
                .cast_mut()
                .cast();
            let text = b"final";

            for segment_id in 0..=CALLBACK_FINAL_CAPACITY as u64 {
                unsafe {
                    on_result_thunk(
                        sys::WISP_SOURCE_MIC,
                        segment_id,
                        1,
                        text.as_ptr().cast(),
                        text.len(),
                        0.0,
                        1.0,
                        f64::NAN,
                        f64::NAN,
                        user_data,
                    );
                }
            }

            let failure = harness
                .transcriber_receiver
                .try_recv()
                .expect("overflowed final must use the terminal failure lane");
            assert!(failure.terminal);
            assert!(
                failure
                    .message
                    .contains(&CALLBACK_FINAL_CAPACITY.to_string())
            );
        }

        #[test]
        #[allow(clippy::too_many_lines)]
        fn audio_callback_accepts_exact_valid_frames_and_rejects_malformed_inputs() {
            let harness = callback_context(true);
            let receiver = harness.audio_receiver.as_ref().unwrap();
            let user_data = std::ptr::from_ref(harness.context.as_ref())
                .cast_mut()
                .cast();
            let mic = [0.1_f32, 0.2];
            let system = [0.3_f32, 0.4, 0.5, 0.6];

            unsafe {
                on_audio_thunk(
                    sys::WISP_SOURCE_MIC,
                    7,
                    1.25,
                    48_000,
                    1,
                    mic.as_ptr(),
                    mic.len(),
                    user_data,
                );
                on_audio_thunk(
                    sys::WISP_SOURCE_SYSTEM,
                    8,
                    2.5,
                    44_100,
                    2,
                    system.as_ptr(),
                    system.len(),
                    user_data,
                );
                on_audio_thunk(99, 9, 0.0, 48_000, 1, mic.as_ptr(), mic.len(), user_data);
                on_audio_thunk(
                    sys::WISP_SOURCE_MIC,
                    9,
                    f64::NAN,
                    48_000,
                    1,
                    mic.as_ptr(),
                    mic.len(),
                    user_data,
                );
                on_audio_thunk(
                    sys::WISP_SOURCE_MIC,
                    9,
                    -1.0,
                    48_000,
                    1,
                    mic.as_ptr(),
                    mic.len(),
                    user_data,
                );
                on_audio_thunk(
                    sys::WISP_SOURCE_MIC,
                    9,
                    0.0,
                    0,
                    1,
                    mic.as_ptr(),
                    mic.len(),
                    user_data,
                );
                on_audio_thunk(
                    sys::WISP_SOURCE_MIC,
                    9,
                    0.0,
                    48_000,
                    0,
                    mic.as_ptr(),
                    mic.len(),
                    user_data,
                );
                on_audio_thunk(
                    sys::WISP_SOURCE_MIC,
                    9,
                    f64::INFINITY,
                    48_000,
                    1,
                    mic.as_ptr(),
                    mic.len(),
                    user_data,
                );
                on_audio_thunk(
                    sys::WISP_SOURCE_MIC,
                    9,
                    0.0,
                    48_000,
                    u32::MAX,
                    mic.as_ptr(),
                    mic.len(),
                    user_data,
                );
                on_audio_thunk(
                    sys::WISP_SOURCE_MIC,
                    9,
                    0.0,
                    48_000,
                    2,
                    mic.as_ptr(),
                    1,
                    user_data,
                );
                on_audio_thunk(
                    sys::WISP_SOURCE_MIC,
                    9,
                    0.0,
                    48_000,
                    1,
                    std::ptr::null(),
                    mic.len(),
                    user_data,
                );
                on_audio_thunk(
                    sys::WISP_SOURCE_MIC,
                    9,
                    0.0,
                    48_000,
                    1,
                    mic.as_ptr(),
                    0,
                    user_data,
                );
                on_audio_thunk(
                    sys::WISP_SOURCE_MIC,
                    9,
                    0.0,
                    48_000,
                    1,
                    mic.as_ptr(),
                    mic.len(),
                    std::ptr::null_mut(),
                );
            }

            let CaptureEvent::Samples(mic_frame) = receiver.try_recv().unwrap() else {
                panic!("expected microphone frame");
            };
            assert_eq!(mic_frame.track_id(), TrackId::MICROPHONE);
            assert_eq!(mic_frame.sequence(), 7);
            assert_eq!(
                mic_frame.timestamp().as_duration(),
                Duration::from_secs_f64(1.25)
            );
            assert_eq!(mic_frame.samples().as_f32(), Some(mic.as_slice()));
            let CaptureEvent::Samples(system_frame) = receiver.try_recv().unwrap() else {
                panic!("expected system frame");
            };
            assert_eq!(system_frame.track_id(), TrackId::SYSTEM);
            assert_eq!(system_frame.sequence(), 8);
            assert_eq!(system_frame.format().channels, 2);
            assert_eq!(system_frame.samples().as_f32(), Some(system.as_slice()));
            assert!(receiver.try_recv().is_none());
        }

        #[test]
        fn audio_overflow_callback_reports_only_valid_nonzero_sources() {
            let harness = callback_context(true);
            let receiver = harness.audio_receiver.as_ref().unwrap();
            let user_data = std::ptr::from_ref(harness.context.as_ref())
                .cast_mut()
                .cast();

            unsafe {
                on_audio_overflow_thunk(sys::WISP_SOURCE_MIC, 11, user_data);
                on_audio_overflow_thunk(sys::WISP_SOURCE_SYSTEM, 22, user_data);
                on_audio_overflow_thunk(99, 33, user_data);
                on_audio_overflow_thunk(sys::WISP_SOURCE_MIC, 0, user_data);
                on_audio_overflow_thunk(sys::WISP_SOURCE_MIC, 44, std::ptr::null_mut());
            }

            assert_eq!(
                receiver.try_recv(),
                Some(CaptureEvent::Overflow {
                    track_id: TrackId::MICROPHONE,
                    dropped_frames: 11,
                })
            );
            assert_eq!(
                receiver.try_recv(),
                Some(CaptureEvent::Overflow {
                    track_id: TrackId::SYSTEM,
                    dropped_frames: 22,
                })
            );
            assert!(receiver.try_recv().is_none());
        }

        #[test]
        fn transcriber_error_callback_preserves_payload_and_defaults_empty_message() {
            let harness = callback_context(false);
            let user_data = std::ptr::from_ref(harness.context.as_ref())
                .cast_mut()
                .cast();
            let recoverable = b"temporary analyzer gap";

            unsafe {
                on_transcriber_error_thunk(
                    0,
                    recoverable.as_ptr().cast(),
                    recoverable.len(),
                    user_data,
                );
                on_transcriber_error_thunk(1, std::ptr::null(), 0, user_data);
                on_transcriber_error_thunk(
                    1,
                    recoverable.as_ptr().cast(),
                    recoverable.len(),
                    std::ptr::null_mut(),
                );
            }

            assert_eq!(
                harness.transcriber_receiver.try_recv(),
                Some(MacosTranscriberFailure {
                    terminal: true,
                    message: "unknown SpeechAnalyzer failure".into(),
                })
            );
            assert_eq!(
                harness.transcriber_receiver.try_recv(),
                Some(MacosTranscriberFailure {
                    terminal: false,
                    message: "temporary analyzer gap".into(),
                })
            );
            assert!(harness.transcriber_receiver.try_recv().is_none());
        }

        #[test]
        fn terminal_error_callback_accepts_known_or_sourceless_failures_only() {
            let harness = callback_context(false);
            let user_data = std::ptr::from_ref(harness.context.as_ref())
                .cast_mut()
                .cast();
            let message = b"writer failed";

            unsafe {
                on_terminal_error_thunk(
                    sys::WISP_SOURCE_MIC,
                    message.as_ptr().cast(),
                    message.len(),
                    user_data,
                );
                on_terminal_error_thunk(-1, std::ptr::null(), 0, user_data);
                on_terminal_error_thunk(99, message.as_ptr().cast(), message.len(), user_data);
                on_terminal_error_thunk(
                    sys::WISP_SOURCE_SYSTEM,
                    message.as_ptr().cast(),
                    message.len(),
                    std::ptr::null_mut(),
                );
            }

            assert_eq!(
                harness.capture_receiver.try_recv(),
                Some(MacosCaptureFailure {
                    track_id: Some(TrackId::MICROPHONE),
                    message: "writer failed".into(),
                })
            );
            assert_eq!(
                harness.capture_receiver.try_recv(),
                Some(MacosCaptureFailure {
                    track_id: None,
                    message: "unknown terminal capture failure".into(),
                })
            );
            assert!(harness.capture_receiver.try_recv().is_none());
        }
    }
}

#[cfg(target_os = "windows")]
mod imp {
    use std::path::Path;
    use std::sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
    };
    use std::time::{Duration, Instant};

    use windows::Foundation::TypedEventHandler;
    use windows::Globalization::Language;
    use windows::Media::SpeechRecognition::{
        SpeechContinuousRecognitionCompletedEventArgs, SpeechContinuousRecognitionMode,
        SpeechContinuousRecognitionResultGeneratedEventArgs, SpeechContinuousRecognitionSession,
        SpeechRecognitionResultStatus, SpeechRecognitionScenario, SpeechRecognitionTopicConstraint,
        SpeechRecognizer,
    };
    use windows::core::HSTRING;
    use wisp_core::SourceLabel;

    use crate::error::{Result, SessionError};
    use crate::{
        CallbackEventClass, CallbackEventReceiver, CallbackEventSender, MergedSessionReceive,
        OneShotSessionLifecycle, Permission, PermissionStatus, SessionConfig, SessionOptions,
        TranscriptionPolicy, WasapiRecording, WindowsPlatformStart, WindowsPlatformStartError,
        WindowsRuntimeControlPublisher, WindowsRuntimeNotification, WindowsTranscriptionMode,
        callback_event_channel_with_final_gap, recv_callback_session_channels_with_control,
        select_windows_transcription_mode, start_windows_platform,
        try_recv_callback_session_channels_with_control,
    };

    /// `WispAudioKit` library version.
    #[must_use]
    pub fn version() -> &'static str {
        "windows-0.1.0"
    }

    /// Windows desktop apps do not get a TCC-style microphone prompt through
    /// this backend. Privacy toggles are surfaced by the setup screen's
    /// Settings link, while actual availability is validated at session start.
    #[must_use]
    pub fn check_permission(_permission: Permission) -> PermissionStatus {
        PermissionStatus::Granted
    }

    /// See [`check_permission`].
    #[must_use]
    pub fn request_permission(_permission: Permission) -> PermissionStatus {
        PermissionStatus::Granted
    }

    /// One transcription update from a running [`Session`].
    #[derive(Debug, Clone, PartialEq)]
    pub struct SessionResult {
        pub source: SourceLabel,
        pub segment_id: u64,
        pub is_final: bool,
        pub text: String,
        pub start_seconds: f64,
        pub end_seconds: f64,
        pub confidence_mean: Option<f64>,
        pub confidence_min: Option<f64>,
    }

    /// Either a transcription result or a log line emitted by the session.
    #[derive(Debug, Clone, PartialEq)]
    pub enum Event {
        Result(SessionResult),
        Log(String),
    }

    fn final_gap_event(dropped_finals: u64) -> Event {
        Event::Log(format!(
            "[FATAL] transcription gap: dropped {dropped_finals} final result(s)"
        ))
    }

    type EventKey = (SourceLabel, u64);

    fn enqueue_event(
        sender: &CallbackEventSender<Event, EventKey>,
        event: Event,
    ) {
        let class = match &event {
            Event::Result(result) if result.is_final => {
                CallbackEventClass::Final(Some((result.source, result.segment_id)))
            },
            Event::Result(result) => {
                CallbackEventClass::Partial((result.source, result.segment_id))
            },
            Event::Log(_) => CallbackEventClass::Log,
        };
        let _ = sender.try_send(class, event);
    }

    fn speech_result_handler(
        sender: &CallbackEventSender<Event, EventKey>,
        lifecycle: &Arc<OneShotSessionLifecycle>,
        microphone_muted: &Arc<AtomicBool>,
    ) -> TypedEventHandler<
        SpeechContinuousRecognitionSession,
        SpeechContinuousRecognitionResultGeneratedEventArgs,
    > {
        let segment_id = Arc::new(AtomicU64::new(1));
        let started_at = Instant::now();
        let handler_sender = sender.clone();
        let handler_lifecycle = Arc::clone(lifecycle);
        let handler_muted = Arc::clone(microphone_muted);
        TypedEventHandler::<
            SpeechContinuousRecognitionSession,
            SpeechContinuousRecognitionResultGeneratedEventArgs,
        >::new(move |_session, args| {
            if !handler_lifecycle.speech_is_running() || handler_muted.load(Ordering::SeqCst) {
                return Ok(());
            }
            let Some(args) = args.as_ref() else {
                return Ok(());
            };
            let result = args.Result()?;
            if result.Status()? != SpeechRecognitionResultStatus::Success {
                return Ok(());
            }
            let text = result.Text()?.to_string_lossy();
            if text.trim().is_empty() {
                return Ok(());
            }
            let now = started_at.elapsed().as_secs_f64();
            let id = segment_id.fetch_add(1, Ordering::SeqCst);
            enqueue_event(
                &handler_sender,
                Event::Result(SessionResult {
                    source: SourceLabel::Mic,
                    segment_id: id,
                    is_final: true,
                    text,
                    start_seconds: now,
                    end_seconds: now,
                    confidence_mean: None,
                    confidence_min: None,
                }),
            );
            Ok(())
        })
    }

    fn speech_completed_handler(
        publisher: Arc<WindowsRuntimeControlPublisher>
    ) -> TypedEventHandler<
        SpeechContinuousRecognitionSession,
        SpeechContinuousRecognitionCompletedEventArgs,
    > {
        TypedEventHandler::<
            SpeechContinuousRecognitionSession,
            SpeechContinuousRecognitionCompletedEventArgs,
        >::new(move |_session, args| {
            let status = args.as_ref().map_or_else(
                || "missing completion status".to_owned(),
                |args| {
                    args.Status()
                        .map_or_else(|error| error.to_string(), |status| format!("{status:?}"))
                },
            );
            publisher.publish(&status);
            Ok(())
        })
    }

    #[derive(Default)]
    struct WindowsCleanupResourceState {
        speech: Option<Arc<WindowsSpeechSession>>,
        recording: Option<Arc<WasapiRecording>>,
        recording_cleanup_complete: bool,
        startup_finished: bool,
        latest_all_report: Option<(u64, super::WindowsCleanupReport)>,
    }

    #[derive(Default)]
    struct WindowsCleanupResources {
        state: std::sync::Mutex<WindowsCleanupResourceState>,
        changed: std::sync::Condvar,
    }

    impl WindowsCleanupResources {
        fn install(
            &self,
            speech: Option<Arc<WindowsSpeechSession>>,
            recording: Arc<WasapiRecording>,
        ) {
            let mut state = self
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            state.speech = speech;
            state.recording = Some(recording);
            self.changed.notify_all();
        }

        fn finish_startup(&self) {
            let mut state = self
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            state.startup_finished = true;
            self.changed.notify_all();
        }

        fn cleanup(
            &self,
            level: super::WindowsCleanupLevel,
        ) -> Option<String> {
            let mut state = self
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            while !state.startup_finished
                && match level {
                    super::WindowsCleanupLevel::SpeechOnly => state.speech.is_none(),
                    super::WindowsCleanupLevel::All => state.recording.is_none(),
                }
            {
                state = self
                    .changed
                    .wait(state)
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
            }
            let speech = state.speech.clone();
            let recording = state.recording.clone();
            let recording_cleanup_complete = state.recording_cleanup_complete;
            drop(state);

            let mut recording_complete = recording_cleanup_complete;
            let cleanup_error = super::run_windows_session_cleanup(
                level,
                &mut recording_complete,
                || speech.map_or(Ok(()), |speech| speech.stop()),
                || {
                    recording.map_or(Ok(()), |recording| {
                        recording.stop().map_err(|error| error.to_string())
                    })
                },
            );
            if recording_complete && !recording_cleanup_complete {
                self.state
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .recording_cleanup_complete = true;
            }
            cleanup_error
        }

        fn finish_all_cleanup(
            &self,
            generation: u64,
            report: super::WindowsCleanupReport,
        ) {
            let mut state = self
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            state.latest_all_report = Some((generation, report));
            self.changed.notify_all();
        }

        fn wait_for_all_cleanup(
            &self,
            generation: u64,
        ) -> super::WindowsCleanupReport {
            let mut state = self
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            while state
                .latest_all_report
                .as_ref()
                .is_none_or(|(reported, _)| *reported < generation)
            {
                state = self
                    .changed
                    .wait(state)
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
            }
            state
                .latest_all_report
                .as_ref()
                .map(|(_, report)| report.clone())
                .unwrap_or_default()
        }
    }

    struct WindowsCleanupCoordinator {
        resources: Arc<WindowsCleanupResources>,
        cleanup: super::BoundedCleanupHandle,
        cleanup_intents: crossbeam_channel::Sender<super::WindowsCleanupIntent>,
        worker: std::sync::Mutex<Option<std::thread::JoinHandle<()>>>,
    }

    impl WindowsCleanupCoordinator {
        fn new(
            runtime_control: crossbeam_channel::Sender<WindowsRuntimeNotification>
        ) -> Result<Self> {
            let resources = Arc::new(WindowsCleanupResources::default());
            let worker_resources = Arc::clone(&resources);
            let finish_resources = Arc::clone(&resources);
            let (cleanup_intents, cleanup_intent_receiver) =
                crossbeam_channel::bounded::<super::WindowsCleanupIntent>(1);
            let (cleanup, worker) = super::BoundedCleanupHandle::spawn(
                move |level| worker_resources.cleanup(level),
                move |level, generation, cleanup_error| {
                    let notification = super::finalize_windows_cleanup_notification(
                        cleanup_intent_receiver.try_recv().ok(),
                        level,
                        cleanup_error.as_deref(),
                    );
                    if let Some(notification) = notification.clone() {
                        let _ = runtime_control.try_send(notification);
                    }
                    if level == super::WindowsCleanupLevel::All {
                        finish_resources.finish_all_cleanup(
                            generation,
                            super::WindowsCleanupReport {
                                error: cleanup_error,
                                notification,
                            },
                        );
                    }
                },
            )
            .map_err(|_| SessionError::Construction)?;
            Ok(Self {
                resources,
                cleanup,
                cleanup_intents,
                worker: std::sync::Mutex::new(Some(worker)),
            })
        }

        fn publisher(
            &self,
            lifecycle: Arc<OneShotSessionLifecycle>,
            policy: TranscriptionPolicy,
        ) -> WindowsRuntimeControlPublisher {
            WindowsRuntimeControlPublisher::new(
                lifecycle,
                self.cleanup_intents.clone(),
                self.cleanup.clone(),
                policy,
            )
        }

        fn install(
            &self,
            speech: Option<Arc<WindowsSpeechSession>>,
            recording: Arc<WasapiRecording>,
        ) {
            self.resources.install(speech, recording);
        }

        fn finish_startup(&self) {
            self.resources.finish_startup();
        }

        fn request_all(&self) -> (u64, bool) {
            self.cleanup.request(super::WindowsCleanupLevel::All)
        }

        fn wait_for_claimed_all(&self) -> super::WindowsCleanupReport {
            let generation = self.cleanup.wait_for_all_generation();
            self.wait_for_all(generation)
        }

        fn wait_for_all(
            &self,
            generation: u64,
        ) -> super::WindowsCleanupReport {
            self.cleanup.wait_for_all_completion(generation);
            self.resources.wait_for_all_cleanup(generation)
        }
    }

    impl Drop for WindowsCleanupCoordinator {
        fn drop(&mut self) {
            self.resources.finish_startup();
            self.cleanup.shutdown();
            let worker = self
                .worker
                .get_mut()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .take();
            if let Some(worker) = worker {
                let _ = worker.join();
            }
        }
    }

    enum PendingWindowsStart {
        Platform {
            recording: Arc<WasapiRecording>,
            speech: Arc<WindowsSpeechSession>,
        },
        RecordOnly {
            recording: Arc<WasapiRecording>,
            notice: String,
        },
    }

    /// Windows capture and transcription session.
    ///
    /// Both paths record WASAPI mic + loopback streams as Ogg/Opus. The
    /// platform path additionally uses Windows' online microphone dictation;
    /// the local-model path is ready for offline transcription to be wired.
    pub struct Session {
        output_dir: std::path::PathBuf,
        config: SessionConfig,
        transcription_policy: TranscriptionPolicy,
        receiver: CallbackEventReceiver<Event, EventKey>,
        sender: CallbackEventSender<Event, EventKey>,
        speech: Option<Arc<WindowsSpeechSession>>,
        recording: Option<Arc<WasapiRecording>>,
        started_at: Option<Instant>,
        lifecycle: Arc<OneShotSessionLifecycle>,
        runtime_control_receiver: crossbeam_channel::Receiver<WindowsRuntimeNotification>,
        cleanup: WindowsCleanupCoordinator,
    }

    impl Session {
        /// Construct a new session.
        ///
        /// # Errors
        /// Returns [`SessionError::InvalidLocale`] if `locale` contains a NUL
        /// byte and [`SessionError::Construction`] if the output directory
        /// cannot be created.
        pub fn new(
            output_dir: impl AsRef<Path>,
            locale: &str,
        ) -> Result<Self> {
            Self::new_with_config(output_dir, SessionConfig::platform_default(locale))
        }

        /// Construct a new session with an explicit recognizer config.
        ///
        /// # Errors
        /// Returns [`SessionError::Construction`] if the output directory
        /// cannot be created.
        pub fn new_with_config(
            output_dir: impl AsRef<Path>,
            config: SessionConfig,
        ) -> Result<Self> {
            Self::new_with_options(output_dir, config.into())
        }

        /// Construct a new session with explicit transcription policy.
        ///
        /// # Errors
        /// Returns [`SessionError::Construction`] if the output directory
        /// cannot be created.
        pub fn new_with_options(
            output_dir: impl AsRef<Path>,
            options: SessionOptions,
        ) -> Result<Self> {
            let (config, transcription_policy) = options.into_parts();
            if config.locale.contains('\0') {
                return Err(SessionError::InvalidLocale(config.locale));
            }
            let output_dir = output_dir.as_ref().to_path_buf();
            std::fs::create_dir_all(&output_dir).map_err(|_| SessionError::Construction)?;
            let (sender, receiver) = callback_event_channel_with_final_gap(final_gap_event);
            let (runtime_control_sender, runtime_control_receiver) = crossbeam_channel::bounded(1);
            let cleanup = WindowsCleanupCoordinator::new(runtime_control_sender)?;
            Ok(Self {
                output_dir,
                config,
                transcription_policy,
                receiver,
                sender,
                speech: None,
                recording: None,
                started_at: None,
                lifecycle: Arc::new(OneShotSessionLifecycle::new()),
                runtime_control_receiver,
                cleanup,
            })
        }

        /// Start capture + transcription. Blocks until ready or fails.
        ///
        /// # Errors
        /// Returns [`SessionError::Start`] when the selected recognizer cannot
        /// be initialized or the selected local model is not installed.
        pub fn start(&mut self) -> Result<()> {
            if !self.lifecycle.begin_start() {
                return Err(SessionError::Start(
                    "Windows session has already been started and cannot be restarted".into(),
                ));
            }
            let startup_transaction = super::WindowsStartTransactionGuard::new(|| {
                super::finish_unwound_windows_start(
                    &self.lifecycle,
                    || self.cleanup.finish_startup(),
                    || self.cleanup.request_all(),
                    |generation| self.cleanup.wait_for_all(generation),
                );
            });
            let pending = select_windows_transcription_mode(
                self.config.recognizer,
                self.transcription_policy,
            )
            .map_err(SessionError::Start)
            .and_then(|mode| match mode {
                WindowsTranscriptionMode::PlatformOnline => self.prepare_windows_speech(),
                WindowsTranscriptionMode::RecordOnly { reason } => {
                    self.prepare_record_only(&reason)
                },
            })?;
            self.stage_cleanup_ownership(&pending);
            self.cleanup.finish_startup();
            startup_transaction.finish();
            super::complete_windows_start_transaction(&self.lifecycle, || {
                self.cleanup.wait_for_claimed_all()
            })?;
            self.commit_pending_start(pending);
            Ok(())
        }

        /// Whether microphone capture reached the running state.
        #[must_use]
        pub fn has_started_capture(&self) -> bool {
            self.lifecycle.capture_is_running()
        }

        /// Suppress microphone recognition results while muted.
        pub fn set_microphone_muted(
            &self,
            muted: bool,
        ) {
            if let Some(speech) = &self.speech {
                speech.microphone_muted.store(muted, Ordering::SeqCst);
            }
            if let Some(recording) = &self.recording {
                recording.set_microphone_muted(muted);
            }
        }

        /// Stop the session. Idempotent.
        pub fn stop(&self) {
            let (claimed, started_retry, report) = super::claim_and_wait_windows_stop(
                &self.lifecycle,
                || self.cleanup.request_all(),
                |generation| self.cleanup.wait_for_all(generation),
            );
            if (claimed || started_retry)
                && let Some(err) = report.and_then(|report| report.error)
            {
                enqueue_event(&self.sender, Event::Log(format!("[WIN] {err}")));
            }
        }

        /// Non-blocking event poll.
        #[must_use]
        pub fn try_recv(&self) -> Option<Event> {
            let Some(recording) = self.recording.as_ref() else {
                return self.receiver.try_recv();
            };
            match try_recv_callback_session_channels_with_control(
                &self.receiver,
                recording.fatal_error_receiver(),
                Some(&self.runtime_control_receiver),
                recording.warning_receiver(),
            ) {
                Some(MergedSessionReceive::Main(event)) => Some(event),
                Some(MergedSessionReceive::Notification(message)) => {
                    Some(Event::Log(format!("[WIN] {message}")))
                },
                Some(MergedSessionReceive::RuntimeControl(control)) => {
                    Some(Self::runtime_control_event(control))
                },
                None => None,
            }
        }

        /// Block until the next event arrives, or return `None` if the
        /// session has been dropped / closed.
        #[must_use]
        pub fn recv(&self) -> Option<Event> {
            if let Some(recording) = &self.recording {
                return match recv_callback_session_channels_with_control(
                    &self.receiver,
                    recording.fatal_error_receiver(),
                    Some(&self.runtime_control_receiver),
                    recording.warning_receiver(),
                    None,
                ) {
                    Some(MergedSessionReceive::Main(event)) => Some(event),
                    Some(MergedSessionReceive::Notification(message)) => {
                        Some(Event::Log(format!("[WIN] {message}")))
                    },
                    Some(MergedSessionReceive::RuntimeControl(control)) => {
                        Some(Self::runtime_control_event(control))
                    },
                    None => None,
                };
            }
            self.receiver.recv()
        }

        /// Block until the next event arrives or `timeout` elapses.
        #[must_use]
        pub fn recv_timeout(
            &self,
            timeout: Duration,
        ) -> Option<Event> {
            if let Some(recording) = &self.recording {
                return match recv_callback_session_channels_with_control(
                    &self.receiver,
                    recording.fatal_error_receiver(),
                    Some(&self.runtime_control_receiver),
                    recording.warning_receiver(),
                    Some(timeout),
                ) {
                    Some(MergedSessionReceive::Main(event)) => Some(event),
                    Some(MergedSessionReceive::Notification(message)) => {
                        Some(Event::Log(format!("[WIN] {message}")))
                    },
                    Some(MergedSessionReceive::RuntimeControl(control)) => {
                        Some(Self::runtime_control_event(control))
                    },
                    None => None,
                };
            }
            self.receiver.recv_timeout(timeout)
        }

        fn prepare_windows_speech(&self) -> Result<PendingWindowsStart> {
            let publisher = Arc::new(
                self.cleanup
                    .publisher(Arc::clone(&self.lifecycle), self.transcription_policy),
            );
            let outcome = start_windows_platform(
                self.transcription_policy,
                || WasapiRecording::start(&self.output_dir),
                || {
                    WindowsSpeechSession::start(
                        &self.config.locale,
                        &self.sender,
                        &self.lifecycle,
                        publisher,
                    )
                },
                |recording: WasapiRecording| recording.stop(),
            );
            match outcome {
                Ok(WindowsPlatformStart::Platform { recording, speech }) => {
                    Ok(PendingWindowsStart::Platform {
                        recording: Arc::new(recording),
                        speech: Arc::new(speech),
                    })
                },
                Ok(WindowsPlatformStart::RecordOnly {
                    recording,
                    speech_error,
                    reason,
                }) => Ok(PendingWindowsStart::RecordOnly {
                    recording: Arc::new(recording),
                    notice: format!(
                        "[WIN] platform dictation unavailable ({speech_error}); {reason}; continuing with local WASAPI recording"
                    ),
                }),
                Err(WindowsPlatformStartError::Recording(error)) => Err(error),
                Err(WindowsPlatformStartError::Speech(reason)) => Err(SessionError::Start(reason)),
            }
        }

        fn prepare_record_only(
            &self,
            reason: &str,
        ) -> Result<PendingWindowsStart> {
            let recording = Arc::new(WasapiRecording::start(&self.output_dir)?);
            Ok(PendingWindowsStart::RecordOnly {
                recording,
                notice: format!(
                    "[WIN] transcription unavailable ({reason}); continuing in record-only mode"
                ),
            })
        }

        fn stage_cleanup_ownership(
            &self,
            pending: &PendingWindowsStart,
        ) {
            match pending {
                PendingWindowsStart::Platform { recording, speech } => {
                    self.cleanup
                        .install(Some(Arc::clone(speech)), Arc::clone(recording));
                },
                PendingWindowsStart::RecordOnly { recording, .. } => {
                    self.cleanup.install(None, Arc::clone(recording));
                },
            }
        }

        fn commit_pending_start(
            &mut self,
            pending: PendingWindowsStart,
        ) {
            let (recording, speech, notice) = match pending {
                PendingWindowsStart::Platform { recording, speech } => (
                    recording,
                    Some(speech),
                    "[WIN] Windows.Media.SpeechRecognition online dictation started for microphone input".into(),
                ),
                PendingWindowsStart::RecordOnly { recording, notice } => {
                    (recording, None, notice)
                },
            };
            let mic_path = recording.mic_path().display().to_string();
            let system_path = recording.system_path().display().to_string();
            self.recording = Some(recording);
            self.speech = speech;
            self.started_at = Some(Instant::now());
            enqueue_event(
                &self.sender,
                Event::Log(format!("[WIN] recording WASAPI microphone to {mic_path}")),
            );
            enqueue_event(
                &self.sender,
                Event::Log(format!(
                    "[WIN] recording WASAPI system loopback to {system_path}"
                )),
            );
            enqueue_event(&self.sender, Event::Log(notice));
        }

        fn runtime_control_event(control: WindowsRuntimeNotification) -> Event {
            Event::Log(super::format_windows_runtime_notification(control))
        }
    }

    impl Drop for Session {
        fn drop(&mut self) {
            self.stop();
        }
    }

    struct WindowsSpeechStartupCleanupGuard {
        recognizer: SpeechRecognizer,
        session: SpeechContinuousRecognitionSession,
        result_token: Option<i64>,
        completed_token: Option<i64>,
        start_attempted: bool,
        progress: super::WindowsSpeechCleanupProgress,
        armed: bool,
    }

    impl WindowsSpeechStartupCleanupGuard {
        fn new(
            recognizer: SpeechRecognizer,
            session: SpeechContinuousRecognitionSession,
        ) -> Self {
            Self {
                recognizer,
                session,
                result_token: None,
                completed_token: None,
                start_attempted: false,
                progress: super::WindowsSpeechCleanupProgress::default(),
                armed: true,
            }
        }

        fn cleanup(&mut self) -> std::result::Result<(), String> {
            let session = &self.session;
            let recognizer = &self.recognizer;
            let result_token = self.result_token;
            let completed_token = self.completed_token;
            super::run_windows_speech_startup_cleanup_bounded(
                &mut self.progress,
                super::WindowsSpeechStartupCleanupState {
                    start_attempted: self.start_attempted,
                    completed_token: completed_token.is_some(),
                    result_token: result_token.is_some(),
                },
                || {
                    session
                        .StopAsync()
                        .and_then(|operation| operation.join())
                        .map_err(|error| error.to_string())
                },
                || {
                    let Some(completed_token) = completed_token else {
                        return Err("registered Completed token is missing".into());
                    };
                    session
                        .RemoveCompleted(completed_token)
                        .map_err(|error| error.to_string())
                },
                || {
                    let Some(result_token) = result_token else {
                        return Err("registered ResultGenerated token is missing".into());
                    };
                    session
                        .RemoveResultGenerated(result_token)
                        .map_err(|error| error.to_string())
                },
                || recognizer.Close().map_err(|error| error.to_string()),
            )
        }

        fn into_start_error(
            mut self,
            primary: impl std::fmt::Display,
        ) -> SessionError {
            let cleanup = self.cleanup();
            self.armed = false;
            super::windows_speech_start_error(primary, cleanup)
        }

        fn disarm(&mut self) {
            self.armed = false;
        }
    }

    impl Drop for WindowsSpeechStartupCleanupGuard {
        fn drop(&mut self) {
            if self.armed {
                let _ = self.cleanup();
            }
        }
    }

    struct WindowsSpeechSession {
        recognizer: SpeechRecognizer,
        session: SpeechContinuousRecognitionSession,
        result_token: i64,
        completed_token: i64,
        microphone_muted: Arc<AtomicBool>,
        cleanup_progress: std::sync::Mutex<super::WindowsSpeechCleanupProgress>,
    }

    impl WindowsSpeechSession {
        fn start(
            locale: &str,
            sender: &CallbackEventSender<Event, EventKey>,
            lifecycle: &Arc<OneShotSessionLifecycle>,
            runtime_control_publisher: Arc<WindowsRuntimeControlPublisher>,
        ) -> Result<Self> {
            let microphone_muted = Arc::new(AtomicBool::new(false));
            let language = Language::CreateLanguage(&HSTRING::from(locale))
                .map_err(|err| SessionError::Start(err.to_string()))?;
            let recognizer = SpeechRecognizer::Create(&language)
                .map_err(|err| SessionError::Start(err.to_string()))?;
            let constraint = SpeechRecognitionTopicConstraint::Create(
                SpeechRecognitionScenario::Dictation,
                &HSTRING::from("meeting transcription"),
            )
            .map_err(|err| SessionError::Start(err.to_string()))?;
            recognizer
                .Constraints()
                .and_then(|constraints| constraints.Append(&constraint))
                .map_err(|err| SessionError::Start(err.to_string()))?;
            let compile = recognizer
                .CompileConstraintsAsync()
                .and_then(|op| op.join())
                .map_err(|err| SessionError::Start(err.to_string()))?;
            if compile
                .Status()
                .map_err(|err| SessionError::Start(err.to_string()))?
                != SpeechRecognitionResultStatus::Success
            {
                return Err(SessionError::Start(
                    "Windows speech constraints failed to compile".into(),
                ));
            }

            let session = recognizer
                .ContinuousRecognitionSession()
                .map_err(|err| SessionError::Start(err.to_string()))?;
            let mut startup_cleanup =
                WindowsSpeechStartupCleanupGuard::new(recognizer.clone(), session.clone());
            let handler = speech_result_handler(sender, lifecycle, &microphone_muted);
            let result_token = match session.ResultGenerated(&handler) {
                Ok(token) => token,
                Err(error) => return Err(startup_cleanup.into_start_error(error)),
            };
            startup_cleanup.result_token = Some(result_token);
            let completed_handler = speech_completed_handler(runtime_control_publisher);
            let completed_token = match session.Completed(&completed_handler) {
                Ok(token) => token,
                Err(error) => return Err(startup_cleanup.into_start_error(error)),
            };
            startup_cleanup.completed_token = Some(completed_token);
            startup_cleanup.start_attempted = true;
            if let Err(error) = session
                .StartWithModeAsync(SpeechContinuousRecognitionMode::Default)
                .and_then(|op| op.join())
            {
                return Err(startup_cleanup.into_start_error(error));
            }
            startup_cleanup.disarm();
            Ok(Self {
                recognizer,
                session,
                result_token,
                completed_token,
                microphone_muted,
                cleanup_progress: std::sync::Mutex::new(
                    super::WindowsSpeechCleanupProgress::default(),
                ),
            })
        }

        fn stop(&self) -> std::result::Result<(), String> {
            let mut progress = self
                .cleanup_progress
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            super::run_windows_speech_cleanup(
                &mut progress,
                super::WINDOWS_SPEECH_RUNTIME_CLEANUP,
                || {
                    self.session
                        .StopAsync()
                        .and_then(|operation| operation.join())
                        .map_err(|error| error.to_string())
                },
                || {
                    self.session
                        .RemoveCompleted(self.completed_token)
                        .map_err(|error| error.to_string())
                },
                || {
                    self.session
                        .RemoveResultGenerated(self.result_token)
                        .map_err(|error| error.to_string())
                },
                || self.recognizer.Close().map_err(|error| error.to_string()),
            )
        }
    }
}

#[cfg(target_os = "linux")]
mod imp {
    use std::path::Path;
    use std::sync::Arc;
    use std::time::Duration;

    use wisp_core::SourceLabel;

    use crate::error::{Result, SessionError};
    use crate::{Permission, PermissionStatus, PipewireRecording, SessionConfig, SessionOptions};

    /// Version label for the Linux `PipeWire` recording backend.
    #[must_use]
    pub fn version() -> &'static str {
        "linux-pipewire-0.1.0"
    }

    /// `PipeWire` permissions are decided by the session manager/desktop portal
    /// when a stream is connected, so startup is the authoritative probe.
    #[must_use]
    pub fn check_permission(_permission: Permission) -> PermissionStatus {
        PermissionStatus::Granted
    }

    /// `PipeWire` permissions are requested as part of stream connection.
    #[must_use]
    pub fn request_permission(_permission: Permission) -> PermissionStatus {
        PermissionStatus::Granted
    }

    /// Linux transcription is not implemented in this recording milestone.
    #[derive(Debug, Clone, PartialEq)]
    pub struct SessionResult {
        pub source: SourceLabel,
        pub segment_id: u64,
        pub is_final: bool,
        pub text: String,
        pub start_seconds: f64,
        pub end_seconds: f64,
        pub confidence_mean: Option<f64>,
        pub confidence_min: Option<f64>,
    }

    /// Linux sessions currently emit recording lifecycle and warning logs.
    #[derive(Debug, Clone, PartialEq)]
    pub enum Event {
        Result(SessionResult),
        Log(String),
    }

    /// Record-only Linux session backed by `PipeWire` and Ogg/Opus.
    pub struct Session {
        output_dir: std::path::PathBuf,
        options: SessionOptions,
        recording: Option<Arc<PipewireRecording>>,
        pending: crossbeam_channel::Receiver<Event>,
        publisher: crossbeam_channel::Sender<Event>,
        started: bool,
    }

    impl Session {
        /// Construct a record-only Linux session.
        ///
        /// # Errors
        /// Returns [`SessionError::InvalidLocale`] for a locale containing NUL
        /// or [`SessionError::Construction`] if the output directory cannot be
        /// created.
        pub fn new(
            output_dir: impl AsRef<Path>,
            locale: &str,
        ) -> Result<Self> {
            Self::new_with_config(output_dir, SessionConfig::platform_default(locale))
        }

        /// Construct a session with an explicit recognizer configuration.
        ///
        /// Linux still records only; the recognizer selection is retained for
        /// future local-transcriber integration.
        ///
        /// # Errors
        /// Returns the same construction errors as [`Self::new`].
        pub fn new_with_config(
            output_dir: impl AsRef<Path>,
            config: SessionConfig,
        ) -> Result<Self> {
            Self::new_with_options(output_dir, config.into())
        }

        /// Construct a session with explicit transcription fallback policy.
        ///
        /// # Errors
        /// Returns the same construction errors as [`Self::new`].
        pub fn new_with_options(
            output_dir: impl AsRef<Path>,
            options: SessionOptions,
        ) -> Result<Self> {
            if options.config().locale.contains('\0') {
                return Err(SessionError::InvalidLocale(options.config().locale.clone()));
            }
            let output_dir = output_dir.as_ref().to_path_buf();
            std::fs::create_dir_all(&output_dir).map_err(|_| SessionError::Construction)?;
            let (publisher, pending) = crossbeam_channel::bounded(32);
            Ok(Self {
                output_dir,
                options,
                recording: None,
                pending,
                publisher,
                started: false,
            })
        }

        /// Start `PipeWire` capture and Ogg/Opus recording.
        ///
        /// Linux transcription remains unavailable. Startup succeeds in
        /// record-only mode only when the configured policy permits it.
        ///
        /// # Errors
        /// Returns [`SessionError::Start`] for repeated starts, a policy that
        /// forbids record-only fallback, or `PipeWire`/recording setup failure.
        pub fn start(&mut self) -> Result<()> {
            if self.started {
                return Err(SessionError::Start(
                    "Linux session has already been started and cannot be restarted".into(),
                ));
            }
            if !self.options.transcription_policy().allow_record_only {
                return Err(SessionError::Start(
                    "Linux transcription is unavailable and session policy forbids record-only fallback"
                        .into(),
                ));
            }
            let recording = Arc::new(PipewireRecording::start(&self.output_dir)?);
            let mic_path = recording.mic_path().display();
            let system_path = recording.system_path().display();
            let _ = self.publisher.try_send(Event::Log(format!(
                "[LINUX] recording PipeWire microphone to {mic_path}"
            )));
            let _ = self.publisher.try_send(Event::Log(format!(
                "[LINUX] recording PipeWire sink monitor to {system_path}"
            )));
            let _ = self.publisher.try_send(Event::Log(
                "[LINUX] transcription is unavailable; continuing in record-only mode".into(),
            ));
            self.recording = Some(recording);
            self.started = true;
            Ok(())
        }

        #[must_use]
        pub const fn has_started_capture(&self) -> bool {
            self.started
        }

        pub fn set_microphone_muted(
            &self,
            muted: bool,
        ) {
            if let Some(recording) = &self.recording {
                recording.set_microphone_muted(muted);
            }
        }

        /// Stop and finalize both Ogg files. Errors are exposed as log events
        /// for compatibility with the existing infallible session facade.
        pub fn stop(&self) {
            if let Some(recording) = &self.recording
                && let Err(error) = recording.stop()
            {
                let _ = self
                    .publisher
                    .try_send(Event::Log(format!("[LINUX] {error}")));
            }
        }

        #[must_use]
        pub fn try_recv(&self) -> Option<Event> {
            if let Some(event) = self.pending.try_recv().ok() {
                return Some(event);
            }
            let recording = self.recording.as_ref()?;
            recording
                .try_recv_warning()
                .map(|warning| Event::Log(format!("[LINUX] {warning}")))
        }

        #[must_use]
        pub fn recv(&self) -> Option<Event> {
            loop {
                if let Some(event) = self.try_recv() {
                    return Some(event);
                }
                if self
                    .recording
                    .as_ref()
                    .is_some_and(|recording| recording.is_finished())
                {
                    return None;
                }
                if let Some(recording) = &self.recording
                    && let Some(warning) =
                        recording.recv_warning_timeout(Duration::from_millis(100))
                {
                    return Some(Event::Log(format!("[LINUX] {warning}")));
                } else if self.recording.is_none() {
                    return self.pending.recv().ok();
                }
            }
        }

        #[must_use]
        pub fn recv_timeout(
            &self,
            timeout: Duration,
        ) -> Option<Event> {
            let started = std::time::Instant::now();
            loop {
                if let Some(event) = self.try_recv() {
                    return Some(event);
                }
                if self
                    .recording
                    .as_ref()
                    .is_some_and(|recording| recording.is_finished())
                {
                    return None;
                }
                let remaining = timeout.saturating_sub(started.elapsed());
                if remaining.is_zero() {
                    return None;
                }
                if let Some(recording) = &self.recording {
                    if let Some(warning) =
                        recording.recv_warning_timeout(remaining.min(Duration::from_millis(100)))
                    {
                        return Some(Event::Log(format!("[LINUX] {warning}")));
                    }
                } else {
                    return self.pending.recv_timeout(remaining).ok();
                }
            }
        }
    }

    impl Drop for Session {
        fn drop(&mut self) {
            self.stop();
        }
    }

    #[cfg(test)]
    mod tests {
        use crate::{
            PrivacyRequirement, SessionConfig, SessionOptions, TranscriberClass,
            TranscriptionPolicy,
        };

        use super::Session;

        #[test]
        fn construction_and_record_only_policy_checks_do_not_touch_hardware() {
            let directory = tempfile::tempdir().unwrap();
            let policy = TranscriptionPolicy {
                privacy: PrivacyRequirement::OfflineRequired,
                preferred: TranscriberClass::LocalModel,
                allow_backend_fallback: false,
                allow_record_only: false,
            };
            let options = SessionOptions::new(SessionConfig::platform_default("en-US"), policy);
            let mut session = Session::new_with_options(directory.path(), options).unwrap();

            assert!(!session.has_started_capture());
            let error = session.start().unwrap_err();
            assert!(error.to_string().contains("forbids record-only fallback"));
            assert!(!session.has_started_capture());
        }

        #[test]
        fn invalid_locale_is_rejected_before_hardware_access() {
            let directory = tempfile::tempdir().unwrap();

            let error = Session::new(directory.path(), "en\0US").err().unwrap();

            assert!(matches!(error, crate::SessionError::InvalidLocale(_)));
        }
    }
}

#[cfg(all(
    not(target_os = "linux"),
    not(target_os = "macos"),
    not(target_os = "windows")
))]
mod imp {
    use std::path::Path;
    use std::time::Duration;

    use wisp_core::SourceLabel;

    use crate::error::{Result, SessionError};
    use crate::{Permission, PermissionStatus, SessionConfig, SessionOptions};

    /// `WispAudioKit` library version. Always empty on non-macOS targets.
    #[must_use]
    pub fn version() -> &'static str {
        ""
    }

    /// Stub — always reports `Granted` on non-macOS targets so callers can
    /// fall through to the (stubbed) session, which will then return
    /// `UnsupportedPlatform`. Keeps the workspace buildable on Linux CI.
    #[must_use]
    pub fn check_permission(_permission: Permission) -> PermissionStatus {
        PermissionStatus::Granted
    }

    /// Stub — see [`check_permission`].
    #[must_use]
    pub fn request_permission(_permission: Permission) -> PermissionStatus {
        PermissionStatus::Granted
    }

    /// One transcription update from a running [`Session`].
    #[derive(Debug, Clone, PartialEq)]
    pub struct SessionResult {
        pub source: SourceLabel,
        pub segment_id: u64,
        pub is_final: bool,
        pub text: String,
        pub start_seconds: f64,
        pub end_seconds: f64,
        pub confidence_mean: Option<f64>,
        pub confidence_min: Option<f64>,
    }

    /// Either a transcription result or a log line emitted by the session.
    #[derive(Debug, Clone, PartialEq)]
    pub enum Event {
        Result(SessionResult),
        Log(String),
    }

    /// Stub session — always returns [`SessionError::UnsupportedPlatform`].
    pub struct Session;

    impl Session {
        /// # Errors
        /// Always returns [`SessionError::UnsupportedPlatform`].
        pub fn new(
            _output_dir: impl AsRef<Path>,
            _locale: &str,
        ) -> Result<Self> {
            Err(SessionError::UnsupportedPlatform)
        }

        /// # Errors
        /// Always returns [`SessionError::UnsupportedPlatform`].
        pub fn new_with_config(
            _output_dir: impl AsRef<Path>,
            _config: SessionConfig,
        ) -> Result<Self> {
            Err(SessionError::UnsupportedPlatform)
        }

        /// # Errors
        /// Always returns [`SessionError::UnsupportedPlatform`].
        pub fn new_with_options(
            _output_dir: impl AsRef<Path>,
            _options: SessionOptions,
        ) -> Result<Self> {
            Err(SessionError::UnsupportedPlatform)
        }

        /// # Errors
        /// Always returns [`SessionError::UnsupportedPlatform`].
        pub fn start(&mut self) -> Result<()> {
            Err(SessionError::UnsupportedPlatform)
        }

        /// Always false on non-macOS targets.
        #[must_use]
        pub fn has_started_capture(&self) -> bool {
            false
        }

        /// No-op on unsupported targets.
        pub fn set_microphone_muted(
            &self,
            _muted: bool,
        ) {
        }

        /// No-op on non-macOS targets.
        pub fn stop(&self) {}

        /// Always returns `None`.
        #[must_use]
        pub fn try_recv(&self) -> Option<Event> {
            None
        }

        /// Always returns `None`.
        #[must_use]
        pub fn recv(&self) -> Option<Event> {
            None
        }

        /// Always returns `None`.
        #[must_use]
        pub fn recv_timeout(
            &self,
            _timeout: Duration,
        ) -> Option<Event> {
            None
        }
    }
}

#[cfg(target_os = "macos")]
pub(crate) use imp::NativeSession;
#[cfg(not(target_os = "macos"))]
pub use imp::Session;
pub use imp::version;
pub use imp::{Event, SessionResult, check_permission, request_permission};
pub use wisp_core::SourceLabel;

/// Source-compatible macOS session facade.
///
/// Every lifecycle and event operation is delegated to [`MacosSession`], so
/// legacy callers receive transcription only after PCM has crossed the
/// backend-neutral capture queue and [`SessionOrchestrator`].
#[cfg(target_os = "macos")]
pub struct Session {
    inner: std::sync::Mutex<MacosSession>,
    control_waiters: std::sync::atomic::AtomicUsize,
}

#[cfg(target_os = "macos")]
struct SessionControlGuard<'a>(&'a std::sync::atomic::AtomicUsize);

#[cfg(target_os = "macos")]
impl<'a> SessionControlGuard<'a> {
    fn new(waiters: &'a std::sync::atomic::AtomicUsize) -> Self {
        waiters.fetch_add(1, std::sync::atomic::Ordering::AcqRel);
        Self(waiters)
    }
}

#[cfg(target_os = "macos")]
impl Drop for SessionControlGuard<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, std::sync::atomic::Ordering::AcqRel);
    }
}

#[cfg(target_os = "macos")]
impl Session {
    const RECEIVE_POLL_INTERVAL: std::time::Duration = std::time::Duration::from_millis(10);

    /// Construct a platform-default macOS session.
    ///
    /// # Errors
    /// Returns path, locale, policy-selection, or native construction errors.
    pub fn new(
        output_dir: impl AsRef<Path>,
        locale: &str,
    ) -> Result<Self> {
        MacosSession::new(output_dir, locale).map(Self::from_macos)
    }

    /// Construct a macOS session from the source-compatible configuration.
    ///
    /// # Errors
    /// Returns path, locale, policy-selection, or native construction errors.
    pub fn new_with_config(
        output_dir: impl AsRef<Path>,
        config: SessionConfig,
    ) -> Result<Self> {
        MacosSession::new_with_config(output_dir, config).map(Self::from_macos)
    }

    /// Construct a macOS session with an explicit transcription policy.
    ///
    /// # Errors
    /// Returns path, locale, policy-selection, or native construction errors.
    pub fn new_with_options(
        output_dir: impl AsRef<Path>,
        options: SessionOptions,
    ) -> Result<Self> {
        MacosSession::new_with_options(output_dir, options).map(Self::from_macos)
    }

    fn from_macos(inner: MacosSession) -> Self {
        Self {
            inner: std::sync::Mutex::new(inner),
            control_waiters: std::sync::atomic::AtomicUsize::new(0),
        }
    }

    /// Start capture and the selected transcription backend.
    ///
    /// # Errors
    /// Returns a transactional platform/backend start failure.
    pub fn start(&mut self) -> Result<()> {
        self.lock_inner().start()
    }

    /// Whether native capture reached the running milestone.
    #[must_use]
    pub fn has_started_capture(&self) -> bool {
        self.lock_inner().has_started_capture()
    }

    /// Replace microphone PCM with silence while retaining both timelines.
    pub fn set_microphone_muted(
        &self,
        muted: bool,
    ) {
        let _control = SessionControlGuard::new(&self.control_waiters);
        self.lock_inner().set_microphone_muted(muted);
    }

    /// Gracefully stop and drain capture, recording, and transcription.
    pub fn stop(&self) {
        let _control = SessionControlGuard::new(&self.control_waiters);
        self.lock_inner().stop();
    }

    /// Receive the next compatibility event.
    #[must_use]
    pub fn recv(&self) -> Option<Event> {
        loop {
            if let Some(event) = self.recv_timeout(std::time::Duration::from_secs(1)) {
                return Some(event);
            }
            if self.lock_inner().is_stopped() {
                return None;
            }
        }
    }

    /// Receive the next compatibility event before `timeout`.
    #[must_use]
    pub fn recv_timeout(
        &self,
        timeout: std::time::Duration,
    ) -> Option<Event> {
        let started_at = std::time::Instant::now();
        let mut first_poll = true;
        loop {
            let remaining = timeout.saturating_sub(started_at.elapsed());
            if self
                .control_waiters
                .load(std::sync::atomic::Ordering::Acquire)
                > 0
            {
                if remaining.is_zero() {
                    return None;
                }
                std::thread::sleep(remaining.min(std::time::Duration::from_millis(1)));
                continue;
            }
            if !first_poll && remaining.is_zero() {
                return self.lock_inner().try_recv();
            }
            first_poll = false;
            let poll_interval = remaining.min(Self::RECEIVE_POLL_INTERVAL);
            let mut inner = self.lock_inner();
            if let Some(event) = inner.recv_timeout(poll_interval) {
                return Some(event);
            }
            if inner.is_stopped() {
                return None;
            }
            drop(inner);
        }
    }

    /// Poll for one compatibility event without blocking.
    #[must_use]
    pub fn try_recv(&self) -> Option<Event> {
        self.lock_inner().try_recv()
    }

    fn lock_inner(&self) -> std::sync::MutexGuard<'_, MacosSession> {
        self.inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

impl SessionResult {
    /// Convert the platform compatibility result into the backend-neutral
    /// transcript contract.
    #[must_use]
    pub fn transcript_event(&self) -> TranscriptEvent {
        let segment = TranscriptSegment {
            track_id: self.source.track_id(),
            segment_id: TranscriptSegmentId::new(self.segment_id),
            text: self.text.clone(),
            start_seconds: self.start_seconds,
            end_seconds: self.end_seconds,
            confidence_mean: self.confidence_mean,
            confidence_min: self.confidence_min,
        };
        if self.is_final {
            TranscriptEvent::Final(segment)
        } else {
            TranscriptEvent::Partial(segment)
        }
    }
}

impl Event {
    /// Return a backend-neutral transcript view for result events while
    /// retaining the existing event surface used by the desktop app.
    #[must_use]
    pub fn transcript_event(&self) -> Option<TranscriptEvent> {
        match self {
            Self::Result(result) => Some(result.transcript_event()),
            Self::Log(_) => None,
        }
    }
}

#[cfg(test)]
mod compatibility_tests {
    use std::sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    };

    use super::{
        BoundedCleanupHandle, CALLBACK_FINAL_CAPACITY, CALLBACK_LOG_CAPACITY,
        CALLBACK_PARTIAL_CAPACITY, CallbackEnqueue, CallbackEventClass, Event,
        MergedSessionReceive, OneShotSessionLifecycle, PrivacyRequirement, RecognizerBackend,
        SessionConfig, SessionResult, SourceLabel, TrackId, TranscriberClass, TranscriptEvent,
        TranscriptionPolicy, WINDOWS_CLEANUP_MAX_ATTEMPTS, WINDOWS_SPEECH_RUNTIME_CLEANUP,
        WindowsAllWaitHook, WindowsCleanupIntent, WindowsCleanupLevel, WindowsCleanupReport,
        WindowsCompletionClaim, WindowsCompletionClaimHook, WindowsPlatformStart,
        WindowsPlatformStartError, WindowsRuntimeControlPublisher, WindowsRuntimeFailureAction,
        WindowsRuntimeNotification, WindowsSpeechCleanupProgress, WindowsSpeechStartupCleanupState,
        WindowsStartTransactionGuard, WindowsStopTransitionHook, WindowsTranscriptionMode,
        callback_event_channel, callback_event_channel_with_final_gap, claim_and_wait_windows_stop,
        complete_windows_start_transaction, finalize_windows_cleanup_notification,
        finish_unwound_windows_start, format_windows_runtime_notification,
        recv_callback_session_channels, recv_session_channels, run_windows_session_cleanup,
        run_windows_speech_cleanup, run_windows_speech_startup_cleanup,
        run_windows_speech_startup_cleanup_bounded, select_windows_transcription_after_failure,
        select_windows_transcription_mode, start_windows_platform,
        try_recv_callback_session_channels, try_recv_callback_session_channels_with_control,
        windows_runtime_completion_notification, windows_runtime_failure_action,
        windows_speech_start_error,
    };

    struct FakeRecording {
        finalized: Arc<AtomicBool>,
        finalization_count: Arc<AtomicUsize>,
        drop_count: Arc<AtomicUsize>,
        cleanup_fails: bool,
    }

    impl FakeRecording {
        fn cleanup(self) -> std::result::Result<(), &'static str> {
            if !self.finalized.swap(true, Ordering::SeqCst) {
                self.finalization_count.fetch_add(1, Ordering::SeqCst);
            }
            if self.cleanup_fails {
                Err("cleanup failed")
            } else {
                Ok(())
            }
        }
    }

    impl Drop for FakeRecording {
        fn drop(&mut self) {
            self.drop_count.fetch_add(1, Ordering::SeqCst);
            if !self.finalized.swap(true, Ordering::SeqCst) {
                self.finalization_count.fetch_add(1, Ordering::SeqCst);
            }
        }
    }

    struct WindowsStartHarness {
        outcome: std::result::Result<
            WindowsPlatformStart<FakeRecording, ()>,
            WindowsPlatformStartError<()>,
        >,
        recording_starts: Arc<AtomicUsize>,
        speech_starts: Arc<AtomicUsize>,
        finalization_count: Arc<AtomicUsize>,
        drop_count: Arc<AtomicUsize>,
    }

    fn run_windows_start_harness(
        policy: TranscriptionPolicy,
        speech_succeeds: bool,
        cleanup_fails: bool,
    ) -> WindowsStartHarness {
        let recording_starts = Arc::new(AtomicUsize::new(0));
        let speech_starts = Arc::new(AtomicUsize::new(0));
        let finalized = Arc::new(AtomicBool::new(false));
        let finalization_count = Arc::new(AtomicUsize::new(0));
        let drop_count = Arc::new(AtomicUsize::new(0));
        let recording_starts_for_factory = Arc::clone(&recording_starts);
        let speech_starts_for_factory = Arc::clone(&speech_starts);
        let finalized_for_factory = Arc::clone(&finalized);
        let finalization_count_for_factory = Arc::clone(&finalization_count);
        let drop_count_for_factory = Arc::clone(&drop_count);
        let outcome = start_windows_platform::<FakeRecording, (), (), _, _>(
            policy,
            move || {
                recording_starts_for_factory.fetch_add(1, Ordering::SeqCst);
                Ok(FakeRecording {
                    finalized: finalized_for_factory,
                    finalization_count: finalization_count_for_factory,
                    drop_count: drop_count_for_factory,
                    cleanup_fails,
                })
            },
            move || {
                speech_starts_for_factory.fetch_add(1, Ordering::SeqCst);
                if speech_succeeds {
                    Ok(())
                } else {
                    Err("speech failed")
                }
            },
            FakeRecording::cleanup,
        );
        WindowsStartHarness {
            outcome,
            recording_starts,
            speech_starts,
            finalization_count,
            drop_count,
        }
    }

    #[test]
    fn callback_queue_coalesces_replaceable_partials() {
        let (sender, receiver) = callback_event_channel::<&str, u64>();

        assert_eq!(
            sender.try_send(CallbackEventClass::Partial(7), "old"),
            CallbackEnqueue::Enqueued
        );
        assert_eq!(
            sender.try_send(CallbackEventClass::Partial(7), "new"),
            CallbackEnqueue::Replaced
        );
        assert_eq!(receiver.try_recv(), Some("new"));
        assert_eq!(receiver.try_recv(), None);
    }

    #[test]
    fn callback_queue_reserves_finals_and_bounds_partials_and_logs() {
        let (sender, receiver) = callback_event_channel::<String, u64>();
        for index in 0..CALLBACK_LOG_CAPACITY {
            assert_eq!(
                sender.try_send(CallbackEventClass::Log, format!("log-{index}")),
                CallbackEnqueue::Enqueued
            );
        }
        assert_eq!(
            sender.try_send(CallbackEventClass::Log, "log-overflow".into()),
            CallbackEnqueue::DroppedFull
        );
        for index in 0..CALLBACK_PARTIAL_CAPACITY {
            assert_eq!(
                sender.try_send(
                    CallbackEventClass::Partial(index as u64),
                    format!("partial-{index}"),
                ),
                CallbackEnqueue::Enqueued
            );
        }
        assert_eq!(
            sender.try_send(
                CallbackEventClass::Partial(CALLBACK_PARTIAL_CAPACITY as u64),
                "partial-overflow".into(),
            ),
            CallbackEnqueue::DroppedFull
        );
        for index in 0..CALLBACK_FINAL_CAPACITY {
            assert_eq!(
                sender.try_send(
                    CallbackEventClass::Final(Some(1_000 + index as u64)),
                    format!("final-{index}"),
                ),
                CallbackEnqueue::Enqueued
            );
        }
        assert_eq!(
            sender.try_send(
                CallbackEventClass::Final(Some(2_000)),
                "final-overflow".into(),
            ),
            CallbackEnqueue::DroppedFull
        );

        assert_eq!(receiver.try_recv().as_deref(), Some("final-0"));
    }

    #[test]
    fn callback_enqueue_drops_instead_of_blocking_on_state_contention() {
        let (sender, receiver) = callback_event_channel::<&str, u64>();
        assert_eq!(
            sender.try_send(CallbackEventClass::Partial(1), "partial"),
            CallbackEnqueue::Enqueued
        );
        let state = sender.state.lock().unwrap();

        assert_eq!(
            sender.try_send(CallbackEventClass::Log, "busy"),
            CallbackEnqueue::DroppedBusy
        );
        assert_eq!(
            sender.try_send(CallbackEventClass::Final(Some(1)), "final"),
            CallbackEnqueue::Enqueued
        );
        drop(state);
        assert_eq!(receiver.try_recv(), Some("final"));
        assert_eq!(receiver.try_recv(), None);
    }

    #[test]
    fn callback_poll_does_not_consume_final_while_state_is_contended() {
        let (sender, receiver) = callback_event_channel::<&str, u64>();
        assert_eq!(
            sender.try_send(CallbackEventClass::Final(Some(1)), "final"),
            CallbackEnqueue::Enqueued
        );
        let state = sender.state.lock().unwrap();

        let started = std::time::Instant::now();
        assert_eq!(receiver.try_recv(), None);
        assert!(started.elapsed() < std::time::Duration::from_millis(50));
        assert_eq!(receiver.finals.len(), 1);

        drop(state);
        assert_eq!(receiver.try_recv(), Some("final"));
    }

    #[test]
    fn callback_timeout_preserves_deadline_and_queued_final_under_state_contention() {
        let (sender, receiver) = callback_event_channel::<&str, u64>();
        assert_eq!(
            sender.try_send(CallbackEventClass::Final(Some(1)), "final"),
            CallbackEnqueue::Enqueued
        );
        let state = Arc::clone(&sender.state);
        let entered = Arc::new(std::sync::Barrier::new(2));
        let release = Arc::new(std::sync::Barrier::new(2));
        let holder_entered = Arc::clone(&entered);
        let holder_release = Arc::clone(&release);
        let holder = std::thread::spawn(move || {
            let _state = state.lock().unwrap();
            holder_entered.wait();
            holder_release.wait();
        });
        entered.wait();

        let started = std::time::Instant::now();
        assert_eq!(
            receiver.recv_timeout(std::time::Duration::from_millis(20)),
            None
        );
        assert!(started.elapsed() >= std::time::Duration::from_millis(10));
        assert!(started.elapsed() < std::time::Duration::from_millis(200));
        assert_eq!(receiver.finals.len(), 1);

        release.wait();
        holder.join().unwrap();
        assert_eq!(receiver.try_recv(), Some("final"));
    }

    #[test]
    fn final_overflow_is_reported_before_lossy_events() {
        fn final_gap(dropped: u64) -> String {
            format!("final-gap-{dropped}")
        }

        let (sender, receiver) = callback_event_channel_with_final_gap::<String, u64>(final_gap);
        assert_eq!(
            sender.try_send(CallbackEventClass::Log, "lossy".into()),
            CallbackEnqueue::Enqueued
        );
        for key in 0..CALLBACK_FINAL_CAPACITY as u64 {
            assert_eq!(
                sender.try_send(CallbackEventClass::Final(Some(key)), format!("final-{key}"),),
                CallbackEnqueue::Enqueued
            );
        }
        assert_eq!(
            sender.try_send(
                CallbackEventClass::Final(Some(CALLBACK_FINAL_CAPACITY as u64)),
                "overflow".into(),
            ),
            CallbackEnqueue::DroppedFull
        );

        for key in 0..CALLBACK_FINAL_CAPACITY as u64 {
            assert_eq!(receiver.try_recv(), Some(format!("final-{key}")));
        }
        assert_eq!(receiver.try_recv().as_deref(), Some("final-gap-1"));
        assert_eq!(receiver.try_recv().as_deref(), Some("lossy"));
    }

    #[test]
    fn final_gap_publication_wakes_receiver_after_full_lane_is_drained() {
        fn final_gap(dropped: u64) -> String {
            format!("final-gap-{dropped}")
        }

        let (mut sender, mut receiver) =
            callback_event_channel_with_final_gap::<String, u64>(final_gap);
        for key in 0..CALLBACK_FINAL_CAPACITY as u64 {
            assert_eq!(
                sender.try_send(CallbackEventClass::Final(Some(key)), format!("final-{key}")),
                CallbackEnqueue::Enqueued
            );
        }
        let overflow_hook = Arc::new(super::CallbackFinalOverflowHook {
            entered: std::sync::Barrier::new(2),
            release: std::sync::Barrier::new(2),
        });
        sender.final_overflow_hook = Some(Arc::clone(&overflow_hook));
        let overflow_sender = sender.clone();
        let overflowing = std::thread::spawn(move || {
            overflow_sender.try_send(
                CallbackEventClass::Final(Some(CALLBACK_FINAL_CAPACITY as u64)),
                "overflow".into(),
            )
        });
        overflow_hook.entered.wait();

        for key in 0..CALLBACK_FINAL_CAPACITY as u64 {
            assert_eq!(receiver.try_recv(), Some(format!("final-{key}")));
        }
        let wait_hook = Arc::new(super::CallbackWaitHook {
            fired: AtomicBool::new(false),
            entered: std::sync::Barrier::new(2),
            release: std::sync::Barrier::new(2),
        });
        receiver.wait_hook = Some(Arc::clone(&wait_hook));
        let waiting =
            std::thread::spawn(move || receiver.recv_timeout(std::time::Duration::from_secs(1)));
        wait_hook.entered.wait();
        overflow_hook.release.wait();
        assert_eq!(overflowing.join().unwrap(), CallbackEnqueue::DroppedFull);
        wait_hook.release.wait();
        assert_eq!(waiting.join().unwrap().as_deref(), Some("final-gap-1"));
    }

    #[test]
    fn overflowed_keyed_final_reconciles_terminal_partial_state() {
        fn final_gap(dropped: u64) -> String {
            format!("final-gap-{dropped}")
        }

        let (sender, receiver) = callback_event_channel_with_final_gap::<String, u64>(final_gap);
        assert_eq!(
            sender.try_send(CallbackEventClass::Partial(100), "partial-100".into()),
            CallbackEnqueue::Enqueued
        );
        for key in 0..CALLBACK_FINAL_CAPACITY as u64 {
            assert_eq!(
                sender.try_send(CallbackEventClass::Final(Some(key)), format!("final-{key}")),
                CallbackEnqueue::Enqueued
            );
        }
        let state = sender.state.lock().unwrap();
        assert_eq!(
            sender.try_send(
                CallbackEventClass::Final(Some(100)),
                "overflow-final-100".into(),
            ),
            CallbackEnqueue::DroppedFull
        );
        assert_eq!(state.partials.len(), 1);
        drop(state);

        for key in 0..CALLBACK_FINAL_CAPACITY as u64 {
            assert_eq!(receiver.try_recv(), Some(format!("final-{key}")));
        }
        assert_eq!(receiver.try_recv().as_deref(), Some("final-gap-1"));
        assert_eq!(receiver.try_recv(), None);
        assert_eq!(
            sender.try_send(CallbackEventClass::Partial(100), "late-100".into()),
            CallbackEnqueue::DroppedFull
        );
        assert_eq!(receiver.try_recv(), None);
    }

    #[test]
    fn final_cleanup_repairs_edge_wake_before_refill() {
        let (sender, receiver) = callback_event_channel::<&str, u64>();
        assert_eq!(
            sender.try_send(CallbackEventClass::Partial(1), "partial"),
            CallbackEnqueue::Enqueued
        );
        assert_eq!(
            sender.try_send(CallbackEventClass::Final(Some(1)), "final"),
            CallbackEnqueue::Enqueued
        );
        assert!(receiver.lossy_ready.is_empty());
        assert!(!sender.state.lock().unwrap().lossy_wake_armed);
        assert_eq!(receiver.try_recv(), Some("final"));

        assert_eq!(
            sender.try_send(CallbackEventClass::Partial(2), "refill"),
            CallbackEnqueue::Enqueued
        );
        assert_eq!(receiver.lossy_ready.len(), 1);
        assert_eq!(
            receiver.recv_timeout(std::time::Duration::from_millis(20)),
            Some("refill")
        );
    }

    #[test]
    fn finalized_high_watermark_rejects_late_partial_after_many_finals() {
        let (sender, receiver) = callback_event_channel::<String, u64>();

        for key in 0..=(CALLBACK_PARTIAL_CAPACITY as u64 + 8) {
            assert_eq!(
                sender.try_send(CallbackEventClass::Partial(key), format!("partial-{key}")),
                CallbackEnqueue::Enqueued
            );
            assert_eq!(
                sender.try_send(CallbackEventClass::Final(Some(key)), format!("final-{key}")),
                CallbackEnqueue::Enqueued
            );
            assert!(sender.state.lock().unwrap().partials.is_empty());
            assert_eq!(receiver.try_recv(), Some(format!("final-{key}")));
        }

        assert_eq!(
            sender.try_send(CallbackEventClass::Partial(0), "late-first".into()),
            CallbackEnqueue::DroppedFull
        );
        assert_eq!(receiver.try_recv(), None);
    }

    #[test]
    fn edge_wake_survives_concurrent_full_capacity_final_and_refill() {
        let (mut sender, receiver) = callback_event_channel::<String, u64>();
        for key in 0..CALLBACK_PARTIAL_CAPACITY as u64 {
            assert_eq!(
                sender.try_send(CallbackEventClass::Partial(key), format!("partial-{key}")),
                CallbackEnqueue::Enqueued
            );
        }
        for index in 0..CALLBACK_LOG_CAPACITY {
            assert_eq!(
                sender.try_send(CallbackEventClass::Log, format!("log-{index}")),
                CallbackEnqueue::Enqueued
            );
        }

        let hook = std::sync::Arc::new(super::CallbackFinalCleanupHook {
            entered: std::sync::Barrier::new(2),
            release: std::sync::Barrier::new(2),
        });
        sender.final_cleanup_hook = Some(std::sync::Arc::clone(&hook));
        let final_sender = sender.clone();
        let final_thread = std::thread::spawn(move || {
            final_sender.try_send(CallbackEventClass::Final(Some(0)), "final-0".into())
        });
        hook.entered.wait();
        assert_eq!(
            sender.try_send(
                CallbackEventClass::Partial(CALLBACK_PARTIAL_CAPACITY as u64),
                "refill".into(),
            ),
            CallbackEnqueue::Enqueued
        );
        hook.release.wait();
        assert_eq!(final_thread.join().unwrap(), CallbackEnqueue::Enqueued);
        assert_eq!(receiver.try_recv().as_deref(), Some("final-0"));

        let mut drained = 0;
        while receiver.try_recv().is_some() {
            drained += 1;
        }
        assert_eq!(drained, CALLBACK_PARTIAL_CAPACITY + CALLBACK_LOG_CAPACITY);
    }

    #[test]
    fn finalized_high_watermarks_are_independent_per_source() {
        let (sender, receiver) = callback_event_channel::<&str, (SourceLabel, u64)>();
        sender.try_send(
            CallbackEventClass::Final(Some((SourceLabel::Mic, 100))),
            "mic-final",
        );
        sender.try_send(
            CallbackEventClass::Final(Some((SourceLabel::System, 2))),
            "system-final",
        );
        assert_eq!(receiver.try_recv(), Some("mic-final"));
        assert_eq!(receiver.try_recv(), Some("system-final"));
        assert_eq!(
            sender.try_send(
                CallbackEventClass::Partial((SourceLabel::Mic, 99)),
                "late-mic",
            ),
            CallbackEnqueue::DroppedFull
        );
        assert_eq!(
            sender.try_send(
                CallbackEventClass::Partial((SourceLabel::System, 3)),
                "new-system",
            ),
            CallbackEnqueue::Enqueued
        );
        assert_eq!(receiver.try_recv(), Some("new-system"));
    }

    #[test]
    fn offline_windows_policy_never_selects_online_platform() {
        let policy = TranscriptionPolicy {
            privacy: PrivacyRequirement::OfflineRequired,
            preferred: TranscriberClass::Platform,
            allow_backend_fallback: true,
            allow_record_only: true,
        };

        assert!(matches!(
            select_windows_transcription_mode(RecognizerBackend::Platform, policy).unwrap(),
            WindowsTranscriptionMode::RecordOnly { .. }
        ));
    }

    #[test]
    fn offline_windows_policy_can_forbid_record_only() {
        let policy = TranscriptionPolicy {
            privacy: PrivacyRequirement::OfflineRequired,
            preferred: TranscriberClass::Platform,
            allow_backend_fallback: false,
            allow_record_only: false,
        };

        assert!(select_windows_transcription_mode(RecognizerBackend::Platform, policy).is_err());
    }

    #[test]
    fn configured_local_model_is_record_only_until_adapter_is_connected() {
        let options: super::SessionOptions =
            SessionConfig::local_model("en-US", "/models/ggml-base.bin").into();

        assert!(matches!(
            select_windows_transcription_mode(
                options.config().recognizer,
                options.transcription_policy(),
            )
            .unwrap(),
            WindowsTranscriptionMode::RecordOnly { .. }
        ));
    }

    #[test]
    fn inconsistent_legacy_recognizer_and_policy_are_rejected() {
        assert!(
            select_windows_transcription_mode(
                RecognizerBackend::Platform,
                TranscriptionPolicy::offline_local_model(),
            )
            .is_err()
        );
    }

    #[test]
    fn windows_runtime_failure_reselects_before_record_only_or_error() {
        let failed = super::BackendId::new("windows-platform-online");
        let record_only_policy = TranscriptionPolicy {
            allow_backend_fallback: true,
            ..TranscriptionPolicy::platform_default()
        };
        assert!(matches!(
            select_windows_transcription_after_failure(record_only_policy, &failed).unwrap(),
            WindowsTranscriptionMode::RecordOnly { .. }
        ));
        assert!(matches!(
            windows_runtime_failure_action(record_only_policy),
            WindowsRuntimeFailureAction::ContinueRecording { .. }
        ));
        assert!(matches!(
            windows_runtime_completion_notification(record_only_policy, "NetworkFailure"),
            WindowsRuntimeNotification::ContinueRecording(message)
                if message.contains("NetworkFailure")
                    && message.contains("continuing with local WASAPI recording")
        ));
        let fail_policy = TranscriptionPolicy {
            allow_backend_fallback: true,
            allow_record_only: false,
            ..TranscriptionPolicy::platform_default()
        };
        assert!(select_windows_transcription_after_failure(fail_policy, &failed).is_err());
        assert!(matches!(
            windows_runtime_failure_action(fail_policy),
            WindowsRuntimeFailureAction::FailStart { .. }
        ));
        assert!(matches!(
            windows_runtime_completion_notification(fail_policy, "MicrophoneUnavailable"),
            WindowsRuntimeNotification::Fatal(message)
                if message.contains("MicrophoneUnavailable")
        ));
    }

    struct CompletionCleanupHarness {
        lifecycle: Arc<OneShotSessionLifecycle>,
        publisher: Arc<WindowsRuntimeControlPublisher>,
        cleanup: BoundedCleanupHandle,
        worker: Option<std::thread::JoinHandle<()>>,
        completed: crossbeam_channel::Receiver<WindowsCleanupLevel>,
        notifications: crossbeam_channel::Receiver<WindowsRuntimeNotification>,
        reports: Arc<TestWindowsCleanupReportLatch>,
        cleanup_attempts: Arc<AtomicUsize>,
        speech_stops: Arc<AtomicUsize>,
        tokens_removed: Arc<AtomicUsize>,
        speech_closes: Arc<AtomicUsize>,
        recording_stops: Arc<AtomicUsize>,
    }

    #[derive(Default)]
    struct TestWindowsCleanupReportLatch {
        latest: std::sync::Mutex<Option<(u64, WindowsCleanupReport)>>,
        changed: std::sync::Condvar,
    }

    impl TestWindowsCleanupReportLatch {
        fn publish(
            &self,
            generation: u64,
            report: WindowsCleanupReport,
        ) {
            let mut latest = self
                .latest
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            *latest = Some((generation, report));
            self.changed.notify_all();
        }

        fn wait(
            &self,
            generation: u64,
        ) -> WindowsCleanupReport {
            let mut latest = self
                .latest
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            while latest
                .as_ref()
                .is_none_or(|(reported, _)| *reported < generation)
            {
                latest = self
                    .changed
                    .wait(latest)
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
            }
            latest
                .as_ref()
                .map(|(_, report)| report.clone())
                .unwrap_or_default()
        }

        fn is_published(
            &self,
            generation: u64,
        ) -> bool {
            self.latest
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .as_ref()
                .is_some_and(|(reported, _)| *reported >= generation)
        }
    }

    struct TestWindowsStartupResource {
        finalized: AtomicBool,
        finalization_count: Arc<AtomicUsize>,
    }

    impl TestWindowsStartupResource {
        fn new(finalization_count: Arc<AtomicUsize>) -> Self {
            Self {
                finalized: AtomicBool::new(false),
                finalization_count,
            }
        }

        fn finalize(&self) {
            if !self.finalized.swap(true, Ordering::SeqCst) {
                self.finalization_count.fetch_add(1, Ordering::SeqCst);
            }
        }
    }

    impl Drop for TestWindowsStartupResource {
        fn drop(&mut self) {
            self.finalize();
        }
    }

    #[derive(Default)]
    struct TestWindowsStartupResourceState {
        installed: Option<Arc<TestWindowsStartupResource>>,
        startup_finished: bool,
    }

    struct TestWindowsStartupResources {
        state: std::sync::Mutex<TestWindowsStartupResourceState>,
        changed: std::sync::Condvar,
        worker_waiting: crossbeam_channel::Sender<()>,
        startup_publications: Arc<AtomicUsize>,
    }

    impl TestWindowsStartupResources {
        fn new(
            worker_waiting: crossbeam_channel::Sender<()>,
            startup_publications: Arc<AtomicUsize>,
        ) -> Self {
            Self {
                state: std::sync::Mutex::new(TestWindowsStartupResourceState::default()),
                changed: std::sync::Condvar::new(),
                worker_waiting,
                startup_publications,
            }
        }

        fn install(
            &self,
            resource: Arc<TestWindowsStartupResource>,
        ) {
            let mut state = self
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            state.installed = Some(resource);
            self.changed.notify_all();
        }

        fn finish_startup(&self) {
            let mut state = self
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if !state.startup_finished {
                state.startup_finished = true;
                self.startup_publications.fetch_add(1, Ordering::SeqCst);
            }
            self.changed.notify_all();
        }

        fn cleanup(&self) -> Option<String> {
            let mut state = self
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if !state.startup_finished {
                self.worker_waiting.send(()).unwrap();
            }
            while !state.startup_finished {
                state = self
                    .changed
                    .wait(state)
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
            }
            let installed = state.installed.clone();
            drop(state);
            if let Some(installed) = installed {
                installed.finalize();
            }
            None
        }
    }

    impl CompletionCleanupHarness {
        fn running(
            policy: TranscriptionPolicy,
            cleanup_fails: bool,
        ) -> Self {
            Self::running_with_failures(policy, if cleanup_fails { usize::MAX } else { 0 })
        }

        fn running_with_failures(
            policy: TranscriptionPolicy,
            failure_attempts: usize,
        ) -> Self {
            let lifecycle = Arc::new(OneShotSessionLifecycle::new());
            assert!(lifecycle.begin_start());
            assert!(lifecycle.complete_start());
            Self::with_lifecycle(lifecycle, policy, failure_attempts)
        }

        fn starting(
            policy: TranscriptionPolicy,
            cleanup_fails: bool,
        ) -> Self {
            let lifecycle = Arc::new(OneShotSessionLifecycle::new());
            assert!(lifecycle.begin_start());
            Self::with_lifecycle(
                lifecycle,
                policy,
                if cleanup_fails { usize::MAX } else { 0 },
            )
        }

        #[allow(clippy::too_many_lines)]
        fn with_lifecycle(
            lifecycle: Arc<OneShotSessionLifecycle>,
            policy: TranscriptionPolicy,
            failure_attempts: usize,
        ) -> Self {
            let speech_stops = Arc::new(AtomicUsize::new(0));
            let tokens_removed = Arc::new(AtomicUsize::new(0));
            let speech_closes = Arc::new(AtomicUsize::new(0));
            let recording_stops = Arc::new(AtomicUsize::new(0));
            let cleanup_attempts = Arc::new(AtomicUsize::new(0));
            let reports = Arc::new(TestWindowsCleanupReportLatch::default());
            let worker_speech_stops = Arc::clone(&speech_stops);
            let worker_tokens_removed = Arc::clone(&tokens_removed);
            let worker_speech_closes = Arc::clone(&speech_closes);
            let worker_recording_stops = Arc::clone(&recording_stops);
            let worker_cleanup_attempts = Arc::clone(&cleanup_attempts);
            let worker_reports = Arc::clone(&reports);
            let (cleanup_intents, cleanup_intent_receiver) =
                crossbeam_channel::bounded::<WindowsCleanupIntent>(1);
            let (notification_sender, notifications) = crossbeam_channel::bounded(1);
            let (completed_sender, completed) = crossbeam_channel::bounded(4);
            let mut speech_progress = WindowsSpeechCleanupProgress::default();
            let mut recording_complete = false;
            let (cleanup, worker) = BoundedCleanupHandle::spawn(
                move |level| {
                    worker_cleanup_attempts.fetch_add(1, Ordering::SeqCst);
                    run_windows_session_cleanup(
                        level,
                        &mut recording_complete,
                        || {
                            run_windows_speech_cleanup(
                                &mut speech_progress,
                                WINDOWS_SPEECH_RUNTIME_CLEANUP,
                                || {
                                    worker_speech_stops.fetch_add(1, Ordering::SeqCst);
                                    Ok(())
                                },
                                || {
                                    worker_tokens_removed.fetch_add(1, Ordering::SeqCst);
                                    Ok(())
                                },
                                || {
                                    worker_tokens_removed.fetch_add(1, Ordering::SeqCst);
                                    Ok(())
                                },
                                || {
                                    let attempt =
                                        worker_speech_closes.fetch_add(1, Ordering::SeqCst) + 1;
                                    if attempt <= failure_attempts {
                                        Err("injected speech Close failure".into())
                                    } else {
                                        Ok(())
                                    }
                                },
                            )
                        },
                        || {
                            let attempt = worker_recording_stops.fetch_add(1, Ordering::SeqCst) + 1;
                            if attempt <= failure_attempts {
                                Err("injected recording stop failure".into())
                            } else {
                                Ok(())
                            }
                        },
                    )
                },
                move |level, generation, cleanup_error| {
                    let notification = finalize_windows_cleanup_notification(
                        cleanup_intent_receiver.try_recv().ok(),
                        level,
                        cleanup_error.as_deref(),
                    );
                    if let Some(notification) = notification.clone() {
                        notification_sender.send(notification).unwrap();
                    }
                    if level == WindowsCleanupLevel::All {
                        worker_reports.publish(
                            generation,
                            WindowsCleanupReport {
                                error: cleanup_error,
                                notification,
                            },
                        );
                    }
                    completed_sender.send(level).unwrap();
                },
            )
            .unwrap();
            let publisher = Arc::new(WindowsRuntimeControlPublisher::new(
                Arc::clone(&lifecycle),
                cleanup_intents,
                cleanup.clone(),
                policy,
            ));
            Self {
                lifecycle,
                publisher,
                cleanup,
                worker: Some(worker),
                completed,
                notifications,
                reports,
                cleanup_attempts,
                speech_stops,
                tokens_removed,
                speech_closes,
                recording_stops,
            }
        }

        fn shutdown(mut self) {
            self.cleanup.shutdown();
            self.worker.take().unwrap().join().unwrap();
        }
    }

    struct InjectedWindowsSessionDrop {
        lifecycle: Arc<OneShotSessionLifecycle>,
        cleanup: BoundedCleanupHandle,
        reports: Arc<TestWindowsCleanupReportLatch>,
        observed_report: Arc<std::sync::Mutex<Option<WindowsCleanupReport>>>,
    }

    impl Drop for InjectedWindowsSessionDrop {
        fn drop(&mut self) {
            let (_, _, report) = claim_and_wait_windows_stop(
                &self.lifecycle,
                || self.cleanup.request(WindowsCleanupLevel::All),
                |generation| {
                    self.cleanup.wait_for_all_completion(generation);
                    self.reports.wait(generation)
                },
            );
            *self
                .observed_report
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = report;
        }
    }

    #[test]
    fn session_drop_runs_bounded_all_cleanup_and_observes_the_terminal_report() {
        let harness =
            CompletionCleanupHarness::running(TranscriptionPolicy::platform_default(), true);
        let observed_report = Arc::new(std::sync::Mutex::new(None));
        drop(InjectedWindowsSessionDrop {
            lifecycle: Arc::clone(&harness.lifecycle),
            cleanup: harness.cleanup.clone(),
            reports: Arc::clone(&harness.reports),
            observed_report: Arc::clone(&observed_report),
        });

        assert_eq!(harness.completed.recv().unwrap(), WindowsCleanupLevel::All);
        assert_eq!(
            harness.cleanup_attempts.load(Ordering::SeqCst),
            WINDOWS_CLEANUP_MAX_ATTEMPTS
        );
        assert_eq!(harness.speech_stops.load(Ordering::SeqCst), 1);
        assert_eq!(harness.tokens_removed.load(Ordering::SeqCst), 2);
        assert_eq!(
            harness.speech_closes.load(Ordering::SeqCst),
            WINDOWS_CLEANUP_MAX_ATTEMPTS
        );
        assert_eq!(
            harness.recording_stops.load(Ordering::SeqCst),
            WINDOWS_CLEANUP_MAX_ATTEMPTS
        );
        assert!(harness.notifications.is_empty());
        assert_eq!(
            observed_report
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .as_ref()
                .and_then(|report| report.error.as_deref()),
            Some(
                "Windows speech cleanup failed: Close: injected speech Close failure; WASAPI recording cleanup failed: injected recording stop failure"
            )
        );
        assert_eq!(
            harness.cleanup.request(WindowsCleanupLevel::All),
            (1, false)
        );
        harness.shutdown();
    }

    #[test]
    fn fatal_completed_stops_recording_before_user_poll() {
        let fatal_policy = TranscriptionPolicy {
            allow_backend_fallback: true,
            allow_record_only: false,
            ..TranscriptionPolicy::platform_default()
        };
        let harness = CompletionCleanupHarness::running(fatal_policy, true);

        assert!(harness.publisher.publish("NetworkFailure"));
        assert_eq!(harness.completed.recv().unwrap(), WindowsCleanupLevel::All);
        assert_eq!(harness.speech_stops.load(Ordering::SeqCst), 1);
        assert_eq!(harness.tokens_removed.load(Ordering::SeqCst), 2);
        assert_eq!(
            harness.speech_closes.load(Ordering::SeqCst),
            WINDOWS_CLEANUP_MAX_ATTEMPTS
        );
        assert_eq!(
            harness.recording_stops.load(Ordering::SeqCst),
            WINDOWS_CLEANUP_MAX_ATTEMPTS
        );
        assert_eq!(
            harness.cleanup_attempts.load(Ordering::SeqCst),
            WINDOWS_CLEANUP_MAX_ATTEMPTS
        );
        assert_eq!(harness.notifications.len(), 1);
        assert!(!harness.publisher.publish("duplicate completion"));
        assert!(!harness.lifecycle.claim_stop());
        assert_eq!(
            harness.recording_stops.load(Ordering::SeqCst),
            WINDOWS_CLEANUP_MAX_ATTEMPTS
        );

        let (_main_sender, main_receiver) = callback_event_channel::<String, u64>();
        let (_fatal_sender, fatal_receiver) = crossbeam_channel::bounded(1);
        let (_warning_sender, warning_receiver) = crossbeam_channel::bounded(1);
        let Some(MergedSessionReceive::RuntimeControl(notification)) =
            try_recv_callback_session_channels_with_control(
                &main_receiver,
                &fatal_receiver,
                Some(&harness.notifications),
                &warning_receiver,
            )
        else {
            panic!("expected completed cleanup notification");
        };
        let message = format_windows_runtime_notification(notification);
        assert!(message.starts_with("[FATAL]"));
        assert!(message.contains("injected speech Close failure"));
        assert!(message.contains("injected recording stop failure"));
        harness.shutdown();
    }

    #[test]
    fn continue_completed_keeps_recording_and_cleans_speech_once() {
        let harness = CompletionCleanupHarness::running_with_failures(
            TranscriptionPolicy::platform_default(),
            WINDOWS_CLEANUP_MAX_ATTEMPTS - 1,
        );

        assert!(harness.publisher.publish("NetworkFailure"));
        assert_eq!(
            harness.completed.recv().unwrap(),
            WindowsCleanupLevel::SpeechOnly
        );
        assert!(harness.lifecycle.capture_is_running());
        assert!(!harness.lifecycle.speech_is_running());
        assert_eq!(harness.speech_stops.load(Ordering::SeqCst), 1);
        assert_eq!(harness.tokens_removed.load(Ordering::SeqCst), 2);
        assert_eq!(
            harness.speech_closes.load(Ordering::SeqCst),
            WINDOWS_CLEANUP_MAX_ATTEMPTS
        );
        assert_eq!(harness.recording_stops.load(Ordering::SeqCst), 0);
        assert_eq!(harness.notifications.len(), 1);
        assert!(!harness.publisher.publish("duplicate completion"));

        assert!(harness.lifecycle.claim_stop());
        harness.cleanup.request(WindowsCleanupLevel::All);
        assert_eq!(harness.completed.recv().unwrap(), WindowsCleanupLevel::All);
        assert_eq!(harness.speech_stops.load(Ordering::SeqCst), 1);
        assert_eq!(harness.tokens_removed.load(Ordering::SeqCst), 2);
        assert_eq!(
            harness.recording_stops.load(Ordering::SeqCst),
            WINDOWS_CLEANUP_MAX_ATTEMPTS
        );
        assert_eq!(
            harness.cleanup_attempts.load(Ordering::SeqCst),
            WINDOWS_CLEANUP_MAX_ATTEMPTS * 2
        );
        harness.shutdown();
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn fatal_claim_stages_intent_and_reserves_all_cleanup_before_stop_can_join() {
        let lifecycle = Arc::new(OneShotSessionLifecycle::new());
        assert!(lifecycle.begin_start());
        assert!(lifecycle.complete_start());
        let (claim_observed_sender, claim_observed) = crossbeam_channel::bounded(1);
        let (claim_release, claim_release_receiver) = crossbeam_channel::bounded(1);
        lifecycle.set_completion_claim_hook(Arc::new(WindowsCompletionClaimHook {
            observed: claim_observed_sender,
            release: claim_release_receiver,
        }));
        let (stop_contended_sender, stop_contended) = crossbeam_channel::bounded(1);
        let (stop_acquired_sender, stop_acquired) = crossbeam_channel::bounded(1);
        lifecycle.set_stop_transition_hook(Arc::new(WindowsStopTransitionHook {
            contended: stop_contended_sender,
            acquired: stop_acquired_sender,
        }));

        let reports = Arc::new(TestWindowsCleanupReportLatch::default());
        let worker_reports = Arc::clone(&reports);
        let (cleanup_intents, cleanup_intent_receiver) =
            crossbeam_channel::bounded::<WindowsCleanupIntent>(1);
        let (cleanup_entered_sender, cleanup_entered) = crossbeam_channel::bounded(1);
        let (cleanup_release, cleanup_release_receiver) = crossbeam_channel::bounded(1);
        let (notification_sender, notifications) = crossbeam_channel::bounded(1);
        let (cleanup, worker) = BoundedCleanupHandle::spawn(
            move |level| {
                assert_eq!(level, WindowsCleanupLevel::All);
                cleanup_entered_sender.send(()).unwrap();
                cleanup_release_receiver.recv().unwrap();
                None
            },
            move |level, generation, error| {
                let notification = finalize_windows_cleanup_notification(
                    cleanup_intent_receiver.try_recv().ok(),
                    level,
                    error.as_deref(),
                );
                if let Some(notification) = notification.clone() {
                    notification_sender.send(notification).unwrap();
                }
                worker_reports.publish(
                    generation,
                    WindowsCleanupReport {
                        error,
                        notification,
                    },
                );
            },
        )
        .unwrap();
        let fatal_policy = TranscriptionPolicy {
            allow_backend_fallback: true,
            allow_record_only: false,
            ..TranscriptionPolicy::platform_default()
        };
        let publisher = Arc::new(WindowsRuntimeControlPublisher::new(
            Arc::clone(&lifecycle),
            cleanup_intents,
            cleanup.clone(),
            fatal_policy,
        ));

        let publishing = {
            let publisher = Arc::clone(&publisher);
            std::thread::spawn(move || publisher.publish("fatal-publication-race"))
        };
        assert_eq!(
            claim_observed.recv().unwrap(),
            WindowsCompletionClaim::Fatal
        );

        let (stop_done_sender, stop_done) = crossbeam_channel::bounded(1);
        let stopping = {
            let lifecycle = Arc::clone(&lifecycle);
            let cleanup = cleanup.clone();
            let reports = Arc::clone(&reports);
            std::thread::spawn(move || {
                let result = claim_and_wait_windows_stop(
                    &lifecycle,
                    || cleanup.request(WindowsCleanupLevel::All),
                    |generation| {
                        cleanup.wait_for_all_completion(generation);
                        reports.wait(generation)
                    },
                );
                stop_done_sender.send(result).unwrap();
            })
        };
        stop_contended.recv().unwrap();
        assert!(stop_acquired.try_recv().is_err());
        assert!(cleanup_entered.try_recv().is_err());
        assert!(notifications.is_empty());
        assert!(stop_done.try_recv().is_err());

        claim_release.send(()).unwrap();
        assert!(publishing.join().unwrap());
        stop_acquired.recv().unwrap();
        assert_eq!(cleanup.wait_for_all_generation(), 1);
        cleanup_entered.recv().unwrap();
        assert!(notifications.is_empty());
        assert!(stop_done.try_recv().is_err());
        cleanup_release.send(()).unwrap();

        let (claimed, started_retry, report) = stop_done.recv().unwrap();
        assert!(!claimed);
        assert!(!started_retry);
        assert!(matches!(
            report.and_then(|report| report.notification),
            Some(WindowsRuntimeNotification::Fatal(message))
                if message.contains("fatal-publication-race")
        ));
        assert!(matches!(
            notifications.recv().unwrap(),
            WindowsRuntimeNotification::Fatal(message)
                if message.contains("fatal-publication-race")
        ));
        stopping.join().unwrap();
        cleanup.shutdown();
        worker.join().unwrap();
    }

    #[test]
    fn stop_after_fatal_waits_for_cleanup_and_notification_publication() {
        let lifecycle = Arc::new(OneShotSessionLifecycle::new());
        assert!(lifecycle.begin_start());
        assert!(lifecycle.complete_start());
        let report_latch = Arc::new(TestWindowsCleanupReportLatch::default());
        let worker_report_latch = Arc::clone(&report_latch);
        let cleanup_count = Arc::new(AtomicUsize::new(0));
        let worker_cleanup_count = Arc::clone(&cleanup_count);
        let (cleanup_intents, cleanup_intent_receiver) =
            crossbeam_channel::bounded::<WindowsCleanupIntent>(1);
        let (cleanup_entered_sender, cleanup_entered) = crossbeam_channel::bounded(1);
        let (release_sender, release) = crossbeam_channel::bounded(1);
        let (notification_sender, notifications) = crossbeam_channel::bounded(1);
        let (cleanup, worker) = BoundedCleanupHandle::spawn(
            move |_level| {
                worker_cleanup_count.fetch_add(1, Ordering::SeqCst);
                cleanup_entered_sender.send(()).unwrap();
                release.recv().unwrap();
                None
            },
            move |level, generation, cleanup_error| {
                let notification = finalize_windows_cleanup_notification(
                    cleanup_intent_receiver.try_recv().ok(),
                    level,
                    cleanup_error.as_deref(),
                );
                if let Some(notification) = notification.clone() {
                    notification_sender.send(notification).unwrap();
                }
                worker_report_latch.publish(
                    generation,
                    WindowsCleanupReport {
                        error: cleanup_error,
                        notification,
                    },
                );
            },
        )
        .unwrap();
        let fatal_policy = TranscriptionPolicy {
            allow_backend_fallback: true,
            allow_record_only: false,
            ..TranscriptionPolicy::platform_default()
        };
        let publisher = WindowsRuntimeControlPublisher::new(
            Arc::clone(&lifecycle),
            cleanup_intents,
            cleanup.clone(),
            fatal_policy,
        );
        let (wait_observed_sender, wait_observed) = crossbeam_channel::bounded(2);
        let (wait_release, wait_release_receiver) = crossbeam_channel::bounded(2);
        cleanup.set_all_wait_hook(Arc::new(WindowsAllWaitHook {
            observed: wait_observed_sender,
            release: wait_release_receiver,
        }));
        assert!(publisher.publish("fatal-before-stop"));
        cleanup_entered.recv().unwrap();

        let (entered_sender, entered) = crossbeam_channel::bounded(2);
        let (done_sender, done) = crossbeam_channel::bounded(2);
        let mut stopping = Vec::new();
        for _ in 0..2 {
            let thread_lifecycle = Arc::clone(&lifecycle);
            let thread_cleanup = cleanup.clone();
            let thread_latch = Arc::clone(&report_latch);
            let thread_entered = entered_sender.clone();
            let thread_done = done_sender.clone();
            stopping.push(std::thread::spawn(move || {
                thread_entered.send(()).unwrap();
                let (requested, _started_retry, report) = claim_and_wait_windows_stop(
                    &thread_lifecycle,
                    || thread_cleanup.request(WindowsCleanupLevel::All),
                    |generation| {
                        assert_eq!(thread_cleanup.wait_for_all_generation(), generation);
                        thread_cleanup.wait_for_all_completion(generation);
                        thread_latch.wait(generation)
                    },
                );
                assert!(!requested);
                assert!(report.is_some());
                thread_done.send(()).unwrap();
            }));
        }
        entered.recv().unwrap();
        entered.recv().unwrap();
        assert_eq!(wait_observed.recv().unwrap(), 1);
        assert!(!report_latch.is_published(1));
        assert!(done.try_recv().is_err());
        release_sender.send(()).unwrap();
        assert!(report_latch.wait(1).error.is_none());
        wait_release.send(()).unwrap();
        done.recv().unwrap();
        done.recv().unwrap();
        assert_eq!(notifications.len(), 1);
        assert_eq!(cleanup_count.load(Ordering::SeqCst), 1);
        for stopping in stopping {
            stopping.join().unwrap();
        }
        cleanup.shutdown();
        worker.join().unwrap();
    }

    #[test]
    fn concurrent_stop_callers_wait_for_the_same_all_cleanup() {
        let lifecycle = Arc::new(OneShotSessionLifecycle::new());
        assert!(lifecycle.begin_start());
        assert!(lifecycle.complete_start());
        let report_latch = Arc::new(TestWindowsCleanupReportLatch::default());
        let worker_report_latch = Arc::clone(&report_latch);
        let cleanup_count = Arc::new(AtomicUsize::new(0));
        let worker_cleanup_count = Arc::clone(&cleanup_count);
        let (cleanup_entered_sender, cleanup_entered) = crossbeam_channel::bounded(1);
        let (release_sender, release) = crossbeam_channel::bounded(1);
        let (cleanup, worker) = BoundedCleanupHandle::spawn(
            move |level| {
                assert_eq!(level, WindowsCleanupLevel::All);
                worker_cleanup_count.fetch_add(1, Ordering::SeqCst);
                cleanup_entered_sender.send(()).unwrap();
                release.recv().unwrap();
                None
            },
            move |_level, generation, error| {
                worker_report_latch.publish(
                    generation,
                    WindowsCleanupReport {
                        error,
                        notification: None,
                    },
                );
            },
        )
        .unwrap();
        let (wait_observed_sender, wait_observed) = crossbeam_channel::bounded(2);
        let (wait_release, wait_release_receiver) = crossbeam_channel::bounded(2);
        cleanup.set_all_wait_hook(Arc::new(WindowsAllWaitHook {
            observed: wait_observed_sender,
            release: wait_release_receiver,
        }));

        let first_lifecycle = Arc::clone(&lifecycle);
        let first_cleanup = cleanup.clone();
        let first_latch = Arc::clone(&report_latch);
        let (done_sender, done) = crossbeam_channel::bounded(2);
        let first_done = done_sender.clone();
        let first = std::thread::spawn(move || {
            let result = claim_and_wait_windows_stop(
                &first_lifecycle,
                || first_cleanup.request(WindowsCleanupLevel::All),
                |generation| {
                    assert_eq!(first_cleanup.wait_for_all_generation(), generation);
                    first_cleanup.wait_for_all_completion(generation);
                    first_latch.wait(generation)
                },
            );
            first_done.send(result.0).unwrap();
        });
        cleanup_entered.recv().unwrap();

        let second_lifecycle = Arc::clone(&lifecycle);
        let second_cleanup = cleanup.clone();
        let second_latch = Arc::clone(&report_latch);
        let second = std::thread::spawn(move || {
            let result = claim_and_wait_windows_stop(
                &second_lifecycle,
                || second_cleanup.request(WindowsCleanupLevel::All),
                |generation| {
                    assert_eq!(second_cleanup.wait_for_all_generation(), generation);
                    second_cleanup.wait_for_all_completion(generation);
                    second_latch.wait(generation)
                },
            );
            done_sender.send(result.0).unwrap();
        });
        assert_eq!(wait_observed.recv().unwrap(), 1);
        assert!(!report_latch.is_published(1));
        assert!(done.try_recv().is_err());
        release_sender.send(()).unwrap();
        assert!(report_latch.wait(1).error.is_none());
        wait_release.send(()).unwrap();
        let claims = [done.recv().unwrap(), done.recv().unwrap()];
        assert_eq!(claims.iter().filter(|claim| **claim).count(), 1);
        assert_eq!(cleanup_count.load(Ordering::SeqCst), 1);
        first.join().unwrap();
        second.join().unwrap();
        cleanup.shutdown();
        worker.join().unwrap();
    }

    #[test]
    fn continue_notice_is_suppressed_when_stop_arrives_during_speech_cleanup() {
        let lifecycle = Arc::new(OneShotSessionLifecycle::new());
        assert!(lifecycle.begin_start());
        assert!(lifecycle.complete_start());
        let (cleanup_intents, cleanup_intent_receiver) =
            crossbeam_channel::bounded::<WindowsCleanupIntent>(1);
        let (cleanup_entered_sender, cleanup_entered) = crossbeam_channel::bounded(1);
        let (release_sender, release) = crossbeam_channel::bounded(1);
        let (notification_sender, notifications) = crossbeam_channel::bounded(1);
        let (completed_sender, completed) = crossbeam_channel::bounded(2);
        let (cleanup, worker) = BoundedCleanupHandle::spawn(
            move |level| {
                if level == WindowsCleanupLevel::SpeechOnly {
                    cleanup_entered_sender.send(()).unwrap();
                    release.recv().unwrap();
                }
                None
            },
            move |level, _generation, cleanup_error| {
                if let Some(notification) = finalize_windows_cleanup_notification(
                    cleanup_intent_receiver.try_recv().ok(),
                    level,
                    cleanup_error.as_deref(),
                ) {
                    notification_sender.send(notification).unwrap();
                }
                completed_sender.send(level).unwrap();
            },
        )
        .unwrap();
        let publisher = WindowsRuntimeControlPublisher::new(
            Arc::clone(&lifecycle),
            cleanup_intents,
            cleanup.clone(),
            TranscriptionPolicy::platform_default(),
        );

        assert!(publisher.publish("continue-then-stop"));
        cleanup_entered.recv().unwrap();
        assert!(lifecycle.claim_stop());
        cleanup.request(WindowsCleanupLevel::All);
        release_sender.send(()).unwrap();
        assert_eq!(completed.recv().unwrap(), WindowsCleanupLevel::SpeechOnly);
        assert_eq!(completed.recv().unwrap(), WindowsCleanupLevel::All);
        assert!(notifications.is_empty());
        cleanup.shutdown();
        worker.join().unwrap();
    }

    #[test]
    fn windows_speech_cleanup_retries_only_failed_steps_and_never_recloses() {
        fn run(
            progress: &mut WindowsSpeechCleanupProgress,
            calls: &[AtomicUsize; 4],
            failed_step: usize,
        ) -> std::result::Result<(), String> {
            let step = |index: usize| {
                let attempt = calls[index].fetch_add(1, Ordering::SeqCst);
                if index == failed_step && attempt == 0 {
                    Err("injected failure".to_owned())
                } else {
                    Ok(())
                }
            };
            run_windows_speech_cleanup(
                progress,
                WINDOWS_SPEECH_RUNTIME_CLEANUP,
                || step(0),
                || step(1),
                || step(2),
                || step(3),
            )
        }

        let labels = [
            "StopAsync",
            "RemoveCompleted",
            "RemoveResultGenerated",
            "Close",
        ];
        for (failed_step, label) in labels.into_iter().enumerate() {
            let calls = std::array::from_fn(|_| AtomicUsize::new(0));
            let mut progress = WindowsSpeechCleanupProgress::default();
            let first = run(&mut progress, &calls, failed_step).unwrap_err();
            assert!(first.contains(label));
            assert!(run(&mut progress, &calls, failed_step).is_ok());
            let after_retry = calls.each_ref().map(|calls| calls.load(Ordering::SeqCst));
            assert!(progress.contains(WindowsSpeechCleanupProgress::CLOSED));
            assert!(run(&mut progress, &calls, failed_step).is_ok());
            assert_eq!(
                calls.each_ref().map(|calls| calls.load(Ordering::SeqCst)),
                after_retry
            );
            assert_eq!(
                calls[3].load(Ordering::SeqCst),
                1 + usize::from(failed_step == 3)
            );
        }
    }

    #[test]
    fn all_cleanup_generation_retries_until_success_then_becomes_terminal() {
        let reports = Arc::new(TestWindowsCleanupReportLatch::default());
        let worker_reports = Arc::clone(&reports);
        let attempts = Arc::new(AtomicUsize::new(0));
        let worker_attempts = Arc::clone(&attempts);
        let (cleanup, worker) = BoundedCleanupHandle::spawn(
            move |level| {
                assert_eq!(level, WindowsCleanupLevel::All);
                let attempt = worker_attempts.fetch_add(1, Ordering::SeqCst) + 1;
                (attempt < WINDOWS_CLEANUP_MAX_ATTEMPTS)
                    .then(|| format!("transient failure {attempt}"))
            },
            move |_level, generation, error| {
                worker_reports.publish(
                    generation,
                    WindowsCleanupReport {
                        error,
                        notification: None,
                    },
                );
                assert_eq!(generation, 1);
            },
        )
        .unwrap();

        let (generation, started) = cleanup.request(WindowsCleanupLevel::All);
        assert_eq!(generation, 1);
        assert!(started);
        cleanup.wait_for_all_completion(generation);
        assert!(reports.wait(generation).error.is_none());
        let (generation, started) = cleanup.request(WindowsCleanupLevel::All);
        assert_eq!(generation, 1);
        assert!(!started);
        assert_eq!(
            attempts.load(Ordering::SeqCst),
            WINDOWS_CLEANUP_MAX_ATTEMPTS
        );
        cleanup.shutdown();
        worker.join().unwrap();
    }

    #[test]
    fn persistent_all_cleanup_failure_stops_at_retry_budget_and_publishes_terminal_report() {
        let reports = Arc::new(TestWindowsCleanupReportLatch::default());
        let worker_reports = Arc::clone(&reports);
        let attempts = Arc::new(AtomicUsize::new(0));
        let worker_attempts = Arc::clone(&attempts);
        let (cleanup, worker) = BoundedCleanupHandle::spawn(
            move |_level| {
                worker_attempts.fetch_add(1, Ordering::SeqCst);
                Some("persistent failure".into())
            },
            move |_level, generation, error| {
                worker_reports.publish(
                    generation,
                    WindowsCleanupReport {
                        error,
                        notification: None,
                    },
                );
                assert_eq!(generation, 1);
            },
        )
        .unwrap();

        let (generation, started) = cleanup.request(WindowsCleanupLevel::All);
        assert!(started);
        cleanup.wait_for_all_completion(generation);
        assert_eq!(
            reports.wait(generation).error.as_deref(),
            Some("persistent failure")
        );
        assert_eq!(
            attempts.load(Ordering::SeqCst),
            WINDOWS_CLEANUP_MAX_ATTEMPTS
        );
        assert_eq!(
            cleanup.request(WindowsCleanupLevel::All),
            (generation, false)
        );
        cleanup.shutdown();
        worker.join().unwrap();
    }

    #[test]
    fn successful_close_terminalizes_failed_speech_prerequisites_without_repeating_work() {
        let calls = std::array::from_fn(|_| AtomicUsize::new(0));
        let recording_calls = AtomicUsize::new(0);
        let mut progress = WindowsSpeechCleanupProgress::default();
        let mut recording_complete = false;
        let stop_speech = |fail_result_once: bool, progress: &mut WindowsSpeechCleanupProgress| {
            let step = |index: usize| {
                let attempt = calls[index].fetch_add(1, Ordering::SeqCst);
                if fail_result_once && index == 2 && attempt == 0 {
                    Err("result removal failed".into())
                } else {
                    Ok(())
                }
            };
            run_windows_speech_cleanup(
                progress,
                WINDOWS_SPEECH_RUNTIME_CLEANUP,
                || step(0),
                || step(1),
                || step(2),
                || step(3),
            )
        };

        let first = run_windows_session_cleanup(
            WindowsCleanupLevel::SpeechOnly,
            &mut recording_complete,
            || stop_speech(true, &mut progress),
            || {
                recording_calls.fetch_add(1, Ordering::SeqCst);
                Ok(())
            },
        )
        .unwrap();
        assert!(first.contains("RemoveResultGenerated"));
        assert_eq!(recording_calls.load(Ordering::SeqCst), 0);

        assert!(
            run_windows_session_cleanup(
                WindowsCleanupLevel::All,
                &mut recording_complete,
                || stop_speech(false, &mut progress),
                || {
                    recording_calls.fetch_add(1, Ordering::SeqCst);
                    Ok(())
                },
            )
            .is_none()
        );
        assert_eq!(
            calls.each_ref().map(|calls| calls.load(Ordering::SeqCst)),
            [1, 1, 1, 1]
        );
        assert_eq!(recording_calls.load(Ordering::SeqCst), 1);

        assert!(
            run_windows_session_cleanup(
                WindowsCleanupLevel::All,
                &mut recording_complete,
                || stop_speech(false, &mut progress),
                || {
                    recording_calls.fetch_add(1, Ordering::SeqCst);
                    Ok(())
                },
            )
            .is_none()
        );
        assert_eq!(recording_calls.load(Ordering::SeqCst), 1);
        assert_eq!(calls[3].load(Ordering::SeqCst), 1);
    }

    #[test]
    fn session_cleanup_aggregates_speech_and_recording_errors_and_retries_incomplete_work() {
        let speech_attempts = AtomicUsize::new(0);
        let recording_attempts = AtomicUsize::new(0);
        let mut recording_complete = false;
        let first = run_windows_session_cleanup(
            WindowsCleanupLevel::All,
            &mut recording_complete,
            || {
                speech_attempts.fetch_add(1, Ordering::SeqCst);
                Err("speech persistent".into())
            },
            || {
                recording_attempts.fetch_add(1, Ordering::SeqCst);
                Err("recording transient".into())
            },
        )
        .unwrap();
        assert!(first.contains("Windows speech cleanup failed: speech persistent"));
        assert!(first.contains("WASAPI recording cleanup failed: recording transient"));
        assert!(!recording_complete);

        let second = run_windows_session_cleanup(
            WindowsCleanupLevel::All,
            &mut recording_complete,
            || {
                speech_attempts.fetch_add(1, Ordering::SeqCst);
                Err("speech persistent".into())
            },
            || {
                recording_attempts.fetch_add(1, Ordering::SeqCst);
                Ok(())
            },
        )
        .unwrap();
        assert!(second.contains("speech persistent"));
        assert!(recording_complete);

        assert!(
            run_windows_session_cleanup(
                WindowsCleanupLevel::All,
                &mut recording_complete,
                || {
                    speech_attempts.fetch_add(1, Ordering::SeqCst);
                    Ok(())
                },
                || {
                    recording_attempts.fetch_add(1, Ordering::SeqCst);
                    Ok(())
                },
            )
            .is_none()
        );
        assert_eq!(speech_attempts.load(Ordering::SeqCst), 3);
        assert_eq!(recording_attempts.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn completed_registration_failure_uses_applicable_startup_cleanup_steps() {
        let calls = std::array::from_fn(|_| AtomicUsize::new(0));
        let mut progress = WindowsSpeechCleanupProgress::default();
        let cleanup = run_windows_speech_startup_cleanup(
            &mut progress,
            WindowsSpeechStartupCleanupState {
                start_attempted: false,
                completed_token: false,
                result_token: true,
            },
            || {
                calls[0].fetch_add(1, Ordering::SeqCst);
                Ok(())
            },
            || {
                calls[1].fetch_add(1, Ordering::SeqCst);
                Ok(())
            },
            || {
                calls[2].fetch_add(1, Ordering::SeqCst);
                Err("remove result failed".into())
            },
            || {
                calls[3].fetch_add(1, Ordering::SeqCst);
                Ok(())
            },
        );
        let super::error::SessionError::Start(message) =
            windows_speech_start_error("Completed registration failed", cleanup)
        else {
            panic!("expected Windows speech start error");
        };
        assert!(message.contains("Completed registration failed"));
        assert!(message.contains("RemoveResultGenerated: remove result failed"));
        assert_eq!(
            calls.each_ref().map(|calls| calls.load(Ordering::SeqCst)),
            [0, 0, 1, 1]
        );

        assert!(
            run_windows_speech_startup_cleanup(
                &mut progress,
                WindowsSpeechStartupCleanupState {
                    start_attempted: false,
                    completed_token: false,
                    result_token: true,
                },
                || Ok(()),
                || Ok(()),
                || {
                    calls[2].fetch_add(1, Ordering::SeqCst);
                    Ok(())
                },
                || {
                    calls[3].fetch_add(1, Ordering::SeqCst);
                    Ok(())
                },
            )
            .is_ok()
        );
        assert_eq!(
            calls.each_ref().map(|calls| calls.load(Ordering::SeqCst)),
            [0, 0, 1, 1]
        );
    }

    #[test]
    fn startup_cleanup_retries_only_close_until_the_third_attempt() {
        let calls = std::array::from_fn(|_| AtomicUsize::new(0));
        let mut progress = WindowsSpeechCleanupProgress::default();
        assert!(
            run_windows_speech_startup_cleanup_bounded(
                &mut progress,
                WindowsSpeechStartupCleanupState {
                    start_attempted: true,
                    completed_token: true,
                    result_token: true,
                },
                || {
                    calls[0].fetch_add(1, Ordering::SeqCst);
                    Ok(())
                },
                || {
                    calls[1].fetch_add(1, Ordering::SeqCst);
                    Ok(())
                },
                || {
                    calls[2].fetch_add(1, Ordering::SeqCst);
                    Ok(())
                },
                || {
                    let attempt = calls[3].fetch_add(1, Ordering::SeqCst) + 1;
                    if attempt < WINDOWS_CLEANUP_MAX_ATTEMPTS {
                        Err(format!("transient Close failure {attempt}"))
                    } else {
                        Ok(())
                    }
                },
            )
            .is_ok()
        );
        assert_eq!(
            calls.each_ref().map(|calls| calls.load(Ordering::SeqCst)),
            [1, 1, 1, WINDOWS_CLEANUP_MAX_ATTEMPTS]
        );
        assert!(progress.contains(WindowsSpeechCleanupProgress::CLOSED));
    }

    #[test]
    fn startup_cleanup_reports_persistent_close_at_the_retry_budget_without_extra_work() {
        let calls = std::array::from_fn(|_| AtomicUsize::new(0));
        let mut progress = WindowsSpeechCleanupProgress::default();
        let cleanup = run_windows_speech_startup_cleanup_bounded(
            &mut progress,
            WindowsSpeechStartupCleanupState {
                start_attempted: true,
                completed_token: true,
                result_token: true,
            },
            || {
                calls[0].fetch_add(1, Ordering::SeqCst);
                Ok(())
            },
            || {
                calls[1].fetch_add(1, Ordering::SeqCst);
                Ok(())
            },
            || {
                calls[2].fetch_add(1, Ordering::SeqCst);
                Ok(())
            },
            || {
                calls[3].fetch_add(1, Ordering::SeqCst);
                Err("persistent Close failure".into())
            },
        );
        assert_eq!(
            cleanup.as_ref().map_err(String::as_str),
            Err("Close: persistent Close failure")
        );
        let super::error::SessionError::Start(message) =
            windows_speech_start_error("primary startup failure", cleanup)
        else {
            panic!("expected Windows speech start error");
        };
        assert!(message.contains("primary startup failure"));
        assert!(message.contains("Close: persistent Close failure"));
        assert_eq!(
            calls.each_ref().map(|calls| calls.load(Ordering::SeqCst)),
            [1, 1, 1, WINDOWS_CLEANUP_MAX_ATTEMPTS]
        );
        assert!(!progress.contains(WindowsSpeechCleanupProgress::CLOSED));
    }

    #[test]
    fn start_mode_failure_aggregates_simultaneous_cleanup_errors_then_retries() {
        let calls = std::array::from_fn(|_| AtomicUsize::new(0));
        let mut progress = WindowsSpeechCleanupProgress::default();
        let step = |index: usize| {
            calls[index].fetch_add(1, Ordering::SeqCst);
            if index < 3 {
                Err(format!("step {index} failed"))
            } else {
                Ok(())
            }
        };
        let cleanup = run_windows_speech_startup_cleanup(
            &mut progress,
            WindowsSpeechStartupCleanupState {
                start_attempted: true,
                completed_token: true,
                result_token: true,
            },
            || step(0),
            || step(1),
            || step(2),
            || step(3),
        );
        let super::error::SessionError::Start(message) =
            windows_speech_start_error("StartWithModeAsync failed", cleanup)
        else {
            panic!("expected Windows speech start error");
        };
        assert!(message.contains("StopAsync: step 0 failed"));
        assert!(message.contains("RemoveCompleted: step 1 failed"));
        assert!(message.contains("RemoveResultGenerated: step 2 failed"));
        assert_eq!(calls[3].load(Ordering::SeqCst), 1);

        assert!(
            run_windows_speech_startup_cleanup(
                &mut progress,
                WindowsSpeechStartupCleanupState {
                    start_attempted: true,
                    completed_token: true,
                    result_token: true,
                },
                || {
                    calls[0].fetch_add(1, Ordering::SeqCst);
                    Ok(())
                },
                || {
                    calls[1].fetch_add(1, Ordering::SeqCst);
                    Ok(())
                },
                || {
                    calls[2].fetch_add(1, Ordering::SeqCst);
                    Ok(())
                },
                || {
                    calls[3].fetch_add(1, Ordering::SeqCst);
                    Ok(())
                },
            )
            .is_ok()
        );
        assert_eq!(
            calls.each_ref().map(|calls| calls.load(Ordering::SeqCst)),
            [1, 1, 1, 1]
        );
    }

    #[test]
    fn close_is_attempted_after_persistent_prerequisite_failures_and_only_repeats_until_success() {
        let calls = std::array::from_fn(|_| AtomicUsize::new(0));
        let mut progress = WindowsSpeechCleanupProgress::default();
        let step = |index: usize, close_fails: bool| {
            calls[index].fetch_add(1, Ordering::SeqCst);
            if index < 3 || close_fails {
                Err(format!("step {index} failed"))
            } else {
                Ok(())
            }
        };

        let first = run_windows_speech_cleanup(
            &mut progress,
            WINDOWS_SPEECH_RUNTIME_CLEANUP,
            || step(0, true),
            || step(1, true),
            || step(2, true),
            || step(3, true),
        )
        .unwrap_err();
        for label in [
            "StopAsync",
            "RemoveCompleted",
            "RemoveResultGenerated",
            "Close",
        ] {
            assert!(first.contains(label));
        }
        assert_eq!(
            calls.each_ref().map(|calls| calls.load(Ordering::SeqCst)),
            [1, 1, 1, 1]
        );

        let second = run_windows_speech_cleanup(
            &mut progress,
            WINDOWS_SPEECH_RUNTIME_CLEANUP,
            || step(0, false),
            || step(1, false),
            || step(2, false),
            || step(3, false),
        )
        .unwrap_err();
        assert!(second.contains("StopAsync"));
        assert!(second.contains("RemoveCompleted"));
        assert!(second.contains("RemoveResultGenerated"));
        assert!(!second.contains("Close"));
        assert_eq!(
            calls.each_ref().map(|calls| calls.load(Ordering::SeqCst)),
            [2, 2, 2, 2]
        );

        assert!(
            run_windows_speech_cleanup(
                &mut progress,
                WINDOWS_SPEECH_RUNTIME_CLEANUP,
                || step(0, false),
                || step(1, false),
                || step(2, false),
                || step(3, false),
            )
            .is_ok()
        );
        assert_eq!(
            calls.each_ref().map(|calls| calls.load(Ordering::SeqCst)),
            [2, 2, 2, 2]
        );
    }

    #[test]
    fn normal_stop_claim_suppresses_completed_for_both_policies() {
        let fatal_policy = TranscriptionPolicy {
            allow_backend_fallback: true,
            allow_record_only: false,
            ..TranscriptionPolicy::platform_default()
        };
        for (index, policy) in [TranscriptionPolicy::platform_default(), fatal_policy]
            .into_iter()
            .enumerate()
        {
            let failure_attempts = if index == 0 {
                WINDOWS_CLEANUP_MAX_ATTEMPTS - 1
            } else {
                0
            };
            let harness = CompletionCleanupHarness::running_with_failures(policy, failure_attempts);
            let barrier = Arc::new(std::sync::Barrier::new(2));
            let stop_lifecycle = Arc::clone(&harness.lifecycle);
            let stop_cleanup = harness.cleanup.clone();
            let stop_barrier = Arc::clone(&barrier);
            let stopping = std::thread::spawn(move || {
                assert!(stop_lifecycle.claim_stop());
                stop_cleanup.request(WindowsCleanupLevel::All);
                stop_barrier.wait();
            });
            barrier.wait();
            assert!(!harness.publisher.publish("late completion"));
            stopping.join().unwrap();
            assert_eq!(harness.completed.recv().unwrap(), WindowsCleanupLevel::All);
            assert!(harness.notifications.is_empty());
            assert_eq!(harness.speech_stops.load(Ordering::SeqCst), 1);
            assert_eq!(harness.tokens_removed.load(Ordering::SeqCst), 2);
            assert_eq!(
                harness.speech_closes.load(Ordering::SeqCst),
                failure_attempts + 1
            );
            assert_eq!(
                harness.recording_stops.load(Ordering::SeqCst),
                failure_attempts + 1
            );
            assert_eq!(
                harness.cleanup_attempts.load(Ordering::SeqCst),
                failure_attempts + 1
            );
            harness.shutdown();
        }
    }

    #[test]
    fn completed_during_starting_aborts_commit_and_cleans_transaction() {
        let harness =
            CompletionCleanupHarness::starting(TranscriptionPolicy::platform_default(), true);
        let barrier = Arc::new(std::sync::Barrier::new(2));
        let completed_barrier = Arc::new(std::sync::Barrier::new(2));
        let publisher = Arc::clone(&harness.publisher);
        let publisher_barrier = Arc::clone(&barrier);
        let publisher_completed = Arc::clone(&completed_barrier);
        let completing = std::thread::spawn(move || {
            publisher_barrier.wait();
            assert!(publisher.publish("completion during startup"));
            publisher_completed.wait();
        });

        barrier.wait();
        completed_barrier.wait();
        let result = complete_windows_start_transaction(&harness.lifecycle, || {
            assert_eq!(harness.completed.recv().unwrap(), WindowsCleanupLevel::All);
            harness.reports.wait(1)
        });
        completing.join().unwrap();
        let Err(super::error::SessionError::Start(reason)) = result else {
            panic!("expected transactional Windows start failure");
        };
        assert!(reason.contains("completion during startup"));
        assert!(reason.contains("injected speech Close failure"));
        assert!(reason.contains("injected recording stop failure"));
        assert_eq!(harness.speech_stops.load(Ordering::SeqCst), 1);
        assert_eq!(harness.tokens_removed.load(Ordering::SeqCst), 2);
        assert_eq!(
            harness.speech_closes.load(Ordering::SeqCst),
            WINDOWS_CLEANUP_MAX_ATTEMPTS
        );
        assert_eq!(
            harness.recording_stops.load(Ordering::SeqCst),
            WINDOWS_CLEANUP_MAX_ATTEMPTS
        );
        assert_eq!(
            harness.cleanup_attempts.load(Ordering::SeqCst),
            WINDOWS_CLEANUP_MAX_ATTEMPTS
        );
        assert!(!harness.lifecycle.capture_is_running());
        harness.shutdown();
    }

    #[test]
    fn start_transaction_unwind_releases_startup_latch_and_joins_cleanup_at_every_stage() {
        for panic_stage in 0..4 {
            let lifecycle = Arc::new(OneShotSessionLifecycle::new());
            assert!(lifecycle.begin_start());
            let finalization_count = Arc::new(AtomicUsize::new(0));
            let startup_publications = Arc::new(AtomicUsize::new(0));
            let (worker_waiting_sender, worker_waiting) = crossbeam_channel::bounded(1);
            let resources = Arc::new(TestWindowsStartupResources::new(
                worker_waiting_sender,
                Arc::clone(&startup_publications),
            ));
            let reports = Arc::new(TestWindowsCleanupReportLatch::default());
            let worker_reports = Arc::clone(&reports);
            let cleanup_attempts = Arc::new(AtomicUsize::new(0));
            let worker_cleanup_attempts = Arc::clone(&cleanup_attempts);
            let worker_resources = Arc::clone(&resources);
            let (cleanup, worker) = BoundedCleanupHandle::spawn(
                move |level| {
                    assert_eq!(level, WindowsCleanupLevel::All);
                    worker_cleanup_attempts.fetch_add(1, Ordering::SeqCst);
                    worker_resources.cleanup()
                },
                move |_level, generation, error| {
                    worker_reports.publish(
                        generation,
                        WindowsCleanupReport {
                            error,
                            notification: None,
                        },
                    );
                },
            )
            .unwrap();

            let unwind = {
                let lifecycle = Arc::clone(&lifecycle);
                let transaction_cleanup = cleanup.clone();
                let transaction_reports = Arc::clone(&reports);
                let transaction_resources = Arc::clone(&resources);
                let resource_finalizations = Arc::clone(&finalization_count);
                std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || {
                    let transaction = WindowsStartTransactionGuard::new(|| {
                        finish_unwound_windows_start(
                            &lifecycle,
                            || transaction_resources.finish_startup(),
                            || transaction_cleanup.request(WindowsCleanupLevel::All),
                            |generation| {
                                transaction_cleanup.wait_for_all_completion(generation);
                                transaction_reports.wait(generation)
                            },
                        );
                    });

                    assert_eq!(
                        transaction_cleanup.request(WindowsCleanupLevel::All),
                        (1, true)
                    );
                    worker_waiting.recv().unwrap();
                    assert_ne!(panic_stage, 0, "before resource preparation");

                    let resource =
                        Arc::new(TestWindowsStartupResource::new(resource_finalizations));
                    assert_ne!(panic_stage, 1, "after resource acquisition");

                    transaction_resources.install(Arc::clone(&resource));
                    assert_ne!(panic_stage, 2, "after cleanup coordinator installation");

                    transaction_resources.finish_startup();
                    assert_ne!(panic_stage, 3, "after startup_finished publication");

                    transaction.finish();
                }))
            };
            assert!(unwind.is_err());
            assert_eq!(startup_publications.load(Ordering::SeqCst), 1);
            assert_eq!(cleanup_attempts.load(Ordering::SeqCst), 1);
            assert_eq!(
                finalization_count.load(Ordering::SeqCst),
                usize::from(panic_stage > 0)
            );
            assert!(reports.is_published(1));
            assert!(reports.wait(1).error.is_none());
            assert_eq!(cleanup.request(WindowsCleanupLevel::All), (1, false));
            cleanup.shutdown();
            worker.join().unwrap();
        }
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn windows_platform_start_harness_retains_or_stops_recording_exactly_once() {
        let run = |policy, speech_succeeds, cleanup_fails| {
            let harness = run_windows_start_harness(policy, speech_succeeds, cleanup_fails);
            (
                harness.outcome,
                harness.recording_starts,
                harness.speech_starts,
                harness.finalization_count,
                harness.drop_count,
            )
        };

        let (platform, recording_starts, speech_starts, finalization_count, drop_count) =
            run(TranscriptionPolicy::platform_default(), true, false);
        let WindowsPlatformStart::Platform {
            recording,
            speech: (),
        } = platform.unwrap()
        else {
            panic!("expected platform speech start");
        };
        assert_eq!(
            recording_starts.load(std::sync::atomic::Ordering::SeqCst),
            1
        );
        assert_eq!(speech_starts.load(std::sync::atomic::Ordering::SeqCst), 1);
        assert_eq!(
            finalization_count.load(std::sync::atomic::Ordering::SeqCst),
            0
        );
        drop(recording);
        assert_eq!(
            finalization_count.load(std::sync::atomic::Ordering::SeqCst),
            1
        );
        assert_eq!(drop_count.load(std::sync::atomic::Ordering::SeqCst), 1);

        let (record_only, recording_starts, speech_starts, finalization_count, drop_count) =
            run(TranscriptionPolicy::platform_default(), false, false);
        let WindowsPlatformStart::RecordOnly {
            recording,
            speech_error,
            reason,
        } = record_only.unwrap()
        else {
            panic!("expected record-only fallback");
        };
        assert_eq!(speech_error, "speech failed");
        assert!(!reason.is_empty());
        assert_eq!(
            recording_starts.load(std::sync::atomic::Ordering::SeqCst),
            1
        );
        assert_eq!(speech_starts.load(std::sync::atomic::Ordering::SeqCst), 1);
        assert_eq!(
            finalization_count.load(std::sync::atomic::Ordering::SeqCst),
            0
        );
        drop(recording);
        assert_eq!(
            finalization_count.load(std::sync::atomic::Ordering::SeqCst),
            1
        );
        assert_eq!(drop_count.load(std::sync::atomic::Ordering::SeqCst), 1);

        let forbidden_policy = TranscriptionPolicy {
            allow_record_only: false,
            ..TranscriptionPolicy::platform_default()
        };
        let (failure, recording_starts, speech_starts, finalization_count, drop_count) =
            run(forbidden_policy, false, false);
        let Err(WindowsPlatformStartError::Speech(reason)) = failure else {
            panic!("expected forbidden fallback to fail");
        };
        assert!(reason.contains("speech failed"));
        assert_eq!(
            recording_starts.load(std::sync::atomic::Ordering::SeqCst),
            1
        );
        assert_eq!(speech_starts.load(std::sync::atomic::Ordering::SeqCst), 1);
        assert_eq!(
            finalization_count.load(std::sync::atomic::Ordering::SeqCst),
            1
        );
        assert_eq!(drop_count.load(std::sync::atomic::Ordering::SeqCst), 1);

        let (failure, _, _, finalization_count, drop_count) = run(forbidden_policy, false, true);
        let Err(WindowsPlatformStartError::Speech(reason)) = failure else {
            panic!("expected cleanup failure to be reported");
        };
        assert!(reason.contains("recording cleanup failed: cleanup failed"));
        assert_eq!(
            finalization_count.load(std::sync::atomic::Ordering::SeqCst),
            1
        );
        assert_eq!(drop_count.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    #[test]
    fn policy_builder_keeps_legacy_recognizer_selection_in_sync() {
        let options = SessionConfig::platform_default("en-US")
            .with_transcription_policy(TranscriptionPolicy::offline_local_model());

        assert_eq!(options.config().recognizer, RecognizerBackend::LocalModel);
    }

    #[test]
    fn legacy_session_config_struct_literal_remains_source_compatible() {
        let config = SessionConfig {
            locale: "en-US".into(),
            recognizer: RecognizerBackend::Platform,
            local_model_path: None,
        };

        assert_eq!(config.locale, "en-US");
    }

    #[test]
    fn windows_adapter_lifecycle_is_one_shot() {
        let lifecycle = OneShotSessionLifecycle::new();
        assert!(lifecycle.begin_start());
        assert!(lifecycle.complete_start());
        assert!(lifecycle.speech_is_running());
        assert!(!lifecycle.begin_start());
        assert!(lifecycle.claim_stop());
        assert!(!lifecycle.claim_stop());
        assert!(!lifecycle.begin_start());
    }

    #[test]
    fn merged_receiver_ignores_closed_warning_and_delivers_fatal_first() {
        let (_main_sender, main_receiver) = crossbeam_channel::bounded::<&str>(1);
        let (fatal_sender, fatal_receiver) = crossbeam_channel::bounded(1);
        let (warning_sender, warning_receiver) = crossbeam_channel::bounded(1);
        drop(warning_sender);
        fatal_sender.send("fatal".to_owned()).unwrap();
        drop(fatal_sender);

        assert!(matches!(
            recv_session_channels(
                &main_receiver,
                &fatal_receiver,
                &warning_receiver,
                Some(std::time::Duration::from_millis(10)),
            ),
            Some(MergedSessionReceive::Notification(message)) if message == "fatal"
        ));
    }

    #[test]
    fn merged_receiver_waits_on_main_after_auxiliary_channels_close() {
        let (main_sender, main_receiver) = crossbeam_channel::bounded(1);
        let (fatal_sender, fatal_receiver) = crossbeam_channel::bounded::<String>(1);
        let (warning_sender, warning_receiver) = crossbeam_channel::bounded::<String>(1);
        drop(fatal_sender);
        drop(warning_sender);
        main_sender.send("event").unwrap();

        assert!(matches!(
            recv_session_channels(
                &main_receiver,
                &fatal_receiver,
                &warning_receiver,
                Some(std::time::Duration::from_millis(10)),
            ),
            Some(MergedSessionReceive::Main("event"))
        ));

        let started = std::time::Instant::now();
        assert!(
            recv_session_channels(
                &main_receiver,
                &fatal_receiver,
                &warning_receiver,
                Some(std::time::Duration::from_millis(20)),
            )
            .is_none()
        );
        assert!(started.elapsed() >= std::time::Duration::from_millis(10));
    }

    #[test]
    fn callback_merger_prioritizes_fatal_then_reserved_final() {
        let (sender, main_receiver) = callback_event_channel::<&str, u64>();
        let (fatal_sender, fatal_receiver) = crossbeam_channel::bounded(1);
        let (_warning_sender, warning_receiver) = crossbeam_channel::bounded(1);
        sender.try_send(CallbackEventClass::Partial(1), "partial");
        sender.try_send(CallbackEventClass::Final(Some(1)), "final");
        fatal_sender.send("fatal".into()).unwrap();

        assert!(matches!(
            recv_callback_session_channels(
                &main_receiver,
                &fatal_receiver,
                &warning_receiver,
                Some(std::time::Duration::from_millis(10)),
            ),
            Some(MergedSessionReceive::Notification(message)) if message == "fatal"
        ));
        assert!(matches!(
            recv_callback_session_channels(
                &main_receiver,
                &fatal_receiver,
                &warning_receiver,
                Some(std::time::Duration::from_millis(10)),
            ),
            Some(MergedSessionReceive::Main("final"))
        ));
    }

    #[test]
    fn callback_poll_prioritizes_fatal_then_final_then_warning() {
        let (sender, main_receiver) = callback_event_channel::<&str, u64>();
        let (fatal_sender, fatal_receiver) = crossbeam_channel::bounded(1);
        let (warning_sender, warning_receiver) = crossbeam_channel::bounded(1);
        sender.try_send(CallbackEventClass::Final(Some(1)), "final");
        warning_sender.send("warning".into()).unwrap();
        fatal_sender.send("fatal".into()).unwrap();

        assert!(matches!(
            try_recv_callback_session_channels(
                &main_receiver,
                &fatal_receiver,
                &warning_receiver,
            ),
            Some(MergedSessionReceive::Notification(message)) if message == "fatal"
        ));
        assert!(matches!(
            try_recv_callback_session_channels(&main_receiver, &fatal_receiver, &warning_receiver,),
            Some(MergedSessionReceive::Main("final"))
        ));
        assert!(matches!(
            try_recv_callback_session_channels(
                &main_receiver,
                &fatal_receiver,
                &warning_receiver,
            ),
            Some(MergedSessionReceive::Notification(message)) if message == "warning"
        ));
    }

    #[test]
    fn callback_poll_busy_never_consumes_warning_before_final_or_gap() {
        fn final_gap(dropped: u64) -> String {
            format!("final-gap-{dropped}")
        }

        let (sender, main_receiver) =
            callback_event_channel_with_final_gap::<String, u64>(final_gap);
        let (_fatal_sender, fatal_receiver) = crossbeam_channel::bounded(1);
        let (warning_sender, warning_receiver) = crossbeam_channel::bounded(2);
        sender.try_send(CallbackEventClass::Final(Some(1)), "final".into());
        warning_sender.send("warning-final".into()).unwrap();
        let state = sender.state.lock().unwrap();
        assert!(
            try_recv_callback_session_channels(&main_receiver, &fatal_receiver, &warning_receiver,)
                .is_none()
        );
        assert_eq!(warning_receiver.len(), 1);
        drop(state);
        assert!(matches!(
            try_recv_callback_session_channels(
                &main_receiver,
                &fatal_receiver,
                &warning_receiver,
            ),
            Some(MergedSessionReceive::Main(event)) if event == "final"
        ));
        assert!(matches!(
            try_recv_callback_session_channels(
                &main_receiver,
                &fatal_receiver,
                &warning_receiver,
            ),
            Some(MergedSessionReceive::Notification(message)) if message == "warning-final"
        ));

        for key in 2..2 + CALLBACK_FINAL_CAPACITY as u64 {
            sender.try_send(CallbackEventClass::Final(Some(key)), format!("final-{key}"));
        }
        sender.try_send(CallbackEventClass::Final(Some(1_000)), "overflow".into());
        for _ in 0..CALLBACK_FINAL_CAPACITY {
            assert!(matches!(
                main_receiver.try_recv(),
                Some(event) if event.starts_with("final-")
            ));
        }
        warning_sender.send("warning-gap".into()).unwrap();
        let state = sender.state.lock().unwrap();
        assert!(
            try_recv_callback_session_channels(&main_receiver, &fatal_receiver, &warning_receiver,)
                .is_none()
        );
        assert_eq!(warning_receiver.len(), 1);
        drop(state);
        assert!(matches!(
            try_recv_callback_session_channels(
                &main_receiver,
                &fatal_receiver,
                &warning_receiver,
            ),
            Some(MergedSessionReceive::Main(event)) if event == "final-gap-1"
        ));
        assert!(matches!(
            try_recv_callback_session_channels(
                &main_receiver,
                &fatal_receiver,
                &warning_receiver,
            ),
            Some(MergedSessionReceive::Notification(message)) if message == "warning-gap"
        ));
    }

    #[test]
    fn callback_receiver_blocking_wait_rechecks_final_before_lossy() {
        let (sender, mut receiver) = callback_event_channel::<&str, u64>();
        let hook = std::sync::Arc::new(super::CallbackWaitHook {
            fired: AtomicBool::new(false),
            entered: std::sync::Barrier::new(2),
            release: std::sync::Barrier::new(2),
        });
        receiver.wait_hook = Some(std::sync::Arc::clone(&hook));
        let waiting =
            std::thread::spawn(move || receiver.recv_timeout(std::time::Duration::from_secs(1)));
        hook.entered.wait();
        assert_eq!(
            sender.try_send(CallbackEventClass::Partial(1), "partial"),
            CallbackEnqueue::Enqueued
        );
        assert_eq!(
            sender.try_send(CallbackEventClass::Final(Some(2)), "final"),
            CallbackEnqueue::Enqueued
        );
        hook.release.wait();

        assert_eq!(waiting.join().unwrap(), Some("final"));
    }

    #[test]
    fn callback_merger_blocking_wait_rechecks_fatal_before_all_main_lanes() {
        let (sender, mut main_receiver) = callback_event_channel::<&str, u64>();
        let (fatal_sender, fatal_receiver) = crossbeam_channel::bounded(1);
        let (warning_sender, warning_receiver) = crossbeam_channel::bounded(1);
        let hook = std::sync::Arc::new(super::CallbackWaitHook {
            fired: AtomicBool::new(false),
            entered: std::sync::Barrier::new(2),
            release: std::sync::Barrier::new(2),
        });
        main_receiver.wait_hook = Some(std::sync::Arc::clone(&hook));
        let waiting = std::thread::spawn(move || {
            recv_callback_session_channels(
                &main_receiver,
                &fatal_receiver,
                &warning_receiver,
                Some(std::time::Duration::from_secs(1)),
            )
        });
        hook.entered.wait();
        sender.try_send(CallbackEventClass::Partial(1), "partial");
        sender.try_send(CallbackEventClass::Final(Some(2)), "final");
        warning_sender.send("warning".into()).unwrap();
        fatal_sender.send("fatal".into()).unwrap();
        hook.release.wait();

        assert!(matches!(
            waiting.join().unwrap(),
            Some(MergedSessionReceive::Notification(message)) if message == "fatal"
        ));
    }

    #[test]
    fn callback_merger_finishes_after_all_production_channels_disconnect() {
        let (sender, main_receiver) = callback_event_channel::<&str, u64>();
        let (fatal_sender, fatal_receiver) = crossbeam_channel::bounded::<String>(1);
        let (warning_sender, warning_receiver) = crossbeam_channel::bounded::<String>(1);
        drop(sender);
        drop(fatal_sender);
        drop(warning_sender);

        assert!(
            recv_callback_session_channels(
                &main_receiver,
                &fatal_receiver,
                &warning_receiver,
                Some(std::time::Duration::from_millis(10)),
            )
            .is_none()
        );
    }

    #[test]
    fn callback_merger_ignores_stale_partial_token_before_final() {
        let (sender, main_receiver) = callback_event_channel::<&str, u64>();
        let (_fatal_sender, fatal_receiver) = crossbeam_channel::bounded::<String>(1);
        let (_warning_sender, warning_receiver) = crossbeam_channel::bounded::<String>(1);
        sender.lossy_ready.try_send(()).unwrap();
        sender.try_send(CallbackEventClass::Final(Some(9)), "final");

        assert!(matches!(
            recv_callback_session_channels(
                &main_receiver,
                &fatal_receiver,
                &warning_receiver,
                Some(std::time::Duration::from_millis(10)),
            ),
            Some(MergedSessionReceive::Main("final"))
        ));
    }

    #[test]
    fn callback_merger_observes_one_total_timeout_with_open_channels() {
        let (_sender, main_receiver) = callback_event_channel::<&str, u64>();
        let (_fatal_sender, fatal_receiver) = crossbeam_channel::bounded::<String>(1);
        let (_warning_sender, warning_receiver) = crossbeam_channel::bounded::<String>(1);
        let started = std::time::Instant::now();

        assert!(
            recv_callback_session_channels(
                &main_receiver,
                &fatal_receiver,
                &warning_receiver,
                Some(std::time::Duration::from_millis(20)),
            )
            .is_none()
        );
        assert!(started.elapsed() >= std::time::Duration::from_millis(10));
    }

    #[test]
    fn compatibility_results_map_all_fields_and_sources() {
        let partial = SessionResult {
            source: SourceLabel::Mic,
            segment_id: 41,
            is_final: false,
            text: "draft".into(),
            start_seconds: 1.25,
            end_seconds: 2.5,
            confidence_mean: Some(0.8),
            confidence_min: Some(0.6),
        };
        let TranscriptEvent::Partial(segment) = partial.transcript_event() else {
            panic!("expected a partial transcript");
        };
        assert_eq!(segment.track_id, TrackId::MICROPHONE);
        assert_eq!(segment.segment_id.get(), 41);
        assert_eq!(segment.text, "draft");
        assert!((segment.start_seconds - 1.25).abs() < f64::EPSILON);
        assert!((segment.end_seconds - 2.5).abs() < f64::EPSILON);
        assert_eq!(segment.confidence_mean, Some(0.8));
        assert_eq!(segment.confidence_min, Some(0.6));

        let final_result = SessionResult {
            source: SourceLabel::System,
            is_final: true,
            ..partial
        };
        assert!(matches!(
            Event::Result(final_result).transcript_event(),
            Some(TranscriptEvent::Final(segment)) if segment.track_id == TrackId::SYSTEM
        ));
        assert!(Event::Log("status".into()).transcript_event().is_none());
    }
}

#[cfg(all(test, target_os = "macos"))]
mod tests {
    use super::*;

    #[test]
    fn version_is_nonempty_and_dotted() {
        let v = version();
        assert!(!v.is_empty(), "version must be non-empty");
        assert!(
            v.chars().next().is_some_and(|c| c.is_ascii_digit()),
            "version should start with a digit, got: {v}"
        );
        assert!(v.contains('.'), "version should be dotted, got: {v}");
    }

    #[test]
    fn session_constructs_and_drops_without_starting() {
        let tmp = tempfile::tempdir().expect("temporary output directory");
        let s = Session::new(tmp.path(), "ja-JP").expect("session new");
        // Pull events: there are none yet because we never started.
        assert!(s.try_recv().is_none());
        drop(s);
        // Drop must run without panicking even though we never called start().
    }
}
