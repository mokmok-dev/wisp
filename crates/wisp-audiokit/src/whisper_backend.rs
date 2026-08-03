//! Offline `whisper.cpp` transcription provider.
//!
//! The provider consumes the same backend-neutral PCM frames as platform
//! recognizers. Inference runs on a dedicated worker so capture queues keep
//! draining while whisper.cpp is decoding.

use std::collections::HashMap;
use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use crossbeam_channel as channel;
use whisper_rs::{FullParams, SamplingStrategy, WhisperContext, WhisperContextParameters};
use wisp_core::{
    AudioFrame, AudioSamples, TrackDescriptor, TrackId, TranscriptEvent, TranscriptSegment,
    TranscriptSegmentId,
};

use crate::{
    Availability, BackendError, BackendErrorKind, BackendId, BackendResult, RecognitionPrivacy,
    TranscriberBackend, TranscriberCapabilities, TranscriberClass, TranscriberConfig,
    TranscriberFactory, TranscriberFeature, TranscriberProbe, UnavailableReason,
};

pub const WHISPER_BACKEND_ID: &str = "whisper-cpp";
const WHISPER_SAMPLE_RATE: u32 = 16_000;
const CHUNK_SAMPLES: usize = 16_000 * 12;
const VAD_WINDOW_SAMPLES: usize = 16_000 * 30 / 1_000;
const VAD_MIN_CONSECUTIVE_WINDOWS: usize = 2;
/// Roughly -46 dBFS. Consecutive windows avoid treating isolated keyboard
/// clicks or capture impulses as speech.
const VAD_RMS_THRESHOLD: f32 = 0.005;
/// Whisper's default threshold for treating a decoded segment as silence.
///
/// whisper.cpp exposes this probability on every segment but does not apply
/// the threshold itself. Filtering here prevents text hallucinated from
/// silence (for example common outro phrases) from reaching transcript
/// consumers.
const NO_SPEECH_PROBABILITY_THRESHOLD: f32 = 0.6;

/// Factory registration for the built-in whisper.cpp provider.
#[derive(Debug, Default, Clone, Copy)]
pub struct WhisperTranscriberFactory;

impl WhisperTranscriberFactory {
    /// Construct the built-in whisper.cpp provider from its model artifact.
    ///
    /// Platform sessions use this construction boundary so future registered
    /// providers can replace the concrete implementation without changing
    /// capture adapters.
    #[must_use]
    pub fn from_artifact(
        path: impl Into<PathBuf>,
        locale: &str,
    ) -> Box<dyn TranscriberBackend> {
        Box::new(WhisperTranscriberBackend::new(path, locale))
    }
}

impl TranscriberFactory for WhisperTranscriberFactory {
    fn backend_id(&self) -> BackendId {
        BackendId::new(WHISPER_BACKEND_ID)
    }

    fn create(
        &self,
        config: &TranscriberConfig,
    ) -> BackendResult<Box<dyn TranscriberBackend>> {
        if config.backend_id != self.backend_id() {
            return Err(WhisperTranscriberBackend::error(
                BackendErrorKind::InvalidState,
                format!("factory cannot construct backend {}", config.backend_id),
            ));
        }
        let path = config.model_artifact.clone().ok_or_else(|| {
            WhisperTranscriberBackend::error(
                BackendErrorKind::MissingModel,
                "Whisper requires a model artifact",
            )
        })?;
        Ok(Self::from_artifact(path, &config.locale))
    }
}

enum AudioCommand {
    Samples {
        track_id: TrackId,
        samples: Vec<f32>,
        gap_samples: usize,
    },
    Gap {
        track_id: TrackId,
        samples: usize,
    },
}

enum ControlCommand {
    Finish,
    Abort,
}

enum WorkerEvent {
    Transcript(TranscriptEvent),
    Finished,
    Failed(String),
}

struct Worker {
    audio: channel::Sender<AudioCommand>,
    control: channel::Sender<ControlCommand>,
    events: channel::Receiver<WorkerEvent>,
    abort_requested: Arc<AtomicBool>,
    join: Option<JoinHandle<()>>,
}

