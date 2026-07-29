//! Shared-mode WASAPI microphone and system-loopback capture.
//!
//! Both endpoints are converted by the Windows audio engine to the format
//! expected by Whisper-family models: 16 kHz, mono, `f32` PCM. COM objects
//! stay on the worker thread that created them.

use std::collections::VecDeque;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};
use std::thread::JoinHandle;
use std::time::Duration;

use crossbeam_channel as channel;
use wasapi::{DeviceEnumerator, Direction, SampleType, StreamMode, WaveFormat};
use wisp_core::SourceLabel;

use crate::ogg_opus_recorder::OggOpusRecorder;
use crate::{Result, SessionError};

/// PCM sample rate produced by [`WasapiCapture`].
pub const WASAPI_SAMPLE_RATE: u32 = 16_000;
/// PCM channel count produced by [`WasapiCapture`].
pub const WASAPI_CHANNELS: u16 = 1;

const EVENT_WAIT_MILLIS: u32 = 100;
const START_TIMEOUT: Duration = Duration::from_secs(5);

/// One packet of normalized PCM from a WASAPI endpoint.
#[derive(Debug, Clone, PartialEq)]
pub struct WasapiPcmChunk {
    pub source: SourceLabel,
    pub samples: Vec<f32>,
}

/// Output produced by a running [`WasapiCapture`].
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
    receiver: channel::Receiver<WasapiCaptureEvent>,
    stop_requested: Arc<AtomicBool>,
    microphone_muted: Arc<AtomicBool>,
    workers: Vec<JoinHandle<()>>,
}

/// Records simultaneous WASAPI microphone and system-loopback streams into
/// `mic.ogg` and `system.ogg`.
pub struct WasapiRecording {
    capture: Mutex<WasapiCapture>,
    writer: Mutex<Option<JoinHandle<io::Result<()>>>>,
    error_receiver: channel::Receiver<String>,
    mic_path: PathBuf,
    system_path: PathBuf,
}

