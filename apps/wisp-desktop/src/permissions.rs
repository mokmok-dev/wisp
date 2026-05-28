//! Glue between `wisp_audiokit`'s blocking permission API and the GPUI
//! main loop.
//!
//! The Swift-side `wisp_permission_request` blocks the calling thread on
//! a `DispatchSemaphore` until the user dismisses the OS dialog. Calling
//! that directly from a render callback would freeze the window, so we
//! ship the work to `cx.background_executor()` and write the result back
//! onto the `AppModel` from the main async context.

use std::process::Command;

use gpui::{App, AsyncApp, Entity};
use wisp_audiokit::{Permission, PermissionStatus, check_permission, request_permission};

use crate::app::AppModel;

/// Read the current OS-side status of both permissions and write them
/// into the model. Used at app launch and after the user returns from
/// System Settings (we re-check on every UI tick — see `main.rs`).
pub fn refresh(model: &Entity<AppModel>, cx: &mut App) {
    let microphone = check_permission(Permission::Microphone);
    let speech = check_permission(Permission::SpeechRecognition);
    model.update(cx, |m, cx| {
        let changed =
            m.permissions.microphone != microphone || m.permissions.speech != speech;
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
pub fn request(perm: Permission, model: Entity<AppModel>, cx: &mut App) {
    // Already in flight — ignore re-entrant clicks.
    if model.read(cx).permissions.pending.is_some() {
        return;
    }
    model.update(cx, |m, cx| {
        m.permissions.pending = Some(perm);
        cx.notify();
    });

    cx.spawn(async move |cx: &mut AsyncApp| {
        // The Swift call blocks for the lifetime of the dialog; run it on
        // the background pool so the GPUI main thread stays responsive
        // (animations, status bar tick, etc.).
        let status = cx
            .background_executor()
            .spawn(async move { request_permission(perm) })
            .await;
        let _ = model.update(cx, |m, cx| {
            m.permissions.set_status(perm, status);
            m.permissions.pending = None;
            cx.notify();
        });
    })
    .detach();
}

/// Open the right System Settings → Privacy & Security pane for `perm`.
/// Used when the permission is already `Denied`, because in that state
/// `request_permission` is a no-op and only the user can re-enable it.
pub fn open_settings(perm: Permission) {
    let url = match perm {
        Permission::Microphone => {
            "x-apple.systempreferences:com.apple.preference.security?Privacy_Microphone"
        },
        Permission::SpeechRecognition => {
            "x-apple.systempreferences:com.apple.preference.security?Privacy_SpeechRecognition"
        },
    };
    // Best-effort: if `open` fails (e.g. URL scheme not registered) there's
    // nothing useful we can show the user without their attention here.
    let _ = Command::new("open").arg(url).spawn();
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
        Permission::SpeechRecognition => "Run Apple's on-device speech model on captured audio.",
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
