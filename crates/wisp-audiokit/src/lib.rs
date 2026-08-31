//! Safe Rust wrapper over Wisp's platform audio/transcription backends.
//!
//! macOS is backed by the Swift `WispAudioKit` framework, using the
//! `SpeechAnalyzer` API for on-device transcription and Core Audio Process
//! Taps for capture.

mod backend;
mod error;
#[cfg(target_os = "macos")]
mod macos_backend;

pub use backend::{
    Availability, BackendError, BackendErrorKind, BackendId, BackendResult, CaptureBackend,
    CaptureCapabilities, CaptureControlEvent, CaptureEventReceiver, CaptureProbe, ControlEnqueue,
    FrameEnqueue, OrchestratorEvent, PrivacyRequirement, RealtimeCaptureSender, RecognitionPrivacy,
    SessionOrchestrator, ShutdownMode, TranscriberBackend, TranscriberCapabilities,
    TranscriberClass, TranscriberConfig, TranscriberFactory, TranscriberFeature, TranscriberProbe,
    TranscriptionPolicy, TranscriptionSelection, UnavailableReason, realtime_capture_channel,
    select_transcriber, select_transcriber_after_failure,
};
pub use error::{Result, SessionError};
#[cfg(target_os = "macos")]
pub use macos_backend::{MacosCaptureBackend, MacosSession, MacosTranscriberBackend};
pub use wisp_core::{
    AudioFormat, AudioFrame, AudioFrameError, AudioSamples, CaptureEvent, MonotonicTimestamp,
    SampleFormat, SourceKind, TrackDescriptor, TrackId, TranscriptEvent, TranscriptSegment,
    TranscriptSegmentId,
};

#[cfg(target_os = "macos")]
use std::path::Path;

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
    /// macOS maps this to `SpeechAnalyzer`.
    Platform,
}

impl RecognizerBackend {
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Platform => platform_recognizer_label(),
        }
    }
}

/// Human-readable label for the platform speech recognizer.
#[must_use]
pub const fn platform_recognizer_label() -> &'static str {
    if cfg!(target_os = "macos") {
        "Apple SpeechAnalyzer"
    } else {
        "Platform speech recognizer"
    }
}

/// Configuration used when constructing a [`Session`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionConfig {
    pub locale: String,
    pub recognizer: RecognizerBackend,
}

impl SessionConfig {
    #[must_use]
    pub fn platform_default(locale: impl Into<String>) -> Self {
        Self {
            locale: locale.into(),
            recognizer: RecognizerBackend::Platform,
        }
    }

    #[must_use]
    pub const fn with_transcription_policy(
        mut self,
        policy: TranscriptionPolicy,
    ) -> SessionOptions {
        // Wisp only ships the platform (SpeechAnalyzer) backend on macOS, so
        // any preferred class resolves to the platform recognizer.
        self.recognizer = RecognizerBackend::Platform;
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
        };
        Self::new(config, policy)
    }
}
#[cfg(target_os = "macos")]
const CALLBACK_FINAL_CAPACITY: usize = 64;
#[cfg(target_os = "macos")]
const CALLBACK_PARTIAL_CAPACITY: usize = 64;
#[cfg(target_os = "macos")]
const CALLBACK_LOG_CAPACITY: usize = 16;
#[cfg(target_os = "macos")]
const CALLBACK_SOURCE_SLOTS: usize = 2;

#[cfg(target_os = "macos")]
trait CallbackEventKey: Clone + PartialEq {
    fn source_slot(&self) -> usize;
    fn sequence(&self) -> u64;
}

#[cfg(target_os = "macos")]
impl CallbackEventKey for u64 {
    fn source_slot(&self) -> usize {
        0
    }

    fn sequence(&self) -> u64 {
        *self
    }
}

#[cfg(target_os = "macos")]
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

#[cfg(target_os = "macos")]
#[derive(Debug, Clone, PartialEq, Eq)]
enum CallbackEventClass<K> {
    Final(Option<K>),
    Partial(K),
    Log,
}

