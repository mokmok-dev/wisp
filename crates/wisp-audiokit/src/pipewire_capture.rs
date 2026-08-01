//! `PipeWire` microphone and default-sink monitor capture for Linux.
//!
//! `PipeWire` performs graph-format conversion to the 16 kHz mono `f32` format
//! consumed by the shared Ogg/Opus recorder. The process callbacks only use
//! the bounded, non-blocking capture queue; disk I/O runs on a separate
//! writer thread. Processing runs on the `PipeWire` main loop rather than its
//! real-time thread because converting mapped PCM into owned frames allocates.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crossbeam_channel as channel;
use pipewire as pw;
use pw::properties::properties;
use pw::spa;
use spa::param::format::{MediaSubtype, MediaType};
use spa::param::format_utils;
use spa::pod::Pod;
use wisp_core::{AudioFrame, CaptureEvent, MonotonicTimestamp, SourceKind, TrackId};

use crate::backend::{
    Availability, BackendError, BackendErrorKind, BackendId, BackendResult, CaptureBackend,
    CaptureCapabilities, CaptureControlEvent, CaptureEventReceiver, CaptureProbe,
    RealtimeCaptureSender, ShutdownMode, realtime_capture_channel,
};
use crate::ogg_opus_recorder::OggOpusRecorder;
use crate::{Result, SessionError};

/// PCM sample rate delivered to the recording queue.
pub const PIPEWIRE_SAMPLE_RATE: u32 = 16_000;
/// PCM channel count delivered to the recording queue.
pub const PIPEWIRE_CHANNELS: u16 = 1;
/// Maximum number of PCM chunks waiting for the recording consumer.
pub const PIPEWIRE_CAPTURE_QUEUE_CAPACITY: usize = 256;

const START_TIMEOUT: Duration = Duration::from_secs(5);
const STOP_POLL_INTERVAL: Duration = Duration::from_millis(20);
const NOTIFICATION_QUEUE_CAPACITY: usize = 16;
const MAX_CAPTURE_FRAMES: usize = PIPEWIRE_SAMPLE_RATE as usize;
const MAX_CAPTURE_BYTES: usize = MAX_CAPTURE_FRAMES * std::mem::size_of::<f32>();
const SILENCE_CHUNK_FRAMES: usize = 1_600;
const MAX_TIMESTAMP_GAP_FRAMES: u64 = 80_000;
const MAX_OVERFLOW_COMPENSATION_FRAMES: u64 = 80_000;
const DISCONTINUITY_THRESHOLD_FRAMES: u64 = 1_280;