/// A local Whisper provider backed by `whisper-rs`/whisper.cpp.
pub struct WhisperTranscriberBackend {
    model_path: PathBuf,
    language: Option<String>,
    worker: Option<Worker>,
    resamplers: HashMap<TrackId, StreamingResampler>,
    dropped_samples: HashMap<TrackId, usize>,
    pending_events: VecDeque<TranscriptEvent>,
}

impl WhisperTranscriberBackend {
    #[must_use]
    pub fn new(
        model_path: impl Into<PathBuf>,
        locale: impl AsRef<str>,
    ) -> Self {
        Self {
            model_path: model_path.into(),
            language: whisper_language(locale.as_ref()),
            worker: None,
            resamplers: HashMap::new(),
            dropped_samples: HashMap::new(),
            pending_events: VecDeque::new(),
        }
    }

    fn error(
        kind: BackendErrorKind,
        message: impl Into<String>,
    ) -> BackendError {
        BackendError::new(BackendId::new(WHISPER_BACKEND_ID), kind, message)
    }

    fn stop_worker(
        &mut self,
        command: ControlCommand,
    ) -> BackendResult<()> {
        let Some(mut worker) = self.worker.take() else {
            return Ok(());
        };
        if matches!(command, ControlCommand::Abort) {
            worker.abort_requested.store(true, Ordering::Release);
        }
        worker
            .control
            .send(command)
            .map_err(|_| Self::error(BackendErrorKind::Internal, "Whisper worker stopped"))?;
        let result = loop {
            match worker.events.recv() {
                Ok(WorkerEvent::Finished) | Err(_) => break Ok(()),
                Ok(WorkerEvent::Failed(message)) => {
                    break Err(Self::error(BackendErrorKind::Internal, message));
                },
                Ok(WorkerEvent::Transcript(event)) => self.pending_events.push_back(event),
            }
        };
        if let Some(join) = worker.join.take() {
            let _ = join.join();
        }
        result
    }

    fn flush_pending_gaps(&mut self) -> BackendResult<()> {
        let Some(worker) = &self.worker else {
            return Ok(());
        };
        for (track_id, samples) in std::mem::take(&mut self.dropped_samples) {
            worker
                .audio
                .send(AudioCommand::Gap { track_id, samples })
                .map_err(|_| Self::error(BackendErrorKind::Internal, "Whisper worker stopped"))?;
        }
        Ok(())
    }
}

impl TranscriberBackend for WhisperTranscriberBackend {
    fn probe(&self) -> TranscriberProbe {
        let availability = if model_file_ready(&self.model_path) {
            Availability::Available
        } else {
            Availability::Unavailable(UnavailableReason::MissingModel(
                self.model_path.display().to_string(),
            ))
        };
        TranscriberProbe {
            backend_id: BackendId::new(WHISPER_BACKEND_ID),
            class: TranscriberClass::LocalModel,
            availability,
            capabilities: TranscriberCapabilities {
                privacy: RecognitionPrivacy::Offline,
                features: vec![
                    TranscriberFeature::Streaming,
                    TranscriberFeature::SegmentTimestamps,
                ],
            },
        }
    }

