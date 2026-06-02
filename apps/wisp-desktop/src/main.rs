//! Wisp desktop app — `GPUI` shell.
//!
//! Wires together the building blocks defined in the sibling modules:
//!
//!   * `app::AppModel` — transcript + lifecycle + library state. UI reads,
//!     the session-runner bridge writes.
//!   * `session_runner::SessionRunner` — background OS thread that owns the
//!     Swift `wisp_audiokit::Session` and surfaces events via a channel.
//!   * `transcript_view::TranscriptView` — the GPUI render target.
//!   * `library` — bridges the in-memory transcript with `wisp_storage`
//!     so sessions persist across restarts and can be reviewed later.
//!
//! Three `cx.spawn` async tasks plumb everything together:
//!
//!   1. Drain `SessionRunner` updates into `AppModel` every ~33ms, doing
//!      DB writes at session boundaries (Started / Stopped).
//!   2. Toggle the ghost-text cursor on the view every 500ms and refresh
//!      the status bar's elapsed counter at 250ms so it stays smooth.
//!   3. Re-poll permission status periodically.

// We deliberately panic loudly on window-setup failures (clearer than a
// silently-dropped Result hidden behind `?` in `main`).
#![allow(clippy::expect_used)]

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use chrono::Utc;
use gpui::{
    App, AppContext, Application, AsyncApp, Bounds, Entity, Timer, TitlebarOptions, WindowBounds,
    WindowHandle, WindowOptions, px, size,
};
use wisp_core::SessionId;
use wisp_storage::Storage;

mod about_view;
mod app;
mod app_menu;
mod library;
mod permissions;
mod session_runner;
mod session_updates;
mod transcript_export;
mod transcript_view;

use app::{AppModel, SessionState};
use app_menu::configure as configure_app_menu;
use library::SharedStorage;
use session_runner::SessionRunner;
use session_updates::apply_update;
use transcript_view::{TranscriptView, cursor_blink_period, ui_tick_period};

/// How often we re-poll permission status from the OS while the
/// onboarding screen is up. The user might flip the toggle in System
/// Settings; without periodic re-checks we'd stay stuck on "Denied" until
/// they manually re-focus our window. 1.5s is unhurried but still feels
/// responsive when they come back.
const PERMISSION_REFRESH_INTERVAL: Duration = Duration::from_millis(1500);

fn main() {
    Application::new().run(|cx| {
        cx.activate(true);

        let data_dir = default_data_directory();
        let recordings_dir = data_dir.join("recordings");
        let storage = open_storage(&data_dir);
        let runner = Arc::new(SessionRunner::spawn());
        let model = cx.new(|_| AppModel::new());

        // Populate the library list synchronously at launch so the first
        // paint of the window already shows the user's saved sessions.
        refresh_library(&storage, &model, cx);

        let bounds = Bounds::centered(None, size(px(900.0), px(640.0)), cx);
        let window_options = WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(bounds)),
            titlebar: Some(TitlebarOptions::default()),
            ..Default::default()
        };

        // Populate the model with the initial permission state so the
        // window opens straight onto onboarding or the library screen,
        // without a flash of the wrong content.
        permissions::refresh(&model, cx);

        let window = open_main_window(
            cx,
            window_options,
            runner.clone(),
            storage.clone(),
            model.clone(),
            recordings_dir.clone(),
        );

        configure_app_menu(
            cx,
            runner.clone(),
            storage.clone(),
            model.clone(),
            recordings_dir,
        );

        spawn_session_update_pump(cx, runner, storage, model.clone());
        spawn_cursor_blink(cx, window);
        spawn_permission_refresh(cx, model);
    });
}

fn open_main_window(
    cx: &mut App,
    window_options: WindowOptions,
    runner: Arc<SessionRunner>,
    storage: SharedStorage,
    model: Entity<AppModel>,
    recordings_dir: PathBuf,
) -> WindowHandle<TranscriptView> {
    cx.open_window(window_options, move |_, cx| {
        cx.new(|cx| {
            let model_for_toggle = model.clone();
            let model_for_request = model.clone();
            let model_for_new = model.clone();
            let model_for_open_history = model.clone();
            let model_for_back = model.clone();
            let storage_for_open_history = storage.clone();
            let recordings_for_toggle = recordings_dir.clone();
            let runner_for_toggle = runner.clone();
            let view = TranscriptView {
                app: model.clone(),
                cursor_visible: true,
                scroll_handle: gpui::ScrollHandle::new(),
                last_signature: (0, 0),
                on_toggle_record: Arc::new(move |_window, cx| {
                    toggle_recording(
                        &runner_for_toggle,
                        &model_for_toggle,
                        &recordings_for_toggle,
                        cx,
                    );
                }),
                on_request_permission: Arc::new(move |perm, _window, cx| {
                    permissions::request(perm, model_for_request.clone(), cx);
                }),
                on_open_settings: Arc::new(move |perm, _window, _cx| {
                    permissions::open_settings(perm);
                    // The next periodic permission refresh picks up the
                    // toggle once the user flips it in System Settings.
                }),
                on_new_session: Arc::new(move |_window, cx| {
                    model_for_new.update(cx, |m, cx| {
                        m.show_new_session();
                        cx.notify();
                    });
                }),
                on_open_history: Arc::new(move |session_id, _window, cx| {
                    open_history(
                        &storage_for_open_history,
                        &model_for_open_history,
                        session_id,
                        cx,
                    );
                }),
                on_back_to_library: Arc::new(move |_window, cx| {
                    model_for_back.update(cx, |m, cx| {
                        m.show_library();
                        cx.notify();
                    });
                }),
            };
            // Re-render whenever the underlying model changes.
            cx.observe(&view.app, |_, _, cx| cx.notify()).detach();
            view
        })
    })
    .expect("failed to open Wisp window")
}

