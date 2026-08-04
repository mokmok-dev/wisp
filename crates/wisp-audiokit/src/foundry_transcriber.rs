//! Foundry Local live audio transcription.
//!
//! This module bridges Foundry's asynchronous streaming session to Wisp's
//! synchronous, pull-based [`TranscriberBackend`] contract.

use std::collections::{HashMap, VecDeque};
use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use crossbeam_channel as channel;
use foundry_local_sdk::{
    FoundryLocalConfig, FoundryLocalManager, LiveAudioTranscriptionResponse,
    LiveAudioTranscriptionSession,
};
use tokio::runtime::Runtime;
use tokio_stream::StreamExt;
use tokio_util::sync::CancellationToken;
use wisp_core::{
    AudioFrame, TrackDescriptor, TrackId, TranscriptEvent, TranscriptSegment, TranscriptSegmentId,
};
use wisp_models::foundry::FoundryLocalProvider;
use wisp_models::{ModelId, ModelProvider, ModelStatus};

use crate::pcm::{StreamingResampler, pcm_to_mono_samples};
use crate::{
    Availability, BackendError, BackendErrorKind, BackendId, BackendResult, RecognitionPrivacy,
    TranscriberBackend, TranscriberCapabilities, TranscriberClass, TranscriberConfig,
    TranscriberFactory, TranscriberFeature, TranscriberProbe, UnavailableReason,
};

pub const FOUNDRY_TRANSCRIBER_BACKEND_ID: &str = "foundry-local-live";
const SAMPLE_RATE: u32 = 16_000;
const AUDIO_QUEUE_CAPACITY: usize = 128;
const DEFAULT_ENGLISH_ALIAS: &str = "nemotron-speech-streaming-en-0.6b";
const DEFAULT_MULTILINGUAL_ALIAS: &str = "nemotron-3.5-asr-streaming-0.6b";
const MODEL_ALIAS_OPTION: &str = "foundry.model_alias";

#[derive(Debug, Default, Clone, Copy)]
pub struct FoundryTranscriberFactory;

impl TranscriberFactory for FoundryTranscriberFactory {
    fn backend_id(&self) -> BackendId {
        BackendId::new(FOUNDRY_TRANSCRIBER_BACKEND_ID)
    }

    fn create(
        &self,
        config: &TranscriberConfig,
    ) -> BackendResult<Box<dyn TranscriberBackend>> {
        if config.backend_id != self.backend_id() {
            return Err(FoundryLiveTranscriberBackend::error(
                BackendErrorKind::InvalidState,
                format!("factory cannot construct backend {}", config.backend_id),
            ));
        }
        let mut backend = FoundryLiveTranscriberBackend::new(&config.locale);
        if let Some(alias) = config.options.get(MODEL_ALIAS_OPTION) {
            backend.model_alias.clone_from(alias);
        }
        Ok(Box::new(backend))
    }
}

enum AudioCommand {
    Append {
        track_id: TrackId,
        bytes: Vec<u8>,
        sample_count: usize,
    },
}

#[derive(Clone, Copy)]
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
    audio: tokio::sync::mpsc::Sender<AudioCommand>,
    control: tokio::sync::mpsc::UnboundedSender<ControlCommand>,
    events: channel::Receiver<WorkerEvent>,
    join: Option<JoinHandle<()>>,
}

pub struct FoundryLiveTranscriberBackend {
    model_alias: String,
    language: Option<String>,
    worker: Option<Worker>,
    resamplers: HashMap<TrackId, StreamingResampler>,
    pending_events: VecDeque<TranscriptEvent>,
}

impl FoundryLiveTranscriberBackend {
    #[must_use]
    pub fn new(locale: impl AsRef<str>) -> Self {
        let (alias, language) = alias_for_locale(locale.as_ref());
        Self {
            model_alias: alias.to_owned(),
            language,
            worker: None,
            resamplers: HashMap::new(),
            pending_events: VecDeque::new(),
        }
    }

    #[must_use]
    pub fn with_model_alias(
        mut self,
        alias: impl Into<String>,
    ) -> Self {
        self.model_alias = alias.into();
        self
    }

