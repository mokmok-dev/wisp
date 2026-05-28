//! Raw FFI bindings to the `WispAudioKit` Swift framework.
//!
//! The Swift side lives under `native/WispAudioKit/` and is built by this
//! crate's `build.rs` into a static library (`libWispAudioKit.a`). The C ABI
//! surface is hand-mirrored from `native/WispAudioKit/include/wisp_audiokit.h`.
//!
//! `WispAudioKit` is macOS-only, so the `extern "C"` block below is gated to
//! `target_os = "macos"`. On other targets the crate exposes only the type
//! aliases and constants — there are no function symbols to link against,
//! and consumers must (and do) cfg-gate any code that touches them.

#![allow(unsafe_code, non_camel_case_types)]

use std::os::raw::{c_char, c_void};
#[cfg(target_os = "macos")]
use std::os::raw::c_int;

/// Opaque handle for a `WispSession`. Construct via [`wisp_session_new`].
#[repr(C)]
pub struct WispSession {
    _private: [u8; 0],
}

/// Source identifier passed to [`WispResultCallback`].
pub const WISP_SOURCE_MIC: i32 = 0;
pub const WISP_SOURCE_SYSTEM: i32 = 1;

/// Callback invoked for each transcription result.
///
/// `text_utf8` is NOT NUL-terminated — use `text_len`. The pointer is valid
/// only for the duration of the call; copy the bytes if you need to keep
/// them.
pub type WispResultCallback = unsafe extern "C" fn(
    source: i32,
    segment_id: u64,
    text_utf8: *const c_char,
    text_len: usize,
    start_seconds: f64,
    end_seconds: f64,
    user_data: *mut c_void,
);

/// Callback invoked for log lines. Same lifetime rules as
/// [`WispResultCallback`].
pub type WispLogCallback =
    unsafe extern "C" fn(message_utf8: *const c_char, message_len: usize, user_data: *mut c_void);

#[cfg(target_os = "macos")]
unsafe extern "C" {
    /// Returns a static, NUL-terminated UTF-8 version string for the
    /// `WispAudioKit` library. The pointer lives for the lifetime of the
    /// process; the caller must not free it.
    pub fn wisp_audiokit_version() -> *const c_char;

    /// Construct a new session. Returns null on failure.
    pub fn wisp_session_new(
        output_dir: *const c_char,
        locale: *const c_char,
        on_result: Option<WispResultCallback>,
        on_log: Option<WispLogCallback>,
        user_data: *mut c_void,
    ) -> *mut WispSession;

    /// Start capture + transcription. Blocks until ready or failed.
    /// Returns 0 on success, non-zero on failure.
    pub fn wisp_session_start(session: *mut WispSession) -> c_int;

    /// Stop capture and wait for results to drain. Blocks.
    pub fn wisp_session_stop(session: *mut WispSession);

    /// Free the session handle. Caller must have called `wisp_session_stop`.
    pub fn wisp_session_free(session: *mut WispSession);

    /// Returns the last error message recorded against this session, or
    /// null. Invalidated by the next mutating call.
    pub fn wisp_session_last_error_message(session: *mut WispSession) -> *const c_char;
}

