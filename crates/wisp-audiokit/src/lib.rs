//! Safe Rust wrapper over Wisp's platform audio/transcription backends.
//!
//! macOS is backed by the Swift `WispAudioKit` framework. Windows exposes
//! setup/configuration for `Windows.Media.SpeechRecognition` and local-model
//! transcription; unsupported platforms keep a stub so the workspace stays
//! buildable.

mod error;

pub use error::{Result, SessionError, SetupError, SetupResult};

use std::path::{Path, PathBuf};

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

/// Transcription engine selected for a session.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RecognizerBackend {
    /// Use the OS-provided speech recognizer for the current platform.
    ///
    /// macOS maps this to `SpeechAnalyzer`. Windows maps this to
    /// `Windows.Media.SpeechRecognition`, which uses the OS microphone path.
    Platform,
    /// Use a downloaded local model. On Windows this is the path intended
    /// for WASAPI mic + loopback PCM so both sides of the call can be
    /// transcribed by the same offline engine.
    LocalModel,
}

impl RecognizerBackend {
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Platform => platform_recognizer_label(),
            Self::LocalModel => "Local model",
        }
    }
}

/// Configuration used when constructing a [`Session`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionConfig {
    pub locale: String,
    pub recognizer: RecognizerBackend,
    pub local_model_path: Option<PathBuf>,
}

impl SessionConfig {
    #[must_use]
    pub fn platform_default(locale: impl Into<String>) -> Self {
        Self {
            locale: locale.into(),
            recognizer: RecognizerBackend::Platform,
            local_model_path: None,
        }
    }

    #[must_use]
    pub fn local_model(
        locale: impl Into<String>,
        path: impl Into<PathBuf>,
    ) -> Self {
        Self {
            locale: locale.into(),
            recognizer: RecognizerBackend::LocalModel,
            local_model_path: Some(path.into()),
        }
    }
}

/// Metadata for the local model offered by the setup screen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LocalModelSpec {
    pub name: &'static str,
    pub filename: &'static str,
    pub url: &'static str,
    pub approx_bytes: u64,
}

/// Current filesystem state for the local model.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LocalModelStatus {
    Missing {
        spec: LocalModelSpec,
        path: PathBuf,
    },
    Ready {
        spec: LocalModelSpec,
        path: PathBuf,
        bytes: u64,
    },
}

impl LocalModelStatus {
    #[must_use]
    pub fn is_ready(&self) -> bool {
        matches!(self, Self::Ready { .. })
    }

    #[must_use]
    pub fn path(&self) -> &Path {
        match self {
            Self::Missing { path, .. } | Self::Ready { path, .. } => path,
        }
    }
}

const LOCAL_MODEL_SPEC: LocalModelSpec = LocalModelSpec {
    name: "Whisper base",
    filename: "ggml-base.bin",
    url: "https://huggingface.co/ggerganov/whisper.cpp/resolve/main/ggml-base.bin",
    approx_bytes: 142 * 1024 * 1024,
};

#[must_use]
pub const fn local_model_spec() -> LocalModelSpec {
    LOCAL_MODEL_SPEC
}

#[must_use]
pub fn local_model_path(data_dir: impl AsRef<Path>) -> PathBuf {
    data_dir
        .as_ref()
        .join("models")
        .join(local_model_spec().filename)
}

#[must_use]
pub fn local_model_status(data_dir: impl AsRef<Path>) -> LocalModelStatus {
    let spec = local_model_spec();
    let path = local_model_path(data_dir);
    match std::fs::metadata(&path) {
        Ok(meta) if meta.is_file() && meta.len() > 0 => LocalModelStatus::Ready {
            spec,
            path,
            bytes: meta.len(),
        },
        _ => LocalModelStatus::Missing { spec, path },
    }
}

/// Download the default local model into Wisp's data directory.
///
/// This is intentionally blocking; UI callers should run it on a background
/// executor and then refresh [`local_model_status`].
///
/// # Errors
/// Returns [`SetupError`] if the model directory cannot be created, the
/// download command fails, or the temporary file cannot be moved into place.
pub fn download_local_model(data_dir: impl AsRef<Path>) -> SetupResult<LocalModelStatus> {
    let data_dir = data_dir.as_ref();
    let final_path = local_model_path(data_dir);
    let Some(model_dir) = final_path.parent() else {
        return Err(SetupError::Install(format!(
            "invalid model path: {}",
            final_path.display()
        )));
    };
    std::fs::create_dir_all(model_dir).map_err(|err| SetupError::CreateModelDirectory {
        path: model_dir.to_path_buf(),
        message: err.to_string(),
    })?;

    let part_path = final_path.with_extension("bin.part");
    let _ = std::fs::remove_file(&part_path);
    download_url(local_model_spec().url, &part_path)?;
    std::fs::rename(&part_path, &final_path)
        .or_else(|_| {
            std::fs::copy(&part_path, &final_path)?;
            std::fs::remove_file(&part_path)
        })
        .map_err(|err| SetupError::Install(err.to_string()))?;
    Ok(local_model_status(data_dir))
}

