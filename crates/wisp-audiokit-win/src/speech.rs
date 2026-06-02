//! Vosk-based streaming transcription for one audio source.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::{self, JoinHandle};
use std::time::Instant;

use crossbeam_channel::Receiver;
use vosk::{CompleteResult, DecodingState, Model, Recognizer};

use crate::capture::{PcmChunk, TARGET_SAMPLE_RATE, join_with_timeout};

/// Callback for transcription updates (`source` is `WISP_SOURCE_*`).
pub type ResultCallback = Box<dyn Fn(i32, u64, String, f64, f64) + Send>;
pub type LogCallback = Box<dyn Fn(String) + Send>;

/// Run Vosk on PCM chunks until `stop` is set.
pub fn spawn_pipeline(
    source: i32,
    model: Arc<Model>,
    pcm_rx: Receiver<PcmChunk>,
    on_result: ResultCallback,
    on_log: LogCallback,
    stop: Arc<AtomicBool>,
) -> JoinHandle<()> {
    thread::Builder::new()
        .name(format!("wisp-vosk-{source}"))
        .spawn(move || {
            let Some(mut recognizer) = Recognizer::new(&model, TARGET_SAMPLE_RATE as f32) else {
                on_log("Failed to create Vosk recognizer".into());
                return;
            };
            recognizer.set_words(true);
            recognizer.set_partial_words(true);

            let started = Instant::now();
            let mut segment_id: u64 = 0;
            let mut last_partial = String::new();

            while !stop.load(Ordering::Relaxed) {
                match pcm_rx.recv_timeout(std::time::Duration::from_millis(100)) {
                    Ok(chunk) => {
                        if chunk.samples.is_empty() {
                            continue;
                        }
                        match recognizer.accept_waveform(&chunk.samples) {
                            Ok(DecodingState::Running) => {
                                let partial = recognizer.partial_result();
                                let text = partial.partial.trim().to_string();
                                if !text.is_empty() && text != last_partial {
                                    last_partial = text.clone();
                                    let now = started.elapsed().as_secs_f64();
                                    on_result(source, segment_id, text, (now - 1.0).max(0.0), now);
                                }
                            },
                            Ok(DecodingState::Finalized) => {
                                let final_text = complete_text(&recognizer.result());
                                if !final_text.is_empty() {
                                    let now = started.elapsed().as_secs_f64();
                                    on_result(
                                        source,
                                        segment_id,
                                        final_text,
                                        (now - 2.0).max(0.0),
                                        now,
                                    );
                                    segment_id = segment_id.saturating_add(1);
                                    last_partial.clear();
                                }
                            },
                            Ok(DecodingState::Failed) => {
                                on_log("Vosk decoding failed".into());
                            },
                            Err(err) => {
                                on_log(format!("Vosk accept_waveform: {err}"));
                            },
                        }
                    },
                    Err(crossbeam_channel::RecvTimeoutError::Timeout) => {},
                    Err(crossbeam_channel::RecvTimeoutError::Disconnected) => break,
                }
            }

            let final_text = complete_text(&recognizer.final_result());
            if !final_text.is_empty() {
                let now = started.elapsed().as_secs_f64();
                on_result(source, segment_id, final_text, (now - 2.0).max(0.0), now);
            }
        })
        .expect("spawn vosk pipeline thread")
}

/// Locate a Vosk model directory for `locale` (e.g. `ja-JP`).
pub fn resolve_model_path(
    locale: &str,
    data_root: &std::path::Path,
) -> Option<std::path::PathBuf> {
    if let Ok(path) = std::env::var("WISP_VOSK_MODEL") {
        let path = std::path::PathBuf::from(path);
        if path.exists() {
            return Some(path);
        }
    }

    let models_dir = data_root.join("models");
    let candidates = model_candidates_for_locale(locale);
    for name in candidates {
        let path = models_dir.join(name);
        if path.exists() {
            return Some(path);
        }
    }
    None
}

pub fn model_candidates_for_locale(locale: &str) -> &'static [&'static str] {
    match locale {
        "ja" | "ja-JP" => &["vosk-model-small-ja-0.22", "vosk-model-ja-0.22"],
        "en" | "en-US" => &["vosk-model-small-en-us-0.15", "vosk-model-en-us-0.22"],
        _ => &["vosk-model-small-en-us-0.15"],
    }
}

/// Load a shared Vosk model from disk.
pub fn load_model(path: &std::path::Path) -> Option<Model> {
    Model::new(path.to_str()?)
}

pub fn stop_pipeline(handle: JoinHandle<()>) {
    join_with_timeout(handle, std::time::Duration::from_secs(3));
}

fn complete_text(result: &CompleteResult<'_>) -> String {
    match result {
        CompleteResult::Single(s) => s.text.trim().to_string(),
        CompleteResult::Multiple(m) => m
            .alternatives
            .first()
            .map(|a| a.text.trim().to_string())
            .unwrap_or_default(),
    }
}
