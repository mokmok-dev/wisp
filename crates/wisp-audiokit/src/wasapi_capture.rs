//! Shared-mode WASAPI microphone and system-loopback capture.
//!
//! Both endpoints are converted by the Windows audio engine to the format
//! expected by Whisper-family models: 16 kHz, mono, `f32` PCM. COM objects
//! stay on the worker thread that created them.

use std::collections::VecDeque;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{
    Arc, Mutex, OnceLock,
    atomic::{AtomicBool, Ordering},
};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crossbeam_channel as channel;
use wasapi::{DeviceEnumerator, Direction, SampleType, StreamMode, WaveFormat};
use wisp_core::{AudioFrame, AudioSamples, CaptureEvent, MonotonicTimestamp, SourceLabel, TrackId};

use crate::backend::{
    CaptureControlEvent, CaptureEventReceiver, RealtimeCaptureSender, StartupCoordinator,
    WorkerFailureRoute, WorkerStartupPhase, publish_ready_and_wait, realtime_capture_channel,
};
use crate::ogg_opus_recorder::OggOpusRecorder;
use crate::{Result, SessionError};

/// PCM sample rate produced by [`WasapiCapture`].
pub const WASAPI_SAMPLE_RATE: u32 = 16_000;
/// PCM channel count produced by [`WasapiCapture`].
pub const WASAPI_CHANNELS: u16 = 1;
/// Maximum number of PCM chunks waiting for the recording/transcription
/// consumer. Full queues drop the newest chunk and publish an overflow event.
pub const WASAPI_CAPTURE_QUEUE_CAPACITY: usize = 256;

const EVENT_WAIT_MILLIS: u32 = 100;
const START_TIMEOUT: Duration = Duration::from_secs(5);
const NOTIFICATION_QUEUE_CAPACITY: usize = 16;
/// Ignore sub-2 ms callback scheduling jitter when comparing packet
/// timestamps with the amount of PCM already written.
const TIMELINE_JITTER_TOLERANCE: Duration = Duration::from_millis(2);
/// A single timestamp discontinuity can add at most five seconds of silence.
/// Larger gaps re-anchor the capture clock so compensation cannot repeat.
const MAX_DISCONTINUITY_COMPENSATION: Duration = Duration::from_secs(5);
const STARTUP_REAPER_POLL_INTERVAL: Duration = Duration::from_millis(50);

#[derive(Default)]
struct StartupWorkerRegistry {
    workers: Vec<JoinHandle<()>>,
    reaper_running: bool,
    startup_reserved: bool,
}

static STARTUP_WORKERS: OnceLock<Mutex<StartupWorkerRegistry>> = OnceLock::new();
#[cfg(test)]
static FORCE_STARTUP_REAPER_SPAWN_FAILURE: AtomicBool = AtomicBool::new(false);

struct StartupAttemptPermit;

impl Drop for StartupAttemptPermit {
    fn drop(&mut self) {
        let mut registry = startup_worker_registry()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        registry.startup_reserved = false;
    }
}

fn startup_worker_registry() -> &'static Mutex<StartupWorkerRegistry> {
    STARTUP_WORKERS.get_or_init(|| Mutex::new(StartupWorkerRegistry::default()))
}

fn reserve_startup_attempt() -> Result<StartupAttemptPermit> {
    reap_finished_startup_workers();
    let mut registry = startup_worker_registry()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if registry.startup_reserved {
        return Err(SessionError::Start(
            "another WASAPI capture startup is already in progress".into(),
        ));
    }
    if !registry.workers.is_empty() {
        drop(registry);
        ensure_startup_reaper();
        return Err(SessionError::Start(
            "a previous timed-out WASAPI startup is still being cleaned up".into(),
        ));
    }
    registry.startup_reserved = true;
    Ok(StartupAttemptPermit)
}

fn retain_startup_workers(workers: &mut Vec<JoinHandle<()>>) {
    if workers.is_empty() {
        return;
    }
    let mut registry = startup_worker_registry()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    registry.workers.append(workers);
    drop(registry);
    ensure_startup_reaper();
}

fn ensure_startup_reaper() {
    let should_spawn = {
        let mut registry = startup_worker_registry()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if registry.workers.is_empty() || registry.reaper_running {
            false
        } else {
            registry.reaper_running = true;
            true
        }
    };
    if !should_spawn {
        return;
    }

    #[cfg(test)]
    let spawn_result = if FORCE_STARTUP_REAPER_SPAWN_FAILURE.swap(false, Ordering::SeqCst) {
        Err(io::Error::other("forced startup reaper spawn failure"))
    } else {
        std::thread::Builder::new()
            .name("wisp-wasapi-startup-reaper".into())
            .spawn(startup_reaper_loop)
    };
    #[cfg(not(test))]
    let spawn_result = std::thread::Builder::new()
        .name("wisp-wasapi-startup-reaper".into())
        .spawn(startup_reaper_loop);
    if spawn_result.is_err() {
        // The worker handles remain in the process-wide registry. A later
        // start attempt can retry spawning the single reaper, but cannot add
        // another potentially hung worker set while these remain outstanding.
        let mut registry = startup_worker_registry()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        registry.reaper_running = false;
    }
}

fn startup_reaper_loop() {
    loop {
        reap_finished_startup_workers();
        let finished = {
            let mut registry = startup_worker_registry()
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if registry.workers.is_empty() {
                registry.reaper_running = false;
                true
            } else {
                false
            }
        };
        if finished {
            return;
        }
        std::thread::sleep(STARTUP_REAPER_POLL_INTERVAL);
    }
}

fn reap_finished_startup_workers() {
    let finished = {
        let mut registry = startup_worker_registry()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut finished = Vec::new();
        let mut index = 0;
        while index < registry.workers.len() {
            if registry.workers[index].is_finished() {
                finished.push(registry.workers.swap_remove(index));
            } else {
                index += 1;
            }
        }
        finished
    };
    for worker in finished {
        let _ = worker.join();
    }
}
/// Legacy Windows PCM packet retained for source compatibility.
#[derive(Debug, Clone, PartialEq)]
pub struct WasapiPcmChunk {
    pub source: SourceLabel,
    pub samples: Vec<f32>,
}

/// Legacy Windows capture event retained for source compatibility.
#[derive(Debug, Clone, PartialEq)]
pub enum WasapiCaptureEvent {
    Samples(WasapiPcmChunk),
    /// A stream failed after both endpoints had started successfully.
    Error {
        source: SourceLabel,
        message: String,
    },
}

/// Owns simultaneous capture of the default microphone and default system
/// output.
///
/// Construction starts both streams and waits until each one is running.
/// Dropping the value stops and joins both worker threads.
pub struct WasapiCapture {
    receiver: Option<CaptureEventReceiver>,
    stop_requested: Arc<AtomicBool>,
    microphone_muted: Arc<AtomicBool>,
    workers: Vec<JoinHandle<()>>,
}

/// Records simultaneous WASAPI microphone and system-loopback streams into
/// `mic.ogg` and `system.ogg`.
pub struct WasapiRecording {
    capture: Mutex<WasapiCapture>,
    writer: Mutex<RecordingWriterState>,
    writer_abort: Arc<AtomicBool>,
    fatal_error_receiver: channel::Receiver<String>,
    warning_receiver: channel::Receiver<String>,
    mic_path: PathBuf,
    system_path: PathBuf,
}

enum RecordingWriterState {
    Active(JoinHandle<io::Result<()>>),
    Complete,
    Failed(SessionError),
}

struct RecordingNotificationSenders {
    fatal: channel::Sender<String>,
    warning: channel::Sender<String>,
}

struct RecordingNotificationReceivers {
    fatal: channel::Receiver<String>,
    warning: channel::Receiver<String>,
}

