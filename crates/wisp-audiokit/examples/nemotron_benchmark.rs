//! Decode one mono WAV through Wisp's Nemotron provider.
//!
//! Usage:
//! `cargo run -p wisp-audiokit --release --example nemotron_benchmark -- MODEL_DIR AUDIO.wav ja-JP`

use std::error::Error;
use std::io;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use sherpa_onnx::Wave;
use wisp_audiokit::{
    AudioFrame, MonotonicTimestamp, NemotronTranscriberBackend, SourceKind, TrackDescriptor,
    TrackId, TranscriberBackend,
};

#[allow(clippy::cast_precision_loss)]
fn main() -> Result<(), Box<dyn Error>> {
    let mut args = std::env::args_os().skip(1);
    let model_dir = required_arg(args.next(), "MODEL_DIR")?;
    let audio_path = required_arg(args.next(), "AUDIO.wav")?;
    let locale = args.next().map_or_else(
        || "ja-JP".into(),
        |value| value.to_string_lossy().into_owned(),
    );
    let wave = Wave::read(&audio_path.to_string_lossy()).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("could not read mono WAV: {}", audio_path.display()),
        )
    })?;
    let sample_rate = u32::try_from(wave.sample_rate())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "negative WAV sample rate"))?;
    let feed_chunk_samples = usize::try_from(sample_rate / 10)?;
    let audio_seconds = wave.samples().len() as f64 / f64::from(sample_rate);
    let mut backend = NemotronTranscriberBackend::new(model_dir, locale);
    let tracks = [TrackDescriptor {
        id: TrackId::MICROPHONE,
        source: SourceKind::Microphone,
        name: "Benchmark".into(),
    }];

    let started_at = Instant::now();
    backend.start(&tracks)?;
    for (sequence, samples) in wave.samples().chunks(feed_chunk_samples).enumerate() {
        let offset = sequence.saturating_mul(feed_chunk_samples);
        let timestamp = Duration::from_secs_f64(offset as f64 / f64::from(sample_rate));
        let frame = AudioFrame::from_f32(
            TrackId::MICROPHONE,
            SourceKind::Microphone,
            u64::try_from(sequence)?,
            MonotonicTimestamp::from_duration(timestamp),
            sample_rate,
            1,
            samples.to_vec(),
        )?;
        backend.push(&frame)?;
    }
    backend.finish()?;
    while let Some(event) = backend.next_event(Duration::ZERO)? {
        if event.is_final() {
            println!("{}", event.segment().text);
        }
    }
    let elapsed = started_at.elapsed();
    println!(
        "audio={audio_seconds:.2}s elapsed={:.2}s end-to-end RTF={:.3}",
        elapsed.as_secs_f64(),
        elapsed.as_secs_f64() / audio_seconds
    );
    Ok(())
}

fn required_arg(
    value: Option<std::ffi::OsString>,
    name: &str,
) -> Result<PathBuf, io::Error> {
    value.map(PathBuf::from).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("missing argument: {name}"),
        )
    })
}
