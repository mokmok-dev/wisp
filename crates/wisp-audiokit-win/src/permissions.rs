//! Windows privacy settings for microphone access.

use crate::capture;
use crate::model_download;
use crate::speech;

const STATUS_UNDETERMINED: i32 = 0;
const STATUS_DENIED: i32 = 1;
const STATUS_GRANTED: i32 = 2;

/// Read microphone permission without prompting.
pub fn microphone_status() -> i32 {
    if capture::probe_microphone() {
        STATUS_GRANTED
    } else {
        STATUS_DENIED
    }
}

/// Speech recognition on Windows uses a local Vosk model — no OS dialog.
/// Report granted when a model is present, otherwise undetermined.
pub fn speech_status(
    data_root: &std::path::Path,
    locale: &str,
) -> i32 {
    if speech::resolve_model_path(locale, data_root).is_some() {
        STATUS_GRANTED
    } else {
        STATUS_UNDETERMINED
    }
}

/// Requesting the microphone opens the Windows privacy settings page.
/// There is no blocking in-process dialog for unpackaged desktop apps.
pub fn request_microphone() -> i32 {
    let _ = std::process::Command::new("cmd")
        .args(["/C", "start", "ms-settings:privacy-microphone"])
        .spawn();
    microphone_status()
}

pub fn request_speech(
    data_root: &std::path::Path,
    locale: &str,
) -> i32 {
    match model_download::ensure_model(locale, data_root, |_, _| {}) {
        Ok(_) => STATUS_GRANTED,
        Err(err) => {
            eprintln!("wisp: Vosk model download failed: {err}");
            speech_status(data_root, locale)
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_constants_match_ffi() {
        assert_eq!(STATUS_UNDETERMINED, 0);
        assert_eq!(STATUS_DENIED, 1);
        assert_eq!(STATUS_GRANTED, 2);
    }
}