fn recording_notification_channels()
-> (RecordingNotificationSenders, RecordingNotificationReceivers) {
    let (fatal, fatal_receiver) = channel::bounded(1);
    let (warning, warning_receiver) = channel::bounded(NOTIFICATION_QUEUE_CAPACITY);
    (
        RecordingNotificationSenders { fatal, warning },
        RecordingNotificationReceivers {
            fatal: fatal_receiver,
            warning: warning_receiver,
        },
    )
}

fn recv_recording_notification(
    fatal: &channel::Receiver<String>,
    warning: &channel::Receiver<String>,
    timeout: Duration,
) -> Option<String> {
    recv_recording_notification_with_wait_hook(fatal, warning, timeout, || {})
}

fn recv_recording_notification_with_wait_hook(
    fatal: &channel::Receiver<String>,
    warning: &channel::Receiver<String>,
    timeout: Duration,
    mut before_wait: impl FnMut(),
) -> Option<String> {
    let started = Instant::now();
    let mut fatal_open = true;
    let mut warning_open = true;
    loop {
        if fatal_open {
            match fatal.try_recv() {
                Ok(message) => return Some(message),
                Err(channel::TryRecvError::Disconnected) => fatal_open = false,
                Err(channel::TryRecvError::Empty) => {},
            }
        }
        if warning_open {
            match warning.try_recv() {
                Ok(message) => return Some(message),
                Err(channel::TryRecvError::Disconnected) => warning_open = false,
                Err(channel::TryRecvError::Empty) => {},
            }
        }
        if !fatal_open && !warning_open {
            return None;
        }

        let remaining = timeout.saturating_sub(started.elapsed());
        if remaining.is_zero() {
            return None;
        }
        let mut selector = channel::Select::new();
        if fatal_open {
            selector.recv(fatal);
        }
        if warning_open {
            selector.recv(warning);
        }
        // Selection is only a wake-up. Looping through the fatal-first sweep
        // prevents an unbiased select from allowing a warning to overtake a
        // fatal that became ready at the same time.
        before_wait();
        if selector.ready_timeout(remaining).is_err() {
            return None;
        }
    }
}

impl WasapiCapture {
    /// Start the default microphone and default-render loopback streams.
    ///
    /// # Errors
    /// Returns [`SessionError::Start`] if either endpoint cannot be opened
    /// within five seconds. Failed startup requests cancellation and transfers
    /// worker ownership to a background reaper so this deadline remains
    /// observable even if a platform call does not return promptly.
    pub fn start() -> Result<Self> {
        let _startup_permit = reserve_startup_attempt()?;
        let tracks = [TrackId::MICROPHONE, TrackId::SYSTEM];
        let (event_senders, receiver) =
            realtime_capture_channel(WASAPI_CAPTURE_QUEUE_CAPACITY, &tracks);
        let (startup_sender, startup_receiver) = channel::bounded(2);
        let stop_requested = Arc::new(AtomicBool::new(false));
        let startup_coordinator = Arc::new(StartupCoordinator::new(2));
        let microphone_muted = Arc::new(AtomicBool::new(false));
        let mut workers = Vec::with_capacity(2);
        let capture_origin = Instant::now();
        let startup_deadline = Instant::now() + START_TIMEOUT;

        for (source, worker_events) in [SourceLabel::Mic, SourceLabel::System]
            .into_iter()
            .zip(event_senders)
        {
            let worker_startup = startup_sender.clone();
            let worker_stop = stop_requested.clone();
            let worker_startup_coordinator = startup_coordinator.clone();
            let worker_muted = microphone_muted.clone();
            let name = match source {
                SourceLabel::Mic => "wisp-wasapi-mic",
                SourceLabel::System => "wisp-wasapi-loopback",
            };
            let worker = std::thread::Builder::new()
                .name(name.into())
                .spawn(move || {
                    capture_worker(
                        source,
                        &worker_events,
                        &worker_startup,
                        &worker_startup_coordinator,
                        &worker_stop,
                        &worker_muted,
                        capture_origin,
                    );
                })
                .map_err(|err| {
                    cancel_startup_workers(&stop_requested, &mut workers);
                    SessionError::Start(format!("failed to spawn {name}: {err}"))
                })?;
            workers.push(worker);
        }
        drop(startup_sender);

        await_capture_startup(
            &startup_receiver,
            &startup_coordinator,
            &stop_requested,
            &mut workers,
            startup_deadline,
        )?;
        Ok(Self {
            receiver: Some(receiver),
            stop_requested,
            microphone_muted,
            workers,
        })
    }

    /// Replace microphone packets with silence while preserving their timing.
    pub fn set_microphone_muted(
        &self,
        muted: bool,
    ) {
        self.microphone_muted.store(muted, Ordering::SeqCst);
    }

    /// Non-blocking event poll.
    #[must_use]
    pub fn try_recv(&self) -> Option<WasapiCaptureEvent> {
        self.receiver
            .as_ref()?
            .try_recv()
            .and_then(legacy_capture_event)
    }

    /// Wait for a PCM packet, runtime error, or timeout.
    #[must_use]
    pub fn recv_timeout(
        &self,
        timeout: Duration,
    ) -> Option<WasapiCaptureEvent> {
        self.receiver
            .as_ref()?
            .recv_timeout(timeout)
            .and_then(legacy_capture_event)
    }

    /// Non-blocking backend-neutral event poll.
    #[must_use]
    pub fn try_recv_capture_event(&self) -> Option<CaptureEvent> {
        self.receiver.as_ref()?.try_recv()
    }

    /// Wait for a backend-neutral event or timeout.
    #[must_use]
    pub fn recv_capture_event_timeout(
        &self,
        timeout: Duration,
    ) -> Option<CaptureEvent> {
        self.receiver.as_ref()?.recv_timeout(timeout)
    }

    /// Stop both streams and wait for their COM workers to exit.
    pub fn stop(&mut self) {
        self.stop_requested.store(true, Ordering::SeqCst);
        join_workers(&mut self.workers);
    }
}

fn await_capture_startup(
    startup_receiver: &channel::Receiver<Startup>,
    startup_coordinator: &StartupCoordinator,
    stop_requested: &AtomicBool,
    workers: &mut Vec<JoinHandle<()>>,
    startup_deadline: Instant,
) -> Result<()> {
    let mut started = Vec::with_capacity(2);
    while started.len() < 2 {
        let remaining = startup_deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            cancel_startup_workers(stop_requested, workers);
            return Err(startup_timeout_error(&started));
        }
        match startup_receiver.recv_timeout(remaining) {
            Ok(Startup::Ready(source)) => {
                started.push(source);
                startup_coordinator.observe_ready();
            },
            Ok(Startup::Failed { source, message }) => {
                cancel_startup_workers(stop_requested, workers);
                return Err(SessionError::Start(format!(
                    "{} WASAPI capture failed: {message}",
                    source_name(source)
                )));
            },
            Err(channel::RecvTimeoutError::Timeout) => {
                cancel_startup_workers(stop_requested, workers);
                return Err(startup_timeout_error(&started));
            },
            Err(channel::RecvTimeoutError::Disconnected) => {
                cancel_startup_workers(stop_requested, workers);
                return Err(SessionError::Start(
                    "WASAPI workers exited during startup".into(),
                ));
            },
        }
    }
    Ok(())
}

fn cancel_startup_workers(
    stop_requested: &AtomicBool,
    workers: &mut Vec<JoinHandle<()>>,
) {
    stop_requested.store(true, Ordering::SeqCst);
    retain_startup_workers(workers);
}

fn startup_timeout_error(started: &[SourceLabel]) -> SessionError {
    SessionError::Start(format!(
        "timed out starting WASAPI capture (ready: {})",
        started
            .iter()
            .map(|source| source_name(*source))
            .collect::<Vec<_>>()
            .join(", ")
    ))
}

