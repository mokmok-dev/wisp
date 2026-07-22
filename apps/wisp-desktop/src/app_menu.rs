//! macOS menu bar: application menu (About, MCP Setup, recording, export,
//! and Quit) plus their keyboard shortcuts.

use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;
use gpui::{App, Entity, KeyBinding, Menu, MenuItem, actions};

use crate::about_view;
use crate::app::{AppError, AppModel, SessionState, View};
use crate::library::SharedStorage;
use crate::mcp_setup_view;
use crate::session_runner::SessionRunner;
use crate::session_updates::{apply_update, retry_pending_persistence, write_recovery_snapshot};
use crate::transcript_export::{self, suggested_export_name};

actions!(
    wisp_desktop,
    [
        Quit,
        About,
        OpenMcpSetup,
        ToggleRecording,
        CopyTranscript,
        ExportTranscript
    ]
);

/// Wire up the menu bar, keyboard shortcuts, and quit handlers.
pub fn configure(
    cx: &mut App,
    runner: Arc<SessionRunner>,
    storage: SharedStorage,
    model: Entity<AppModel>,
    on_set_local_mcp_enabled: Arc<dyn Fn(bool, &mut App)>,
    data_dir: PathBuf,
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

    let model_for_mcp_setup = model.clone();
    let mcp_setup_window: Rc<RefCell<Option<gpui::WindowHandle<mcp_setup_view::McpSetupView>>>> =
        Rc::new(RefCell::new(None));
    cx.on_action(move |_: &OpenMcpSetup, cx| {
        let existing = *mcp_setup_window.borrow();
        if let Some(handle) = existing {
            if cx.active_window() == Some(handle.into()) {
                return;
            }
            if handle
                .update(cx, |_, window, _| window.activate_window())
                .is_ok()
            {
                return;
            }
        }

        let window = mcp_setup_view::open(
            cx,
            model_for_mcp_setup.clone(),
            on_set_local_mcp_enabled.clone(),
        );
        *mcp_setup_window.borrow_mut() = Some(window);
    });

    // Start/stop recording straight from the menu bar (and Cmd+R), so the
    // user doesn't have to reach for the in-window Record button. Reuses the
    // same state machine as that button via `toggle_recording`.
    let runner_for_toggle = runner.clone();
    let model_for_toggle = model.clone();
    let storage_for_toggle = storage.clone();
    let data_for_toggle = data_dir;
    cx.on_action(move |_: &ToggleRecording, cx| {
        crate::toggle_recording(
            &runner_for_toggle,
            &model_for_toggle,
            &storage_for_toggle,
            &data_for_toggle,
            &recordings_dir,
            cx,
        );
    });

    let model_for_copy = model.clone();
    cx.on_action(move |_: &CopyTranscript, cx| {
        let app = model_for_copy.read(cx);
        if !matches!(app.view, View::LiveSession | View::History { .. }) {
            return;
        }
        transcript_export::copy_transcript_to_clipboard(&app.segments, cx);
    });

    let model_for_export = model.clone();
    cx.on_action(move |_: &ExportTranscript, cx| {
        let app = model_for_export.read(cx);
        if !matches!(app.view, View::LiveSession | View::History { .. }) {
            return;
        }
        let session = app.viewed_session.clone();
        let name =
            suggested_export_name(session.as_ref().map(|s| s.title.as_str()), "transcript");
        let segments = app.segments.clone();
        transcript_export::export_transcript(segments, session, &name, cx);
    });

    cx.bind_keys([
        KeyBinding::new("cmd-q", Quit, None),
        KeyBinding::new("cmd-,", OpenMcpSetup, None),
        KeyBinding::new("cmd-r", ToggleRecording, None),
        KeyBinding::new("cmd-shift-c", CopyTranscript, None),
        KeyBinding::new("cmd-shift-e", ExportTranscript, None),
    ]);

    // The recording item's label flips between "Start" and "Stop" with the
    // session state. `set_menus` rebuilds the whole native menu, so we only
    // call it when the label actually changes (not on every transcript tick).
    let mut last_label = {
        let app = model.read(cx);
        recording_menu_label(app.state, app.has_pending_persistence())
    };
    cx.set_menus(build_menus(last_label));
    cx.observe(&model, move |model, cx| {
        let app = model.read(cx);
        let label = recording_menu_label(app.state, app.has_pending_persistence());
        if label != last_label {
            last_label = label;
            cx.set_menus(build_menus(label));
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

/// Build the application menu with the recording item carrying `record_label`.
fn build_menus(record_label: &'static str) -> Vec<Menu> {
    vec![Menu {
        name: "Wisp".into(),
        items: vec![
            MenuItem::action("About Wisp", About),
            MenuItem::separator(),
            MenuItem::action("MCP Setup…", OpenMcpSetup),
            MenuItem::separator(),
            MenuItem::action(record_label, ToggleRecording),
            MenuItem::separator(),
            MenuItem::action("Copy Transcript", CopyTranscript),
            MenuItem::action("Export Transcript…", ExportTranscript),
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
            m.set_state(SessionState::Stopping);
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
    use gpui::MenuItem;

    use super::{OpenMcpSetup, build_menus, recording_menu_label};
    use crate::app::SessionState;

    #[test]
    fn menu_exposes_mcp_setup_action() {
        let menus = build_menus("Start Recording");
        let menu = menus.first().expect("Wisp menu");

        let actions = menu
            .items
            .iter()
            .filter_map(|item| match item {
                MenuItem::Action { name, action, .. } if name.as_ref() == "MCP Setup…" => {
                    Some(action.as_ref())
                },
                _ => None,
            })
            .collect::<Vec<_>>();

        assert_eq!(actions.len(), 1);
        assert!(actions[0].as_any().is::<OpenMcpSetup>());
    }

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
