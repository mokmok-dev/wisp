//! Safe Rust wrapper over `WispAudioKit` (the Swift framework).
//!
//! Wraps the raw FFI from `wisp-audiokit-sys`. Available on macOS and Windows;
//! on other platforms everything is stubbed out so the workspace stays buildable.

mod error;

pub use error::{Result, SessionError};

/// TCC-style OS permission gated by Wisp at startup.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Permission {
    /// Microphone access. Required for the mic capture path.
    Microphone,
    /// On-device speech recognition. Required for both pipelines.
    SpeechRecognition,
}

/// Current state of a single [`Permission`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PermissionStatus {
    /// The user has not been asked yet; calling [`request_permission`] will
    /// trigger the OS dialog.
    Undetermined,
    /// The user explicitly denied this permission. Re-requesting won't show
    /// a dialog — the user has to flip it in System Settings.
    Denied,
    /// Granted; the corresponding capture path can be used.
    Granted,
    /// Restricted by a system policy (e.g. parental controls). Only
    /// reachable for `SpeechRecognition`.
    Restricted,
}

impl PermissionStatus {
    /// Convenience: true iff the underlying capability is usable.
    #[must_use]
    pub fn is_granted(self) -> bool {
        matches!(self, Self::Granted)
    }
}

#[cfg(any(target_os = "macos", target_os = "windows"))]
mod imp {
    use std::ffi::{CStr, CString};
    use std::path::Path;
    use std::ptr::NonNull;
    use std::time::Duration;

    use crossbeam_channel as channel;
    use wisp_audiokit_sys as sys;
    use wisp_core::SourceLabel;

    use crate::error::{Result, SessionError};
    use crate::{Permission, PermissionStatus};

    fn permission_to_raw(perm: Permission) -> i32 {
        match perm {
            Permission::Microphone => sys::WISP_PERMISSION_MICROPHONE,
            Permission::SpeechRecognition => sys::WISP_PERMISSION_SPEECH_RECOGNITION,
        }
    }

    fn status_from_raw(raw: i32) -> PermissionStatus {
        match raw {
            sys::WISP_PERMISSION_STATUS_GRANTED => PermissionStatus::Granted,
            sys::WISP_PERMISSION_STATUS_DENIED => PermissionStatus::Denied,
            sys::WISP_PERMISSION_STATUS_RESTRICTED => PermissionStatus::Restricted,
            // Treat negative ("invalid permission id") as undetermined too —
            // we never pass an invalid id from safe Rust, and conflating the
            // two keeps the surface tidy.
            _ => PermissionStatus::Undetermined,
        }
    }

    /// Read the current status of `permission` from the OS. Never prompts.
    #[must_use]
    pub fn check_permission(permission: Permission) -> PermissionStatus {
        // SAFETY: simple value-in, value-out call into Swift; no pointers.
        let raw = unsafe { sys::wisp_permission_status(permission_to_raw(permission)) };
        status_from_raw(raw)
    }

    /// Show the OS permission prompt for `permission` (only if the user has
    /// not been asked yet) and block until they respond. Returns the
    /// resulting status. If the status is already determined, returns it
    /// immediately without prompting.
    ///
    /// Safe to call from any thread; the macOS APIs marshal the dialog to
    /// the main thread internally. Callers from a UI event loop should run
    /// this on a worker thread to keep the UI responsive while the user
    /// reads the prompt.
    #[must_use]
    pub fn request_permission(permission: Permission) -> PermissionStatus {
        // SAFETY: simple value-in, value-out call into Swift; no pointers.
        let raw = unsafe { sys::wisp_permission_request(permission_to_raw(permission)) };
        status_from_raw(raw)
    }

