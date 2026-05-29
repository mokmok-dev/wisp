//! macOS menu bar: application menu (About, Quit) and Cmd+Q.

use std::sync::Arc;
use std::time::Duration;

use gpui::{App, Entity, KeyBinding, Menu, MenuItem, actions};

use crate::about_view;
use crate::app::{AppModel, SessionState};
use crate::library::SharedStorage;
use crate::session_runner::SessionRunner;
use crate::session_updates::apply_update;

actions!(wisp_desktop, [Quit, About]);

/// Wire up the menu bar, keyboard shortcuts, and quit handlers.
pub fn configure(
    cx: &mut App,
    runner: Arc<SessionRunner>,
    storage: SharedStorage,
    model: Entity<AppModel>,
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

    cx.bind_keys([KeyBinding::new("cmd-q", Quit, None)]);

    cx.set_menus(vec![Menu {
        name: "Wisp".into(),
        items: vec![
            MenuItem::action("About Wisp", About),
            MenuItem::separator(),
            MenuItem::action("Quit Wisp", Quit),
        ],
    }]);

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
        SessionState::Recording { .. }
            | SessionState::Starting
            | SessionState::Stopping
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