    fn start(
        &mut self,
        tracks: &[TrackDescriptor],
    ) -> BackendResult<()> {
        if self.worker.is_some() {
            return Err(Self::error(
                BackendErrorKind::InvalidState,
                "Whisper provider is already running",
            ));
        }
        if tracks.is_empty() {
            return Err(Self::error(
                BackendErrorKind::UnsupportedFormat,
                "Whisper requires at least one audio track",
            ));
        }
        if !model_file_ready(&self.model_path) {
            return Err(Self::error(
                BackendErrorKind::MissingModel,
                format!("Whisper model is missing: {}", self.model_path.display()),
            ));
        }

        // Load before starting the thread so setup failures remain transactional.
        let context =
            WhisperContext::new_with_params(&self.model_path, WhisperContextParameters::default())
                .map_err(|error| {
                    Self::error(
                        BackendErrorKind::Internal,
                        format!("could not load Whisper model: {error}"),
                    )
                })?;
        let (audio_tx, audio_rx) = channel::bounded(128);
        let (control_tx, control_rx) = channel::unbounded();
        let (event_tx, event_rx) = channel::unbounded();
        let language = self.language.clone();
        let abort_requested = Arc::new(AtomicBool::new(false));
        let worker_abort = Arc::clone(&abort_requested);
        let join = thread::Builder::new()
            .name("wisp-whisper".into())
            .spawn(move || {
                run_worker(
                    &context,
                    language.as_deref(),
                    &audio_rx,
                    &control_rx,
                    &event_tx,
                    &worker_abort,
                );
            })
            .map_err(|error| {
                Self::error(
                    BackendErrorKind::Internal,
                    format!("could not start Whisper worker: {error}"),
                )
            })?;
        self.worker = Some(Worker {
            audio: audio_tx,
            control: control_tx,
            events: event_rx,
            abort_requested,
            join: Some(join),
        });
        Ok(())
    }

    fn push(
        &mut self,
        frame: &AudioFrame,
    ) -> BackendResult<()> {
        let mono = frame_to_mono_samples(frame)?;
        let resampler = self.resamplers.entry(frame.track_id()).or_insert_with(|| {
            StreamingResampler::new(frame.format().sample_rate, WHISPER_SAMPLE_RATE)
        });
        if resampler.source_rate != frame.format().sample_rate {
            return Err(Self::error(
                BackendErrorKind::UnsupportedFormat,
                "Whisper does not support changing a track sample rate during a session",
            ));
        }
        let samples = resampler.push(&mono);
        let Some(worker) = &self.worker else {
            return Err(Self::error(
                BackendErrorKind::InvalidState,
                "Whisper provider is not running",
            ));
        };
        let gap_samples = self.dropped_samples.remove(&frame.track_id()).unwrap_or(0);
        match worker.audio.try_send(AudioCommand::Samples {
            track_id: frame.track_id(),
            samples,
            gap_samples,
        }) {
            Ok(()) => Ok(()),
            Err(channel::TrySendError::Full(AudioCommand::Samples {
                track_id,
                samples,
                gap_samples,
            })) => {
                let dropped = self.dropped_samples.entry(track_id).or_default();
                *dropped = dropped
                    .saturating_add(gap_samples)
                    .saturating_add(samples.len());
                Ok(())
            },
            Err(channel::TrySendError::Disconnected(_)) => Err(Self::error(
                BackendErrorKind::Internal,
                "Whisper worker stopped",
            )),
            Err(channel::TrySendError::Full(AudioCommand::Gap { .. })) => unreachable!(),
        }
    }

    fn push_gap(
        &mut self,
        track_id: TrackId,
        dropped_frames: u64,
    ) -> BackendResult<()> {
        let Some(source_rate) = self
            .resamplers
            .get(&track_id)
            .map(|state| state.source_rate)
        else {
            return Ok(());
        };
        let samples = dropped_frames
            .saturating_mul(u64::from(WHISPER_SAMPLE_RATE))
            .checked_div(u64::from(source_rate))
            .and_then(|count| usize::try_from(count).ok())
            .unwrap_or(usize::MAX);
        let Some(worker) = &self.worker else {
            return Ok(());
        };
        match worker
            .audio
            .try_send(AudioCommand::Gap { track_id, samples })
        {
            Ok(()) => Ok(()),
            Err(channel::TrySendError::Full(_)) => {
                let dropped = self.dropped_samples.entry(track_id).or_default();
                *dropped = dropped.saturating_add(samples);
                Ok(())
            },
            Err(channel::TrySendError::Disconnected(_)) => Err(Self::error(
                BackendErrorKind::Internal,
                "Whisper worker stopped",
            )),
        }
    }

