//! Glue between `wisp_audiokit`'s blocking permission API and the GPUI
//! main loop.
//!
//! The Swift-side `wisp_permission_request` blocks the calling thread on
//! a `DispatchSemaphore` until the user dismisses the OS dialog. Calling
//! that directly from a render callback would freeze the window, so we
//! ship the work to `cx.background_executor()` and write the result back
//! onto the `AppModel` from the main async context.

use std::process::Command;
use std::time::Duration;

use gpui::{App, AsyncApp, Entity, Timer};
use wisp_audiokit::{Permission, PermissionStatus, check_permission, request_permission};

use crate::app::{AppModel, SpeechDownloadProgress};

/// Locale used for session start and the default Vosk model on Windows.
const APP_LOCALE: &str = "ja-JP";

/// Read the current OS-side status of both permissions and write them
/// into the model. Used at app launch and after the user returns from
/// System Settings (we re-check on every UI tick — see `main.rs`).
pub fn refresh(
    model: &Entity<AppModel>,
    cx: &mut App,
) {
    let microphone = check_permission(Permission::Microphone);
    let speech = check_permission(Permission::SpeechRecognition);
    model.update(cx, |m, cx| {
        let changed = m.permissions.microphone != microphone || m.permissions.speech != speech;
        m.permissions.microphone = microphone;
        m.permissions.speech = speech;
        if changed {
            cx.notify();
        }
    });
}

/// Kick off an OS permission prompt for `perm` on a background thread,
/// write the resulting status back into the model, and clear the pending
/// flag. Marks `perm` pending immediately so the UI can show a spinner.
pub fn request(
    perm: Permission,
    model: Entity<AppModel>,
    cx: &mut App,
) {
    // Already in flight — ignore re-entrant clicks.
    if model.read(cx).permissions.pending.is_some() {
        return;
    }
    model.update(cx, |m, cx| {
        m.permissions.pending = Some(perm);
        m.permissions.speech_download = None;
        cx.notify();
    });

    #[cfg(target_os = "windows")]
    if perm == Permission::SpeechRecognition {
        request_windows_speech_model(model, cx);
        return;
    }

    cx.spawn(async move |cx: &mut AsyncApp| {
        let status = cx
            .background_executor()
            .spawn(async move { request_permission(perm) })
            .await;
        finish_request(model, cx, perm, status);
    })
    .detach();
}

#[cfg(target_os = "windows")]
fn request_windows_speech_model(
    model: Entity<AppModel>,
    cx: &mut App,
) {
    use crossbeam_channel::unbounded;
    use wisp_audiokit::ensure_speech_model;

    cx.spawn(async move |cx: &mut AsyncApp| {
        let (progress_tx, progress_rx) = unbounded::<SpeechDownloadProgress>();

        let download = cx.background_executor().spawn(async move {
            ensure_speech_model(APP_LOCALE, |received, total| {
                let _ = progress_tx.send(SpeechDownloadProgress { received, total });
            })
            .map(|()| PermissionStatus::Granted)
            .unwrap_or(PermissionStatus::Undetermined)
        });

        while !download.is_finished() {
            while let Ok(progress) = progress_rx.try_recv() {
                let _ = model.update(cx, |m, cx| {
                    m.permissions.speech_download = Some(progress);
                    cx.notify();
                });
            }
            Timer::after(Duration::from_millis(100)).await;
        }

        let status = download.await;
        finish_request(model, cx, Permission::SpeechRecognition, status);
    })
    .detach();
}

fn finish_request(
    model: Entity<AppModel>,
    cx: &mut AsyncApp,
    perm: Permission,
    status: PermissionStatus,
) {
    let _ = model.update(cx, |m, cx| {
        m.permissions.set_status(perm, status);
        m.permissions.pending = None;
        m.permissions.speech_download = None;
        cx.notify();
    });
}

/// Open the right System Settings → Privacy & Security pane for `perm`.
/// Used when the permission is already `Denied`, because in that state
/// `request_permission` is a no-op and only the user can re-enable it.
pub fn open_settings(perm: Permission) {
    #[cfg(target_os = "macos")]
    {
        let url = match perm {
            Permission::Microphone => {
                "x-apple.systempreferences:com.apple.preference.security?Privacy_Microphone"
            },
            Permission::SpeechRecognition => {
                "x-apple.systempreferences:com.apple.preference.security?Privacy_SpeechRecognition"
            },
        };
        let _ = Command::new("open").arg(url).spawn();
    }

    #[cfg(target_os = "windows")]
    {
        match perm {
            Permission::Microphone => {
                let _ = Command::new("cmd")
                    .args(["/C", "start", "", "ms-settings:privacy-microphone"])
                    .spawn();
            },
            Permission::SpeechRecognition => {
                let models_dir = crate::data_dir::wisp_data_root().join("models");
                let _ = std::fs::create_dir_all(&models_dir);
                let _ = Command::new("explorer").arg(models_dir).spawn();
            },
        }
    }
}

/// Human-readable label for an onboarding row.
pub fn label(perm: Permission) -> &'static str {
    match perm {
        Permission::Microphone => "Microphone",
        Permission::SpeechRecognition => "Speech Recognition",
    }
}

/// One-sentence rationale shown under the row title.
pub fn rationale(perm: Permission) -> &'static str {
    match perm {
        Permission::Microphone => "Capture your voice for on-device transcription.",
        #[cfg(target_os = "macos")]
        Permission::SpeechRecognition => "Run Apple's on-device speech model on captured audio.",
        #[cfg(target_os = "windows")]
        Permission::SpeechRecognition => {
            "Downloads a small Japanese Vosk model (~40 MB, once) for on-device transcription."
        },
        #[cfg(not(any(target_os = "macos", target_os = "windows")))]
        Permission::SpeechRecognition => "Run on-device speech recognition on captured audio.",
    }
}

/// Short status label rendered on the right of an onboarding row.
pub fn status_label(status: PermissionStatus) -> &'static str {
    match status {
        PermissionStatus::Undetermined => "Not requested",
        PermissionStatus::Denied => "Denied",
        PermissionStatus::Granted => "Granted",
        PermissionStatus::Restricted => "Restricted",
    }
}

/// Status line while a Vosk model zip is downloading (Windows speech row).
#[cfg(target_os = "windows")]
pub fn speech_download_status(progress: SpeechDownloadProgress) -> String {
    match progress.total {
        Some(total) if total > 0 => {
            let pct = progress.received.saturating_mul(100) / total;
            format!("Downloading… {pct}%")
        },
        _ => "Downloading…".to_string(),
    }
}

/// Primary action label for an onboarding row button.
pub fn action_label(
    perm: Permission,
    status: PermissionStatus,
) -> &'static str {
    match (perm, status) {
        #[cfg(target_os = "windows")]
        (Permission::SpeechRecognition, PermissionStatus::Undetermined) => "Download",
        (_, PermissionStatus::Denied) => "Open Settings",
        _ => "Allow",
    }
}