    fn error(
        kind: BackendErrorKind,
        message: impl Into<String>,
    ) -> BackendError {
        BackendError::new(
            BackendId::new(FOUNDRY_TRANSCRIBER_BACKEND_ID),
            kind,
            message,
        )
    }

    fn stop_worker(
        &mut self,
        command: ControlCommand,
    ) -> BackendResult<()> {
        let Some(mut worker) = self.worker.take() else {
            return Ok(());
        };
        let worker_already_stopped = worker.control.send(command).is_err();
        let mut result = Ok(());
        while let Ok(event) = worker.events.recv() {
            match event {
                WorkerEvent::Transcript(event) if matches!(command, ControlCommand::Finish) => {
                    self.pending_events.push_back(event);
                },
                WorkerEvent::Transcript(_) => {},
                WorkerEvent::Finished => break,
                WorkerEvent::Failed(message) => {
                    result = Err(Self::error(BackendErrorKind::Internal, message));
                    break;
                },
            }
        }
        if let Some(join) = worker.join.take()
            && join.join().is_err()
            && result.is_ok()
        {
            result = Err(Self::error(
                BackendErrorKind::Internal,
                "Foundry worker panicked",
            ));
        }
        if worker_already_stopped && result.is_ok() {
            result = Err(Self::error(
                BackendErrorKind::Internal,
                "Foundry worker stopped before shutdown",
            ));
        }
        result
    }
}

