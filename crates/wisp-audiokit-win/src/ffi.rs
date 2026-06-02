//! C ABI exports consumed by `wisp-audiokit-sys`.

use std::ffi::{CStr, CString};
use std::os::raw::{c_char, c_int, c_void};
use std::path::PathBuf;
use std::ptr;
use std::sync::OnceLock;

use parking_lot::Mutex;

use crate::permissions;
use crate::session::{SessionState, default_data_root, parent_data_root};

pub const WISP_SOURCE_MIC: i32 = 0;
pub const WISP_SOURCE_SYSTEM: i32 = 1;

pub const WISP_PERMISSION_MICROPHONE: i32 = 0;
pub const WISP_PERMISSION_SPEECH_RECOGNITION: i32 = 1;

pub const WISP_PERMISSION_STATUS_UNDETERMINED: i32 = 0;
pub const WISP_PERMISSION_STATUS_DENIED: i32 = 1;
pub const WISP_PERMISSION_STATUS_GRANTED: i32 = 2;
pub const WISP_PERMISSION_STATUS_RESTRICTED: i32 = 3;

pub type WispResultCallback = unsafe extern "C" fn(
    source: i32,
    segment_id: u64,
    text_utf8: *const c_char,
    text_len: usize,
    start_seconds: f64,
    end_seconds: f64,
    user_data: *mut c_void,
);

pub type WispLogCallback =
    unsafe extern "C" fn(message_utf8: *const c_char, message_len: usize, user_data: *mut c_void);

struct WispSession {
    inner: Mutex<SessionState>,
    locale: String,
    data_root: PathBuf,
}

static VERSION: OnceLock<CString> = OnceLock::new();

fn version_string() -> &'static CStr {
    VERSION
        .get_or_init(|| CString::new("0.1.0").expect("version is valid CString"))
        .as_c_str()
}

/// # Safety
/// See `wisp_audiokit.h`.
#[no_mangle]
pub unsafe extern "C" fn wisp_audiokit_version() -> *const c_char {
    version_string().as_ptr()
}

/// # Safety
/// See `wisp_audiokit.h`.
#[no_mangle]
pub unsafe extern "C" fn wisp_session_new(
    output_dir: *const c_char,
    locale: *const c_char,
    on_result: Option<WispResultCallback>,
    on_log: Option<WispLogCallback>,
    user_data: *mut c_void,
) -> *mut WispSession {
    if output_dir.is_null() || locale.is_null() {
        return ptr::null_mut();
    }
    let output_dir = match CStr::from_ptr(output_dir).to_str() {
        Ok(s) => PathBuf::from(s),
        Err(_) => return ptr::null_mut(),
    };
    let locale = match CStr::from_ptr(locale).to_str() {
        Ok(s) => s.to_string(),
        Err(_) => return ptr::null_mut(),
    };
    let data_root = parent_data_root(&output_dir);
    let session = WispSession {
        inner: Mutex::new(SessionState::new(
            output_dir,
            locale.clone(),
            data_root.clone(),
            on_result,
            on_log,
            user_data as usize,
        )),
        locale,
        data_root,
    };
    Box::into_raw(Box::new(session))
}

/// # Safety
/// See `wisp_audiokit.h`.
#[no_mangle]
pub unsafe extern "C" fn wisp_session_start(session: *mut WispSession) -> c_int {
    if session.is_null() {
        return 1;
    }
    let session = &*session;
    let mut inner = session.inner.lock();
    inner.start()
}

/// # Safety
/// See `wisp_audiokit.h`.
#[no_mangle]
pub unsafe extern "C" fn wisp_session_stop(session: *mut WispSession) {
    if session.is_null() {
        return;
    }
    let session = &*session;
    session.inner.lock().stop();
}

/// # Safety
/// See `wisp_audiokit.h`.
#[no_mangle]
pub unsafe extern "C" fn wisp_session_free(session: *mut WispSession) {
    if session.is_null() {
        return;
    }
    let mut boxed = Box::from_raw(session);
    boxed.inner.lock().stop();
    drop(boxed);
}

/// # Safety
/// See `wisp_audiokit.h`.
#[no_mangle]
pub unsafe extern "C" fn wisp_session_last_error_message(
    session: *mut WispSession
) -> *const c_char {
    if session.is_null() {
        return ptr::null();
    }
    let session = &*session;
    match session.inner.lock().last_error_cstr() {
        Some(s) => s.as_ptr(),
        None => ptr::null(),
    }
}

/// # Safety
/// See `wisp_audiokit.h`.
#[no_mangle]
pub unsafe extern "C" fn wisp_permission_status(permission: i32) -> c_int {
    let data_root = default_data_root();
    let locale = "ja-JP".to_string();
    match permission {
        WISP_PERMISSION_MICROPHONE => permissions::microphone_status(),
        WISP_PERMISSION_SPEECH_RECOGNITION => permissions::speech_status(&data_root, &locale),
        _ => -1,
    }
}

/// # Safety
/// See `wisp_audiokit.h`.
#[no_mangle]
pub unsafe extern "C" fn wisp_permission_request(permission: i32) -> c_int {
    let data_root = default_data_root();
    let locale = "ja-JP".to_string();
    match permission {
        WISP_PERMISSION_MICROPHONE => permissions::request_microphone(),
        WISP_PERMISSION_SPEECH_RECOGNITION => permissions::request_speech(&data_root, &locale),
        _ => -1,
    }
}
