//! macOS menu bar: application menu (About, recording, export, and Quit)
//! plus their keyboard shortcuts.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;
use gpui::{App, Entity, KeyBinding, Menu, MenuItem, actions};
use gpui_kit::component::{WindowExt as _, notification::Notification};

use crate::about_view;
use crate::app::{AppError, AppModel, SessionState, View};
use crate::library::SharedStorage;
use crate::session_runner::SessionRunner;
use crate::session_updates::{apply_update, retry_pending_persistence, write_recovery_snapshot};
use crate::settings::{self, ThemePreference};
use crate::transcript_export::{self, suggested_export_name};

actions!(
    wisp_desktop,
    [
        Quit,
        About,
        OpenSettings,
        ToggleRecording,
        CopyTranscript,
        ExportTranscript,
        ToggleTheme,
        FollowSystemAppearance,
        LightAppearance,
        DarkAppearance
    ]
);

/// Wire up the menu bar, keyboard shortcuts, and quit handlers.
pub fn configure(
    cx: &mut App,
    runner: Arc<SessionRunner>,
    storage: SharedStorage,
    model: Entity<AppModel>,
    recordings_dir: PathBuf,
) {
    let runner_for_quit = runner.clone();
    let model_for_quit = model.clone();
    let storage_for_quit = storage.clone();

    cx.on_action(move |_: &Quit, cx| {
        if graceful_stop_session(&runner_for_quit, &model_for_quit, &storage_for_quit, cx) {
            cx.quit();
        }
    });

    cx.on_action(|_: &About, cx| {
        about_view::open(cx);
    });

    let model_for_settings = model.clone();
    cx.on_action(move |_: &OpenSettings, cx| {
        model_for_settings.update(cx, |model, cx| {
            model.show_settings();
            cx.notify();
        });
    });

    // Start/stop recording straight from the menu bar (and Cmd+R), so the
    // user doesn't have to reach for the in-window Record button. Reuses the
    // same state machine as that button via `toggle_recording`.
    let runner_for_toggle = runner.clone();
    let model_for_toggle = model.clone();
    let storage_for_toggle = storage.clone();
    cx.on_action(move |_: &ToggleRecording, cx| {
        crate::toggle_recording(
            &runner_for_toggle,
            &model_for_toggle,
            &storage_for_toggle,
            &recordings_dir,
            cx,
        );
    });

    install_transcript_actions(cx, model.clone());

    // Light/dark appearance. The title-bar button and the menu entries all go
    // through the same persisted preference, and rebuild the menu so the
    // checks in the Appearance submenu stay current.
    let model_for_theme = model.clone();
    cx.on_action(move |_: &ToggleTheme, cx| {
        settings::toggle(cx);
        refresh_menus(cx, &model_for_theme);
    });
    let model_for_system = model.clone();
    cx.on_action(move |_: &FollowSystemAppearance, cx| {
        settings::set_system(cx);
        refresh_menus(cx, &model_for_system);
    });
    let model_for_light = model.clone();
    cx.on_action(move |_: &LightAppearance, cx| {
        settings::set_light(cx);
        refresh_menus(cx, &model_for_light);
    });
    let model_for_dark = model.clone();
    cx.on_action(move |_: &DarkAppearance, cx| {
        settings::set_dark(cx);
        refresh_menus(cx, &model_for_dark);
    });

    cx.bind_keys([
        KeyBinding::new("cmd-q", Quit, None),
        KeyBinding::new("cmd-,", OpenSettings, None),
        KeyBinding::new("cmd-r", ToggleRecording, None),
        KeyBinding::new("cmd-shift-c", CopyTranscript, None),
        KeyBinding::new("cmd-shift-e", ExportTranscript, None),
        KeyBinding::new("cmd-shift-d", ToggleTheme, None),
    ]);

    // The recording item's label flips between "Start" and "Stop" with the
    // session state. `set_menus` rebuilds the whole native menu, so we only
    // call it when the label actually changes (not on every transcript tick).
    let mut last_label = {
        let app = model.read(cx);
        recording_menu_label(app.state, app.has_pending_persistence())
    };
    refresh_menus(cx, &model);
    cx.observe(&model, move |model, cx| {
        let label = {
            let app = model.read(cx);
            recording_menu_label(app.state, app.has_pending_persistence())
        };
        if label != last_label {
            last_label = label;
            refresh_menus(cx, &model);
        }
    })
    .detach();

    let runner_for_shutdown = runner;
    let model_for_shutdown = model;
    let storage_for_shutdown = storage;
    let _ = cx.on_app_quit(move |cx| {
        let _ = graceful_stop_session(
            &runner_for_shutdown,
            &model_for_shutdown,
            &storage_for_shutdown,
            cx,
        );
        async move {}
    });
}