    fn next_event(
        &mut self,
        timeout: Duration,
    ) -> BackendResult<Option<TranscriptEvent>> {
        if let Some(event) = self.pending_events.pop_front() {
            return Ok(Some(event));
        }
        let Some(worker) = &self.worker else {
            return Ok(None);
        };
        let event = if timeout.is_zero() {
            match worker.events.try_recv() {
                Ok(event) => Some(event),
                Err(channel::TryRecvError::Empty) => None,
                Err(channel::TryRecvError::Disconnected) => {
                    return Err(Self::error(
                        BackendErrorKind::Internal,
                        "Whisper worker event channel disconnected",
                    ));
                },
            }
        } else {
            match worker.events.recv_timeout(timeout) {
                Ok(event) => Some(event),
                Err(channel::RecvTimeoutError::Timeout) => None,
                Err(channel::RecvTimeoutError::Disconnected) => {
                    return Err(Self::error(
                        BackendErrorKind::Internal,
                        "Whisper worker event channel disconnected",
                    ));
                },
            }
        };
        match event {
            Some(WorkerEvent::Transcript(event)) => Ok(Some(event)),
            Some(WorkerEvent::Failed(message)) => {
                Err(Self::error(BackendErrorKind::Internal, message))
            },
            Some(WorkerEvent::Finished) | None => Ok(None),
        }
    }

    fn finish(&mut self) -> BackendResult<()> {
        self.flush_pending_gaps()?;
        self.stop_worker(ControlCommand::Finish)
    }

    fn abort(&mut self) -> BackendResult<()> {
        self.stop_worker(ControlCommand::Abort)
    }
}

impl Drop for WhisperTranscriberBackend {
    fn drop(&mut self) {
        let _ = self.stop_worker(ControlCommand::Abort);
    }
}

fn model_file_ready(path: &Path) -> bool {
    crate::local_model_specs()
        .into_iter()
        .find(|spec| path.file_name().is_some_and(|name| name == spec.filename))
        .is_some_and(|spec| {
            std::fs::metadata(path).is_ok_and(|metadata| {
                metadata.is_file()
                    && metadata.len() == spec.bytes
                    && crate::verify_file_sha256(path, spec.sha256).is_ok()
            })
        })
}

fn whisper_language(locale: &str) -> Option<String> {
    let language = locale
        .split(['-', '_'])
        .next()
        .filter(|language| !language.is_empty())?
        .to_ascii_lowercase();
    Some(language)
}

fn frame_to_mono_samples(frame: &AudioFrame) -> BackendResult<Vec<f32>> {
    pcm_to_mono_samples(frame).map_err(|format| {
        WhisperTranscriberBackend::error(
            BackendErrorKind::UnsupportedFormat,
            format!("Whisper supports f32/i16 PCM, received {format:?}"),
        )
    })
}

pub(crate) fn pcm_to_mono_samples(frame: &AudioFrame) -> Result<Vec<f32>, wisp_core::SampleFormat> {
    let channels = frame.format().channels;
    let mono = match frame.samples() {
        AudioSamples::F32(samples) => downmix(samples.iter().copied(), channels),
        AudioSamples::I16(samples) => downmix(
            samples.iter().map(|sample| f32::from(*sample) / 32_768.0),
            channels,
        ),
        _ => return Err(frame.format().sample_format.clone()),
    };
    Ok(mono)
}

fn downmix(
    samples: impl Iterator<Item = f32>,
    channels: u16,
) -> Vec<f32> {
    let samples = samples.collect::<Vec<_>>();
    samples
        .chunks_exact(usize::from(channels))
        .map(|frame| frame.iter().sum::<f32>() / f32::from(channels))
        .collect()
}

#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_precision_loss,
    clippy::cast_sign_loss
)]
pub(crate) struct StreamingResampler {
    pub(crate) source_rate: u32,
    target_rate: u32,
    position: f64,
    previous: Option<f32>,
}

impl StreamingResampler {
    pub(crate) const fn new(
        source_rate: u32,
        target_rate: u32,
    ) -> Self {
        Self {
            source_rate,
            target_rate,
            position: 0.0,
            previous: None,
        }
    }

