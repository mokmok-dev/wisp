//! WASAPI capture for the default microphone or render-device loopback.

use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use crossbeam_channel::Sender;
use hound::{SampleFormat, WavSpec, WavWriter};
use parking_lot::Mutex;
use rubato::{FftFixedIn, Resampler};
use wasapi::{DeviceEnumerator, Direction, SampleType, StreamMode, WaveFormat, initialize_mta};

/// Mono PCM at 16 kHz — what Vosk expects.
pub const TARGET_SAMPLE_RATE: u32 = 16_000;

/// One chunk of captured audio for the speech engine.
#[derive(Debug, Clone)]
pub struct PcmChunk {
    pub samples: Vec<i16>,
}

/// Capture from the default microphone.
pub fn spawn_mic(
    wav_path: std::path::PathBuf,
    pcm_tx: Sender<PcmChunk>,
    stop: Arc<AtomicBool>,
) -> JoinHandle<()> {
    thread::Builder::new()
        .name("wisp-mic-capture".into())
        .spawn(move || {
            if let Err(err) = capture_loop(false, wav_path, pcm_tx, stop) {
                eprintln!("wisp: mic capture failed: {err}");
            }
        })
        .expect("spawn mic capture thread")
}

/// Capture system audio via WASAPI loopback on the default render device.
pub fn spawn_loopback(
    wav_path: std::path::PathBuf,
    pcm_tx: Sender<PcmChunk>,
    stop: Arc<AtomicBool>,
) -> JoinHandle<()> {
    thread::Builder::new()
        .name("wisp-loopback-capture".into())
        .spawn(move || {
            if let Err(err) = capture_loop(true, wav_path, pcm_tx, stop) {
                eprintln!("wisp: loopback capture failed: {err}");
            }
        })
        .expect("spawn loopback capture thread")
}

fn capture_loop(
    loopback: bool,
    wav_path: std::path::PathBuf,
    pcm_tx: Sender<PcmChunk>,
    stop: Arc<AtomicBool>,
) -> Result<(), String> {
    initialize_mta().map_err(|e| format!("COM init failed: {e}"))?;

    let enumerator = DeviceEnumerator::new().map_err(|e| e.to_string())?;
    let device_dir = if loopback {
        Direction::Render
    } else {
        Direction::Capture
    };
    let device = enumerator
        .get_default_device(&device_dir)
        .map_err(|e| e.to_string())?;
    let mut audio_client = device.get_iaudioclient().map_err(|e| e.to_string())?;

    let mix_format = audio_client.get_mixformat().map_err(|e| e.to_string())?;
    let sample_rate = mix_format.get_samplespersec();
    let channels = mix_format.get_nchannels();
    let bits = mix_format.get_bitspersample();
    let sample_type = mix_format.get_subformat().map_err(|e| e.to_string())?;
    let blockalign = mix_format.get_blockalign() as usize;

    let (_, min_time) = audio_client
        .get_device_period()
        .map_err(|e| e.to_string())?;
    let mode = StreamMode::EventsShared {
        autoconvert: true,
        buffer_duration_hns: min_time,
    };
    audio_client
        .initialize_client(&mix_format, &Direction::Capture, &mode)
        .map_err(|e| e.to_string())?;

    let h_event = audio_client
        .set_get_eventhandle()
        .map_err(|e| e.to_string())?;
    let capture_client = audio_client
        .get_audiocaptureclient()
        .map_err(|e| e.to_string())?;

    let wav_spec = WavSpec {
        channels: 1,
        sample_rate: TARGET_SAMPLE_RATE,
        bits_per_sample: 16,
        sample_format: SampleFormat::Int,
    };
    let wav_writer = Mutex::new(
        WavWriter::create(&wav_path, wav_spec)
            .map_err(|e| format!("create wav {}: {e}", wav_path.display()))?,
    );

    let mut resampler = build_resampler(sample_rate as usize)?;
    let mut raw_queue: VecDeque<u8> = VecDeque::new();
    let chunk_frames = 1024usize;
    let chunk_bytes = blockalign * chunk_frames;

    audio_client.start_stream().map_err(|e| e.to_string())?;

    while !stop.load(Ordering::Relaxed) {
        capture_client
            .read_from_device_to_deque(&mut raw_queue)
            .map_err(|e| e.to_string())?;

        while raw_queue.len() >= chunk_bytes {
            let mut chunk_bytes_vec = vec![0u8; chunk_bytes];
            for byte in &mut chunk_bytes_vec {
                *byte = raw_queue.pop_front().unwrap_or(0);
            }
            let mono = bytes_to_mono_f32(&chunk_bytes_vec, channels as usize, bits, sample_type);
            let resampled = resample_chunk(&mut resampler, &mono, sample_rate)?;
            let pcm_i16: Vec<i16> = resampled
                .iter()
                .map(|&s| (s.clamp(-1.0, 1.0) * i16::MAX as f32) as i16)
                .collect();

            if pcm_i16.is_empty() {
                continue;
            }

            {
                let writer = wav_writer.lock();
                for sample in &pcm_i16 {
                    writer
                        .write_sample(*sample)
                        .map_err(|e| format!("wav write: {e}"))?;
                }
            }
            let _ = pcm_tx.send(PcmChunk { samples: pcm_i16 });
        }

        if h_event.wait_for_event(500).is_err() {
            // Timeout — keep looping until `stop` is set.
        }
    }

    audio_client.stop_stream().ok();
    wav_writer.lock().finalize().map_err(|e| e.to_string())?;
    Ok(())
}