/// Copy and export act on the transcript shown in the live or history view.
/// Menu actions run without a window argument, so feedback goes to the
/// frontmost window when one exists.
fn install_transcript_actions(
    cx: &mut App,
    model: Entity<AppModel>,
) {
    let model_for_copy = model.clone();
    cx.on_action(move |_: &CopyTranscript, cx| {
        let copied = {
            let app = model_for_copy.read(cx);
            if !matches!(
                app.view,
                View::LiveSession | View::History { .. } | View::Settings
            ) {
                return;
            }
            transcript_export::copy_transcript_to_clipboard(&app.segments, cx)
        };
        push_notification(
            cx,
            if copied {
                Notification::success("Transcript copied to the clipboard.")
            } else {
                Notification::error("There is no transcript to copy yet.")
            },
        );
    });

    cx.on_action(move |_: &ExportTranscript, cx| {
        let (text, name) = {
            let app = model.read(cx);
            if !matches!(
                app.view,
                View::LiveSession | View::History { .. } | View::Settings
            ) {
                return;
            }
            let session = app.viewed_session.as_ref();
            (
                transcript_export::format_transcript_markdown(session, &app.segments),
                suggested_export_name(session.map(|s| s.title.as_str()), "transcript"),
            )
        };
        if text.is_empty() {
            push_notification(
                cx,
                Notification::error("There is no transcript to export yet."),
            );
            return;
        }
        let started = transcript_export::export_transcript(text, &name, cx);
        push_notification(
            cx,
            if started {
                Notification::success("Choose where to save the Markdown file.")
            } else {
                Notification::error("The transcript could not be exported.")
            },
        );
    });
}

/// Route menu feedback to the frontmost window. Menu actions run without a
/// window argument, and the app may have no window open at all.
fn push_notification(
    cx: &mut App,
    note: Notification,
) {
    if let Some(handle) = cx.active_window() {
        let _ = handle.update(cx, |_, window, cx| window.push_notification(note, cx));
    }
}

/// The recording menu item's label for the given session state: "Stop" while
/// a session is live (or transitioning), "Start" otherwise.
fn recording_menu_label(
    state: SessionState,
    pending_persistence: bool,
) -> &'static str {
    if pending_persistence {
        return "Retry Saving Session";
    }
    match state {
        SessionState::Recording { .. } | SessionState::Starting | SessionState::Stopping => {
            "Stop Recording"
        },
        SessionState::Idle | SessionState::Failed => "Start Recording",
    }
}

/// Rebuild the native menu bar from the current recording label and theme
/// preference. Called by the theme actions (to refresh the Appearance checks)
/// and when the recording state changes.
pub(crate) fn refresh_menus(
    cx: &mut App,
    model: &Entity<AppModel>,
) {
    let (record_label, theme) = {
        let app = model.read(cx);
        (
            recording_menu_label(app.state, app.has_pending_persistence()),
            settings::preference(cx),
        )
    };
    cx.set_menus(build_menus(record_label, theme));
}