    #[allow(
        clippy::cast_possible_truncation,
        clippy::cast_precision_loss,
        clippy::cast_sign_loss
    )]
    pub(crate) fn push(
        &mut self,
        samples: &[f32],
    ) -> Vec<f32> {
        if samples.is_empty() {
            return Vec::new();
        }
        if self.source_rate == self.target_rate {
            return samples.to_vec();
        }
        let mut joined = Vec::with_capacity(samples.len() + usize::from(self.previous.is_some()));
        if let Some(previous) = self.previous {
            joined.push(previous);
        }
        joined.extend_from_slice(samples);
        let mut output = Vec::new();
        let step = f64::from(self.source_rate) / f64::from(self.target_rate);
        while self.position + 1.0 < joined.len() as f64 {
            let left = self.position.floor() as usize;
            let fraction = (self.position - left as f64) as f32;
            output.push(joined[left].mul_add(1.0 - fraction, joined[left + 1] * fraction));
            self.position += step;
        }
        self.position -= (joined.len() - 1) as f64;
        self.previous = joined.last().copied();
        output
    }
}

fn run_worker(
    context: &WhisperContext,
    language: Option<&str>,
    audio: &channel::Receiver<AudioCommand>,
    control: &channel::Receiver<ControlCommand>,
    events: &channel::Sender<WorkerEvent>,
    abort_requested: &Arc<AtomicBool>,
) {
    let mut buffers = HashMap::<TrackId, Vec<f32>>::new();
    let mut offsets = HashMap::<TrackId, usize>::new();
    let mut next_ids = HashMap::<TrackId, u64>::new();
    loop {
        if let Ok(command) = control.try_recv() {
            match command {
                ControlCommand::Finish => {
                    while let Ok(command) = audio.try_recv() {
                        if let Err(error) = process_audio_command(
                            context,
                            language,
                            &mut buffers,
                            &mut offsets,
                            &mut next_ids,
                            command,
                            events,
                            abort_requested,
                        ) {
                            let _ = events.send(WorkerEvent::Failed(error));
                            return;
                        }
                    }
                    finish_worker(
                        context,
                        language,
                        &mut buffers,
                        &mut offsets,
                        &mut next_ids,
                        events,
                        abort_requested,
                    );
                },
                ControlCommand::Abort => {
                    let _ = events.send(WorkerEvent::Finished);
                },
            }
            return;
        }
        let command = match audio.recv_timeout(Duration::from_millis(10)) {
            Ok(command) => command,
            Err(channel::RecvTimeoutError::Timeout) => continue,
            Err(channel::RecvTimeoutError::Disconnected) => return,
        };
        if let Err(error) = process_audio_command(
            context,
            language,
            &mut buffers,
            &mut offsets,
            &mut next_ids,
            command,
            events,
            abort_requested,
        ) {
            if abort_requested.load(Ordering::Acquire) {
                let _ = events.send(WorkerEvent::Finished);
                return;
            }
            let _ = events.send(WorkerEvent::Failed(error));
            return;
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn process_audio_command(
    context: &WhisperContext,
    language: Option<&str>,
    buffers: &mut HashMap<TrackId, Vec<f32>>,
    offsets: &mut HashMap<TrackId, usize>,
    next_ids: &mut HashMap<TrackId, u64>,
    command: AudioCommand,
    events: &channel::Sender<WorkerEvent>,
    abort_requested: &Arc<AtomicBool>,
) -> Result<(), String> {
    let (track_id, gap_samples, samples) = match command {
        AudioCommand::Samples {
            track_id,
            samples,
            gap_samples,
        } => (track_id, gap_samples, Some(samples)),
        AudioCommand::Gap { track_id, samples } => (track_id, samples, None),
    };

    if gap_samples > 0 {
        // A dropped interval advances the recognition timeline but should not
        // be decoded as an equally large silence buffer. Flush pre-gap audio
        // first so timestamp ordering remains exact.
        let pending = std::mem::take(buffers.entry(track_id).or_default());
        if !pending.is_empty() {
            transcribe_chunk(
                context,
                language,
                track_id,
                &pending,
                offsets.entry(track_id).or_default(),
                next_ids.entry(track_id).or_default(),
                events,
                abort_requested,
            )?;
        }
        let offset = offsets.entry(track_id).or_default();
        *offset = offset.saturating_add(gap_samples);
    }
    if let Some(samples) = samples {
        buffers.entry(track_id).or_default().extend(samples);
    }
    let buffer = buffers.entry(track_id).or_default();
    while buffer.len() >= CHUNK_SAMPLES {
        let chunk = buffer.drain(..CHUNK_SAMPLES).collect::<Vec<_>>();
        transcribe_chunk(
            context,
            language,
            track_id,
            &chunk,
            offsets.entry(track_id).or_default(),
            next_ids.entry(track_id).or_default(),
            events,
            abort_requested,
        )?;
    }
    Ok(())
}

fn finish_worker(
    context: &WhisperContext,
    language: Option<&str>,
    buffers: &mut HashMap<TrackId, Vec<f32>>,
    offsets: &mut HashMap<TrackId, usize>,
    next_ids: &mut HashMap<TrackId, u64>,
    events: &channel::Sender<WorkerEvent>,
    abort_requested: &Arc<AtomicBool>,
) {
    for (track_id, samples) in std::mem::take(buffers) {
        if !samples.is_empty()
            && let Err(error) = transcribe_chunk(
                context,
                language,
                track_id,
                &samples,
                offsets.entry(track_id).or_default(),
                next_ids.entry(track_id).or_default(),
                events,
                abort_requested,
            )
        {
            if abort_requested.load(Ordering::Acquire) {
                let _ = events.send(WorkerEvent::Finished);
                return;
            }
            let _ = events.send(WorkerEvent::Failed(error));
            return;
        }
    }
    let _ = events.send(WorkerEvent::Finished);
}

#[allow(clippy::cast_precision_loss, clippy::too_many_arguments)]
fn transcribe_chunk(
    context: &WhisperContext,
    language: Option<&str>,
    track_id: TrackId,
    samples: &[f32],
    offset_samples: &mut usize,
    next_id: &mut u64,
    events: &channel::Sender<WorkerEvent>,
    abort_requested: &Arc<AtomicBool>,
) -> Result<(), String> {
    if !contains_probable_speech(samples) {
        *offset_samples = offset_samples.saturating_add(samples.len());
        return Ok(());
    }

    let mut state = context
        .create_state()
        .map_err(|error| format!("could not create Whisper state: {error}"))?;
    let mut params = FullParams::new(SamplingStrategy::Greedy { best_of: 1 });
    params.set_language(language);
    params.set_translate(false);
    params.set_no_context(false);
    let threads = thread::available_parallelism().map_or(2, |count| count.get().min(8));
    params.set_n_threads(i32::try_from(threads).unwrap_or(2));
    let inference_abort = Arc::clone(abort_requested);
    let abort_callback: Box<dyn FnMut() -> bool> =
        Box::new(move || inference_abort.load(Ordering::Acquire));
    params.set_abort_callback_safe::<_, Box<dyn FnMut() -> bool>>(Some(abort_callback));
    let started_at = Instant::now();
    state
        .full(params, samples)
        .map_err(|error| format!("Whisper inference failed: {error}"))?;
    let elapsed = started_at.elapsed();
    let audio_seconds = samples.len() as f64 / f64::from(WHISPER_SAMPLE_RATE);
    let realtime_factor = elapsed.as_secs_f64() / audio_seconds;
    eprintln!(
        "[WHISPER] decoded {audio_seconds:.2}s in {:.2}s (RTF {realtime_factor:.3})",
        elapsed.as_secs_f64()
    );
    let offset_seconds = *offset_samples as f64 / f64::from(WHISPER_SAMPLE_RATE);
    for segment in state.as_iter() {
        if is_probable_no_speech(segment.no_speech_probability()) {
            continue;
        }
        let text = segment
            .to_str_lossy()
            .map_err(|error| format!("Whisper returned invalid text: {error}"))?
            .trim()
            .to_owned();
        if text.is_empty() {
            continue;
        }
        let transcript = TranscriptSegment {
            track_id,
            segment_id: TranscriptSegmentId::new(*next_id),
            text,
            start_seconds: offset_seconds + segment.start_timestamp() as f64 / 100.0,
            end_seconds: offset_seconds + segment.end_timestamp() as f64 / 100.0,
            confidence_mean: None,
            confidence_min: None,
        };
        *next_id = next_id.wrapping_add(1);
        if events
            .send(WorkerEvent::Transcript(TranscriptEvent::Final(transcript)))
            .is_err()
        {
            return Ok(());
        }
    }
    *offset_samples = offset_samples.saturating_add(samples.len());
    Ok(())
}

#[allow(clippy::cast_precision_loss)]
fn contains_probable_speech(samples: &[f32]) -> bool {
    let mut consecutive = 0;
    for window in samples.chunks(VAD_WINDOW_SAMPLES) {
        if window.len() < VAD_WINDOW_SAMPLES / 2 {
            continue;
        }
        let mean_square =
            window.iter().map(|sample| sample * sample).sum::<f32>() / window.len() as f32;
        if mean_square.sqrt() >= VAD_RMS_THRESHOLD {
            consecutive += 1;
            if consecutive >= VAD_MIN_CONSECUTIVE_WINDOWS {
                return true;
            }
        } else {
            consecutive = 0;
        }
    }
    false
}

fn is_probable_no_speech(probability: f32) -> bool {
    probability.is_finite() && probability > NO_SPEECH_PROBABILITY_THRESHOLD
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use wisp_core::{
        AudioFormat, AudioFrame, AudioSamples, MonotonicTimestamp, SampleFormat, SourceKind,
        TrackDescriptor, TrackId,
    };

    use crate::{
        Availability, BackendErrorKind, BackendId, TranscriberBackend, TranscriberConfig,
        TranscriberFactory,
    };

    use super::{
        NO_SPEECH_PROBABILITY_THRESHOLD, StreamingResampler, VAD_WINDOW_SAMPLES,
        WHISPER_BACKEND_ID, WhisperTranscriberBackend, WhisperTranscriberFactory,
        contains_probable_speech, frame_to_mono_samples, is_probable_no_speech, model_file_ready,
        whisper_language,
    };

    #[test]
    fn high_no_speech_probability_is_filtered_as_hallucination() {
        assert!(is_probable_no_speech(
            NO_SPEECH_PROBABILITY_THRESHOLD + f32::EPSILON
        ));
        assert!(is_probable_no_speech(1.0));
    }

    #[test]
    fn speech_and_invalid_probabilities_are_not_filtered() {
        assert!(!is_probable_no_speech(NO_SPEECH_PROBABILITY_THRESHOLD));
        assert!(!is_probable_no_speech(0.0));
        assert!(!is_probable_no_speech(f32::NAN));
        assert!(!is_probable_no_speech(f32::INFINITY));
    }

    #[test]
    fn energy_vad_skips_silence_and_short_impulses() {
        assert!(!contains_probable_speech(&vec![0.0; 16_000]));
        let mut click = vec![0.0; 16_000];
        click[500] = 1.0;
        assert!(!contains_probable_speech(&click));
    }

    #[test]
    fn energy_vad_accepts_sustained_quiet_speech() {
        let samples = (0..VAD_WINDOW_SAMPLES * 3)
            .map(|index| if index % 2 == 0 { 0.008 } else { -0.008 })
            .collect::<Vec<_>>();
        assert!(contains_probable_speech(&samples));
    }

    #[test]
    fn locale_maps_to_whisper_language() {
        assert_eq!(whisper_language("ja-JP").as_deref(), Some("ja"));
        assert_eq!(whisper_language("en_US").as_deref(), Some("en"));
    }

    #[test]
    fn resampler_changes_rate_and_preserves_edges() {
        let mut resampler = StreamingResampler::new(4, 8);
        let mut output = resampler.push(&[0.0, 1.0]);
        output.extend(resampler.push(&[0.0, -1.0]));
        assert_eq!(output.len(), 6);
        assert!(output[0].abs() < f32::EPSILON);
        assert!(output.iter().all(|sample| (-1.0..=1.0).contains(sample)));
    }

    #[test]
    fn resampler_is_identical_across_frame_boundaries() {
        let samples = [0.0, 0.25, 1.0, -0.5, -1.0, 0.75];
        let mut whole = StreamingResampler::new(44_100, 16_000);
        let expected = whole.push(&samples);
        let mut split = StreamingResampler::new(44_100, 16_000);
        let mut actual = split.push(&samples[..2]);
        actual.extend(split.push(&samples[2..4]));
        actual.extend(split.push(&samples[4..]));
        assert_eq!(actual, expected);
    }

    #[test]
    fn probe_and_lifecycle_reject_missing_or_corrupt_models() {
        let directory = tempfile::tempdir().expect("temp dir");
        let model = directory.path().join("model.bin");
        let mut backend = WhisperTranscriberBackend::new(&model, "en-US");
        assert!(matches!(
            backend.probe().availability,
            Availability::Unavailable(_)
        ));
        assert_eq!(
            backend.start(&[]).unwrap_err().kind,
            BackendErrorKind::UnsupportedFormat
        );
        std::fs::write(&model, vec![0_u8; 1024 * 1024]).expect("write corrupt model");
        assert!(!model_file_ready(&model));
        let tracks = [TrackDescriptor {
            id: TrackId::MICROPHONE,
            source: SourceKind::Microphone,
            name: "Microphone".into(),
        }];
        assert_eq!(
            backend.start(&tracks).unwrap_err().kind,
            BackendErrorKind::MissingModel
        );
        assert!(backend.next_event(Duration::ZERO).unwrap().is_none());
        backend.abort().unwrap();
        backend.finish().unwrap();
    }

    #[test]
    fn factory_uses_stable_id_and_requires_model_artifact() {
        let factory = WhisperTranscriberFactory;
        let config = TranscriberConfig {
            backend_id: BackendId::new(WHISPER_BACKEND_ID),
            locale: "ja-JP".into(),
            model_artifact: None,
            options: std::collections::BTreeMap::new(),
        };
        assert_eq!(factory.backend_id(), config.backend_id);
        let Err(error) = factory.create(&config) else {
            panic!("missing artifact must fail");
        };
        assert_eq!(error.kind, BackendErrorKind::MissingModel);
    }

    #[test]
    fn pcm_conversion_supports_f32_and_i16_and_rejects_other_formats() {
        let f32_frame = AudioFrame::from_f32(
            TrackId::MICROPHONE,
            SourceKind::Microphone,
            0,
            MonotonicTimestamp::default(),
            16_000,
            2,
            vec![1.0, -1.0, 0.5, 0.5],
        )
        .expect("f32 frame");
        assert_eq!(frame_to_mono_samples(&f32_frame).unwrap(), [0.0, 0.5]);

        let i16_frame = AudioFrame::try_new(
            TrackId::SYSTEM,
            SourceKind::SystemAudio,
            0,
            MonotonicTimestamp::default(),
            AudioFormat {
                sample_rate: 16_000,
                channels: 1,
                sample_format: SampleFormat::I16,
            },
            2,
            AudioSamples::I16(vec![i16::MIN, i16::MAX]),
        )
        .expect("i16 frame");
        let converted = frame_to_mono_samples(&i16_frame).unwrap();
        assert!((converted[0] + 1.0).abs() < f32::EPSILON);
        assert!((converted[1] - 0.999_969_5).abs() < 0.000_001);

        let unsupported = AudioFrame::try_new(
            TrackId::SYSTEM,
            SourceKind::SystemAudio,
            1,
            MonotonicTimestamp::default(),
            AudioFormat {
                sample_rate: 16_000,
                channels: 1,
                sample_format: SampleFormat::U16,
            },
            1,
            AudioSamples::U16(vec![0]),
        )
        .expect("u16 frame");
        assert_eq!(
            frame_to_mono_samples(&unsupported).unwrap_err().kind,
            BackendErrorKind::UnsupportedFormat
        );
    }
}
