//! Records a short Ogg/Opus session through the macOS capture backend.
//!
//! This is a diagnostic tool for exercising the Rust Ogg/Opus recorder outside
//! the desktop app. It creates a recording-only capture session, lets Core
//! Audio collect microphone and system PCM for `seconds` seconds, then stops
//! gracefully (writing EOS) so the produced `mic.ogg` / `system.ogg` can be
//! inspected or played back.
//!
//! It requires microphone permission (and, for a non-empty `system.ogg`,
//! screen-recording permission, since system audio uses a Process Tap).
//! It is deliberately not part of the test suite; run it manually with:
//!
//! ```sh
//! cargo run -p wisp-audiokit --example record -- <output-dir> <seconds>
//! ```

#![cfg(target_os = "macos")]

use std::error::Error;
use std::path::Path;
use std::time::{Duration, Instant};

use wisp_audiokit::{CaptureBackend, MacosCaptureBackend, ShutdownMode};

fn main() -> Result<(), Box<dyn Error>> {
    let mut args = std::env::args().skip(1);
    let output_dir = args
        .next()
        .unwrap_or_else(|| "/tmp/wisp-rec-test".to_owned());
    let seconds = args
        .next()
        .and_then(|s| s.parse::<u64>().ok())
        .map_or(5, |s| s.max(1));

    let output = Path::new(&output_dir);
    std::fs::create_dir_all(output)?;

    let mut backend = MacosCaptureBackend::new_recording_only(output, "ja-JP")?;
    backend.start()?;
    eprintln!(
        "Recording {} second(s) into {} …",
        seconds,
        output.display()
    );

    let deadline = Instant::now() + Duration::from_secs(seconds);
    while Instant::now() < deadline {
        // Drain events to keep the recorder fed and observe failures.
        backend.next_event(Duration::from_millis(100))?;
    }

    backend.stop(ShutdownMode::Graceful)?;
    eprintln!("Stopped. Output: {}", output.display());
    for name in ["mic.ogg", "system.ogg"] {
        let path = output.join(name);
        let size = std::fs::metadata(&path).map_or(0, |m| m.len());
        eprintln!("  {}: {} bytes", path.display(), size);
    }
    Ok(())
}