impl TranscriberBackend for FoundryLiveTranscriberBackend {
    fn probe(&self) -> TranscriberProbe {
        let alias = self.model_alias.clone();
        let availability = match thread::Builder::new()
            .name("wisp-foundry-probe".into())
            .spawn(move || probe_model(&alias))
            .and_then(|join| {
                join.join()
                    .map_err(|_| std::io::Error::other("Foundry probe panicked"))
            }) {
            Ok(availability) => availability,
            Err(error) => Availability::Unavailable(UnavailableReason::InitializationFailed(
                error.to_string(),
            )),
        };
        TranscriberProbe {
            backend_id: BackendId::new(FOUNDRY_TRANSCRIBER_BACKEND_ID),
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
                "Foundry provider is already running",
            ));
        }
        if tracks.is_empty() {
            return Err(Self::error(
                BackendErrorKind::UnsupportedFormat,
                "Foundry requires at least one audio track",
            ));
        }

        let track_ids = tracks.iter().map(|track| track.id).collect::<Vec<_>>();
        let model_alias = self.model_alias.clone();
        let language = self.language.clone();
        let (audio_tx, audio_rx) = tokio::sync::mpsc::channel(AUDIO_QUEUE_CAPACITY);
        let (control_tx, control_rx) = tokio::sync::mpsc::unbounded_channel();
        let (event_tx, event_rx) = channel::unbounded();
        let (ready_tx, ready_rx) = channel::bounded(1);
        let join = thread::Builder::new()
            .name("wisp-foundry".into())
            .spawn(move || {
                let runtime = match Runtime::new() {
                    Ok(runtime) => runtime,
                    Err(error) => {
                        let _ = ready_tx
                            .send(Err(format!("could not create Foundry runtime: {error}")));
                        return;
                    },
                };
                runtime.block_on(run_worker(
                    &model_alias,
                    language,
                    track_ids,
                    audio_rx,
                    control_rx,
                    event_tx,
                    ready_tx,
                ));
            })
            .map_err(|error| {
                Self::error(
                    BackendErrorKind::Internal,
                    format!("could not start Foundry worker: {error}"),
                )
            })?;

        match ready_rx.recv() {
            Ok(Ok(())) => {
                self.worker = Some(Worker {
                    audio: audio_tx,
                    control: control_tx,
                    events: event_rx,
                    join: Some(join),
                });
                Ok(())
            },
            Ok(Err(message)) => {
                let _ = join.join();
                Err(Self::error(classify_start_error(&message), message))
            },
            Err(_) => {
                let _ = join.join();
                Err(Self::error(
                    BackendErrorKind::Internal,
                    "Foundry worker stopped during startup",
                ))
            },
        }
    }

    fn push(
        &mut self,
        frame: &AudioFrame,
    ) -> BackendResult<()> {
        let mono = pcm_to_mono_samples(frame).map_err(|format| {
            Self::error(
                BackendErrorKind::UnsupportedFormat,
                format!("Foundry supports f32/i16 PCM, received {format:?}"),
            )
        })?;
        let resampler = self
            .resamplers
            .entry(frame.track_id())
            .or_insert_with(|| StreamingResampler::new(frame.format().sample_rate, SAMPLE_RATE));
        if resampler.source_rate != frame.format().sample_rate {
            return Err(Self::error(
                BackendErrorKind::UnsupportedFormat,
                "Foundry does not support changing sample rate during a session",
            ));
        }
        let samples = resampler.push(&mono);
        let command = AudioCommand::Append {
            track_id: frame.track_id(),
            bytes: pcm_i16_le_bytes(&samples),
            sample_count: samples.len(),
        };
        let Some(worker) = &self.worker else {
            return Err(Self::error(
                BackendErrorKind::InvalidState,
                "Foundry provider is not running",
            ));
        };
        match worker.audio.try_send(command) {
            Ok(()) | Err(tokio::sync::mpsc::error::TrySendError::Full(_)) => Ok(()),
            Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => Err(Self::error(
                BackendErrorKind::Internal,
                "Foundry worker stopped",
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
        match worker.events.recv_timeout(timeout) {
            Ok(WorkerEvent::Transcript(event)) => Ok(Some(event)),
            Ok(WorkerEvent::Finished) | Err(channel::RecvTimeoutError::Timeout) => Ok(None),
            Ok(WorkerEvent::Failed(message)) => {
                Err(Self::error(BackendErrorKind::Internal, message))
            },
            Err(channel::RecvTimeoutError::Disconnected) => Err(Self::error(
                BackendErrorKind::Internal,
                "Foundry worker event channel disconnected",
            )),
        }
    }

    fn finish(&mut self) -> BackendResult<()> {
        self.stop_worker(ControlCommand::Finish)
    }

    fn abort(&mut self) -> BackendResult<()> {
        self.stop_worker(ControlCommand::Abort)
    }
}

impl Drop for FoundryLiveTranscriberBackend {
    fn drop(&mut self) {
        let _ = self.stop_worker(ControlCommand::Abort);
    }
}

async fn run_worker(
    model_alias: &str,
    language: Option<String>,
    track_ids: Vec<TrackId>,
    mut audio_rx: tokio::sync::mpsc::Receiver<AudioCommand>,
    mut control_rx: tokio::sync::mpsc::UnboundedReceiver<ControlCommand>,
    event_tx: channel::Sender<WorkerEvent>,
    ready_tx: channel::Sender<std::result::Result<(), String>>,
) {
    let cancellation = CancellationToken::new();
    let initialized =
        initialize_sessions(model_alias, language, &track_ids, &event_tx, &cancellation).await;
    let (sessions, mut stream_tasks, accepted_samples) = match initialized {
        Ok(value) => value,
        Err(message) => {
            let _ = ready_tx.send(Err(message));
            return;
        },
    };
    let _ = ready_tx.send(Ok(()));

    loop {
        tokio::select! {
            biased;
            command = control_rx.recv() => {
                let command = command.unwrap_or(ControlCommand::Abort);
                match command {
                    ControlCommand::Finish => {
                        audio_rx.close();
                        while let Some(command) = audio_rx.recv().await {
                            if let Err(message) = append_audio(
                                command,
                                &sessions,
                                &accepted_samples,
                                &cancellation,
                            ).await {
                                cancellation.cancel();
                                let message = append_cleanup_error(
                                    message,
                                    shutdown_sessions(
                                        &sessions,
                                        &mut stream_tasks,
                                        &cancellation,
                                    ).await,
                                );
                                let _ = event_tx.send(WorkerEvent::Failed(message));
                                return;
                            }
                        }
                    },
                    ControlCommand::Abort => cancellation.cancel(),
                }
                if let Err(error) =
                    shutdown_sessions(&sessions, &mut stream_tasks, &cancellation).await
                {
                    let _ = event_tx.send(WorkerEvent::Failed(error));
                    return;
                }
                let _ = event_tx.send(WorkerEvent::Finished);
                return;
            }
            command = audio_rx.recv() => {
                let Some(command) = command else {
                    continue;
                };
                if let Err(message) =
                    append_audio(command, &sessions, &accepted_samples, &cancellation).await
                {
                    cancellation.cancel();
                    let message = append_cleanup_error(
                        message,
                        shutdown_sessions(&sessions, &mut stream_tasks, &cancellation).await,
                    );
                    let _ = event_tx.send(WorkerEvent::Failed(message));
                    return;
                }
            }
        }
    }
}

type SessionMap = HashMap<TrackId, Arc<LiveAudioTranscriptionSession>>;
type AcceptedSamples = HashMap<TrackId, Arc<AtomicU64>>;
type StreamTasks = Vec<tokio::task::JoinHandle<()>>;

async fn append_audio(
    command: AudioCommand,
    sessions: &SessionMap,
    accepted_samples: &AcceptedSamples,
    cancellation: &CancellationToken,
) -> std::result::Result<(), String> {
    let AudioCommand::Append {
        track_id,
        bytes,
        sample_count,
    } = command;
    let session = sessions
        .get(&track_id)
        .ok_or_else(|| format!("received audio for unknown track {}", track_id.get()))?;
    session
        .append(&bytes, Some(cancellation.clone()))
        .await
        .map_err(|error| format!("could not append Foundry audio: {error}"))?;
    if let Some(total) = accepted_samples.get(&track_id) {
        total.fetch_add(sample_count as u64, Ordering::Relaxed);
    }
    Ok(())
}

async fn initialize_sessions(
    model_alias: &str,
    language: Option<String>,
    track_ids: &[TrackId],
    event_tx: &channel::Sender<WorkerEvent>,
    cancellation: &CancellationToken,
) -> std::result::Result<(SessionMap, StreamTasks, AcceptedSamples), String> {
    let manager = FoundryLocalManager::create(foundry_config())
        .map_err(|error| format!("could not initialize Foundry Local: {error}"))?;
    let model = manager
        .catalog()
        .get_model(model_alias)
        .await
        .map_err(|error| format!("could not resolve Foundry model {model_alias}: {error}"))?;
    if !model
        .is_cached()
        .await
        .map_err(|error| format!("could not inspect Foundry model cache: {error}"))?
    {
        return Err(format!("Foundry model is not cached: {model_alias}"));
    }
    model
        .load()
        .await
        .map_err(|error| format!("could not load Foundry model {model_alias}: {error}"))?;

    let mut sessions = HashMap::new();
    let mut tasks = Vec::new();
    let mut accepted = HashMap::new();
    for &track_id in track_ids {
        if sessions.contains_key(&track_id) {
            continue;
        }
        let mut session = model
            .create_audio_client()
            .create_live_transcription_session();
        session.settings.sample_rate = SAMPLE_RATE;
        session.settings.channels = 1;
        session.settings.bits_per_sample = 16;
        session.settings.language.clone_from(&language);
        if let Err(error) = session.start(Some(cancellation.clone())).await {
            let message = append_cleanup_error(
                format!("could not start Foundry track {}: {error}", track_id.get()),
                shutdown_sessions(&sessions, &mut tasks, cancellation).await,
            );
            return Err(message);
        }
        let mut stream = match session.get_stream().await {
            Ok(stream) => stream,
            Err(error) => {
                let mut cleanup_errors = Vec::new();
                if let Err(stop_error) = session.stop(Some(cancellation.clone())).await {
                    cleanup_errors.push(format!(
                        "could not stop Foundry track {}: {stop_error}",
                        track_id.get()
                    ));
                }
                if let Err(existing_error) =
                    shutdown_sessions(&sessions, &mut tasks, cancellation).await
                {
                    cleanup_errors.push(existing_error);
                }
                let message = format!("could not read Foundry track {}: {error}", track_id.get());
                return Err(if cleanup_errors.is_empty() {
                    message
                } else {
                    format!(
                        "{message}; cleanup also failed: {}",
                        cleanup_errors.join("; ")
                    )
                });
            },
        };
        let session = Arc::new(session);
        let sample_count = Arc::new(AtomicU64::new(0));
        let task_event_tx = event_tx.clone();
        let task_sample_count = Arc::clone(&sample_count);
        tasks.push(tokio::spawn(async move {
            let mut state = SegmentState::default();
            while let Some(response) = stream.next().await {
                match response {
                    Ok(response) => {
                        state.fallback_end_seconds =
                            samples_to_seconds(task_sample_count.load(Ordering::Relaxed));
                        if let Some(event) = response_to_event(track_id, &response, &mut state) {
                            let _ = task_event_tx.send(WorkerEvent::Transcript(event));
                        }
                    },
                    Err(error) => {
                        let _ = task_event_tx.send(WorkerEvent::Failed(format!(
                            "Foundry response failed: {error}"
                        )));
                        return;
                    },
                }
            }
        }));
        sessions.insert(track_id, session);
        accepted.insert(track_id, sample_count);
    }
    Ok((sessions, tasks, accepted))
}

async fn shutdown_sessions(
    sessions: &SessionMap,
    stream_tasks: &mut StreamTasks,
    cancellation: &CancellationToken,
) -> std::result::Result<(), String> {
    let mut errors = Vec::new();
    for (track_id, session) in sessions {
        if let Err(error) = session.stop(Some(cancellation.clone())).await {
            errors.push(format!(
                "could not stop Foundry track {}: {error}",
                track_id.get()
            ));
        }
    }
    let stop_failed = !errors.is_empty();
    for task in stream_tasks.drain(..) {
        if stop_failed {
            task.abort();
        }
        if let Err(error) = task.await
            && !error.is_cancelled()
        {
            errors.push(format!("Foundry response task failed: {error}"));
        }
    }
    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors.join("; "))
    }
}

