//! macOS menu bar: application menu (About, Start/Stop Recording, Quit)
//! plus the Cmd+Q and Cmd+R shortcuts.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use gpui::{App, Entity, KeyBinding, Menu, MenuItem, actions};

use crate::about_view;
use crate::app::{AppModel, SessionState, View};
use crate::library::SharedStorage;
use crate::session_runner::SessionRunner;
use crate::session_updates::apply_update;
use crate::transcript_export::{self, suggested_export_name};

actions!(
    wisp_desktop,
    [
        Quit,
        About,
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
    data_dir: PathBuf,
    recordings_dir: PathBuf,
) {
    let runner_for_quit = runner.clone();
    let model_for_quit = model.clone();
    let storage_for_quit = storage.clone();

    cx.on_action(move |_: &Quit, cx| {
        graceful_stop_session(&runner_for_quit, &model_for_quit, &storage_for_quit, cx);
        cx.quit();
    });

    cx.on_action(|_: &About, cx| {
        about_view::open(cx);
    });

    // Start/stop recording straight from the menu bar (and Cmd+R), so the
    // user doesn't have to reach for the in-window Record button. Reuses the
    // same state machine as that button via `toggle_recording`.
    let runner_for_toggle = runner.clone();
    let model_for_toggle = model.clone();
    let data_for_toggle = data_dir;
    cx.on_action(move |_: &ToggleRecording, cx| {
        crate::toggle_recording(
            &runner_for_toggle,
            &model_for_toggle,
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
        let title = app.viewed_session.as_ref().map(|s| s.title.as_str());
        let name = suggested_export_name(title, "transcript");
        transcript_export::export_transcript(app.segments.clone(), &name, cx);
    });

    cx.bind_keys([
        KeyBinding::new("cmd-q", Quit, None),
        KeyBinding::new("cmd-r", ToggleRecording, None),
        KeyBinding::new("cmd-shift-c", CopyTranscript, None),
        KeyBinding::new("cmd-shift-e", ExportTranscript, None),
    ]);

    // The recording item's label flips between "Start" and "Stop" with the
    // session state. `set_menus` rebuilds the whole native menu, so we only
    // call it when the label actually changes (not on every transcript tick).
    let mut last_label = recording_menu_label(model.read(cx).state);
    cx.set_menus(build_menus(last_label));
    cx.observe(&model, move |model, cx| {
        let label = recording_menu_label(model.read(cx).state);
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
        graceful_stop_session(
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
fn recording_menu_label(state: SessionState) -> &'static str {
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
/// worker to finish so segments can be persisted before exit.
fn graceful_stop_session(
    runner: &SessionRunner,
    model: &Entity<AppModel>,
    storage: &SharedStorage,
    cx: &mut App,
) {
    let needs_stop = model.read(cx).state;
    let needs_stop = matches!(
        needs_stop,
        SessionState::Recording { .. } | SessionState::Starting
    );
    if needs_stop {
        runner.stop();
        model.update(cx, |m, cx| {
            m.set_state(SessionState::Stopping);
            cx.notify();
        });
    }

    let should_wait = matches!(
        model.read(cx).state,
        SessionState::Recording { .. } | SessionState::Starting | SessionState::Stopping
    );
    if !should_wait {
        return;
    }

    let updates = runner.wait_for_idle(Duration::from_secs(5));
    let _ = model.update(cx, |m, cx| {
        for update in updates {
            apply_update(update, m, storage);
        }
        cx.notify();
    });
}