struct CaptureWorker {
    stop_requested: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

impl CaptureWorker {
    fn stop(&mut self) {
        self.stop_requested.store(true, Ordering::Release);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

enum Startup {
    Ready(TrackId),
    Failed(TrackId, String),
}

struct StreamData {
    track_id: TrackId,
    source: SourceKind,
    sender: RealtimeCaptureSender,
    startup: channel::Sender<Startup>,
    format: spa::param::audio::AudioInfoRaw,
    sequence: u64,
    next_timestamp_frame: Option<u64>,
    capture_origin: Instant,
    microphone_muted: Arc<AtomicBool>,
    startup_reported: bool,
    streaming: bool,
    format_valid: bool,
}

/// Owns `PipeWire` capture of the default microphone and, when the graph
/// exposes one, the default sink monitor.
///
/// Microphone startup is required. Sink-monitor startup is best-effort:
/// systems without a default sink still produce a valid empty `system.ogg`.
pub struct PipewireCapture {
    receiver: Option<CaptureEventReceiver>,
    microphone_muted: Arc<AtomicBool>,
    workers: Vec<CaptureWorker>,
    startup_warnings: Vec<String>,
}

impl PipewireCapture {
    /// Start `PipeWire` microphone and sink-monitor capture.
    ///
    /// # Errors
    /// Returns [`SessionError::Start`] when the `PipeWire` daemon or default
    /// microphone cannot be opened within five seconds.
    pub fn start() -> Result<Self> {
        let tracks = [TrackId::MICROPHONE, TrackId::SYSTEM];
        let (senders, receiver) =
            realtime_capture_channel(PIPEWIRE_CAPTURE_QUEUE_CAPACITY, &tracks);
        let microphone_muted = Arc::new(AtomicBool::new(false));
        let (startup_sender, startup_receiver) = channel::bounded(2);
        let mut workers = Vec::with_capacity(2);
        let capture_origin = Instant::now();

        for ((track_id, sink_monitor), sender) in
            [(TrackId::MICROPHONE, false), (TrackId::SYSTEM, true)]
                .into_iter()
                .zip(senders)
        {
            let worker_stop = Arc::new(AtomicBool::new(false));
            let stop_requested = Arc::clone(&worker_stop);
            let worker_startup = startup_sender.clone();
            let worker_muted = Arc::clone(&microphone_muted);
            let name = if sink_monitor {
                "wisp-pipewire-sink-monitor"
            } else {
                "wisp-pipewire-microphone"
            };
            let handle = std::thread::Builder::new()
                .name(name.into())
                .spawn(move || {
                    capture_worker(
                        track_id,
                        sink_monitor,
                        sender,
                        worker_startup,
                        stop_requested,
                        worker_muted,
                        capture_origin,
                    );
                })
                .map_err(|error| {
                    stop_workers(&mut workers);
                    SessionError::Start(format!("failed to spawn {name}: {error}"))
                })?;
            workers.push(CaptureWorker {
                stop_requested: worker_stop,
                handle: Some(handle),
            });
        }
        drop(startup_sender);

        let deadline = Instant::now() + START_TIMEOUT;
        let mut microphone_ready = false;
        let mut system_finished_startup = false;
        let mut startup_warnings = Vec::new();
        while !microphone_ready || !system_finished_startup {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                if !microphone_ready {
                    stop_workers(&mut workers);
                    return Err(SessionError::Start(
                        "timed out starting PipeWire microphone capture".into(),
                    ));
                }
                startup_warnings
                    .push("PipeWire sink monitor did not become ready before timeout".into());
                workers[1].stop();
                break;
            }
            match startup_receiver.recv_timeout(remaining) {
                Ok(Startup::Ready(TrackId::MICROPHONE)) => microphone_ready = true,
                Ok(Startup::Ready(TrackId::SYSTEM)) => system_finished_startup = true,
                Ok(Startup::Failed(TrackId::MICROPHONE, message)) => {
                    stop_workers(&mut workers);
                    return Err(SessionError::Start(format!(
                        "PipeWire microphone capture failed: {message}"
                    )));
                },
                Ok(Startup::Failed(TrackId::SYSTEM, message)) => {
                    system_finished_startup = true;
                    startup_warnings
                        .push(format!("PipeWire sink monitor is unavailable: {message}"));
                },
                Ok(Startup::Ready(_))
                | Ok(Startup::Failed(_, _))
                | Err(channel::RecvTimeoutError::Timeout) => {},
                Err(channel::RecvTimeoutError::Disconnected) => {
                    stop_workers(&mut workers);
                    return Err(SessionError::Start(
                        "PipeWire workers exited during startup".into(),
                    ));
                },
            }
        }

        Ok(Self {
            receiver: Some(receiver),
            microphone_muted,
            workers,
            startup_warnings,
        })
    }

    /// Replace microphone packets with silence while preserving timing.
    pub fn set_microphone_muted(
        &self,
        muted: bool,
    ) {
        self.microphone_muted.store(muted, Ordering::Release);
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

    /// Warnings produced while probing optional capture tracks.
    #[must_use]
    pub fn startup_warnings(&self) -> &[String] {
        &self.startup_warnings
    }

    /// Stop and join every `PipeWire` loop.
    pub fn stop(&mut self) {
        stop_workers(&mut self.workers);
    }
}

impl Drop for PipewireCapture {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Backend-neutral, one-shot adapter around [`PipewireCapture`].
///
/// Construction and [`CaptureBackend::probe`] do not access live audio
/// hardware, making capability selection safe in headless environments.
pub struct PipewireCaptureBackend {
    capture: Option<PipewireCapture>,
    started: bool,
}

impl PipewireCaptureBackend {
    #[must_use]
    pub const fn new() -> Self {
        Self {
            capture: None,
            started: false,
        }
    }
}

impl Default for PipewireCaptureBackend {
    fn default() -> Self {
        Self::new()
    }
}

impl CaptureBackend for PipewireCaptureBackend {
    fn probe(&self) -> CaptureProbe {
        CaptureProbe {
            backend_id: BackendId::new("pipewire"),
            availability: Availability::Available,
            capabilities: CaptureCapabilities {
                tracks: vec![
                    wisp_core::TrackDescriptor {
                        id: TrackId::MICROPHONE,
                        source: SourceKind::Microphone,
                        name: "Default microphone".into(),
                    },
                    wisp_core::TrackDescriptor {
                        id: TrackId::SYSTEM,
                        source: SourceKind::SystemAudio,
                        name: "Default sink monitor (optional)".into(),
                    },
                ],
                simultaneous_tracks: true,
                monotonic_timestamps: true,
                device_change_notifications: false,
            },
        }
    }

    fn start(&mut self) -> BackendResult<()> {
        if self.started {
            return Err(pipewire_backend_error(
                BackendErrorKind::InvalidState,
                "PipeWire capture backend is one-shot",
            ));
        }
        self.started = true;
        self.capture = Some(PipewireCapture::start().map_err(|error| {
            pipewire_backend_error(BackendErrorKind::DeviceUnavailable, error.to_string())
        })?);
        Ok(())
    }

    fn next_event(
        &mut self,
        timeout: Duration,
    ) -> BackendResult<Option<CaptureEvent>> {
        let Some(capture) = &self.capture else {
            return if self.started {
                Ok(None)
            } else {
                Err(pipewire_backend_error(
                    BackendErrorKind::InvalidState,
                    "PipeWire capture backend has not started",
                ))
            };
        };
        Ok(capture.recv_capture_event_timeout(timeout))
    }

    fn stop(
        &mut self,
        mode: ShutdownMode,
    ) -> BackendResult<()> {
        let Some(capture) = &mut self.capture else {
            return Ok(());
        };
        capture.stop();
        if mode == ShutdownMode::Abort {
            self.capture = None;
        }
        Ok(())
    }
}

enum WriterState {
    Active(JoinHandle<io::Result<()>>),
    Complete,
    Failed(SessionError),
}

/// Records `PipeWire` microphone and sink-monitor streams into `mic.ogg` and
/// `system.ogg`.
pub struct PipewireRecording {
    capture: Mutex<PipewireCapture>,
    writer: Mutex<WriterState>,
    notifications: channel::Receiver<String>,
    fatal_notifications: channel::Receiver<String>,
    finished: Arc<AtomicBool>,
    mic_path: PathBuf,
    system_path: PathBuf,
}

impl PipewireRecording {
    /// Start capture and create both Ogg/Opus output streams.
    ///
    /// # Errors
    /// Returns [`SessionError::Start`] for capture, file, or writer failures.
    pub fn start(output_dir: impl AsRef<Path>) -> Result<Self> {
        let output_dir = output_dir.as_ref();
        fs::create_dir_all(output_dir).map_err(|error| {
            SessionError::Start(format!(
                "failed to create recording directory {}: {error}",
                output_dir.display()
            ))
        })?;
        let mic_path = output_dir.join("mic.ogg");
        let system_path = output_dir.join("system.ogg");
        let mut capture = PipewireCapture::start()?;
        let receiver = capture.receiver.take().ok_or_else(|| {
            SessionError::Start("PipeWire capture receiver was already consumed".into())
        })?;
        let (notification_sender, notifications) = channel::bounded(NOTIFICATION_QUEUE_CAPACITY);
        let (fatal_sender, fatal_notifications) = channel::bounded(1);
        for warning in capture.startup_warnings() {
            let _ = notification_sender.try_send(warning.clone());
        }
        let stop_requests = capture
            .workers
            .iter()
            .map(|worker| Arc::clone(&worker.stop_requested))
            .collect::<Vec<_>>();
        let finished = Arc::new(AtomicBool::new(false));
        let writer_finished = Arc::clone(&finished);
        let (writer_start_sender, writer_start_receiver) = channel::bounded(1);
        let writer = std::thread::Builder::new()
            .name("wisp-linux-ogg-opus-writer".into())
            .spawn(move || {
                let result = writer_start_receiver
                    .recv()
                    .map_err(|_| io::Error::other("Linux Ogg writer startup was cancelled"))
                    .and_then(
                        |(receiver, mic, system): (
                            CaptureEventReceiver,
                            OggOpusRecorder,
                            OggOpusRecorder,
                        )| {
                            recording_loop(&receiver, mic, system, &notification_sender)
                        },
                    );
                if let Err(error) = &result {
                    for stop_requested in &stop_requests {
                        stop_requested.store(true, Ordering::Release);
                    }
                    let _ = fatal_sender.try_send(format!(
                        "fatal Linux Ogg writer failure; capture is stopping: {error}"
                    ));
                }
                writer_finished.store(true, Ordering::Release);
                result
            })
            .map_err(|error| {
                capture.stop();
                SessionError::Start(format!("failed to spawn Ogg/Opus writer: {error}"))
            })?;
        let mic = match OggOpusRecorder::create(&mic_path) {
            Ok(mic) => mic,
            Err(error) => {
                capture.stop();
                drop(writer_start_sender);
                let _ = writer.join();
                return Err(recording_start_error(&mic_path, &error));
            },
        };
        let system = match OggOpusRecorder::create(&system_path) {
            Ok(system) => system,
            Err(error) => {
                capture.stop();
                drop(mic);
                drop(writer_start_sender);
                let _ = writer.join();
                return Err(recording_start_error(&system_path, &error));
            },
        };
        if writer_start_sender.send((receiver, mic, system)).is_err() {
            capture.stop();
            let _ = writer.join();
            return Err(SessionError::Start(
                "Linux Ogg writer exited before recording started".into(),
            ));
        }

        Ok(Self {
            capture: Mutex::new(capture),
            writer: Mutex::new(WriterState::Active(writer)),
            notifications,
            fatal_notifications,
            finished,
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

    pub fn set_microphone_muted(
        &self,
        muted: bool,
    ) {
        if let Ok(capture) = self.capture.lock() {
            capture.set_microphone_muted(muted);
        }
    }

    /// Poll a recoverable capture warning (device disconnect, overflow, or
    /// unavailable sink monitor).
    #[must_use]
    pub fn try_recv_warning(&self) -> Option<String> {
        self.fatal_notifications
            .try_recv()
            .ok()
            .or_else(|| self.notifications.try_recv().ok())
    }

    /// Wait for a recoverable capture warning.
    #[must_use]
    pub fn recv_warning_timeout(
        &self,
        timeout: Duration,
    ) -> Option<String> {
        if let Ok(fatal) = self.fatal_notifications.try_recv() {
            return Some(fatal);
        }
        channel::select! {
            recv(self.fatal_notifications) -> message => message.ok(),
            recv(self.notifications) -> message => message.ok(),
            default(timeout) => None,
        }
    }

    /// Whether the writer has exited. Queued notifications may still be read.
    #[must_use]
    pub fn is_finished(&self) -> bool {
        self.finished.load(Ordering::Acquire)
    }

    /// Stop producers, drain the bounded queue, finalize both Ogg streams, and
    /// join the writer. This operation is idempotent.
    ///
    /// # Errors
    /// Returns a finalization or writer-thread error.
    pub fn stop(&self) -> Result<()> {
        let mut capture = self
            .capture
            .lock()
            .map_err(|_| SessionError::Start("PipeWire capture lock is poisoned".into()))?;
        capture.stop();
        drop(capture);

        let mut writer = self
            .writer
            .lock()
            .map_err(|_| SessionError::Start("Ogg writer lock is poisoned".into()))?;
        let state = std::mem::replace(&mut *writer, WriterState::Complete);
        let result = match state {
            WriterState::Active(handle) => match handle.join() {
                Ok(result) => result.map_err(|error| {
                    SessionError::Start(format!("failed to finalize Linux Ogg files: {error}"))
                }),
                Err(_) => Err(SessionError::Start(
                    "Linux Ogg writer thread panicked".into(),
                )),
            },
            WriterState::Complete => Ok(()),
            WriterState::Failed(error) => Err(error),
        };
        if let Err(error) = &result {
            *writer = WriterState::Failed(error.clone());
        }
        result
    }
}

impl Drop for PipewireRecording {
    fn drop(&mut self) {
        let _ = self.stop();
    }
}

fn capture_worker(
    track_id: TrackId,
    sink_monitor: bool,
    sender: RealtimeCaptureSender,
    startup: channel::Sender<Startup>,
    stop_requested: Arc<AtomicBool>,
    microphone_muted: Arc<AtomicBool>,
    capture_origin: Instant,
) {
    if let Err(error) = run_capture_worker(
        track_id,
        sink_monitor,
        sender,
        &startup,
        stop_requested,
        microphone_muted,
        capture_origin,
    ) {
        let _ = startup.try_send(Startup::Failed(track_id, error));
    }
}

#[allow(clippy::too_many_lines)]
fn run_capture_worker(
    track_id: TrackId,
    sink_monitor: bool,
    sender: RealtimeCaptureSender,
    startup: &channel::Sender<Startup>,
    stop_requested: Arc<AtomicBool>,
    microphone_muted: Arc<AtomicBool>,
    capture_origin: Instant,
) -> std::result::Result<(), String> {
    let mainloop = pw::main_loop::MainLoopRc::new(None).map_err(|error| error.to_string())?;
    let context =
        pw::context::ContextRc::new(&mainloop, None).map_err(|error| error.to_string())?;
    let core = context
        .connect_rc(None)
        .map_err(|error| error.to_string())?;
    let mut props = properties! {
        *pw::keys::MEDIA_TYPE => "Audio",
        *pw::keys::MEDIA_CATEGORY => "Capture",
        *pw::keys::MEDIA_ROLE => "Communication",
        *pw::keys::APP_NAME => "Wisp",
    };
    if sink_monitor {
        props.insert(*pw::keys::STREAM_CAPTURE_SINK, "true");
    }
    let stream_name = if sink_monitor {
        "wisp-system-audio"
    } else {
        "wisp-microphone"
    };
    let stream =
        pw::stream::StreamBox::new(&core, stream_name, props).map_err(|error| error.to_string())?;
    let source = if sink_monitor {
        SourceKind::SystemAudio
    } else {
        SourceKind::Microphone
    };
    let data = StreamData {
        track_id,
        source,
        sender,
        startup: startup.clone(),
        format: spa::param::audio::AudioInfoRaw::new(),
        sequence: 0,
        next_timestamp_frame: None,
        capture_origin,
        microphone_muted,
        startup_reported: false,
        streaming: false,
        format_valid: false,
    };
    let quit_on_error = mainloop.clone();
    let quit_on_bad_format = mainloop.clone();
    let quit_on_bad_buffer = mainloop.clone();
    let listener = stream
        .add_local_listener_with_user_data(data)
        .state_changed(move |_, data, _, state| match state {
            pw::stream::StreamState::Streaming => {
                data.streaming = true;
                report_ready_if_valid(data);
            },
            pw::stream::StreamState::Error(message) => {
                if data.startup_reported {
                    let _ = data.sender.send_control(CaptureControlEvent::Error {
                        track_id: Some(data.track_id),
                        message,
                        recoverable: false,
                    });
                } else {
                    data.startup_reported = true;
                    let _ = data
                        .startup
                        .try_send(Startup::Failed(data.track_id, message.clone()));
                }
                quit_on_error.quit();
            },
            _ => {},
        })
        .param_changed(move |_, data, id, param| {
            if id != spa::param::ParamType::Format.as_raw() {
                return;
            }
            let valid = param.is_some_and(|param| {
                matches!(
                    format_utils::parse_format(param),
                    Ok((MediaType::Audio, MediaSubtype::Raw))
                ) && data.format.parse(param).is_ok()
                    && data.format.format() == spa::param::audio::AudioFormat::F32LE
                    && data.format.rate() == PIPEWIRE_SAMPLE_RATE
                    && data.format.channels() == u32::from(PIPEWIRE_CHANNELS)
            });
            data.format_valid = valid;
            if valid {
                report_ready_if_valid(data);
            } else {
                report_fatal_format(data);
                quit_on_bad_format.quit();
            }
        })
        .process(move |stream, data| {
            if !data.format_valid {
                return;
            }
            let Some(mut buffer) = stream.dequeue_buffer() else {
                return;
            };
            let Some(plane) = buffer.datas_mut().first_mut() else {
                return;
            };
            let offset = usize::try_from(plane.chunk().offset()).unwrap_or(usize::MAX);
            let size = usize::try_from(plane.chunk().size()).unwrap_or(0);
            if size > MAX_CAPTURE_BYTES || size % std::mem::size_of::<f32>() != 0 {
                data.format_valid = false;
                let _ = data.sender.send_control(CaptureControlEvent::Error {
                    track_id: Some(data.track_id),
                    message: format!(
                        "PipeWire supplied invalid capture buffer metadata ({size} bytes)"
                    ),
                    recoverable: false,
                });
                quit_on_bad_buffer.quit();
                return;
            }
            let Some(bytes) = plane.data() else {
                return;
            };
            let Some(end) = offset.checked_add(size) else {
                return;
            };
            let Some(bytes) = bytes.get(offset..end) else {
                return;
            };
            let mut samples = decode_f32le(bytes);
            if samples.is_empty() {
                return;
            }
            if data.track_id == TrackId::MICROPHONE && data.microphone_muted.load(Ordering::Relaxed)
            {
                samples.fill(0.0);
            }
            let observed_timestamp_frame =
                duration_to_frames(data.capture_origin.elapsed(), PIPEWIRE_SAMPLE_RATE);
            let timestamp_frame =
                reconcile_timestamp_frame(data.next_timestamp_frame, observed_timestamp_frame);
            let timestamp = frames_to_duration(timestamp_frame, PIPEWIRE_SAMPLE_RATE);
            let Ok(frame) = AudioFrame::from_f32(
                data.track_id,
                data.source.clone(),
                data.sequence,
                MonotonicTimestamp::from_duration(timestamp),
                PIPEWIRE_SAMPLE_RATE,
                PIPEWIRE_CHANNELS,
                samples,
            ) else {
                return;
            };
            data.next_timestamp_frame =
                Some(timestamp_frame.saturating_add(u64::from(frame.frame_count())));
            data.sequence = data.sequence.saturating_add(1);
            let _ = data.sender.try_send(frame);
        })
        .register()
        .map_err(|error| error.to_string())?;

    let mut audio_info = spa::param::audio::AudioInfoRaw::new();
    audio_info.set_format(spa::param::audio::AudioFormat::F32LE);
    audio_info.set_rate(PIPEWIRE_SAMPLE_RATE);
    audio_info.set_channels(u32::from(PIPEWIRE_CHANNELS));
    let object = spa::pod::Object {
        type_: spa::utils::SpaTypes::ObjectParamFormat.as_raw(),
        id: spa::param::ParamType::EnumFormat.as_raw(),
        properties: audio_info.into(),
    };
    let values = spa::pod::serialize::PodSerializer::serialize(
        io::Cursor::new(Vec::new()),
        &spa::pod::Value::Object(object),
    )
    .map_err(|error| error.to_string())?
    .0
    .into_inner();
    let param = Pod::from_bytes(&values).ok_or_else(|| "invalid PipeWire format pod".to_owned())?;
    let mut params = [param];
    stream
        .connect(
            spa::utils::Direction::Input,
            None,
            pw::stream::StreamFlags::AUTOCONNECT | pw::stream::StreamFlags::MAP_BUFFERS,
            &mut params,
        )
        .map_err(|error| error.to_string())?;

    let quit_on_stop = mainloop.clone();
    let timer_stop = stop_requested;
    let timer = mainloop.loop_().add_timer(move |_| {
        if timer_stop.load(Ordering::Acquire) {
            quit_on_stop.quit();
        }
    });
    timer
        .update_timer(Some(STOP_POLL_INTERVAL), Some(STOP_POLL_INTERVAL))
        .into_result()
        .map_err(|error| error.to_string())?;
    mainloop.run();
    drop(timer);
    drop(listener);
    Ok(())
}

fn report_ready_if_valid(data: &mut StreamData) {
    if data.streaming && data.format_valid && !data.startup_reported {
        data.startup_reported = true;
        let _ = data.startup.try_send(Startup::Ready(data.track_id));
    }
}

fn report_fatal_format(data: &mut StreamData) {
    const MESSAGE: &str = "PipeWire negotiated an unsupported audio format";
    if data.startup_reported {
        let _ = data.sender.send_control(CaptureControlEvent::Error {
            track_id: Some(data.track_id),
            message: MESSAGE.into(),
            recoverable: false,
        });
    } else {
        data.startup_reported = true;
        let _ = data
            .startup
            .try_send(Startup::Failed(data.track_id, MESSAGE.into()));
    }
}

fn recording_loop(
    receiver: &CaptureEventReceiver,
    mut mic: OggOpusRecorder,
    mut system: OggOpusRecorder,
    notifications: &channel::Sender<String>,
) -> io::Result<()> {
    let mut mic_timeline = TrackTimeline::default();
    let mut system_timeline = TrackTimeline::default();
    while let Some(event) = receiver.recv() {
        match event {
            CaptureEvent::Samples(frame) => {
                let samples = frame
                    .samples()
                    .as_f32()
                    .ok_or_else(|| io::Error::other("PipeWire produced a non-f32 capture frame"))?;
                let timestamp_frame =
                    duration_to_frames(frame.timestamp().as_duration(), PIPEWIRE_SAMPLE_RATE);
                match frame.track_id() {
                    TrackId::MICROPHONE => write_aligned_frame(
                        &mut mic,
                        &mut mic_timeline,
                        timestamp_frame,
                        samples,
                        TrackId::MICROPHONE,
                        notifications,
                    )?,
                    TrackId::SYSTEM => write_aligned_frame(
                        &mut system,
                        &mut system_timeline,
                        timestamp_frame,
                        samples,
                        TrackId::SYSTEM,
                        notifications,
                    )?,
                    _ => {},
                }
            },
            CaptureEvent::Overflow {
                track_id,
                dropped_frames,
            } => {
                if dropped_frames > MAX_OVERFLOW_COMPENSATION_FRAMES {
                    return Err(io::Error::other(format!(
                        "PipeWire reported an implausible overflow on track {}: {dropped_frames} frames",
                        track_id.get()
                    )));
                }
                match track_id {
                    TrackId::MICROPHONE => {
                        push_silence(&mut mic, dropped_frames)?;
                        mic_timeline.advance(dropped_frames);
                    },
                    TrackId::SYSTEM => {
                        push_silence(&mut system, dropped_frames)?;
                        system_timeline.advance(dropped_frames);
                    },
                    _ => {},
                }
                let _ = notifications.try_send(format!(
                    "PipeWire capture overflow on track {}: dropped {dropped_frames} frames",
                    track_id.get()
                ));
            },
            CaptureEvent::Error {
                track_id,
                message,
                recoverable,
            } => {
                if !recoverable && (track_id.is_none() || track_id == Some(TrackId::MICROPHONE)) {
                    return Err(io::Error::other(format!(
                        "required PipeWire microphone capture failed: {message}"
                    )));
                }
                let _ = notifications.try_send(format!(
                    "PipeWire track {} disconnected (recoverable={recoverable}): {message}",
                    track_id.map_or(0, TrackId::get)
                ));
            },
            _ => {},
        }
    }
    let final_cursor = mic_timeline.encoded.max(system_timeline.encoded);
    push_silence(&mut mic, final_cursor.saturating_sub(mic_timeline.encoded))?;
    push_silence(
        &mut system,
        final_cursor.saturating_sub(system_timeline.encoded),
    )?;
    mic.finish()?;
    system.finish()
}

#[derive(Default)]
struct TrackTimeline {
    logical: u64,
    encoded: u64,
}

impl TrackTimeline {
    fn advance(
        &mut self,
        frames: u64,
    ) {
        self.logical = self.logical.saturating_add(frames);
        self.encoded = self.encoded.saturating_add(frames);
    }
}

fn write_aligned_frame(
    recorder: &mut OggOpusRecorder,
    timeline: &mut TrackTimeline,
    timestamp_frame: u64,
    samples: &[f32],
    track_id: TrackId,
    notifications: &channel::Sender<String>,
) -> io::Result<()> {
    let gap = timestamp_frame.saturating_sub(timeline.logical);
    let bounded_gap = gap.min(MAX_TIMESTAMP_GAP_FRAMES);
    if gap > MAX_TIMESTAMP_GAP_FRAMES {
        let _ = notifications.try_send(format!(
            "PipeWire timestamp gap on track {} was clamped from {gap} to {bounded_gap} frames",
            track_id.get()
        ));
    }
    push_silence(recorder, bounded_gap)?;
    recorder.push(samples)?;
    let sample_count = u64::try_from(samples.len()).map_err(io::Error::other)?;
    timeline.logical = timeline
        .logical
        .max(timestamp_frame)
        .saturating_add(sample_count);
    timeline.encoded = timeline
        .encoded
        .saturating_add(bounded_gap)
        .saturating_add(sample_count);
    Ok(())
}

fn push_silence(
    recorder: &mut OggOpusRecorder,
    mut frames: u64,
) -> io::Result<()> {
    const SILENCE: [f32; SILENCE_CHUNK_FRAMES] = [0.0; SILENCE_CHUNK_FRAMES];
    while frames > 0 {
        let chunk =
            usize::try_from(frames.min(SILENCE_CHUNK_FRAMES as u64)).map_err(io::Error::other)?;
        recorder.push(&SILENCE[..chunk])?;
        frames -= u64::try_from(chunk).map_err(io::Error::other)?;
    }
    Ok(())
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

fn pipewire_backend_error(
    kind: BackendErrorKind,
    message: impl Into<String>,
) -> BackendError {
    BackendError::new(BackendId::new("pipewire"), kind, message)
}

fn decode_f32le(bytes: &[u8]) -> Vec<f32> {
    bytes
        .chunks_exact(std::mem::size_of::<f32>())
        .map(|sample| f32::from_le_bytes([sample[0], sample[1], sample[2], sample[3]]))
        .collect()
}

fn reconcile_timestamp_frame(
    expected: Option<u64>,
    observed: u64,
) -> u64 {
    expected.map_or(observed, |expected| {
        if observed > expected.saturating_add(DISCONTINUITY_THRESHOLD_FRAMES) {
            observed
        } else {
            expected
        }
    })
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
    duration
        .as_secs()
        .saturating_mul(u64::from(sample_rate))
        .saturating_add(
            u64::from(duration.subsec_nanos()).saturating_mul(u64::from(sample_rate))
                / 1_000_000_000,
        )
}

fn stop_workers(workers: &mut [CaptureWorker]) {
    for worker in workers.iter_mut() {
        worker.stop_requested.store(true, Ordering::Release);
    }
    for worker in workers.iter_mut() {
        if let Some(handle) = worker.handle.take() {
            let _ = handle.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use std::fs::File;
    use std::time::Duration;

    use ogg::PacketReader;
    use ropus::{Channels, DecodeMode, Decoder};
    use wisp_core::{AudioFrame, MonotonicTimestamp};

    use crate::backend::{CaptureControlEvent, realtime_capture_channel};
    use crate::ogg_opus_recorder::OggOpusRecorder;
    use crate::{CaptureBackend, SourceKind, TrackId};

    use super::{
        MAX_OVERFLOW_COMPENSATION_FRAMES, PIPEWIRE_SAMPLE_RATE, PipewireCaptureBackend,
        decode_f32le, frames_to_duration, reconcile_timestamp_frame, recording_loop,
    };

    #[test]
    fn decodes_complete_little_endian_f32_samples() {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&0.25_f32.to_le_bytes());
        bytes.extend_from_slice(&(-0.5_f32).to_le_bytes());
        bytes.extend_from_slice(&[1, 2]);

        assert_eq!(decode_f32le(&bytes), [0.25, -0.5]);
    }

    #[test]
    fn capture_timestamp_uses_negotiated_frame_clock() {
        assert_eq!(
            frames_to_duration(24_000, 16_000),
            Duration::from_millis(1_500)
        );
    }

    #[test]
    fn timestamp_reconciliation_preserves_real_stream_pauses() {
        assert_eq!(reconcile_timestamp_frame(None, 500), 500);
        assert_eq!(reconcile_timestamp_frame(Some(1_000), 1_100), 1_000);
        assert_eq!(reconcile_timestamp_frame(Some(1_000), 4_000), 4_000);
    }

    #[test]
    fn probe_is_hardware_free_and_advertises_separate_tracks() {
        let probe = PipewireCaptureBackend::new().probe();

        assert!(probe.availability.is_available());
        assert_eq!(probe.capabilities.tracks.len(), 2);
        assert_eq!(probe.capabilities.tracks[0].id, TrackId::MICROPHONE);
        assert_eq!(probe.capabilities.tracks[1].source, SourceKind::SystemAudio);
    }

    #[test]
    fn recording_loop_routes_decodable_tracks_and_aligns_them() {
        let directory = tempfile::tempdir().unwrap();
        let mic_path = directory.path().join("mic.ogg");
        let system_path = directory.path().join("system.ogg");
        let mic = OggOpusRecorder::create(&mic_path).unwrap();
        let system = OggOpusRecorder::create(&system_path).unwrap();
        let (senders, receiver) =
            realtime_capture_channel(8, &[TrackId::MICROPHONE, TrackId::SYSTEM]);
        senders[0]
            .try_send(make_frame(TrackId::MICROPHONE, 100, vec![0.8; 320]))
            .unwrap();
        senders[1]
            .try_send(make_frame(TrackId::SYSTEM, 200, vec![-0.8; 160]))
            .unwrap();
        drop(senders);
        let (notification_sender, _) = crossbeam_channel::bounded(8);

        recording_loop(&receiver, mic, system, &notification_sender).unwrap();

        let mic_pcm = decode_ogg(&mic_path);
        let system_pcm = decode_ogg(&system_path);
        assert_eq!(mic_pcm.len(), 420 * 3);
        assert_eq!(system_pcm.len(), mic_pcm.len());
        assert!(mean(&mic_pcm[450..900]) > 0.2);
        assert!(mean(&system_pcm[700..1_000]) < -0.2);
    }

    #[test]
    fn recording_loop_finalizes_two_valid_empty_streams() {
        let directory = tempfile::tempdir().unwrap();
        let mic_path = directory.path().join("mic.ogg");
        let system_path = directory.path().join("system.ogg");
        let mic = OggOpusRecorder::create(&mic_path).unwrap();
        let system = OggOpusRecorder::create(&system_path).unwrap();
        let (senders, receiver) =
            realtime_capture_channel(2, &[TrackId::MICROPHONE, TrackId::SYSTEM]);
        drop(senders);
        let (notification_sender, _) = crossbeam_channel::bounded(2);

        recording_loop(&receiver, mic, system, &notification_sender).unwrap();

        assert!(decode_ogg(&mic_path).is_empty());
        assert!(decode_ogg(&system_path).is_empty());
    }

    #[test]
    fn recording_loop_inserts_dropped_pcm_and_delivers_warning() {
        let directory = tempfile::tempdir().unwrap();
        let mic_path = directory.path().join("mic.ogg");
        let system_path = directory.path().join("system.ogg");
        let mic = OggOpusRecorder::create(&mic_path).unwrap();
        let system = OggOpusRecorder::create(&system_path).unwrap();
        let (senders, receiver) =
            realtime_capture_channel(1, &[TrackId::MICROPHONE, TrackId::SYSTEM]);
        senders[0]
            .try_send(make_frame(TrackId::MICROPHONE, 0, vec![0.5; 320]))
            .unwrap();
        senders[0]
            .try_send(make_frame(TrackId::MICROPHONE, 320, vec![0.5; 320]))
            .unwrap();
        drop(senders);
        let (notification_sender, notifications) = crossbeam_channel::bounded(2);

        recording_loop(&receiver, mic, system, &notification_sender).unwrap();

        let mic_pcm = decode_ogg(&mic_path);
        assert_eq!(mic_pcm.len(), 640 * 3);
        assert!(mean_abs(&mic_pcm[200..700]) > 0.1);
        assert!(mean_abs(&mic_pcm[1_300..1_800]) < 0.1);
        assert!(notifications.recv().unwrap().contains("dropped 320 frames"));
    }

    #[test]
    fn recording_loop_rejects_implausibly_large_overflow() {
        let directory = tempfile::tempdir().unwrap();
        let mic = OggOpusRecorder::create(&directory.path().join("mic.ogg")).unwrap();
        let system = OggOpusRecorder::create(&directory.path().join("system.ogg")).unwrap();
        let (senders, receiver) =
            realtime_capture_channel(1, &[TrackId::MICROPHONE, TrackId::SYSTEM]);
        senders[0]
            .report_dropped_frames(MAX_OVERFLOW_COMPENSATION_FRAMES + 1)
            .unwrap();
        drop(senders);
        let (notification_sender, _) = crossbeam_channel::bounded(1);

        let error = recording_loop(&receiver, mic, system, &notification_sender).unwrap_err();

        assert!(error.to_string().contains("implausible overflow"));
    }

    #[test]
    fn recording_loop_fails_on_required_microphone_error() {
        let directory = tempfile::tempdir().unwrap();
        let mic = OggOpusRecorder::create(&directory.path().join("mic.ogg")).unwrap();
        let system = OggOpusRecorder::create(&directory.path().join("system.ogg")).unwrap();
        let (senders, receiver) =
            realtime_capture_channel(1, &[TrackId::MICROPHONE, TrackId::SYSTEM]);
        senders[0]
            .send_control(CaptureControlEvent::Error {
                track_id: Some(TrackId::MICROPHONE),
                message: "device disappeared".into(),
                recoverable: false,
            })
            .unwrap();
        drop(senders);
        let (notification_sender, _) = crossbeam_channel::bounded(1);

        let error = recording_loop(&receiver, mic, system, &notification_sender).unwrap_err();

        assert!(error.to_string().contains("required PipeWire microphone"));
    }

    #[test]
    fn recording_loop_treats_optional_system_error_as_warning() {
        let directory = tempfile::tempdir().unwrap();
        let mic = OggOpusRecorder::create(&directory.path().join("mic.ogg")).unwrap();
        let system = OggOpusRecorder::create(&directory.path().join("system.ogg")).unwrap();
        let (senders, receiver) =
            realtime_capture_channel(1, &[TrackId::MICROPHONE, TrackId::SYSTEM]);
        senders[1]
            .send_control(CaptureControlEvent::Error {
                track_id: Some(TrackId::SYSTEM),
                message: "monitor disappeared".into(),
                recoverable: false,
            })
            .unwrap();
        drop(senders);
        let (notification_sender, notifications) = crossbeam_channel::bounded(1);

        recording_loop(&receiver, mic, system, &notification_sender).unwrap();

        assert!(
            notifications
                .recv()
                .unwrap()
                .contains("monitor disappeared")
        );
    }

    /// Opt-in smoke test for Linux CI jobs that provide a `PipeWire` virtual
    /// microphone (and optionally a default sink monitor).
    #[test]
    #[ignore = "requires a running PipeWire graph with a default microphone"]
    fn pipewire_virtual_node_integration() {
        let directory = tempfile::tempdir().unwrap();
        let recording = super::PipewireRecording::start(directory.path()).unwrap();
        std::thread::sleep(Duration::from_millis(250));
        recording.stop().unwrap();

        // Full parsing/decoding catches truncated headers, missing EOS pages,
        // and invalid Opus packets even when the virtual nodes emit silence.
        let _mic = decode_ogg(recording.mic_path());
        let _system = decode_ogg(recording.system_path());
    }

    fn make_frame(
        track_id: TrackId,
        timestamp_frame: u64,
        samples: Vec<f32>,
    ) -> AudioFrame {
        let source = if track_id == TrackId::MICROPHONE {
            SourceKind::Microphone
        } else {
            SourceKind::SystemAudio
        };
        AudioFrame::from_f32(
            track_id,
            source,
            0,
            MonotonicTimestamp::from_duration(frames_to_duration(
                timestamp_frame,
                PIPEWIRE_SAMPLE_RATE,
            )),
            PIPEWIRE_SAMPLE_RATE,
            1,
            samples,
        )
        .unwrap()
    }

    fn decode_ogg(path: &std::path::Path) -> Vec<f32> {
        let mut reader = PacketReader::new(File::open(path).unwrap());
        let head = reader.read_packet().unwrap().unwrap();
        let pre_skip = usize::from(u16::from_le_bytes([head.data[10], head.data[11]]));
        let _tags = reader.read_packet().unwrap().unwrap();
        let mut decoder = Decoder::new(48_000, Channels::Mono).unwrap();
        let mut samples = Vec::new();
        let mut final_granule = 0_u64;
        while let Some(packet) = reader.read_packet().unwrap() {
            let mut packet_pcm = vec![0.0; 5_760];
            let count = decoder
                .decode_float(&packet.data, &mut packet_pcm, DecodeMode::Normal)
                .unwrap();
            samples.extend_from_slice(&packet_pcm[..count]);
            if packet.last_in_stream() {
                final_granule = packet.absgp_page();
            }
        }
        let audio_samples = usize::try_from(final_granule).unwrap() - pre_skip;
        samples.drain(..pre_skip);
        samples.truncate(audio_samples);
        samples
    }

    fn mean(samples: &[f32]) -> f32 {
        let sample_count = f32::from(u16::try_from(samples.len()).unwrap());
        samples.iter().sum::<f32>() / sample_count
    }

    fn mean_abs(samples: &[f32]) -> f32 {
        let sample_count = f32::from(u16::try_from(samples.len()).unwrap());
        samples.iter().map(|sample| sample.abs()).sum::<f32>() / sample_count
    }
}