#[must_use]
pub const fn requires_recognizer_setup() -> bool {
    cfg!(target_os = "windows")
}

#[must_use]
pub const fn platform_recognizer_label() -> &'static str {
    if cfg!(target_os = "windows") {
        "Windows.Media.SpeechRecognition"
    } else if cfg!(target_os = "macos") {
        "Apple SpeechAnalyzer"
    } else {
        "Platform recognizer"
    }
}

fn download_url(
    url: &str,
    destination: &Path,
) -> SetupResult<()> {
    if let Some(path) = url.strip_prefix("file://") {
        std::fs::copy(path, destination)
            .map(|_| ())
            .map_err(|err| SetupError::Download(err.to_string()))?;
        return Ok(());
    }
    #[cfg(target_os = "windows")]
    let mut cmd = {
        let mut cmd = std::process::Command::new("powershell.exe");
        cmd.args([
            "-NoProfile",
            "-ExecutionPolicy",
            "Bypass",
            "-Command",
            "$ProgressPreference='SilentlyContinue'; Invoke-WebRequest -Uri $args[0] -OutFile $args[1]",
            url,
        ]);
        cmd.arg(destination);
        cmd
    };
    #[cfg(not(target_os = "windows"))]
    let mut cmd = {
        let mut cmd = std::process::Command::new("curl");
        cmd.args(["--fail", "--location", "--show-error", "--output"]);
        cmd.arg(destination);
        cmd.arg(url);
        cmd
    };
    let output = cmd
        .output()
        .map_err(|err| SetupError::Download(err.to_string()))?;
    if output.status.success() {
        return Ok(());
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    Err(SetupError::Download(stderr.trim().to_string()))
}

#[cfg(target_os = "macos")]
mod imp {
    use std::ffi::{CStr, CString};
    use std::path::Path;
    use std::ptr::NonNull;
    use std::time::Duration;

    use crossbeam_channel as channel;
    use wisp_audiokit_sys as sys;
    use wisp_core::SourceLabel;

    use crate::error::{Result, SessionError};
    use crate::{Permission, PermissionStatus, SessionConfig};

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
            Self::new_with_config(output_dir, SessionConfig::platform_default(locale))
        }

        /// Construct a new session with an explicit recognizer config.
        ///
        /// macOS currently always uses Apple's `SpeechAnalyzer`; the
        /// recognizer selection is accepted so the desktop shell can use one
        /// session-start path across platforms.
        ///
        /// # Errors
        /// See [`Self::new`].
        pub fn new_with_config(
            output_dir: impl AsRef<Path>,
            config: SessionConfig,
        ) -> Result<Self> {
            let output_dir = output_dir.as_ref();
            let path_str = output_dir
                .to_str()
                .ok_or_else(|| SessionError::InvalidPath(output_dir.to_path_buf()))?;
            let path_c = CString::new(path_str)
                .map_err(|_| SessionError::InvalidPath(output_dir.to_path_buf()))?;
            let locale_c = CString::new(config.locale.clone())
                .map_err(|_| SessionError::InvalidLocale(config.locale))?;

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
        pub fn start(&mut self) -> Result<()> {
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

#[cfg(target_os = "windows")]
mod imp {
    use std::path::Path;
    use std::sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
    };
    use std::time::{Duration, Instant};

    use crossbeam_channel as channel;
    use windows::Foundation::TypedEventHandler;
    use windows::Globalization::Language;
    use windows::Media::SpeechRecognition::{
        SpeechContinuousRecognitionMode, SpeechContinuousRecognitionResultGeneratedEventArgs,
        SpeechContinuousRecognitionSession, SpeechRecognitionResultStatus,
        SpeechRecognitionScenario, SpeechRecognitionTopicConstraint, SpeechRecognizer,
    };
    use windows::core::{HSTRING, Interface};
    use wisp_core::SourceLabel;

    use crate::error::{Result, SessionError};
    use crate::{
        LocalModelStatus, Permission, PermissionStatus, RecognizerBackend, SessionConfig,
        local_model_status,
    };

    /// `WispAudioKit` library version.
    #[must_use]
    pub fn version() -> &'static str {
        "windows-0.1.0"
    }

    /// Windows desktop apps do not get a TCC-style microphone prompt through
    /// this backend. Privacy toggles are surfaced by the setup screen's
    /// Settings link, while actual availability is validated at session start.
    #[must_use]
    pub fn check_permission(_permission: Permission) -> PermissionStatus {
        PermissionStatus::Granted
    }

    /// See [`check_permission`].
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

    /// Windows session backed by `Windows.Media.SpeechRecognition`.
    ///
    /// The platform recognizer path uses the OS microphone recognition
    /// session. The local-model path preflights model installation and is
    /// where WASAPI mic + loopback transcription will be wired.
    pub struct Session {
        output_dir: std::path::PathBuf,
        config: SessionConfig,
        receiver: channel::Receiver<Event>,
        sender: channel::Sender<Event>,
        speech: Option<WindowsSpeechSession>,
        started_at: Option<Instant>,
        is_running: Arc<AtomicBool>,
    }

    impl Session {
        /// Construct a new session.
        ///
        /// # Errors
        /// Returns [`SessionError::InvalidLocale`] if `locale` contains a NUL
        /// byte and [`SessionError::Construction`] if the output directory
        /// cannot be created.
        pub fn new(
            output_dir: impl AsRef<Path>,
            locale: &str,
        ) -> Result<Self> {
            Self::new_with_config(output_dir, SessionConfig::platform_default(locale))
        }

        /// Construct a new session with an explicit recognizer config.
        ///
        /// # Errors
        /// Returns [`SessionError::Construction`] if the output directory
        /// cannot be created.
        pub fn new_with_config(
            output_dir: impl AsRef<Path>,
            config: SessionConfig,
        ) -> Result<Self> {
            if config.locale.contains('\0') {
                return Err(SessionError::InvalidLocale(config.locale));
            }
            let output_dir = output_dir.as_ref().to_path_buf();
            std::fs::create_dir_all(&output_dir).map_err(|_| SessionError::Construction)?;
            let (sender, receiver) = channel::unbounded();
            Ok(Self {
                output_dir,
                config,
                receiver,
                sender,
                speech: None,
                started_at: None,
                is_running: Arc::new(AtomicBool::new(false)),
            })
        }

        /// Start capture + transcription. Blocks until ready or fails.
        ///
        /// # Errors
        /// Returns [`SessionError::Start`] when the selected recognizer cannot
        /// be initialized or the selected local model is not installed.
        pub fn start(&mut self) -> Result<()> {
            match self.config.recognizer {
                RecognizerBackend::Platform => self.start_windows_speech(),
                RecognizerBackend::LocalModel => self.start_local_model(),
            }
        }

        /// Stop the session. Idempotent.
        pub fn stop(&self) {
            if !self.is_running.swap(false, Ordering::SeqCst) {
                return;
            }
            if let Some(speech) = &self.speech {
                speech.stop();
            }
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
        #[must_use]
        pub fn recv_timeout(
            &self,
            timeout: Duration,
        ) -> Option<Event> {
            self.receiver.recv_timeout(timeout).ok()
        }

        fn start_windows_speech(&mut self) -> Result<()> {
            let speech = WindowsSpeechSession::start(
                &self.config.locale,
                self.sender.clone(),
                self.is_running.clone(),
            )?;
            self.speech = Some(speech);
            self.started_at = Some(Instant::now());
            let _ = self.sender.send(Event::Log(
                "[WIN] Windows.Media.SpeechRecognition started for microphone input".into(),
            ));
            let _ = self.sender.send(Event::Log(
                "[WIN] Select Local model after downloading it to transcribe WASAPI mic + loopback PCM".into(),
            ));
            Ok(())
        }

        fn start_local_model(&mut self) -> Result<()> {
            let Some(path) = self.config.local_model_path.as_ref() else {
                return Err(SessionError::Start(
                    "local model backend selected but no model path was configured".into(),
                ));
            };
            let data_dir = path
                .parent()
                .and_then(std::path::Path::parent)
                .unwrap_or_else(|| Path::new(""));
            if !matches!(local_model_status(data_dir), LocalModelStatus::Ready { .. }) {
                return Err(SessionError::Start(format!(
                    "local model is not installed at {}; use setup to download it",
                    path.display()
                )));
            }
            let _ = self.sender.send(Event::Log(format!(
                "[WIN] local model ready at {}; WASAPI local transcription is not wired yet",
                path.display()
            )));
            Err(SessionError::Start(
                "local model transcription will use WASAPI mic + loopback, but the engine is not wired in this build".into(),
            ))
        }
    }

    impl Drop for Session {
        fn drop(&mut self) {
            self.stop();
        }
    }

    struct WindowsSpeechSession {
        recognizer: SpeechRecognizer,
        session: SpeechContinuousRecognitionSession,
        result_token: i64,
    }

    impl WindowsSpeechSession {
        fn start(
            locale: &str,
            sender: channel::Sender<Event>,
            is_running: Arc<AtomicBool>,
        ) -> Result<Self> {
            let language = Language::CreateLanguage(&HSTRING::from(locale))
                .map_err(|err| SessionError::Start(err.to_string()))?;
            let recognizer = SpeechRecognizer::Create(&language)
                .map_err(|err| SessionError::Start(err.to_string()))?;
            let constraint = SpeechRecognitionTopicConstraint::Create(
                SpeechRecognitionScenario::Dictation,
                &HSTRING::from("meeting transcription"),
            )
            .map_err(|err| SessionError::Start(err.to_string()))?;
            recognizer
                .Constraints()
                .and_then(|constraints| constraints.Append(&constraint.cast()?))
                .map_err(|err| SessionError::Start(err.to_string()))?;
            let compile = recognizer
                .CompileConstraintsAsync()
                .and_then(|op| op.get())
                .map_err(|err| SessionError::Start(err.to_string()))?;
            if compile
                .Status()
                .map_err(|err| SessionError::Start(err.to_string()))?
                != SpeechRecognitionResultStatus::Success
            {
                return Err(SessionError::Start(
                    "Windows speech constraints failed to compile".into(),
                ));
            }

            let segment_id = Arc::new(AtomicU64::new(1));
            let started_at = Instant::now();
            let handler_sender = sender;
            let handler_running = is_running.clone();
            let handler_segment_id = segment_id;
            let handler = TypedEventHandler::<
                SpeechContinuousRecognitionSession,
                SpeechContinuousRecognitionResultGeneratedEventArgs,
            >::new(move |_session, args| {
                if !handler_running.load(Ordering::SeqCst) {
                    return Ok(());
                }
                let Some(args) = args.as_ref() else {
                    return Ok(());
                };
                let result = args.Result()?;
                if result.Status()? != SpeechRecognitionResultStatus::Success {
                    return Ok(());
                }
                let text = result.Text()?.to_string_lossy();
                if text.trim().is_empty() {
                    return Ok(());
                }
                let now = started_at.elapsed().as_secs_f64();
                let id = handler_segment_id.fetch_add(1, Ordering::SeqCst);
                let _ = handler_sender.send(Event::Result(SessionResult {
                    source: SourceLabel::Mic,
                    segment_id: id,
                    text,
                    start_seconds: now,
                    end_seconds: now,
                }));
                Ok(())
            });

            let session = recognizer
                .ContinuousRecognitionSession()
                .map_err(|err| SessionError::Start(err.to_string()))?;
            let result_token = session
                .ResultGenerated(&handler)
                .map_err(|err| SessionError::Start(err.to_string()))?;
            is_running.store(true, Ordering::SeqCst);
            session
                .StartWithModeAsync(SpeechContinuousRecognitionMode::Default)
                .and_then(|op| op.get())
                .map_err(|err| {
                    is_running.store(false, Ordering::SeqCst);
                    SessionError::Start(err.to_string())
                })?;
            Ok(Self {
                recognizer,
                session,
                result_token,
            })
        }

        fn stop(&self) {
            let _ = self.session.StopAsync().and_then(|op| op.get());
            let _ = self.session.RemoveResultGenerated(self.result_token);
            let _ = self.recognizer.Close();
        }
    }
}

#[cfg(all(not(target_os = "macos"), not(target_os = "windows")))]
mod imp {
    use std::path::Path;
    use std::time::Duration;

    use wisp_core::SourceLabel;

    use crate::error::{Result, SessionError};
    use crate::{Permission, PermissionStatus, SessionConfig};

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
        pub fn new_with_config(
            _output_dir: impl AsRef<Path>,
            _config: SessionConfig,
        ) -> Result<Self> {
            Err(SessionError::UnsupportedPlatform)
        }

        /// # Errors
        /// Always returns [`SessionError::UnsupportedPlatform`].
        pub fn start(&mut self) -> Result<()> {
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

#[cfg(all(test, target_os = "macos"))]
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
