//! Raw FFI bindings to the `WispAudioKit` Swift framework.
//!
//! The Swift side lives under `native/WispAudioKit/` and is built by this
//! crate's `build.rs` into a static library (`libWispAudioKit.a`). The C ABI
//! surface is hand-mirrored from `native/WispAudioKit/include/wisp_audiokit.h`.
//!
//! The `extern "C"` block is available on macOS (Swift `WispAudioKit`) and
//! Windows (`wisp-audiokit-win`). On other targets the crate exposes only the
//! type aliases and constants.

#![allow(unsafe_code, non_camel_case_types)]

// Pull the Windows staticlib/rlib into the link line so the `extern "C"` symbols
// below resolve for dependents (macOS uses `build.rs` + Swift instead).
#[cfg(all(target_os = "windows", feature = "windows-backend"))]
extern crate wisp_audiokit_win;

#[cfg(any(target_os = "macos", target_os = "windows"))]
use std::os::raw::c_int;
use std::os::raw::{c_char, c_void};

/// Opaque handle for a `WispSession`. Construct via [`wisp_session_new`].
#[repr(C)]
pub struct WispSession {
    _private: [u8; 0],
}

/// Source identifier passed to [`WispResultCallback`].
pub const WISP_SOURCE_MIC: i32 = 0;
pub const WISP_SOURCE_SYSTEM: i32 = 1;

/// Permission identifiers passed to [`wisp_permission_status`] /
/// [`wisp_permission_request`].
pub const WISP_PERMISSION_MICROPHONE: i32 = 0;
pub const WISP_PERMISSION_SPEECH_RECOGNITION: i32 = 1;

/// Status returned by [`wisp_permission_status`] /
/// [`wisp_permission_request`]. Negative values mean "invalid permission id".
pub const WISP_PERMISSION_STATUS_UNDETERMINED: i32 = 0;
pub const WISP_PERMISSION_STATUS_DENIED: i32 = 1;
pub const WISP_PERMISSION_STATUS_GRANTED: i32 = 2;
pub const WISP_PERMISSION_STATUS_RESTRICTED: i32 = 3;

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

#[cfg(any(target_os = "macos", target_os = "windows"))]
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

    /// Returns the current status of the given permission without prompting.
    /// `permission` is one of `WISP_PERMISSION_*`; the return value is a
    /// `WISP_PERMISSION_STATUS_*` value, or a negative number for an
    /// unknown permission id.
    pub fn wisp_permission_status(permission: i32) -> c_int;

    /// Trigger the OS permission prompt (only if the status is currently
    /// undetermined) and block until the user responds. Returns the
    /// resulting `WISP_PERMISSION_STATUS_*`. Safe to call from any thread —
    /// the macOS APIs marshal the dialog to the main thread internally.
    pub fn wisp_permission_request(permission: i32) -> c_int;
}
