//! Raw FFI bindings to the `WispAudioKit` Swift framework.
//!
//! The Swift side lives under `native/WispAudioKit/` and is built by this
//! crate's `build.rs` into a static library (`libWispAudioKit.a`). The C ABI
//! surface is hand-mirrored from `native/WispAudioKit/include/wisp_audiokit.h`.
//!
//! On non-macOS targets every binding is a stub returning a null pointer so
//! the workspace stays buildable.

#![allow(unsafe_code)]

use std::os::raw::c_char;

#[cfg(target_os = "macos")]
unsafe extern "C" {
    /// Returns a static, NUL-terminated UTF-8 version string for the
    /// `WispAudioKit` library. The pointer lives for the lifetime of the
    /// process; the caller must not free it.
    pub fn wisp_audiokit_version() -> *const c_char;
}

#[cfg(not(target_os = "macos"))]
/// Stub: `WispAudioKit` is macOS-only. Always returns a null pointer.
///
/// # Safety
/// Trivially safe; included only so the workspace builds on non-macOS hosts.
#[must_use]
pub unsafe fn wisp_audiokit_version() -> *const c_char {
    std::ptr::null()
}