#[cfg(target_os = "macos")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CallbackEnqueue {
    Enqueued,
    Replaced,
    DroppedFull,
    DroppedBusy,
}

#[cfg(target_os = "macos")]
struct CallbackQueueState<T, K> {
    partials: std::collections::VecDeque<(K, T)>,
    logs: std::collections::VecDeque<T>,
    finalized_high_watermarks: [Option<u64>; CALLBACK_SOURCE_SLOTS],
    lossy_wake_armed: bool,
}

#[cfg(target_os = "macos")]
struct CallbackTerminalWatermarks {
    initialized: [std::sync::atomic::AtomicBool; CALLBACK_SOURCE_SLOTS],
    sequences: [std::sync::atomic::AtomicU64; CALLBACK_SOURCE_SLOTS],
}

#[cfg(target_os = "macos")]
impl Default for CallbackTerminalWatermarks {
    fn default() -> Self {
        Self {
            initialized: std::array::from_fn(|_| std::sync::atomic::AtomicBool::new(false)),
            sequences: std::array::from_fn(|_| std::sync::atomic::AtomicU64::new(0)),
        }
    }
}

#[cfg(target_os = "macos")]
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

#[cfg(target_os = "macos")]
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

#[cfg(target_os = "macos")]
struct CallbackEventReceiver<T, K> {
    state: std::sync::Arc<std::sync::Mutex<CallbackQueueState<T, K>>>,
    lossy_ready: crossbeam_channel::Receiver<()>,
    finals: crossbeam_channel::Receiver<(Option<K>, T)>,
    final_gap_ready: crossbeam_channel::Receiver<()>,
    dropped_finals: std::sync::Arc<std::sync::atomic::AtomicU64>,
    terminal_high_watermarks: std::sync::Arc<CallbackTerminalWatermarks>,
    final_gap_factory: Option<fn(u64) -> T>,
}

#[cfg(target_os = "macos")]
enum CallbackSweep<T> {
    Event(T),
    Empty,
    Busy,
}

#[cfg(all(test, target_os = "macos"))]
struct CallbackFinalCleanupHook {
    entered: std::sync::Barrier,
    release: std::sync::Barrier,
}

#[cfg(all(test, target_os = "macos"))]
struct CallbackFinalOverflowHook {
    entered: std::sync::Barrier,
    release: std::sync::Barrier,
}

#[cfg(target_os = "macos")]
fn callback_event_channel_with_final_gap<T, K>(
    final_gap_factory: fn(u64) -> T
) -> (CallbackEventSender<T, K>, CallbackEventReceiver<T, K>) {
    callback_event_channel_inner(Some(final_gap_factory))
}

#[cfg(target_os = "macos")]
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
        },
    )
}

#[cfg(target_os = "macos")]
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

#[cfg(target_os = "macos")]
impl<T, K> CallbackEventReceiver<T, K>
where
    K: CallbackEventKey,
{
    fn try_recv(&self) -> Option<T> {
        self.try_recv_result().ok()
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
}

#[cfg(target_os = "macos")]
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

#[cfg(target_os = "macos")]
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

#[cfg(target_os = "macos")]
fn reconcile_empty_lossy_wake<T, K>(
    state: &mut CallbackQueueState<T, K>,
    lossy_ready: &crossbeam_channel::Receiver<()>,
) {
    while lossy_ready.try_recv().is_ok() {}
    state.lossy_wake_armed = false;
}

#[cfg(target_os = "macos")]
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

#[cfg(target_os = "macos")]
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

#[cfg(target_os = "macos")]
fn saturating_atomic_increment(value: &std::sync::atomic::AtomicU64) {
    let _ = value.fetch_update(
        std::sync::atomic::Ordering::Relaxed,
        std::sync::atomic::Ordering::Relaxed,
        |current| Some(current.saturating_add(1)),
    );
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

#[cfg(target_os = "macos")]
pub(crate) use imp::NativeSession;
#[cfg(target_os = "macos")]
pub use imp::version;
#[cfg(target_os = "macos")]
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

#[cfg(target_os = "macos")]
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

#[cfg(target_os = "macos")]
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