fn append_cleanup_error(
    message: String,
    cleanup: std::result::Result<(), String>,
) -> String {
    match cleanup {
        Ok(()) => message,
        Err(error) => format!("{message}; cleanup also failed: {error}"),
    }
}

fn probe_model(alias: &str) -> Availability {
    let provider = match FoundryLocalProvider::new("wisp") {
        Ok(provider) => provider,
        Err(error) => {
            return Availability::Unavailable(UnavailableReason::InitializationFailed(
                error.to_string(),
            ));
        },
    };
    match provider.status(&ModelId::new(alias)) {
        Ok(ModelStatus::Downloaded | ModelStatus::Loaded) => Availability::Available,
        Ok(ModelStatus::NotDownloaded) => {
            Availability::Unavailable(UnavailableReason::MissingModel(alias.to_owned()))
        },
        Ok(ModelStatus::Unavailable(reason)) => {
            Availability::Unavailable(UnavailableReason::InitializationFailed(reason))
        },
        Err(error) => {
            Availability::Unavailable(UnavailableReason::InitializationFailed(error.to_string()))
        },
    }
}

fn foundry_config() -> FoundryLocalConfig {
    FoundryLocalConfig::new("wisp").web_service_urls("http://127.0.0.1:0")
}

fn classify_start_error(message: &str) -> BackendErrorKind {
    if message.contains("not cached") {
        BackendErrorKind::MissingModel
    } else {
        BackendErrorKind::Internal
    }
}

