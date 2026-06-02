//! Windows implementation of the `WispAudioKit` C ABI (`wisp_audiokit.h`).
//!
//! Captures microphone and system (loopback) audio via WASAPI, writes WAV
//! files, and transcribes each stream with Vosk.

mod capture;
mod ffi;
mod permissions;
mod session;
mod speech;

pub use ffi::*;
