//! Streaming Nemotron transcription through sherpa-onnx.
//!
//! Nemotron keeps encoder state between small chunks, so every audio frame is
//! processed once instead of repeatedly decoding a rolling long-form window.

use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use crossbeam_channel as channel;
use sherpa_onnx::{OnlineRecognizer, OnlineRecognizerConfig, OnlineStream};
use wisp_core::{
    AudioFrame, TrackDescriptor, TrackId, TranscriptEvent, TranscriptSegment, TranscriptSegmentId,
};

use crate::whisper_backend::{StreamingResampler, pcm_to_mono_samples};
use crate::{
    Availability, BackendError, BackendErrorKind, BackendId, BackendResult, RecognitionPrivacy,
    TranscriberBackend, TranscriberCapabilities, TranscriberClass, TranscriberConfig,
    TranscriberFactory, TranscriberFeature, TranscriberProbe, UnavailableReason,
};

pub const NEMOTRON_BACKEND_ID: &str = "nemotron-sherpa-onnx";
const SAMPLE_RATE: u32 = 16_000;
const SAMPLE_RATE_I32: i32 = 16_000;
const AUDIO_QUEUE_CAPACITY: usize = 128;
const SILENCE_BLOCK_SAMPLES: usize = 1_600;
const ENCODER_FILENAME: &str = "encoder.int8.onnx";
const DECODER_FILENAME: &str = "decoder.int8.onnx";
const JOINER_FILENAME: &str = "joiner.int8.onnx";
const TOKENS_FILENAME: &str = "tokens.txt";

#[derive(Debug, Default, Clone, Copy)]
pub struct NemotronTranscriberFactory;

impl NemotronTranscriberFactory {
    #[must_use]
    pub fn from_artifact(
        path: impl Into<PathBuf>,
        locale: &str,
    ) -> Box<dyn TranscriberBackend> {
        Box::new(NemotronTranscriberBackend::new(path, locale))
    }
}

impl TranscriberFactory for NemotronTranscriberFactory {
    fn backend_id(&self) -> BackendId {
        BackendId::new(NEMOTRON_BACKEND_ID)
    }

