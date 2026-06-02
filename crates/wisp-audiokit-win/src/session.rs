//! Live capture + transcription session (mic + system loopback).

use std::ffi::CString;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;
use std::time::{SystemTime, UNIX_EPOCH};

use crossbeam_channel::{Sender, unbounded};
use parking_lot::Mutex;

use crate::capture::{self, PcmChunk};
use crate::ffi::{WISP_SOURCE_MIC, WISP_SOURCE_SYSTEM, WispLogCallback, WispResultCallback};
use crate::speech::{self, LogCallback, ResultCallback};

pub struct SessionState {
    output_dir: PathBuf,
    locale: String,
    data_root: PathBuf,
    on_result: Option<WispResultCallback>,
    on_log: Option<WispLogCallback>,
    user_data: usize,
    stop: Arc<AtomicBool>,
    last_error: Mutex<Option<CString>>,
    mic_pcm_tx: Option<Sender<PcmChunk>>,
    sys_pcm_tx: Option<Sender<PcmChunk>>,
    mic_capture: Option<JoinHandle<()>>,
    sys_capture: Option<JoinHandle<()>>,
    mic_vosk: Option<JoinHandle<()>>,
    sys_vosk: Option<JoinHandle<()>>,
    model: Option<Arc<vosk::Model>>,
}

impl SessionState {
    pub fn new(
        output_dir: PathBuf,
        locale: String,
        data_root: PathBuf,
        on_result: Option<WispResultCallback>,
        on_log: Option<WispLogCallback>,
        user_data: usize,
    ) -> Self {
        Self {
            output_dir,
            locale,
            data_root,
            on_result,
            on_log,
            user_data,
            stop: Arc::new(AtomicBool::new(false)),
            last_error: Mutex::new(None),
            mic_pcm_tx: None,
            sys_pcm_tx: None,
            mic_capture: None,
            sys_capture: None,
            mic_vosk: None,
            sys_vosk: None,
            model: None,
        }
    }

    pub fn set_error(
        &self,
        msg: impl Into<String>,
    ) {
        let msg =
            CString::new(msg.into()).unwrap_or_else(|_| CString::new("error").expect("fallback"));
        *self.last_error.lock() = Some(msg);
    }

    pub fn last_error_cstr(&self) -> Option<CString> {
        self.last_error.lock().clone()
    }

    pub fn start(&mut self) -> i32 {
        if !capture::probe_microphone() {
            self.set_error(
                "Microphone access denied. Enable Microphone for desktop apps in Settings → Privacy.",
            );
            return 1;
        }

        let model_path = match speech::resolve_model_path(&self.locale, &self.data_root) {
            Some(p) => p,
            None => {
                self.set_error(format!(
                    "Vosk speech model not found. Download a model for {} into {} \
                     (or set WISP_VOSK_MODEL). See https://alphacephei.com/vosk/models",
                    self.locale,
                    self.data_root.join("models").display()
                ));
                return 1;
            },
        };

        let model = match speech::load_model(&model_path) {
            Some(m) => Arc::new(m),
            None => {
                self.set_error(format!(
                    "Failed to load Vosk model at {}",
                    model_path.display()
                ));
                return 1;
            },
        };
        self.model = Some(model.clone());

        std::fs::create_dir_all(&self.output_dir)
            .map_err(|e| e.to_string())
            .ok();
        let ts = timestamp_slug();
        let mic_wav = self.output_dir.join(format!("mic-{ts}.wav"));
        let sys_wav = self.output_dir.join(format!("system-{ts}.wav"));

        let (mic_tx, mic_rx) = unbounded();
        let (sys_tx, sys_rx) = unbounded();
        self.mic_pcm_tx = Some(mic_tx);
        self.sys_pcm_tx = Some(sys_tx);

        let stop = self.stop.clone();
        stop.store(false, Ordering::Relaxed);

        let on_result_cb = self.on_result;
        let on_log_cb = self.on_log;
        let user_data = self.user_data;

        let mic_on_result: ResultCallback = Box::new({
            let cb = on_result_cb;
            move |source, seg, text, start, end| {
                emit_result(cb, source, seg, &text, start, end, user_data);
            }
        });
        let sys_on_result: ResultCallback = Box::new({
            let cb = on_result_cb;
            move |source, seg, text, start, end| {
                emit_result(cb, source, seg, &text, start, end, user_data);
            }
        });
        let on_log_mic: LogCallback = Box::new({
            let cb = on_log_cb;
            move |msg| emit_log(cb, &msg, user_data)
        });
        let on_log_sys: LogCallback = Box::new({
            let cb = on_log_cb;
            move |msg| emit_log(cb, &msg, user_data)
        });

        self.mic_vosk = Some(speech::spawn_pipeline(
            WISP_SOURCE_MIC,
            model.clone(),
            mic_rx,
            mic_on_result,
            on_log_mic,
            stop.clone(),
        ));
        self.sys_vosk = Some(speech::spawn_pipeline(
            WISP_SOURCE_SYSTEM,
            model,
            sys_rx,
            sys_on_result,
            on_log_sys,
            stop.clone(),
        ));

        let mic_sender = self.mic_pcm_tx.as_ref().expect("mic sender").clone();
        let sys_sender = self.sys_pcm_tx.as_ref().expect("sys sender").clone();
        self.mic_capture = Some(capture::spawn_mic(mic_wav, mic_sender, stop.clone()));
        self.sys_capture = Some(capture::spawn_loopback(sys_wav, sys_sender, stop.clone()));

        emit_log(
            on_log_cb,
            "Windows session started (WASAPI + Vosk)",
            user_data,
        );
        0
    }

    pub fn stop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        self.mic_pcm_tx = None;
        self.sys_pcm_tx = None;

        if let Some(h) = self.mic_capture.take() {
            capture::join_with_timeout(h, std::time::Duration::from_secs(3));
        }
        if let Some(h) = self.sys_capture.take() {
            capture::join_with_timeout(h, std::time::Duration::from_secs(3));
        }
        if let Some(h) = self.mic_vosk.take() {
            speech::stop_pipeline(h);
        }
        if let Some(h) = self.sys_vosk.take() {
            speech::stop_pipeline(h);
        }
    }
}

fn timestamp_slug() -> String {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs().to_string())
        .unwrap_or_else(|_| "0".into())
}

fn emit_result(
    cb: Option<WispResultCallback>,
    source: i32,
    segment_id: u64,
    text: &str,
    start_seconds: f64,
    end_seconds: f64,
    user_data: usize,
) {
    let Some(cb) = cb else { return };
    unsafe {
        cb(
            source,
            segment_id,
            text.as_ptr().cast(),
            text.len(),
            start_seconds,
            end_seconds,
            user_data as *mut std::ffi::c_void,
        );
    }
}

fn emit_log(
    cb: Option<WispLogCallback>,
    message: &str,
    user_data: usize,
) {
    let Some(cb) = cb else { return };
    unsafe {
        cb(
            message.as_ptr().cast(),
            message.len(),
            user_data as *mut std::ffi::c_void,
        );
    }
}

/// Default Wisp data directory on Windows: `%LOCALAPPDATA%\dev.mokmok.wisp\`.
pub fn default_data_root() -> PathBuf {
    if let Ok(dir) = std::env::var("WISP_DATA_DIR") {
        return PathBuf::from(dir);
    }
    std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir)
        .join("dev.mokmok.wisp")
}

pub fn parent_data_root(output_dir: &Path) -> PathBuf {
    output_dir
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(default_data_root)
}