    /// `WispAudioKit` library version (e.g. `"0.1.0"`).
    ///
    /// # Panics
    /// Panics if the Swift side's version string is not valid UTF-8. It
    /// ships as a static ASCII constant, so this only fires on build-time
    /// binary corruption.
    #[must_use]
    #[allow(clippy::expect_used)]
    pub fn version() -> &'static str {
        // SAFETY: returns a static UTF-8 C string that lives forever.
        unsafe {
            let ptr = sys::wisp_audiokit_version();
            CStr::from_ptr(ptr)
                .to_str()
                .expect("`WispAudioKit` version is valid UTF-8")
        }
    }

    // ---- Types ---------------------------------------------------------

    /// One transcription update from a running [`Session`].
    #[derive(Debug, Clone, PartialEq)]
    pub struct SessionResult {
        pub source: SourceLabel,
        pub segment_id: u64,
        pub text: String,
        pub start_seconds: f64,
        pub end_seconds: f64,
    }

    /// Either a transcription result or a log line emitted by the session.
    #[derive(Debug, Clone, PartialEq)]
    pub enum Event {
        Result(SessionResult),
        Log(String),
    }

    // ---- Session -------------------------------------------------------

    /// Owns one running (or yet-to-be-started) capture + transcription session.
    ///
    /// Construct with [`Session::new`], start with [`Session::start`], pull
    /// events from [`Session::recv`] / [`Session::try_recv`], and drop to
    /// release. Drop calls `wisp_session_stop` + `wisp_session_free` so a
    /// running session is always cleaned up.
    pub struct Session {
        handle: NonNull<sys::WispSession>,
        receiver: channel::Receiver<Event>,
        // Kept alive so the callbacks' user_data pointer stays valid for
        // as long as the Swift side might call them.
        _ctx: Box<CallbackContext>,
    }

    // SAFETY: Session owns the C handle and the receiver. The handle is
    // an opaque pointer we never deref ourselves; the C side serializes
    // access internally, so it is sound to move the handle across threads.
    // (`Session` stays `!Sync` overall because the `NonNull` field is
    // `!Sync` — only `Send` needs the manual impl.)
    unsafe impl Send for Session {}

    // Swift may invoke `on_result_thunk` / `on_log_thunk` from different
    // threads. The thunks form `&CallbackContext` from a raw `user_data`
    // pointer, so `CallbackContext` must be `Sync`. `crossbeam_channel`'s
    // `Sender` is `Sync` (unlike `std::sync::mpsc::Sender`), which lets
    // those callbacks fire concurrently without UB.
    struct CallbackContext {
        sender: channel::Sender<Event>,
    }

    impl Session {
        /// Construct a new session. Does no I/O — call [`Self::start`] next.
        ///
        /// `output_dir` is the directory in which the per-session WAV files
        /// will be written (created if needed). `locale` is a BCP-47
        /// language tag passed to the Swift speech recognizer
        /// (e.g. `"ja-JP"`).
        ///
        /// # Errors
        /// Returns [`SessionError::InvalidPath`] / [`SessionError::InvalidLocale`]
        /// when the inputs contain a NUL byte, and [`SessionError::Construction`]
        /// when the Swift side rejects them (e.g. the directory could not
        /// be created).
        pub fn new(
            output_dir: impl AsRef<Path>,
            locale: &str,
        ) -> Result<Self> {
            let output_dir = output_dir.as_ref();
            let path_str = output_dir
                .to_str()
                .ok_or_else(|| SessionError::InvalidPath(output_dir.to_path_buf()))?;
            let path_c = CString::new(path_str)
                .map_err(|_| SessionError::InvalidPath(output_dir.to_path_buf()))?;
            let locale_c =
                CString::new(locale).map_err(|_| SessionError::InvalidLocale(locale.to_owned()))?;

            let (sender, receiver) = channel::unbounded();
            let ctx = Box::new(CallbackContext { sender });
            let user_data = std::ptr::from_ref::<CallbackContext>(ctx.as_ref()) as *mut _;

            // SAFETY: pointers are valid for the duration of the call and
            // `user_data` is kept alive by holding `ctx` in `Session`.
            let raw = unsafe {
                sys::wisp_session_new(
                    path_c.as_ptr(),
                    locale_c.as_ptr(),
                    Some(on_result_thunk),
                    Some(on_log_thunk),
                    user_data,
                )
            };
            let handle = NonNull::new(raw).ok_or(SessionError::Construction)?;
            Ok(Self {
                handle,
                receiver,
                _ctx: ctx,
            })
        }

        /// Start capture + transcription. Blocks until ready or fails.
        ///
        /// # Errors
        /// Returns [`SessionError::Start`] with the Swift-side error
        /// message on failure (permission denial, missing audio device,
        /// model download failure, ...).
        pub fn start(&self) -> Result<()> {
            // SAFETY: handle is non-null and the Swift side serializes
            // start/stop/free internally.
            let rc = unsafe { sys::wisp_session_start(self.handle.as_ptr()) };
            if rc == 0 {
                return Ok(());
            }
            let msg = unsafe { sys::wisp_session_last_error_message(self.handle.as_ptr()) };
            let detail = if msg.is_null() {
                format!("unknown error (rc={rc})")
            } else {
                // SAFETY: Swift documents the pointer is valid until the
                // next mutating call; we copy out immediately.
                unsafe { CStr::from_ptr(msg) }
                    .to_string_lossy()
                    .into_owned()
            };
            Err(SessionError::Start(detail))
        }

        /// Stop the session and wait for buffered results to drain. Blocks.
        /// Idempotent — safe to call multiple times.
        pub fn stop(&self) {
            // SAFETY: handle is non-null; stop is idempotent on the Swift side.
            unsafe { sys::wisp_session_stop(self.handle.as_ptr()) };
        }

        /// Non-blocking event poll.
        #[must_use]
        pub fn try_recv(&self) -> Option<Event> {
            self.receiver.try_recv().ok()
        }

        /// Block until the next event arrives, or return `None` if the
        /// session has been dropped / closed.
        #[must_use]
        pub fn recv(&self) -> Option<Event> {
            self.receiver.recv().ok()
        }

        /// Block until the next event arrives or `timeout` elapses.
        /// Returns `None` on timeout or when the session has been
        /// dropped / closed.
        #[must_use]
        pub fn recv_timeout(
            &self,
            timeout: Duration,
        ) -> Option<Event> {
            self.receiver.recv_timeout(timeout).ok()
        }
    }

    impl Drop for Session {
        fn drop(&mut self) {
            // SAFETY: handle is non-null and we own it. Stop is a no-op if
            // the session was never started or has already stopped.
            unsafe {
                sys::wisp_session_stop(self.handle.as_ptr());
                sys::wisp_session_free(self.handle.as_ptr());
            }
        }
    }

    // ---- Callback thunks ----------------------------------------------

    unsafe extern "C" fn on_result_thunk(
        source: i32,
        segment_id: u64,
        text_utf8: *const std::os::raw::c_char,
        text_len: usize,
        start_seconds: f64,
        end_seconds: f64,
        user_data: *mut std::os::raw::c_void,
    ) {
        if user_data.is_null() {
            return;
        }
        // SAFETY: user_data was set by Session::new to point at a
        // CallbackContext kept alive by Session.
        let ctx = unsafe { &*(user_data.cast::<CallbackContext>()) };
        let text = if text_utf8.is_null() || text_len == 0 {
            String::new()
        } else {
            // SAFETY: Swift guarantees (ptr, len) is a valid UTF-8 slice
            // for the duration of the call.
            let bytes = unsafe { std::slice::from_raw_parts(text_utf8.cast::<u8>(), text_len) };
            String::from_utf8_lossy(bytes).into_owned()
        };
        let label = match source {
            sys::WISP_SOURCE_MIC => SourceLabel::Mic,
            sys::WISP_SOURCE_SYSTEM => SourceLabel::System,
            _ => return,
        };
        let _ = ctx.sender.send(Event::Result(SessionResult {
            source: label,
            segment_id,
            text,
            start_seconds,
            end_seconds,
        }));
    }

    unsafe extern "C" fn on_log_thunk(
        message_utf8: *const std::os::raw::c_char,
        message_len: usize,
        user_data: *mut std::os::raw::c_void,
    ) {
        if user_data.is_null() {
            return;
        }
        let ctx = unsafe { &*(user_data.cast::<CallbackContext>()) };
        let text = if message_utf8.is_null() || message_len == 0 {
            String::new()
        } else {
            let bytes =
                unsafe { std::slice::from_raw_parts(message_utf8.cast::<u8>(), message_len) };
            String::from_utf8_lossy(bytes).into_owned()
        };
        let _ = ctx.sender.send(Event::Log(text));
    }
}

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
mod imp {
    use std::path::Path;
    use std::time::Duration;