/// Build the application menu with the recording item carrying `record_label`
/// and checkmarks for the active appearance preference.
fn build_menus(
    record_label: &'static str,
    theme: ThemePreference,
) -> Vec<Menu> {
    vec![Menu {
        name: "Wisp".into(),
        disabled: false,
        items: vec![
            MenuItem::action("About Wisp", About),
            MenuItem::separator(),
            MenuItem::action(record_label, ToggleRecording),
            MenuItem::separator(),
            MenuItem::action("Copy Transcript", CopyTranscript),
            MenuItem::action("Export Transcript…", ExportTranscript),
            MenuItem::separator(),
            MenuItem::action("Settings…", OpenSettings),
            MenuItem::separator(),
            MenuItem::action("Toggle Light/Dark Appearance", ToggleTheme),
            MenuItem::submenu(Menu {
                name: "Appearance".into(),
                disabled: false,
                items: vec![
                    MenuItem::action("Follow System", FollowSystemAppearance)
                        .checked(theme == ThemePreference::System),
                    MenuItem::action("Light", LightAppearance)
                        .checked(theme == ThemePreference::Light),
                    MenuItem::action("Dark", DarkAppearance)
                        .checked(theme == ThemePreference::Dark),
                ],
            }),
            MenuItem::separator(),
            MenuItem::action("Quit Wisp", Quit),
        ],
    }]
}

/// If a recording is active (or stopping), request stop and wait for the
/// worker to finish so segments can be persisted before exit. Returns false
/// when an explicit quit must be cancelled because neither final persistence
/// nor a durable recovery snapshot could be completed.
fn graceful_stop_session(
    runner: &SessionRunner,
    model: &Entity<AppModel>,
    storage: &SharedStorage,
    cx: &mut App,
) -> bool {
    if model.read(cx).has_pending_persistence() {
        return model.update(cx, |model, cx| {
            let succeeded = retry_pending_persistence(model, storage);
            cx.notify();
            succeeded
        });
    }

    let state = model.read(cx).state;
    let needs_stop = matches!(
        state,
        SessionState::Recording { .. } | SessionState::Starting
    );
    if needs_stop {
        runner.stop();
        model.update(cx, |m, cx| {
            m.set_state(m.state.request_stop());
            cx.notify();
        });
    }

    let session_id = {
        let app = model.read(cx);
        if matches!(
            app.state,
            SessionState::Recording { .. } | SessionState::Starting | SessionState::Stopping
        ) {
            app.current_session_id
        } else {
            None
        }
    };
    if let Some(session_id) = session_id {
        let updates = runner.wait_for_idle(session_id, Duration::from_secs(5));
        model.update(cx, |m, cx| {
            for update in updates {
                apply_update(update, m, storage);
            }
            cx.notify();
        });
    }

    model.update(cx, |m, cx| {
        // A stopped transcript whose transaction rolled back gets one
        // immediate retry. The record/menu button exposes the same retry if
        // the user elects to remain in the app.
        if m.has_pending_persistence() {
            retry_pending_persistence(m, storage);
        }

        if !m.has_unsettled_session() {
            cx.notify();
            return true;
        }

        let worker_is_still_active = m.state.is_active();
        if worker_is_still_active && m.pending_session_write.is_none() {
            m.pending_session_write = Some(crate::app::PendingSessionWrite::Finalise {
                ended_at: Utc::now(),
            });
        }
        let recovery_result = write_recovery_snapshot(m);
        let recovery_is_durable = recovery_result.is_ok();
        if let Err(error) = recovery_result {
            m.last_error = Some(AppError::Persistence(format!(
                "cannot quit safely because the recovery transcript could not be saved: {error}"
            )));
        }
        cx.notify();

        // A snapshot is sufficient only after the worker has stopped. If the
        // five-second stop barrier timed out, keep an explicit quit pending so
        // later native callbacks cannot be lost. The OS-level quit callback
        // still performs this same best-effort snapshot when exit is not
        // cancellable by GPUI.
        !worker_is_still_active && recovery_is_durable
    })
}

#[cfg(test)]
mod tests {
    use super::recording_menu_label;
    use crate::app::SessionState;

    #[test]
    fn menu_exposes_persistence_retry_instead_of_a_new_recording() {
        assert_eq!(
            recording_menu_label(SessionState::Failed, true),
            "Retry Saving Session"
        );
        assert_eq!(
            recording_menu_label(SessionState::Failed, false),
            "Start Recording"
        );
    }
}