    fn create(
        &self,
        config: &TranscriberConfig,
    ) -> BackendResult<Box<dyn TranscriberBackend>> {
        if config.backend_id != self.backend_id() {
            return Err(NemotronTranscriberBackend::error(
                BackendErrorKind::InvalidState,
                format!("factory cannot construct backend {}", config.backend_id),
            ));
        }
        let path = config.model_artifact.clone().ok_or_else(|| {
            NemotronTranscriberBackend::error(
                BackendErrorKind::MissingModel,
                "Nemotron requires a model bundle",
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

pub struct NemotronTranscriberBackend {
    model_path: PathBuf,
    language: String,
    worker: Option<Worker>,
    resamplers: HashMap<TrackId, StreamingResampler>,
    dropped_samples: HashMap<TrackId, usize>,
    pending_events: VecDeque<TranscriptEvent>,
}

impl NemotronTranscriberBackend {
    #[must_use]
    pub fn new(
        model_path: impl Into<PathBuf>,
        locale: impl AsRef<str>,
    ) -> Self {
        Self {
            model_path: model_path.into(),
            language: nemotron_language(locale.as_ref()),
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
        BackendError::new(BackendId::new(NEMOTRON_BACKEND_ID), kind, message)
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
            .map_err(|_| Self::error(BackendErrorKind::Internal, "Nemotron worker stopped"))?;
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
        for (track_id, gap_samples) in std::mem::take(&mut self.dropped_samples) {
            worker
                .audio
                .send(AudioCommand::Samples {
                    track_id,
                    samples: Vec::new(),
                    gap_samples,
                })
                .map_err(|_| Self::error(BackendErrorKind::Internal, "Nemotron worker stopped"))?;
        }
        Ok(())
    }
}

impl TranscriberBackend for NemotronTranscriberBackend {
    fn probe(&self) -> TranscriberProbe {
        let availability = if model_bundle_ready(&self.model_path) {
            Availability::Available
        } else {
            Availability::Unavailable(UnavailableReason::MissingModel(
                self.model_path.display().to_string(),
            ))
        };
        TranscriberProbe {
            backend_id: BackendId::new(NEMOTRON_BACKEND_ID),
            class: TranscriberClass::LocalModel,
            availability,
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

    fn start(
        &mut self,
        tracks: &[TrackDescriptor],
    ) -> BackendResult<()> {
        if self.worker.is_some() {
            return Err(Self::error(
                BackendErrorKind::InvalidState,
                "Nemotron provider is already running",
            ));
        }
        if tracks.is_empty() {
            return Err(Self::error(
                BackendErrorKind::UnsupportedFormat,
                "Nemotron requires at least one audio track",
            ));
        }
        if !model_bundle_ready(&self.model_path) {
            return Err(Self::error(
                BackendErrorKind::MissingModel,
                format!(
                    "Nemotron model bundle is incomplete: {}",
                    self.model_path.display()
                ),
            ));
        }

        let recognizer = create_recognizer(&self.model_path).ok_or_else(|| {
            Self::error(
                BackendErrorKind::Internal,
                "could not load Nemotron ONNX model bundle",
            )
        })?;
        let track_ids = tracks.iter().map(|track| track.id).collect::<Vec<_>>();
        let (audio_tx, audio_rx) = channel::bounded(AUDIO_QUEUE_CAPACITY);
        let (control_tx, control_rx) = channel::unbounded();
        let (event_tx, event_rx) = channel::unbounded();
        let language = self.language.clone();
        let abort_requested = Arc::new(AtomicBool::new(false));
        let worker_abort = Arc::clone(&abort_requested);
        let join = thread::Builder::new()
            .name("wisp-nemotron".into())
            .spawn(move || {
                run_worker(
                    recognizer,
                    &track_ids,
                    &language,
                    &audio_rx,
                    &control_rx,
                    &event_tx,
                    &worker_abort,
                );
            })
            .map_err(|error| {
                Self::error(
                    BackendErrorKind::Internal,
                    format!("could not start Nemotron worker: {error}"),
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
        let mono = pcm_to_mono_samples(frame).map_err(|format| {
            Self::error(
                BackendErrorKind::UnsupportedFormat,
                format!("Nemotron supports f32/i16 PCM, received {format:?}"),
            )
        })?;
        let resampler = self
            .resamplers
            .entry(frame.track_id())
            .or_insert_with(|| StreamingResampler::new(frame.format().sample_rate, SAMPLE_RATE));
        if resampler.source_rate != frame.format().sample_rate {
            return Err(Self::error(
                BackendErrorKind::UnsupportedFormat,
                "Nemotron does not support changing sample rate during a session",
            ));
        }
        let samples = resampler.push(&mono);
        let Some(worker) = &self.worker else {
            return Err(Self::error(
                BackendErrorKind::InvalidState,
                "Nemotron provider is not running",
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
                "Nemotron worker stopped",
            )),
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
            .saturating_mul(u64::from(SAMPLE_RATE))
            .checked_div(u64::from(source_rate))
            .and_then(|count| usize::try_from(count).ok())
            .unwrap_or(usize::MAX);
        let dropped = self.dropped_samples.entry(track_id).or_default();
        *dropped = dropped.saturating_add(samples);
        Ok(())
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
        match worker.events.recv_timeout(timeout) {
            Ok(WorkerEvent::Transcript(event)) => Ok(Some(event)),
            Ok(WorkerEvent::Finished) | Err(channel::RecvTimeoutError::Timeout) => Ok(None),
            Ok(WorkerEvent::Failed(message)) => {
                Err(Self::error(BackendErrorKind::Internal, message))
            },
            Err(channel::RecvTimeoutError::Disconnected) => Err(Self::error(
                BackendErrorKind::Internal,
                "Nemotron worker event channel disconnected",
            )),
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

impl Drop for NemotronTranscriberBackend {
    fn drop(&mut self) {
        let _ = self.stop_worker(ControlCommand::Abort);
    }
}

struct TrackState {
    stream: OnlineStream,
    segment_id: u64,
    utterance_start_samples: usize,
    accepted_samples: usize,
    last_text: String,
}

fn create_recognizer(model_path: &Path) -> Option<OnlineRecognizer> {
    let mut config = OnlineRecognizerConfig::default();
    config.model_config.transducer.encoder = Some(
        model_path
            .join(ENCODER_FILENAME)
            .to_string_lossy()
            .into_owned(),
    );
    config.model_config.transducer.decoder = Some(
        model_path
            .join(DECODER_FILENAME)
            .to_string_lossy()
            .into_owned(),
    );
    config.model_config.transducer.joiner = Some(
        model_path
            .join(JOINER_FILENAME)
            .to_string_lossy()
            .into_owned(),
    );
    config.model_config.tokens = Some(
        model_path
            .join(TOKENS_FILENAME)
            .to_string_lossy()
            .into_owned(),
    );
    config.model_config.num_threads = 2;
    config.model_config.provider = Some("cpu".into());
    config.decoding_method = Some("greedy_search".into());
    config.max_active_paths = 4;
    config.enable_endpoint = true;
    config.rule1_min_trailing_silence = 2.4;
    config.rule2_min_trailing_silence = 1.2;
    config.rule3_min_utterance_length = 20.0;
    OnlineRecognizer::create(&config)
}

// Ownership keeps the native recognizer alive on exactly the worker thread
// for the lifetime of every stream derived from it.
#[allow(clippy::needless_pass_by_value)]
fn run_worker(
    recognizer: OnlineRecognizer,
    track_ids: &[TrackId],
    language: &str,
    audio: &channel::Receiver<AudioCommand>,
    control: &channel::Receiver<ControlCommand>,
    events: &channel::Sender<WorkerEvent>,
    abort_requested: &Arc<AtomicBool>,
) {
    let mut tracks = track_ids
        .iter()
        .map(|track_id| {
            let stream = recognizer.create_stream();
            stream.set_option("language", language);
            (
                *track_id,
                TrackState {
                    stream,
                    segment_id: 0,
                    utterance_start_samples: 0,
                    accepted_samples: 0,
                    last_text: String::new(),
                },
            )
        })
        .collect::<HashMap<_, _>>();

    loop {
        if abort_requested.load(Ordering::Acquire) {
            let _ = events.send(WorkerEvent::Finished);
            return;
        }
        if let Ok(command) = control.try_recv() {
            match command {
                ControlCommand::Finish => {
                    while let Ok(command) = audio.try_recv() {
                        process_audio_command(&recognizer, &mut tracks, command, events);
                    }
                    finish_streams(&recognizer, &mut tracks, events);
                },
                ControlCommand::Abort => {
                    let _ = events.send(WorkerEvent::Finished);
                },
            }
            return;
        }
        match audio.recv_timeout(Duration::from_millis(10)) {
            Ok(command) => process_audio_command(&recognizer, &mut tracks, command, events),
            Err(channel::RecvTimeoutError::Timeout) => {},
            Err(channel::RecvTimeoutError::Disconnected) => return,
        }
    }
}

fn process_audio_command(
    recognizer: &OnlineRecognizer,
    tracks: &mut HashMap<TrackId, TrackState>,
    command: AudioCommand,
    events: &channel::Sender<WorkerEvent>,
) {
    let AudioCommand::Samples {
        track_id,
        samples,
        gap_samples,
    } = command;
    let Some(state) = tracks.get_mut(&track_id) else {
        let _ = events.send(WorkerEvent::Failed(format!(
            "Nemotron received unknown track {}",
            track_id.get()
        )));
        return;
    };
    if gap_samples > 0 {
        accept_silence(&state.stream, gap_samples);
        state.accepted_samples = state.accepted_samples.saturating_add(gap_samples);
    }
    state.stream.accept_waveform(SAMPLE_RATE_I32, &samples);
    state.accepted_samples = state.accepted_samples.saturating_add(samples.len());
    decode_ready(recognizer, track_id, state, events, false);
}

fn accept_silence(
    stream: &OnlineStream,
    mut samples: usize,
) {
    let silence = [0.0; SILENCE_BLOCK_SAMPLES];
    while samples > 0 {
        let count = samples.min(silence.len());
        stream.accept_waveform(SAMPLE_RATE_I32, &silence[..count]);
        samples -= count;
    }
}

#[allow(clippy::cast_precision_loss)]
fn decode_ready(
    recognizer: &OnlineRecognizer,
    track_id: TrackId,
    state: &mut TrackState,
    events: &channel::Sender<WorkerEvent>,
    force_final: bool,
) {
    let started_at = Instant::now();
    let mut decode_steps = 0_u64;
    while recognizer.is_ready(&state.stream) {
        recognizer.decode(&state.stream);
        decode_steps += 1;
    }
    if decode_steps > 0 {
        let audio_seconds = state.accepted_samples as f64 / f64::from(SAMPLE_RATE);
        eprintln!(
            "[NEMOTRON] decoded {decode_steps} step(s) at {audio_seconds:.2}s in {:.3}s",
            started_at.elapsed().as_secs_f64()
        );
    }

    let endpoint = recognizer.is_endpoint(&state.stream);
    let Some(result) = recognizer.get_result(&state.stream) else {
        return;
    };
    let text = result.text.trim().to_owned();
    let is_final = force_final || endpoint || result.is_final;
    if !text.is_empty() && (text != state.last_text || is_final) {
        let segment = TranscriptSegment {
            track_id,
            segment_id: TranscriptSegmentId::new(state.segment_id),
            text: text.clone(),
            start_seconds: state.utterance_start_samples as f64 / f64::from(SAMPLE_RATE),
            end_seconds: state.accepted_samples as f64 / f64::from(SAMPLE_RATE),
            confidence_mean: None,
            confidence_min: None,
        };
        let event = if is_final {
            TranscriptEvent::Final(segment)
        } else {
            TranscriptEvent::Partial(segment)
        };
        let _ = events.send(WorkerEvent::Transcript(event));
        state.last_text = text;
    }
    if endpoint {
        recognizer.reset(&state.stream);
        state.segment_id = state.segment_id.wrapping_add(1);
        state.utterance_start_samples = state.accepted_samples;
        state.last_text.clear();
    }
}

fn finish_streams(
    recognizer: &OnlineRecognizer,
    tracks: &mut HashMap<TrackId, TrackState>,
    events: &channel::Sender<WorkerEvent>,
) {
    for (track_id, state) in tracks {
        state.stream.input_finished();
        decode_ready(recognizer, *track_id, state, events, true);
    }
    let _ = events.send(WorkerEvent::Finished);
}

fn model_bundle_ready(path: &Path) -> bool {
    [
        ENCODER_FILENAME,
        DECODER_FILENAME,
        JOINER_FILENAME,
        TOKENS_FILENAME,
    ]
    .into_iter()
    .all(|filename| {
        std::fs::metadata(path.join(filename))
            .is_ok_and(|metadata| metadata.is_file() && metadata.len() > 0)
    })
}

fn nemotron_language(locale: &str) -> String {
    locale
        .split(['-', '_'])
        .next()
        .filter(|language| !language.is_empty())
        .unwrap_or("auto")
        .to_ascii_lowercase()
}

#[cfg(test)]
mod tests {
    use crate::{
        Availability, BackendErrorKind, BackendId, TranscriberBackend, TranscriberConfig,
        TranscriberFactory,
    };

    use super::{
        DECODER_FILENAME, ENCODER_FILENAME, JOINER_FILENAME, NEMOTRON_BACKEND_ID,
        NemotronTranscriberBackend, NemotronTranscriberFactory, TOKENS_FILENAME,
        model_bundle_ready, nemotron_language,
    };

    #[test]
    fn locale_maps_to_nemotron_stream_language() {
        assert_eq!(nemotron_language("ja-JP"), "ja");
        assert_eq!(nemotron_language("en_US"), "en");
        assert_eq!(nemotron_language(""), "auto");
    }

    #[test]
    fn model_bundle_requires_every_artifact() {
        let directory = tempfile::tempdir().expect("temp dir");
        assert!(!model_bundle_ready(directory.path()));
        for filename in [
            ENCODER_FILENAME,
            DECODER_FILENAME,
            JOINER_FILENAME,
            TOKENS_FILENAME,
        ] {
            std::fs::write(directory.path().join(filename), [1]).expect("write artifact");
        }
        assert!(model_bundle_ready(directory.path()));
        let backend = NemotronTranscriberBackend::new(directory.path(), "ja-JP");
        assert_eq!(backend.probe().availability, Availability::Available);
    }

    #[test]
    fn factory_requires_a_model_bundle() {
        let factory = NemotronTranscriberFactory;
        let config = TranscriberConfig {
            backend_id: BackendId::new(NEMOTRON_BACKEND_ID),
            locale: "ja-JP".into(),
            model_artifact: None,
            options: std::collections::BTreeMap::new(),
        };
        let Err(error) = factory.create(&config) else {
            panic!("missing bundle must fail");
        };
        assert_eq!(error.kind, BackendErrorKind::MissingModel);
    }
}
