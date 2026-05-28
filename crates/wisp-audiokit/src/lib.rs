//! Safe Rust wrapper over `WispAudioKit` (the Swift framework).
//!
//! Wraps the raw FFI from `wisp-audiokit-sys`. macOS-only; on other platforms
//! everything is stubbed out so the workspace stays buildable.

#[cfg(target_os = "macos")]
mod imp {
    use std::ffi::CStr;

    /// Returns the `WispAudioKit` library version (e.g. `"0.1.0"`).
    ///
    /// # Panics
    /// Panics if the Swift side's version string is not valid UTF-8. It ships
    /// as a static ASCII constant, so this only fires on build-time binary
    /// corruption.
    #[must_use]
    #[allow(clippy::expect_used)]
    pub fn version() -> &'static str {
        // SAFETY: `wisp_audiokit_version` returns a static UTF-8 C string
        // that lives for the lifetime of the process and is never null per
        // its Swift implementation in `Bridge.swift`.
        unsafe {
            let ptr = wisp_audiokit_sys::wisp_audiokit_version();
            CStr::from_ptr(ptr)
                .to_str()
                .expect("`WispAudioKit` version is valid UTF-8")
        }
    }
}

#[cfg(not(target_os = "macos"))]
mod imp {
    /// Returns the `WispAudioKit` library version. Always empty on non-macOS
    /// targets — `WispAudioKit` only ships for macOS.
    #[must_use]
    pub fn version() -> &'static str {
        ""
    }
}

pub use imp::version;

#[cfg(all(test, target_os = "macos"))]
mod tests {
    use super::version;

    #[test]
    fn version_is_nonempty_and_dotted() {
        let v = version();
        assert!(!v.is_empty(), "version must be non-empty");
        assert!(
            v.chars().next().is_some_and(|c| c.is_ascii_digit()),
            "version should start with a digit, got: {v}"
        );
        assert!(
            v.contains('.'),
            "version should be dotted (e.g. '0.1.0'), got: {v}"
        );
    }
}