impl WasapiCapture {
    /// Start the default microphone and default-render loopback streams.
    ///
    /// # Errors
    /// Returns [`SessionError::Start`] if either endpoint cannot be opened
    /// within five seconds. If one endpoint has already started, it is
    /// stopped and joined before the error is returned.
    pub fn start() -> Result<Self> {
        let (event_sender, receiver) = channel::unbounded();
        let (startup_sender, startup_receiver) = channel::bounded(2);
        let stop_requested = Arc::new(AtomicBool::new(false));
        let microphone_muted = Arc::new(AtomicBool::new(false));
        let mut workers = Vec::with_capacity(2);

        for source in [SourceLabel::Mic, SourceLabel::System] {
            let worker_events = event_sender.clone();
            let worker_startup = startup_sender.clone();
            let worker_stop = stop_requested.clone();
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
                        &worker_stop,
                        &worker_muted,
                    );
                })
                .map_err(|err| {
                    stop_requested.store(true, Ordering::SeqCst);
                    join_workers(&mut workers);
                    SessionError::Start(format!("failed to spawn {name}: {err}"))
                })?;
            workers.push(worker);
        }
        drop(startup_sender);
        drop(event_sender);

        let mut started = Vec::with_capacity(2);
        while started.len() < 2 {
            match startup_receiver.recv_timeout(START_TIMEOUT) {
                Ok(Startup::Ready(source)) => started.push(source),
                Ok(Startup::Failed { source, message }) => {
                    stop_requested.store(true, Ordering::SeqCst);
                    join_workers(&mut workers);
                    return Err(SessionError::Start(format!(
                        "{} WASAPI capture failed: {message}",
                        source_name(source)
                    )));
                },
                Err(channel::RecvTimeoutError::Timeout) => {
                    stop_requested.store(true, Ordering::SeqCst);
                    join_workers(&mut workers);
                    return Err(SessionError::Start(format!(
                        "timed out starting WASAPI capture (ready: {})",
                        started
                            .iter()
                            .map(|source| source_name(*source))
                            .collect::<Vec<_>>()
                            .join(", ")
                    )));
                },
                Err(channel::RecvTimeoutError::Disconnected) => {
                    stop_requested.store(true, Ordering::SeqCst);
                    join_workers(&mut workers);
                    return Err(SessionError::Start(
                        "WASAPI workers exited during startup".into(),
                    ));
                },
            }
        }

        Ok(Self {
            receiver,
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
        self.receiver.try_recv().ok()
    }

    /// Wait for a PCM packet, runtime error, or timeout.
    #[must_use]
    pub fn recv_timeout(
        &self,
        timeout: Duration,
    ) -> Option<WasapiCaptureEvent> {
        self.receiver.recv_timeout(timeout).ok()
    }

    /// Stop both streams and wait for their COM workers to exit.
    pub fn stop(&mut self) {
        self.stop_requested.store(true, Ordering::SeqCst);
        join_workers(&mut self.workers);
    }
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

        let receiver = capture.receiver.clone();
        let worker_stop = capture.stop_requested.clone();
        let (error_sender, error_receiver) = channel::unbounded();
        let writer = std::thread::Builder::new()
            .name("wisp-ogg-opus-writer".into())
            .spawn(move || {
                let result = recording_loop(&receiver, mic_recorder, system_recorder);
                if let Err(err) = &result {
                    worker_stop.store(true, Ordering::SeqCst);
                    let _ = error_sender.send(err.to_string());
                }
                result
            })
            .map_err(|err| {
                capture.stop();
                SessionError::Start(format!("failed to spawn Ogg/Opus writer: {err}"))
            })?;

        Ok(Self {
            capture: Mutex::new(capture),
            writer: Mutex::new(Some(writer)),
            error_receiver,
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
        self.error_receiver.try_recv().ok()
    }

    #[must_use]
    pub fn recv_error_timeout(
        &self,
        timeout: Duration,
    ) -> Option<String> {
        self.error_receiver.recv_timeout(timeout).ok()
    }

    pub(crate) fn error_receiver(&self) -> &channel::Receiver<String> {
        &self.error_receiver
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
        let Some(writer) = writer.take() else {
            return Ok(());
        };
        writer
            .join()
            .map_err(|_| SessionError::Start("Ogg writer thread panicked".into()))?
            .map_err(|err| SessionError::Start(format!("failed to finalize Ogg files: {err}")))
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

fn recording_loop(
    receiver: &channel::Receiver<WasapiCaptureEvent>,
    mut mic: OggOpusRecorder,
    mut system: OggOpusRecorder,
) -> io::Result<()> {
    while let Ok(event) = receiver.recv() {
        match event {
            WasapiCaptureEvent::Samples(chunk) => match chunk.source {
                SourceLabel::Mic => mic.push(&chunk.samples)?,
                SourceLabel::System => system.push(&chunk.samples)?,
            },
            WasapiCaptureEvent::Error { source, message } => {
                return Err(io::Error::other(format!(
                    "{} WASAPI stream failed: {message}",
                    source_name(source)
                )));
            },
        }
    }
    mic.finish()?;
    system.finish()
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
    event_sender: &channel::Sender<WasapiCaptureEvent>,
    startup_sender: &channel::Sender<Startup>,
    stop_requested: &AtomicBool,
    microphone_muted: &AtomicBool,
) {
    let result = run_capture(
        source,
        event_sender,
        startup_sender,
        stop_requested,
        microphone_muted,
    );
    if let Err(message) = result {
        let startup_receiver_closed = startup_sender
            .send(Startup::Failed {
                source,
                message: message.clone(),
            })
            .is_err();
        if startup_receiver_closed && !stop_requested.load(Ordering::SeqCst) {
            let _ = event_sender.send(WasapiCaptureEvent::Error { source, message });
        }
    }
}

fn run_capture(
    source: SourceLabel,
    event_sender: &channel::Sender<WasapiCaptureEvent>,
    startup_sender: &channel::Sender<Startup>,
    stop_requested: &AtomicBool,
    microphone_muted: &AtomicBool,
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
    startup_sender
        .send(Startup::Ready(source))
        .map_err(|err| err.to_string())?;

    let mut bytes = VecDeque::new();
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
        event_sender
            .send(WasapiCaptureEvent::Samples(WasapiPcmChunk {
                source,
                samples,
            }))
            .map_err(|err| err.to_string())?;
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

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;

    use super::drain_f32_samples;

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
}