/// Drain `SessionRunner` updates into the model every ~33ms.
///
/// At the same time, persist the recording lifecycle into storage:
/// `Started` inserts a session row, `Stopped` writes finalised segments
/// and stamps `ended_at`, `Error` clears the in-flight session so it
/// doesn't dangle in the library as a half-recorded row.
fn spawn_session_update_pump(
    cx: &mut App,
    runner: Arc<SessionRunner>,
    storage: SharedStorage,
    model: Entity<AppModel>,
) {
    cx.spawn(async move |cx: &mut AsyncApp| {
        loop {
            Timer::after(Duration::from_millis(33)).await;
            let updates = runner.drain_updates();
            if updates.is_empty() {
                continue;
            }
            let result = model.update(cx, |model, cx| {
                for u in updates {
                    apply_update(u, model, &storage);
                }
                cx.notify();
            });
            if result.is_err() {
                break;
            }
        }
    })
    .detach();
}

/// Toggle the ghost-text cursor and refresh the status-bar elapsed counter.
fn spawn_cursor_blink(
    cx: &mut App,
    window: WindowHandle<TranscriptView>,
) {
    cx.spawn(async move |cx: &mut AsyncApp| {
        let mut elapsed = Duration::ZERO;
        loop {
            Timer::after(ui_tick_period()).await;
            elapsed += ui_tick_period();
            let ticks = elapsed.as_millis() / cursor_blink_period().as_millis();
            let blink = ticks.is_multiple_of(2);
            let result = window.update(cx, |view, _, cx| {
                view.cursor_visible = blink;
                cx.notify();
            });
            if result.is_err() {
                break;
            }
        }
    })
    .detach();
}

/// Re-read permission state from the OS on a fixed interval. The user may
/// have flipped a toggle in System Settings; we have no event-driven way
/// to learn about that, so we poll. Cheap (two
/// `AVAudioApplication`/`SFSpeechRecognizer` getters).
fn spawn_permission_refresh(
    cx: &mut App,
    model: Entity<AppModel>,
) {
    cx.spawn(async move |cx: &mut AsyncApp| {
        loop {
            Timer::after(PERMISSION_REFRESH_INTERVAL).await;
            let result = cx.update(|cx| permissions::refresh(&model, cx));
            if result.is_err() {
                break;
            }
        }
    })
    .detach();
}

pub(crate) fn toggle_recording(
    runner: &SessionRunner,
    model: &gpui::Entity<AppModel>,
    recordings_dir: &std::path::Path,
    cx: &mut gpui::App,
) {
    let state = model.read(cx).state;
    match state {
        SessionState::Idle | SessionState::Failed => {
            // Per-session subdirectory so each recording's WAVs stay
            // grouped and we can show them as a single library row.
            let session_dir = recordings_dir.join(library::session_dir_name(Utc::now()));
            model.update(cx, |m, cx| {
                m.segments.clear();
                m.last_error = None;
                m.set_state(SessionState::Starting);
                cx.notify();
            });
            runner.start(session_dir, "ja-JP".to_string());
        },
        SessionState::Recording { .. } => {
            model.update(cx, |m, cx| {
                m.set_state(SessionState::Stopping);
                cx.notify();
            });
            runner.stop();
        },
        SessionState::Starting | SessionState::Stopping => {
            // ignore taps while a transition is in flight
        },
    }
}

fn open_history(
    storage: &SharedStorage,
    model: &Entity<AppModel>,
    session_id: SessionId,
    cx: &mut App,
) {
    let Ok(store) = storage.lock() else {
        return;
    };
    let Some(session) = store.sessions().get(session_id).ok().flatten() else {
        return;
    };
    let segments = library::load_history(&store, session_id).unwrap_or_default();
    drop(store);
    model.update(cx, |m, cx| {
        m.show_history(session, segments);
        cx.notify();
    });
}

fn refresh_library(
    storage: &SharedStorage,
    model: &Entity<AppModel>,
    cx: &mut App,
) {
    let Ok(store) = storage.lock() else {
        return;
    };
    let Ok(list) = store.sessions().list() else {
        return;
    };
    drop(store);
    model.update(cx, |m, cx| {
        m.set_library(list);
        cx.notify();
    });
}

fn open_storage(data_dir: &std::path::Path) -> SharedStorage {
    // If the on-disk DB can't be opened (disk full, perms), fall back to
    // an in-memory store so the app still starts. We log the path to
    // stderr so it shows up in the system log; the user will see an
    // empty library and no persistence, which is the right failure mode
    // for this kind of catastrophic disk error.
    let storage = Storage::open(data_dir).or_else(|err| {
        eprintln!(
            "wisp: failed to open storage at {}: {err}; falling back to in-memory",
            data_dir.display()
        );
        Storage::open_in_memory()
    });
    let storage = storage.expect("open in-memory storage as last-resort fallback");
    Arc::new(Mutex::new(storage))
}

fn default_data_directory() -> PathBuf {
    if let Ok(dir) = std::env::var("WISP_DATA_DIR") {
        return PathBuf::from(dir);
    }
    // ~/Library/Application Support/dev.mokmok.wisp/ on macOS, or
    // a temp dir if we can't resolve $HOME. The sessions DB lives at
    // <this>/sessions.db and per-session WAV directories under
    // <this>/recordings/<dir-name>/.
    let mut p = std::env::var_os("HOME").map_or_else(std::env::temp_dir, PathBuf::from);
    p.push("Library");
    p.push("Application Support");
    p.push("dev.mokmok.wisp");
    p
}