#[allow(clippy::cast_precision_loss)]
fn samples_to_seconds(samples: u64) -> f64 {
    samples as f64 / f64::from(SAMPLE_RATE)
}

#[derive(Debug, Default)]
struct SegmentState {
    segment_id: u64,
    utterance_start_seconds: f64,
    fallback_end_seconds: f64,
}

fn response_to_event(
    track_id: TrackId,
    response: &LiveAudioTranscriptionResponse,
    state: &mut SegmentState,
) -> Option<TranscriptEvent> {
    let text = response
        .content
        .first()
        .map(|content| content.text.trim())
        .filter(|text| !text.is_empty())?
        .to_owned();
    let start_seconds = response.start_time.unwrap_or(state.utterance_start_seconds);
    let end_seconds = response
        .end_time
        .unwrap_or(state.fallback_end_seconds.max(start_seconds));
    let segment = TranscriptSegment {
        track_id,
        segment_id: TranscriptSegmentId::new(state.segment_id),
        text,
        start_seconds,
        end_seconds,
        confidence_mean: None,
        confidence_min: None,
    };
    if response.is_final {
        state.segment_id = state.segment_id.saturating_add(1);
        state.utterance_start_seconds = end_seconds;
        Some(TranscriptEvent::Final(segment))
    } else {
        Some(TranscriptEvent::Partial(segment))
    }
}