impl Drop for WasapiCapture {
    fn drop(&mut self) {
        self.stop();
    }
}

impl WasapiRecording {
    /// Start capture and write two mono Ogg/Opus streams under `output_dir`.
    ///
    /// # Errors
    /// Returns [`SessionError::Start`] if capture, file creation, or the
    /// recording worker cannot be started.
    pub fn start(output_dir: impl AsRef<Path>) -> Result<Self> {
        let output_dir = output_dir.as_ref();
        std::fs::create_dir_all(output_dir).map_err(|err| {
            SessionError::Start(format!(
                "failed to create recording directory {}: {err}",
                output_dir.display()
            ))
        })?;
        let mic_path = output_dir.join("mic.ogg");
        let system_path = output_dir.join("system.ogg");
        let mut capture = WasapiCapture::start()?;
        let mic_recorder = match OggOpusRecorder::create(&mic_path) {
            Ok(recorder) => recorder,
            Err(err) => {
                capture.stop();
                return Err(recording_start_error(&mic_path, &err));
            },
        };
        let system_recorder = match OggOpusRecorder::create(&system_path) {
            Ok(recorder) => recorder,
            Err(err) => {
                capture.stop();
                return Err(recording_start_error(&system_path, &err));
            },
        };

        let receiver = capture.receiver.take().ok_or_else(|| {
            SessionError::Start("WASAPI capture receiver was already consumed".into())
        })?;
        let producer_stop = capture.stop_requested.clone();
        let writer_abort = Arc::new(AtomicBool::new(false));
        let worker_abort = writer_abort.clone();
        let (notifications, notification_receivers) = recording_notification_channels();
        let writer = std::thread::Builder::new()
            .name("wisp-ogg-opus-writer".into())
            .spawn(move || {
                run_recording_worker(
                    &receiver,
                    mic_recorder,
                    system_recorder,
                    &notifications,
                    &producer_stop,
                    &worker_abort,
                )
            })
            .map_err(|err| {
                capture.stop();
                SessionError::Start(format!("failed to spawn Ogg/Opus writer: {err}"))
            })?;

        Ok(Self {
            capture: Mutex::new(capture),
            writer: Mutex::new(RecordingWriterState::Active(writer)),
            writer_abort,
            fatal_error_receiver: notification_receivers.fatal,
            warning_receiver: notification_receivers.warning,
            mic_path,
            system_path,
        })
    }

    #[must_use]
    pub fn mic_path(&self) -> &Path {
        &self.mic_path
    }

    #[must_use]
    pub fn system_path(&self) -> &Path {
        &self.system_path
    }

    /// Replace microphone packets with silence while preserving file timing.
    pub fn set_microphone_muted(
        &self,
        muted: bool,
    ) {
        if let Ok(capture) = self.capture.lock() {
            capture.set_microphone_muted(muted);
        }
    }

    #[must_use]
    pub fn try_recv_error(&self) -> Option<String> {
        self.try_recv_fatal().or_else(|| self.try_recv_warning())
    }

    #[must_use]
    pub fn recv_error_timeout(
        &self,
        timeout: Duration,
    ) -> Option<String> {
        recv_recording_notification(&self.fatal_error_receiver, &self.warning_receiver, timeout)
    }

    pub(crate) const fn fatal_error_receiver(&self) -> &channel::Receiver<String> {
        &self.fatal_error_receiver
    }

    pub(crate) const fn warning_receiver(&self) -> &channel::Receiver<String> {
        &self.warning_receiver
    }

    pub(crate) fn try_recv_fatal(&self) -> Option<String> {
        self.fatal_error_receiver.try_recv().ok()
    }

    pub(crate) fn try_recv_warning(&self) -> Option<String> {
        self.warning_receiver.try_recv().ok()
    }

    /// Stop capture, finalize both Ogg streams, and join the writer.
    ///
    /// # Errors
    /// Returns [`SessionError::Start`] when finalization fails or a recording
    /// worker panics.
    pub fn stop(&self) -> Result<()> {
        let mut capture = self
            .capture
            .lock()
            .map_err(|_| SessionError::Start("WASAPI capture lock is poisoned".into()))?;
        capture.stop();
        drop(capture);

        let mut writer = self
            .writer
            .lock()
            .map_err(|_| SessionError::Start("Ogg writer lock is poisoned".into()))?;
        let state = std::mem::replace(&mut *writer, RecordingWriterState::Complete);
        let result = match state {
            RecordingWriterState::Active(writer) => match writer.join() {
                Ok(result) => result.map_err(|err| {
                    SessionError::Start(format!("failed to finalize Ogg files: {err}"))
                }),
                Err(_) => Err(SessionError::Start("Ogg writer thread panicked".into())),
            },
            RecordingWriterState::Complete => Ok(()),
            RecordingWriterState::Failed(error) => Err(error),
        };
        if let Err(error) = &result {
            *writer = RecordingWriterState::Failed(error.clone());
        }
        result
    }

    /// Abort pending silence compensation, stop capture, and finalize the
    /// audio already written.
    ///
    /// # Errors
    /// Returns the same finalization errors as [`Self::stop`].
    pub fn abort(&self) -> Result<()> {
        self.writer_abort.store(true, Ordering::SeqCst);
        self.stop()
    }
}

impl Drop for WasapiRecording {
    fn drop(&mut self) {
        let _ = self.stop();
    }
}

enum Startup {
    Ready(SourceLabel),
    Failed {
        source: SourceLabel,
        message: String,
    },
}

struct WorkerStartupContext<'a> {
    sender: &'a channel::Sender<Startup>,
    coordinator: &'a StartupCoordinator,
    phase: &'a WorkerStartupPhase,
}

fn run_recording_worker(
    receiver: &CaptureEventReceiver,
    mic: OggOpusRecorder,
    system: OggOpusRecorder,
    notifications: &RecordingNotificationSenders,
    producer_stop: &AtomicBool,
    abort_requested: &AtomicBool,
) -> io::Result<()> {
    let result = recording_loop(
        receiver,
        mic,
        system,
        &notifications.warning,
        abort_requested,
    );
    if let Err(error) = &result {
        producer_stop.store(true, Ordering::SeqCst);
        abort_requested.store(true, Ordering::SeqCst);
        let _ = notifications
            .fatal
            .send(format!("recording failed: {error}"));
    }
    result
}