fn build_resampler(input_rate: usize) -> Result<FftFixedIn<f32>, String> {
    let chunk_in = 1024usize;
    FftFixedIn::<f32>::new(input_rate, TARGET_SAMPLE_RATE as usize, chunk_in, 1, 1)
        .map_err(|e| format!("resampler: {e}"))
}

fn resample_chunk(
    resampler: &mut FftFixedIn<f32>,
    mono: &[f32],
    input_rate: u32,
) -> Result<Vec<f32>, String> {
    if mono.is_empty() {
        return Ok(Vec::new());
    }
    if input_rate == TARGET_SAMPLE_RATE {
        return Ok(mono.to_vec());
    }
    let waves_in = vec![mono.to_vec()];
    let waves_out = resampler
        .process(&waves_in, None)
        .map_err(|e| format!("resample: {e}"))?;
    Ok(waves_out.into_iter().flatten().collect())
}

fn bytes_to_mono_f32(
    bytes: &[u8],
    channels: usize,
    bits: u16,
    sample_type: SampleType,
) -> Vec<f32> {
    let channels = channels.max(1);
    match (sample_type, bits) {
        (SampleType::Float, 32) => {
            let samples: Vec<f32> = bytes
                .chunks_exact(4)
                .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
                .collect();
            let frames = samples.len() / channels;
            (0..frames)
                .map(|frame| {
                    let start = frame * channels;
                    let sum: f32 = samples[start..start + channels].iter().sum();
                    sum / channels as f32
                })
                .collect()
        },
        (SampleType::Int, 16) => {
            let samples: Vec<i16> = bytes
                .chunks_exact(2)
                .map(|c| i16::from_le_bytes([c[0], c[1]]))
                .collect();
            let frames = samples.len() / channels;
            (0..frames)
                .map(|frame| {
                    let start = frame * channels;
                    let sum: i32 = samples[start..start + channels]
                        .iter()
                        .map(|&s| i32::from(s))
                        .sum();
                    (sum as f32 / channels as f32) / i16::MAX as f32
                })
                .collect()
        },
        _ => Vec::new(),
    }
}

/// Probe whether the default capture device can be opened (microphone permission).
pub fn probe_microphone() -> bool {
    initialize_mta().is_ok()
        && (|| {
            let enumerator = DeviceEnumerator::new().ok()?;
            let device = enumerator.get_default_device(&Direction::Capture).ok()?;
            let mut client = device.get_iaudioclient().ok()?;
            let format = WaveFormat::new(16, 16, &SampleType::Int, 16_000, 1, None);
            let mode = StreamMode::EventsShared {
                autoconvert: true,
                buffer_duration_hns: 100_000,
            };
            client
                .initialize_client(&format, &Direction::Capture, &mode)
                .ok()?;
            client.stop_stream().ok();
            Some(())
        })()
        .is_some()
}

/// Short sleep used while tearing down capture threads.
pub fn join_with_timeout(
    handle: JoinHandle<()>,
    timeout: Duration,
) {
    let start = std::time::Instant::now();
    while start.elapsed() < timeout {
        if handle.is_finished() {
            break;
        }
        thread::sleep(Duration::from_millis(10));
    }
}