fn alias_for_locale(locale: &str) -> (&'static str, Option<String>) {
    let language = locale
        .split(['-', '_'])
        .next()
        .filter(|language| !language.is_empty())
        .map(str::to_ascii_lowercase);
    match language.as_deref() {
        Some("en") => (DEFAULT_ENGLISH_ALIAS, Some("en".to_owned())),
        Some(language) => (DEFAULT_MULTILINGUAL_ALIAS, Some(language.to_owned())),
        None => (DEFAULT_MULTILINGUAL_ALIAS, None),
    }
}

#[allow(clippy::cast_possible_truncation)]
fn pcm_i16_le_bytes(samples: &[f32]) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(samples.len() * size_of::<i16>());
    for &sample in samples {
        let scaled = (sample.clamp(-1.0, 1.0) * 32_768.0)
            .round()
            .clamp(f32::from(i16::MIN), f32::from(i16::MAX)) as i16;
        bytes.extend_from_slice(&scaled.to_le_bytes());
    }
    bytes
}

#[cfg(test)]
mod tests {
    use foundry_local_sdk::{ContentPart, LiveAudioTranscriptionResponse};
    use wisp_core::{TrackId, TranscriptEvent};

    use super::{
        DEFAULT_ENGLISH_ALIAS, DEFAULT_MULTILINGUAL_ALIAS, SegmentState, alias_for_locale,
        pcm_i16_le_bytes, response_to_event,
    };

    fn response(
        text: &str,
        is_final: bool,
        start_time: Option<f64>,
        end_time: Option<f64>,
    ) -> LiveAudioTranscriptionResponse {
        LiveAudioTranscriptionResponse {
            content: vec![ContentPart {
                text: text.to_owned(),
                transcript: text.to_owned(),
            }],
            is_final,
            start_time,
            end_time,
            id: None,
        }
    }

    #[test]
    fn locale_selects_streaming_alias_and_language() {
        assert_eq!(
            alias_for_locale("en-US"),
            (DEFAULT_ENGLISH_ALIAS, Some("en".to_owned()))
        );
        assert_eq!(
            alias_for_locale("ja-JP"),
            (DEFAULT_MULTILINGUAL_ALIAS, Some("ja".to_owned()))
        );
        assert_eq!(alias_for_locale(""), (DEFAULT_MULTILINGUAL_ALIAS, None));
    }

    #[test]
    fn pcm_conversion_clamps_and_writes_little_endian() {
        assert_eq!(
            pcm_i16_le_bytes(&[-2.0, 0.0, 1.0, 2.0]),
            [0x00, 0x80, 0x00, 0x00, 0xff, 0x7f, 0xff, 0x7f]
        );
    }

    #[test]
    fn partial_and_final_share_segment_then_advance() {
        let mut state = SegmentState {
            fallback_end_seconds: 1.25,
            ..SegmentState::default()
        };
        let partial = response_to_event(
            TrackId::MICROPHONE,
            &response(" hello ", false, None, None),
            &mut state,
        )
        .expect("partial");
        assert!(matches!(partial, TranscriptEvent::Partial(_)));
        assert_eq!(partial.segment().segment_id.get(), 0);
        assert!((partial.segment().end_seconds - 1.25).abs() < f64::EPSILON);

        let final_event = response_to_event(
            TrackId::MICROPHONE,
            &response("hello world", true, Some(0.2), Some(1.5)),
            &mut state,
        )
        .expect("final");
        assert!(final_event.is_final());
        assert_eq!(final_event.segment().segment_id.get(), 0);

        state.fallback_end_seconds = 2.0;
        let next = response_to_event(
            TrackId::MICROPHONE,
            &response("next", true, None, None),
            &mut state,
        )
        .expect("next final");
        assert_eq!(next.segment().segment_id.get(), 1);
        assert!((next.segment().start_seconds - 1.5).abs() < f64::EPSILON);
    }

    #[test]
    fn empty_response_is_ignored() {
        assert!(
            response_to_event(
                TrackId::SYSTEM,
                &response("  ", true, None, None),
                &mut SegmentState::default()
            )
            .is_none()
        );
    }
}