fn recording_loop(
    receiver: &CaptureEventReceiver,
    mut mic: OggOpusRecorder,
    mut system: OggOpusRecorder,
    notification_sender: &channel::Sender<String>,
    abort_requested: &AtomicBool,
) -> io::Result<()> {
    let mut mic_timeline = TrackTimeline::default();
    let mut system_timeline = TrackTimeline::default();
    while let Some(event) = receiver.recv() {
        match event {
            CaptureEvent::Samples(frame) => {
                if frame.format().sample_rate != WASAPI_SAMPLE_RATE
                    || frame.format().channels != WASAPI_CHANNELS
                {
                    return Err(io::Error::other(format!(
                        "unsupported WASAPI recording format: {} Hz, {} channels",
                        frame.format().sample_rate,
                        frame.format().channels
                    )));
                }
                let samples = frame.samples().as_f32().ok_or_else(|| {
                    io::Error::other(format!(
                        "unsupported WASAPI recording sample format: {:?}",
                        frame.format().sample_format
                    ))
                })?;
                match frame.track_id() {
                    TrackId::MICROPHONE => {
                        let gap = mic_timeline.gap_before(&frame);
                        let written_gap = push_silence(&mut mic, gap.frames, abort_requested)?;
                        mic.push(samples)?;
                        mic_timeline.observe(&frame, gap, written_gap);
                    },
                    TrackId::SYSTEM => {
                        let gap = system_timeline.gap_before(&frame);
                        let written_gap = push_silence(&mut system, gap.frames, abort_requested)?;
                        system.push(samples)?;
                        system_timeline.observe(&frame, gap, written_gap);
                    },
                    track_id => {
                        return Err(io::Error::other(format!(
                            "unexpected WASAPI track {}",
                            track_id.get()
                        )));
                    },
                }
            },
            CaptureEvent::Overflow {
                track_id,
                dropped_frames,
            } => {
                match track_id {
                    TrackId::MICROPHONE => {
                        let compensation = mic_timeline.overflow_before(dropped_frames);
                        let written = push_silence(&mut mic, compensation.frames, abort_requested)?;
                        mic_timeline.observe_overflow(compensation.logical_frames, written);
                    },
                    TrackId::SYSTEM => {
                        let compensation = system_timeline.overflow_before(dropped_frames);
                        let written =
                            push_silence(&mut system, compensation.frames, abort_requested)?;
                        system_timeline.observe_overflow(compensation.logical_frames, written);
                    },
                    _ => {},
                }
                let _ = notification_sender.try_send(format!(
                    "capture overflow on track {}: dropped {dropped_frames} audio frames",
                    track_id.get()
                ));
            },
            CaptureEvent::Error {
                track_id, message, ..
            } => {
                return Err(io::Error::other(format!(
                    "{} WASAPI stream failed: {message}",
                    track_id.map_or("unknown", track_name)
                )));
            },
            _ => {},
        }
    }
    // The shared session end is the later observed track end. Padding the
    // shorter stream preserves mic/system alignment when one endpoint starts
    // later, goes idle, or stops producing first.
    let shared_session_end = mic_timeline
        .written_frames()
        .max(system_timeline.written_frames());
    push_silence(
        &mut mic,
        mic_timeline.padding_to(shared_session_end),
        abort_requested,
    )?;
    push_silence(
        &mut system,
        system_timeline.padding_to(shared_session_end),
        abort_requested,
    )?;
    mic.finish()?;
    system.finish()
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct GapCompensation {
    frames: u64,
    inferred_drop_frames: u64,
    reanchor: bool,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct OverflowCompensation {
    logical_frames: u64,
    frames: u64,
}

#[derive(Default)]
struct TrackTimeline {
    // All frame counts are measured from the shared capture origin. Timestamp
    // gaps above TIMELINE_JITTER_TOLERANCE become silence even for contiguous
    // sequence numbers (an idle endpoint emits no packets).
    next_sequence: Option<u64>,
    written_frames: u64,
    logical_cursor_frames: u64,
    // A sequence gap may be observed before its aggregated Overflow event.
    // This credit makes that later report accounting-only instead of writing
    // the same silence twice. A later sample expires unused credit so it
    // cannot cancel an unrelated overflow notification.
    inferred_unreported_frames: u64,
}

impl TrackTimeline {
    fn gap_before(
        &self,
        frame: &AudioFrame,
    ) -> GapCompensation {
        let sample_rate = frame.format().sample_rate;
        let timestamp_frames = duration_to_frames(frame.timestamp().as_duration(), sample_rate);
        let logical_gap_frames = timestamp_frames.saturating_sub(self.logical_cursor_frames);
        let jitter_frames = duration_to_frames(TIMELINE_JITTER_TOLERANCE, sample_rate);
        if logical_gap_frames <= jitter_frames {
            return GapCompensation::default();
        }

        let compensable_frames = duration_to_frames(MAX_DISCONTINUITY_COMPENSATION, sample_rate);
        let sequence_has_gap = self
            .next_sequence
            .map_or(frame.sequence() > 0, |next_sequence| {
                frame.sequence() > next_sequence
            });
        GapCompensation {
            frames: logical_gap_frames.min(compensable_frames),
            inferred_drop_frames: if sequence_has_gap {
                logical_gap_frames
            } else {
                0
            },
            reanchor: true,
        }
    }

    fn observe(
        &mut self,
        frame: &AudioFrame,
        gap: GapCompensation,
        written_gap_frames: u64,
    ) {
        let next_sequence = frame.sequence().saturating_add(1);
        self.next_sequence = Some(
            self.next_sequence
                .map_or(next_sequence, |current| current.max(next_sequence)),
        );
        self.written_frames = self
            .written_frames
            .saturating_add(written_gap_frames)
            .saturating_add(u64::from(frame.frame_count()));
        if gap.reanchor {
            let timestamp_frames =
                duration_to_frames(frame.timestamp().as_duration(), frame.format().sample_rate);
            self.logical_cursor_frames =
                timestamp_frames.saturating_add(u64::from(frame.frame_count()));
        } else {
            self.logical_cursor_frames = self
                .logical_cursor_frames
                .saturating_add(u64::from(frame.frame_count()));
        }
        self.inferred_unreported_frames = gap.inferred_drop_frames;
    }

    fn overflow_before(
        &mut self,
        dropped_frames: u64,
    ) -> OverflowCompensation {
        let inferred = dropped_frames.min(self.inferred_unreported_frames);
        self.inferred_unreported_frames -= inferred;
        let logical_frames = dropped_frames - inferred;
        OverflowCompensation {
            logical_frames,
            frames: logical_frames.min(duration_to_frames(
                MAX_DISCONTINUITY_COMPENSATION,
                WASAPI_SAMPLE_RATE,
            )),
        }
    }

    fn observe_overflow(
        &mut self,
        logical_frames: u64,
        written_frames: u64,
    ) {
        self.written_frames = self.written_frames.saturating_add(written_frames);
        self.logical_cursor_frames = self.logical_cursor_frames.saturating_add(logical_frames);
    }

    const fn written_frames(&self) -> u64 {
        self.written_frames
    }

    fn padding_to(
        &self,
        shared_session_end: u64,
    ) -> u64 {
        shared_session_end
            .saturating_sub(self.written_frames)
            .min(duration_to_frames(
                MAX_DISCONTINUITY_COMPENSATION,
                WASAPI_SAMPLE_RATE,
            ))
    }
}

fn frames_to_duration(
    frames: u64,
    sample_rate: u32,
) -> Duration {
    let sample_rate = u64::from(sample_rate);
    let seconds = frames / sample_rate;
    let remainder = frames % sample_rate;
    let nanos = remainder.saturating_mul(1_000_000_000) / sample_rate;
    Duration::new(seconds, u32::try_from(nanos).unwrap_or(999_999_999))
}

fn duration_to_frames(
    duration: Duration,
    sample_rate: u32,
) -> u64 {
    let sample_rate = u64::from(sample_rate);
    let whole = duration.as_secs().saturating_mul(sample_rate);
    let fractional = u64::from(duration.subsec_nanos())
        .saturating_mul(sample_rate)
        .saturating_add(500_000_000)
        / 1_000_000_000;
    whole.saturating_add(fractional)
}

fn push_silence(
    recorder: &mut OggOpusRecorder,
    frames: u64,
    abort_requested: &AtomicBool,
) -> io::Result<u64> {
    push_silence_chunks(frames, abort_requested, |samples| recorder.push(samples))
}

fn push_silence_chunks(
    mut frames: u64,
    abort_requested: &AtomicBool,
    mut push: impl FnMut(&[f32]) -> io::Result<()>,
) -> io::Result<u64> {
    const SILENCE: [f32; 320] = [0.0; 320];
    let mut written_frames = 0_u64;
    while frames > 0 {
        if abort_requested.load(Ordering::SeqCst) {
            break;
        }
        let chunk_frames = frames.min(320);
        let chunk = usize::try_from(chunk_frames)
            .map_err(|_| io::Error::other("silence chunk length overflow"))?;
        push(&SILENCE[..chunk])?;
        frames -= chunk_frames;
        written_frames = written_frames.saturating_add(chunk_frames);
    }
    Ok(written_frames)
}

fn recording_start_error(
    path: &Path,
    error: &io::Error,
) -> SessionError {
    SessionError::Start(format!(
        "failed to create Ogg/Opus recording {}: {error}",
        path.display()
    ))
}

fn capture_worker(
    source: SourceLabel,
    event_sender: &RealtimeCaptureSender,
    startup_sender: &channel::Sender<Startup>,
    startup_coordinator: &StartupCoordinator,
    stop_requested: &AtomicBool,
    microphone_muted: &AtomicBool,
    capture_origin: Instant,
) {
    let startup_phase = WorkerStartupPhase::default();
    let startup = WorkerStartupContext {
        sender: startup_sender,
        coordinator: startup_coordinator,
        phase: &startup_phase,
    };
    let result = run_capture(
        source,
        event_sender,
        &startup,
        stop_requested,
        microphone_muted,
        capture_origin,
    );
    if let Err(message) = result {
        publish_worker_failure(
            source,
            message,
            event_sender,
            startup_sender,
            &startup_phase,
        );
    }
}

fn publish_worker_failure(
    source: SourceLabel,
    message: String,
    event_sender: &RealtimeCaptureSender,
    startup_sender: &channel::Sender<Startup>,
    startup_phase: &WorkerStartupPhase,
) {
    match startup_phase.failure_route() {
        WorkerFailureRoute::Startup => {
            let _ = startup_sender.send(Startup::Failed { source, message });
        },
        WorkerFailureRoute::Runtime => {
            let _ = event_sender.send_control(CaptureControlEvent::Error {
                track_id: Some(source.track_id()),
                message,
                recoverable: false,
            });
        },
    }
}

fn run_capture(
    source: SourceLabel,
    event_sender: &RealtimeCaptureSender,
    startup: &WorkerStartupContext<'_>,
    stop_requested: &AtomicBool,
    microphone_muted: &AtomicBool,
    capture_origin: Instant,
) -> std::result::Result<(), String> {
    wasapi::initialize_mta()
        .ok()
        .map_err(|err| err.to_string())?;
    let _com = ComApartment;

    let endpoint_direction = match source {
        SourceLabel::Mic => Direction::Capture,
        SourceLabel::System => Direction::Render,
    };
    let enumerator = DeviceEnumerator::new().map_err(|err| err.to_string())?;
    let device = enumerator
        .get_default_device(&endpoint_direction)
        .map_err(|err| err.to_string())?;
    let mut audio_client = device.get_iaudioclient().map_err(|err| err.to_string())?;
    let format = WaveFormat::new(
        32,
        32,
        &SampleType::Float,
        WASAPI_SAMPLE_RATE as usize,
        WASAPI_CHANNELS as usize,
        None,
    );
    let (_, minimum_period) = audio_client
        .get_device_period()
        .map_err(|err| err.to_string())?;
    let mode = StreamMode::EventsShared {
        autoconvert: true,
        buffer_duration_hns: minimum_period,
    };

    // A capture stream opened against a render endpoint makes wasapi-rs set
    // AUDCLNT_STREAMFLAGS_LOOPBACK. Microphone capture uses the same stream
    // direction against a capture endpoint.
    audio_client
        .initialize_client(&format, &Direction::Capture, &mode)
        .map_err(|err| err.to_string())?;
    let event_handle = audio_client
        .set_get_eventhandle()
        .map_err(|err| err.to_string())?;
    let capture_client = audio_client
        .get_audiocaptureclient()
        .map_err(|err| err.to_string())?;
    audio_client.start_stream().map_err(|err| err.to_string())?;
    // A worker that reported Ready must not enter its runtime loop until the
    // coordinator has observed both Ready messages. This makes the startup /
    // runtime failure boundary explicit: every later failure goes to the
    // control channel rather than an already-abandoned startup queue.
    publish_ready_and_wait(
        startup.sender,
        Startup::Ready(source),
        startup.coordinator,
        startup.phase,
        stop_requested,
    )
    .map_err(|err| err.to_string())?;

    let mut bytes = VecDeque::new();
    let mut sequence = 0_u64;
    while !stop_requested.load(Ordering::SeqCst) {
        // A timeout is expected when the endpoint is idle and gives the stop
        // flag a bounded response time. Only ask for a buffer after WASAPI
        // signals that one is ready.
        if event_handle.wait_for_event(EVENT_WAIT_MILLIS).is_err() {
            continue;
        }
        capture_client
            .read_from_device_to_deque(&mut bytes)
            .map_err(|err| err.to_string())?;
        let samples = drain_f32_samples(&mut bytes);
        if samples.is_empty() {
            continue;
        }
        let samples = if source == SourceLabel::Mic && microphone_muted.load(Ordering::SeqCst) {
            vec![0.0; samples.len()]
        } else {
            samples
        };
        let packet_frames = u64::try_from(samples.len()).map_err(|error| error.to_string())?;
        let packet_started_at = capture_origin
            .elapsed()
            .saturating_sub(frames_to_duration(packet_frames, WASAPI_SAMPLE_RATE));
        let frame = AudioFrame::from_f32(
            source.track_id(),
            source.source_kind(),
            sequence,
            MonotonicTimestamp::from_duration(packet_started_at),
            WASAPI_SAMPLE_RATE,
            WASAPI_CHANNELS,
            samples,
        )
        .map_err(|error| error.to_string())?;
        sequence = sequence.saturating_add(1);
        let _ = event_sender
            .try_send(frame)
            .map_err(|error| error.to_string())?;
    }

    audio_client.stop_stream().map_err(|err| err.to_string())
}

struct ComApartment;

impl Drop for ComApartment {
    fn drop(&mut self) {
        wasapi::deinitialize();
    }
}

fn drain_f32_samples(bytes: &mut VecDeque<u8>) -> Vec<f32> {
    let sample_count = bytes.len() / size_of::<f32>();
    let mut samples = Vec::with_capacity(sample_count);
    for _ in 0..sample_count {
        let Some(b0) = bytes.pop_front() else {
            break;
        };
        let Some(b1) = bytes.pop_front() else {
            break;
        };
        let Some(b2) = bytes.pop_front() else {
            break;
        };
        let Some(b3) = bytes.pop_front() else {
            break;
        };
        samples.push(f32::from_le_bytes([b0, b1, b2, b3]));
    }
    samples
}

fn join_workers(workers: &mut Vec<JoinHandle<()>>) {
    for worker in workers.drain(..) {
        let _ = worker.join();
    }
}

const fn source_name(source: SourceLabel) -> &'static str {
    match source {
        SourceLabel::Mic => "microphone",
        SourceLabel::System => "system loopback",
    }
}

const fn track_name(track_id: TrackId) -> &'static str {
    match track_id {
        TrackId::MICROPHONE => "microphone",
        TrackId::SYSTEM => "system loopback",
        _ => "unknown",
    }
}