    use wisp_core::SourceLabel;

    use crate::error::{Result, SessionError};
    use crate::{Permission, PermissionStatus};

    /// `WispAudioKit` library version. Always empty on non-macOS targets.
    #[must_use]
    pub fn version() -> &'static str {
        ""
    }

    /// Stub — always reports `Granted` on non-macOS targets so callers can
    /// fall through to the (stubbed) session, which will then return
    /// `UnsupportedPlatform`. Keeps the workspace buildable on Linux CI.
    #[must_use]
    pub fn check_permission(_permission: Permission) -> PermissionStatus {
        PermissionStatus::Granted
    }

    /// Stub — see [`check_permission`].
    #[must_use]
    pub fn request_permission(_permission: Permission) -> PermissionStatus {
        PermissionStatus::Granted
    }

    /// One transcription update from a running [`Session`].
    #[derive(Debug, Clone, PartialEq)]
    pub struct SessionResult {
        pub source: SourceLabel,
        pub segment_id: u64,
        pub text: String,
        pub start_seconds: f64,
        pub end_seconds: f64,
    }

    /// Either a transcription result or a log line emitted by the session.
    #[derive(Debug, Clone, PartialEq)]
    pub enum Event {
        Result(SessionResult),
        Log(String),
    }

    /// Stub session — always returns [`SessionError::UnsupportedPlatform`].
    pub struct Session;

    impl Session {
        /// # Errors
        /// Always returns [`SessionError::UnsupportedPlatform`].
        pub fn new(
            _output_dir: impl AsRef<Path>,
            _locale: &str,
        ) -> Result<Self> {
            Err(SessionError::UnsupportedPlatform)
        }

        /// # Errors
        /// Always returns [`SessionError::UnsupportedPlatform`].
        pub fn start(&self) -> Result<()> {
            Err(SessionError::UnsupportedPlatform)
        }

        /// No-op on non-macOS targets.
        pub fn stop(&self) {}

        /// Always returns `None`.
        #[must_use]
        pub fn try_recv(&self) -> Option<Event> {
            None
        }

        /// Always returns `None`.
        #[must_use]
        pub fn recv(&self) -> Option<Event> {
            None
        }

        /// Always returns `None`.
        #[must_use]
        pub fn recv_timeout(
            &self,
            _timeout: Duration,
        ) -> Option<Event> {
            None
        }
    }
}

pub use imp::version;
pub use imp::{Event, Session, SessionResult, check_permission, request_permission};
pub use wisp_core::SourceLabel;

#[cfg(all(test, any(target_os = "macos", target_os = "windows")))]
mod tests {
    use super::*;

    #[test]
    fn version_is_nonempty_and_dotted() {
        let v = version();
        assert!(!v.is_empty(), "version must be non-empty");
        assert!(
            v.chars().next().is_some_and(|c| c.is_ascii_digit()),
            "version should start with a digit, got: {v}"
        );
        assert!(v.contains('.'), "version should be dotted, got: {v}");
    }

    #[test]
    fn session_constructs_and_drops_without_starting() {
        let tmp = std::env::temp_dir().join(format!("wisp-audiokit-test-{}", std::process::id()));
        let s = Session::new(&tmp, "ja-JP").expect("session new");
        // Pull events: there are none yet because we never started.
        assert!(s.try_recv().is_none());
        drop(s);
        // Drop must run without panicking even though we never called start().
    }
}