fn legacy_capture_event(event: CaptureEvent) -> Option<WasapiCaptureEvent> {
    match event {
        CaptureEvent::Samples(frame) => {
            let source = source_label_from_track(frame.track_id())?;
            let sample_format = frame.format().sample_format.clone();
            match frame.into_samples() {
                AudioSamples::F32(samples) => Some(WasapiCaptureEvent::Samples(WasapiPcmChunk {
                    source,
                    samples,
                })),
                _ => Some(WasapiCaptureEvent::Error {
                    source,
                    message: format!("unsupported WASAPI sample format: {sample_format:?}"),
                }),
            }
        },
        CaptureEvent::Overflow {
            track_id,
            dropped_frames,
        } => source_label_from_track(track_id).map(|source| WasapiCaptureEvent::Error {
            source,
            message: format!("capture overflow: dropped {dropped_frames} audio frames"),
        }),
        CaptureEvent::Error {
            track_id: Some(track_id),
            message,
            ..
        } => source_label_from_track(track_id)
            .map(|source| WasapiCaptureEvent::Error { source, message }),
        _ => None,
    }
}

const fn source_label_from_track(track_id: TrackId) -> Option<SourceLabel> {
    match track_id {
        TrackId::MICROPHONE => Some(SourceLabel::Mic),
        TrackId::SYSTEM => Some(SourceLabel::System),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::fs::File;
    use std::sync::{
        Arc, Mutex, OnceLock,
        atomic::{AtomicBool, Ordering},
    };
    use std::time::{Duration, Instant};

    use ogg::PacketReader;
    use wisp_core::{AudioFrame, CaptureEvent, MonotonicTimestamp, SourceKind, TrackId};

    use crate::backend::{FrameEnqueue, StartupCoordinator, realtime_capture_channel};
    use crate::ogg_opus_recorder::OggOpusRecorder;

    use super::{
        FORCE_STARTUP_REAPER_SPAWN_FAILURE, NOTIFICATION_QUEUE_CAPACITY, Startup, TrackTimeline,
        WasapiCaptureEvent, WasapiPcmChunk, await_capture_startup, drain_f32_samples,
        ensure_startup_reaper, legacy_capture_event, publish_worker_failure, push_silence,
        push_silence_chunks, reap_finished_startup_workers, recording_loop,
        recording_notification_channels, recv_recording_notification,
        recv_recording_notification_with_wait_hook, reserve_startup_attempt,
        retain_startup_workers, run_recording_worker, startup_worker_registry,
    };

    static STARTUP_REGISTRY_TEST_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

    fn startup_registry_test_lock() -> std::sync::MutexGuard<'static, ()> {
        STARTUP_REGISTRY_TEST_LOCK
            .get_or_init(|| Mutex::new(()))
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn wait_for_startup_registry_empty() {
        let deadline = Instant::now() + Duration::from_secs(1);
        loop {
            reap_finished_startup_workers();
            if startup_worker_registry()
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .workers
                .is_empty()
            {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "startup worker registry did not drain"
            );
            std::thread::yield_now();
        }
    }

    #[test]
    fn pcm_bytes_are_decoded_and_partial_sample_is_retained() {
        let mut bytes = [0.25_f32.to_le_bytes(), (-0.5_f32).to_le_bytes()]
            .concat()
            .into_iter()
            .chain([7, 8])
            .collect::<VecDeque<_>>();

        assert_eq!(drain_f32_samples(&mut bytes), vec![0.25, -0.5]);
        assert_eq!(bytes, VecDeque::from([7, 8]));
    }

    #[test]
    fn production_startup_path_returns_by_deadline_with_hung_worker() {
        let _registry_guard = startup_registry_test_lock();
        wait_for_startup_registry_empty();
        let (startup_sender, startup_receiver) = crossbeam_channel::bounded::<Startup>(2);
        let stop_requested = Arc::new(AtomicBool::new(false));
        let coordinator = StartupCoordinator::new(2);
        let (release_sender, release_receiver) = crossbeam_channel::bounded::<()>(0);
        let (exited_sender, exited_receiver) = crossbeam_channel::bounded(1);
        let hung_worker = std::thread::spawn(move || {
            let _ = release_receiver.recv();
            let _ = exited_sender.send(());
        });
        let mut workers = vec![hung_worker];
        let started = Instant::now();

        let result = await_capture_startup(
            &startup_receiver,
            &coordinator,
            &stop_requested,
            &mut workers,
            Instant::now() + Duration::from_millis(20),
        );

        assert!(
            matches!(result, Err(crate::SessionError::Start(message)) if message.contains("timed out"))
        );
        assert!(started.elapsed() < Duration::from_millis(250));
        assert!(stop_requested.load(Ordering::SeqCst));
        assert!(workers.is_empty());
        release_sender.send(()).unwrap();
        exited_receiver
            .recv_timeout(Duration::from_secs(1))
            .unwrap();
        wait_for_startup_registry_empty();
        drop(startup_sender);
    }

    #[test]
    fn outstanding_startup_worker_set_bounds_repeated_start_attempts() {
        let _registry_guard = startup_registry_test_lock();
        wait_for_startup_registry_empty();
        let (release_sender, release_receiver) = crossbeam_channel::bounded::<()>(0);
        let worker = std::thread::spawn(move || {
            let _ = release_receiver.recv();
        });
        let mut workers = vec![worker];
        retain_startup_workers(&mut workers);
        assert!(workers.is_empty());

        for _ in 0..4 {
            assert!(reserve_startup_attempt().is_err());
        }
        assert_eq!(
            startup_worker_registry()
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .workers
                .len(),
            1
        );

        release_sender.send(()).unwrap();
        wait_for_startup_registry_empty();
        drop(reserve_startup_attempt().unwrap());
    }

    #[test]
    fn reaper_spawn_failure_retains_worker_ownership_for_retry() {
        let _registry_guard = startup_registry_test_lock();
        wait_for_startup_registry_empty();
        let (release_sender, release_receiver) = crossbeam_channel::bounded::<()>(0);
        let worker = std::thread::spawn(move || {
            let _ = release_receiver.recv();
        });
        let mut workers = vec![worker];
        FORCE_STARTUP_REAPER_SPAWN_FAILURE.store(true, Ordering::SeqCst);
        retain_startup_workers(&mut workers);

        {
            let registry = startup_worker_registry()
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            assert_eq!(registry.workers.len(), 1);
            assert!(!registry.reaper_running);
        }
        ensure_startup_reaper();
        release_sender.send(()).unwrap();
        wait_for_startup_registry_empty();
    }

    #[test]
    fn legacy_capture_surface_keeps_struct_fields_and_enum_patterns() {
        let frame = AudioFrame::from_f32(
            TrackId::MICROPHONE,
            SourceKind::Microphone,
            0,
            MonotonicTimestamp::default(),
            16_000,
            1,
            vec![0.25, -0.5],
        )
        .unwrap();

        let Some(WasapiCaptureEvent::Samples(WasapiPcmChunk { source, samples })) =
            legacy_capture_event(CaptureEvent::Samples(frame))
        else {
            panic!("expected the legacy samples variant");
        };
        assert_eq!(source, wisp_core::SourceLabel::Mic);
        assert_eq!(samples, [0.25, -0.5]);

        let error = WasapiCaptureEvent::Error {
            source: wisp_core::SourceLabel::System,
            message: "failed".into(),
        };
        assert!(matches!(
            error,
            WasapiCaptureEvent::Error {
                source: wisp_core::SourceLabel::System,
                message,
            } if message == "failed"
        ));
    }

    #[test]
    fn fatal_notification_has_reserved_capacity_when_warning_queue_is_full() {
        let (senders, receivers) = recording_notification_channels();
        for index in 0..NOTIFICATION_QUEUE_CAPACITY {
            senders
                .warning
                .try_send(format!("warning {index}"))
                .unwrap();
        }
        assert!(senders.warning.try_send("another warning".into()).is_err());

        senders.fatal.send("recording failed".into()).unwrap();
        assert_eq!(receivers.fatal.try_recv().unwrap(), "recording failed");
    }

    #[test]
    fn queued_fatal_survives_disconnected_warning_channel() {
        let (senders, receivers) = recording_notification_channels();
        senders.fatal.send("recording failed".into()).unwrap();
        drop(senders);

        assert_eq!(
            recv_recording_notification(
                &receivers.fatal,
                &receivers.warning,
                Duration::from_millis(10),
            )
            .unwrap(),
            "recording failed"
        );
    }

    #[test]
    fn blocking_notification_wait_rechecks_fatal_before_warning() {
        let (senders, receivers) = recording_notification_channels();
        let entered = Arc::new(std::sync::Barrier::new(2));
        let release = Arc::new(std::sync::Barrier::new(2));
        let waiter_entered = Arc::clone(&entered);
        let waiter_release = Arc::clone(&release);
        let waiter = std::thread::spawn(move || {
            recv_recording_notification_with_wait_hook(
                &receivers.fatal,
                &receivers.warning,
                Duration::from_secs(1),
                || {
                    waiter_entered.wait();
                    waiter_release.wait();
                },
            )
        });
        entered.wait();
        senders.warning.send("warning".into()).unwrap();
        senders.fatal.send("fatal".into()).unwrap();
        release.wait();

        assert_eq!(waiter.join().unwrap().as_deref(), Some("fatal"));
    }

    #[test]
    fn recording_worker_delivers_fatal_when_warning_queue_is_full() {
        let directory = tempfile::tempdir().unwrap();
        let mic = OggOpusRecorder::create(&directory.path().join("mic.ogg")).unwrap();
        let system = OggOpusRecorder::create(&directory.path().join("system.ogg")).unwrap();
        let (capture_senders, receiver) = realtime_capture_channel(1, &[TrackId::MICROPHONE]);
        capture_senders[0]
            .try_send(
                AudioFrame::from_f32(
                    TrackId::MICROPHONE,
                    SourceKind::Microphone,
                    0,
                    MonotonicTimestamp::default(),
                    8_000,
                    1,
                    vec![0.0; 80],
                )
                .unwrap(),
            )
            .unwrap();
        drop(capture_senders);

        let (notifications, receivers) = recording_notification_channels();
        for index in 0..NOTIFICATION_QUEUE_CAPACITY {
            notifications
                .warning
                .try_send(format!("warning {index}"))
                .unwrap();
        }
        let producer_stop = AtomicBool::new(false);
        let abort_requested = AtomicBool::new(false);
        assert!(
            run_recording_worker(
                &receiver,
                mic,
                system,
                &notifications,
                &producer_stop,
                &abort_requested,
            )
            .is_err()
        );
        drop(notifications);

        assert!(producer_stop.load(Ordering::SeqCst));
        assert!(abort_requested.load(Ordering::SeqCst));
        assert!(
            recv_recording_notification(
                &receivers.fatal,
                &receivers.warning,
                Duration::from_millis(10),
            )
            .unwrap()
            .contains("unsupported WASAPI recording format")
        );
    }

    #[test]
    fn terminal_overflow_is_silence_and_tracks_finish_at_shared_end() {
        let directory = tempfile::tempdir().unwrap();
        let mic_path = directory.path().join("mic.ogg");
        let system_path = directory.path().join("system.ogg");
        let mic = OggOpusRecorder::create(&mic_path).unwrap();
        let system = OggOpusRecorder::create(&system_path).unwrap();
        let (capture_senders, receiver) = realtime_capture_channel(1, &[TrackId::MICROPHONE]);
        let make_frame = |sequence, milliseconds| {
            AudioFrame::from_f32(
                TrackId::MICROPHONE,
                SourceKind::Microphone,
                sequence,
                MonotonicTimestamp::from_duration(Duration::from_millis(milliseconds)),
                16_000,
                1,
                vec![0.0; 160],
            )
            .unwrap()
        };
        assert_eq!(
            capture_senders[0].try_send(make_frame(0, 0)).unwrap(),
            FrameEnqueue::Enqueued
        );
        assert_eq!(
            capture_senders[0].try_send(make_frame(1, 10)).unwrap(),
            FrameEnqueue::Dropped
        );
        drop(capture_senders);
        let (warning_sender, _warning_receiver) = crossbeam_channel::bounded(1);
        let abort_requested = AtomicBool::new(false);

        recording_loop(&receiver, mic, system, &warning_sender, &abort_requested).unwrap();
        assert_eq!(ogg_audio_frames(&mic_path), 320);
        assert_eq!(ogg_audio_frames(&system_path), 320);
    }

    #[test]
    fn samples_then_overflow_compensates_missing_interval_exactly_once() {
        let first = frame(TrackId::MICROPHONE, 0, 0);
        let after_gap = frame(TrackId::MICROPHONE, 2, 20);

        let mut timeline = TrackTimeline::default();
        let first_gap = timeline.gap_before(&first);
        timeline.observe(&first, first_gap, first_gap.frames);
        let inferred_gap = timeline.gap_before(&after_gap);
        assert_eq!(inferred_gap.frames, 160);
        timeline.observe(&after_gap, inferred_gap, inferred_gap.frames);
        assert_eq!(timeline.overflow_before(160).frames, 0);
        assert_eq!(timeline.written_frames(), 480);
    }

    #[test]
    fn partial_overflow_consumes_only_matching_inferred_credit() {
        let first = frame(TrackId::MICROPHONE, 0, 0);
        let after_gap = frame(TrackId::MICROPHONE, 3, 30);
        let mut timeline = TrackTimeline::default();
        let first_gap = timeline.gap_before(&first);
        timeline.observe(&first, first_gap, first_gap.frames);
        let inferred_gap = timeline.gap_before(&after_gap);
        assert_eq!(inferred_gap.frames, 320);
        timeline.observe(&after_gap, inferred_gap, inferred_gap.frames);

        assert_eq!(timeline.overflow_before(160).frames, 0);
        assert_eq!(timeline.overflow_before(160).frames, 0);
        assert_eq!(timeline.overflow_before(160).frames, 160);
    }

    #[test]
    fn oversized_overflow_compensation_is_capped() {
        let mut timeline = TrackTimeline::default();

        let compensation = timeline.overflow_before(960_000);
        assert_eq!(compensation.logical_frames, 960_000);
        assert_eq!(compensation.frames, 80_000);
    }

    #[test]
    fn later_sample_expires_stale_inferred_overflow_credit() {
        let first = frame(TrackId::MICROPHONE, 0, 0);
        let after_gap = frame(TrackId::MICROPHONE, 3, 30);
        let later = frame(TrackId::MICROPHONE, 4, 40);
        let mut timeline = TrackTimeline::default();
        let first_gap = timeline.gap_before(&first);
        timeline.observe(&first, first_gap, first_gap.frames);
        let inferred_gap = timeline.gap_before(&after_gap);
        timeline.observe(&after_gap, inferred_gap, inferred_gap.frames);
        assert_eq!(timeline.overflow_before(160).frames, 0);
        let later_gap = timeline.gap_before(&later);
        timeline.observe(&later, later_gap, later_gap.frames);

        assert_eq!(timeline.overflow_before(160).frames, 160);
    }

    #[test]
    fn large_discontinuity_is_capped_and_capture_clock_reanchors() {
        let mut timeline = TrackTimeline::default();
        let first = frame(TrackId::MICROPHONE, 0, 60_000);
        let gap = timeline.gap_before(&first);
        assert_eq!(gap.frames, 80_000);
        timeline.observe(&first, gap, gap.frames);

        assert_eq!(
            timeline
                .gap_before(&frame(TrackId::MICROPHONE, 1, 60_010))
                .frames,
            0
        );
    }

    #[test]
    fn above_cap_overflow_then_sample_writes_one_cap_and_advances_full_outage() {
        let first = frame(TrackId::MICROPHONE, 0, 0);
        let after_outage = frame(TrackId::MICROPHONE, 2, 60_010);
        let mut timeline = TrackTimeline::default();
        let first_gap = timeline.gap_before(&first);
        timeline.observe(&first, first_gap, first_gap.frames);

        let overflow = timeline.overflow_before(960_000);
        assert_eq!(overflow.logical_frames, 960_000);
        assert_eq!(overflow.frames, 80_000);
        timeline.observe_overflow(overflow.logical_frames, overflow.frames);
        let sample_gap = timeline.gap_before(&after_outage);
        assert_eq!(sample_gap.frames, 0);
        timeline.observe(&after_outage, sample_gap, sample_gap.frames);

        assert_eq!(timeline.written_frames(), 80_320);
    }

    #[test]
    fn above_cap_sample_then_overflow_writes_one_cap_and_consumes_raw_credit() {
        let first = frame(TrackId::MICROPHONE, 0, 0);
        let after_outage = frame(TrackId::MICROPHONE, 2, 60_010);
        let mut timeline = TrackTimeline::default();
        let first_gap = timeline.gap_before(&first);
        timeline.observe(&first, first_gap, first_gap.frames);

        let sample_gap = timeline.gap_before(&after_outage);
        assert_eq!(sample_gap.frames, 80_000);
        assert_eq!(sample_gap.inferred_drop_frames, 960_000);
        timeline.observe(&after_outage, sample_gap, sample_gap.frames);
        let overflow = timeline.overflow_before(960_000);
        assert_eq!(overflow.logical_frames, 0);
        assert_eq!(overflow.frames, 0);

        assert_eq!(timeline.written_frames(), 80_320);
    }

    #[test]
    fn delayed_first_frames_and_terminal_padding_share_one_origin() {
        let mut microphone = TrackTimeline::default();
        let mut system = TrackTimeline::default();
        let mic_frame = frame(TrackId::MICROPHONE, 0, 0);
        let system_frame = frame(TrackId::SYSTEM, 0, 30);

        let mic_gap = microphone.gap_before(&mic_frame);
        microphone.observe(&mic_frame, mic_gap, mic_gap.frames);
        let system_gap = system.gap_before(&system_frame);
        assert_eq!(system_gap.frames, 480);
        system.observe(&system_frame, system_gap, system_gap.frames);

        let shared_end = microphone.written_frames().max(system.written_frames());
        assert_eq!(microphone.padding_to(shared_end), 480);
        assert_eq!(system.padding_to(shared_end), 0);
    }

    #[test]
    fn contiguous_sequence_timestamp_gap_is_silence_but_jitter_is_not() {
        let first = frame(TrackId::MICROPHONE, 0, 0);

        let mut idle_gap = TrackTimeline::default();
        let first_gap = idle_gap.gap_before(&first);
        idle_gap.observe(&first, first_gap, first_gap.frames);
        assert_eq!(
            idle_gap
                .gap_before(&frame(TrackId::MICROPHONE, 1, 30))
                .frames,
            320
        );

        let mut jitter = TrackTimeline::default();
        let first_gap = jitter.gap_before(&first);
        jitter.observe(&first, first_gap, first_gap.frames);
        assert_eq!(
            jitter.gap_before(&frame(TrackId::MICROPHONE, 1, 11)).frames,
            0
        );
    }

    #[test]
    fn large_silence_stops_before_writing_when_cancellation_is_requested() {
        let directory = tempfile::tempdir().unwrap();
        let mut recorder =
            OggOpusRecorder::create(&directory.path().join("cancelled.ogg")).unwrap();
        let abort_requested = AtomicBool::new(true);

        assert_eq!(
            push_silence(&mut recorder, 1_601, &abort_requested).unwrap(),
            0
        );
        recorder.finish().unwrap();
    }

    #[test]
    fn abort_cancels_short_alignment_padding_before_first_chunk() {
        let directory = tempfile::tempdir().unwrap();
        let mut recorder = OggOpusRecorder::create(&directory.path().join("padding.ogg")).unwrap();
        let abort_requested = AtomicBool::new(true);

        assert_eq!(
            push_silence(&mut recorder, 160, &abort_requested).unwrap(),
            0
        );
        recorder.finish().unwrap();
    }

    #[test]
    fn graceful_alignment_padding_is_not_cancelled_by_producer_stop() {
        let directory = tempfile::tempdir().unwrap();
        let mut recorder = OggOpusRecorder::create(&directory.path().join("graceful.ogg")).unwrap();
        let abort_requested = AtomicBool::new(false);

        assert_eq!(
            push_silence(&mut recorder, 1_600, &abort_requested).unwrap(),
            1_600
        );
        recorder.finish().unwrap();
    }

    #[test]
    fn abort_is_polled_between_every_twenty_millisecond_silence_chunk() {
        let abort_requested = AtomicBool::new(false);
        let mut chunks = 0;

        let written = push_silence_chunks(1_600, &abort_requested, |samples| {
            chunks += 1;
            assert_eq!(samples.len(), 320);
            abort_requested.store(true, Ordering::SeqCst);
            Ok(())
        })
        .unwrap();

        assert_eq!(chunks, 1);
        assert_eq!(written, 320);
    }

    #[test]
    fn post_ready_worker_failure_is_published_only_to_runtime_control() {
        let (senders, receiver) = realtime_capture_channel(1, &[TrackId::MICROPHONE]);
        let event_sender = &senders[0];
        let (startup_sender, startup_receiver) = crossbeam_channel::bounded(1);
        let startup_phase = crate::backend::WorkerStartupPhase::default();
        startup_phase.mark_ready_published();

        publish_worker_failure(
            wisp_core::SourceLabel::Mic,
            "runtime failed".into(),
            event_sender,
            &startup_sender,
            &startup_phase,
        );

        assert!(startup_receiver.try_recv().is_err());
        assert!(matches!(
            receiver.try_recv(),
            Some(CaptureEvent::Error {
                track_id: Some(TrackId::MICROPHONE),
                message,
                recoverable: false,
            }) if message == "runtime failed"
        ));
    }

    #[test]
    fn pre_ready_worker_failure_stays_on_startup_channel() {
        let (senders, receiver) = realtime_capture_channel(1, &[TrackId::MICROPHONE]);
        let event_sender = &senders[0];
        let (startup_sender, startup_receiver) = crossbeam_channel::bounded(1);
        let startup_phase = crate::backend::WorkerStartupPhase::default();

        publish_worker_failure(
            wisp_core::SourceLabel::Mic,
            "startup failed".into(),
            event_sender,
            &startup_sender,
            &startup_phase,
        );

        assert!(matches!(
            startup_receiver.try_recv(),
            Ok(Startup::Failed {
                source: wisp_core::SourceLabel::Mic,
                message,
            }) if message == "startup failed"
        ));
        assert!(receiver.try_recv().is_none());
    }

    fn frame(
        track_id: TrackId,
        sequence: u64,
        milliseconds: u64,
    ) -> AudioFrame {
        let source = match track_id {
            TrackId::MICROPHONE => SourceKind::Microphone,
            TrackId::SYSTEM => SourceKind::SystemAudio,
            _ => SourceKind::Other("test".into()),
        };
        AudioFrame::from_f32(
            track_id,
            source,
            sequence,
            MonotonicTimestamp::from_duration(Duration::from_millis(milliseconds)),
            16_000,
            1,
            vec![0.0; 160],
        )
        .unwrap()
    }

    fn ogg_audio_frames(path: &std::path::Path) -> u64 {
        let mut reader = PacketReader::new(File::open(path).unwrap());
        let head = reader.read_packet().unwrap().unwrap();
        let pre_skip = u64::from(u16::from_le_bytes([head.data[10], head.data[11]]));
        let mut last = reader.read_packet().unwrap().unwrap();
        while let Some(packet) = reader.read_packet().unwrap() {
            last = packet;
        }
        last.absgp_page().saturating_sub(pre_skip) / 3
    }
}
