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
//!      finalising the pre-allocated DB row when the worker stops.
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
use wisp_audiokit::SessionError;
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

use app::{AppError, AppModel, PendingSessionWrite, SessionState};
use app_menu::configure as configure_app_menu;
use library::SharedStorage;
use session_runner::{SessionRunner, SessionStart};
use session_updates::apply_update;
use transcript_view::{
    TranscriptView, cursor_blink_period, new_transcript_list_state, ui_tick_period,
};

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
        let model = cx.new(|_| {
            let mut model = AppModel::new();
            session_updates::recover_pending_sessions(&mut model, &storage, &recordings_dir);
            model
        });

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
            MainWindowDeps {
                runner: runner.clone(),
                storage: storage.clone(),
                model: model.clone(),
                recordings_dir: recordings_dir.clone(),
            },
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

struct MainWindowDeps {
    runner: Arc<SessionRunner>,
    storage: SharedStorage,
    model: Entity<AppModel>,
    recordings_dir: PathBuf,
}

#[allow(clippy::too_many_lines)]
fn open_main_window(
    cx: &mut App,
    window_options: WindowOptions,
    deps: MainWindowDeps,
) -> WindowHandle<TranscriptView> {
    let MainWindowDeps {
        runner,
        storage,
        model,
        recordings_dir,
    } = deps;
    cx.open_window(window_options, move |_, cx| {
        cx.new(|cx| {
            let model_for_toggle = model.clone();
            let model_for_mute = model.clone();
            let model_for_request = model.clone();
            let model_for_new = model.clone();
            let model_for_open_history = model.clone();
            let model_for_back = model.clone();
            let storage_for_toggle = storage.clone();
            let storage_for_open_history = storage.clone();
            let recordings_for_toggle = recordings_dir.clone();
            let runner_for_toggle = runner.clone();
            let runner_for_mute = runner.clone();
            let (transcript_list, follow_transcript) = new_transcript_list_state();
            let view = TranscriptView {
                app: model.clone(),
                cursor_visible: true,
                transcript_list,
                transcript_list_count: 0,
                transcript_active_len: 0,
                transcript_list_view: app::View::Library,
                follow_transcript,
                last_signature: (0, 0),
                on_toggle_record: Arc::new(move |_window, cx| {
                    toggle_recording(
                        &runner_for_toggle,
                        &model_for_toggle,
                        &storage_for_toggle,
                        &recordings_for_toggle,
                        cx,
                    );
                }),
                on_toggle_microphone_mute: Arc::new(move |_window, cx| {
                    let muted = !model_for_mute.read(cx).microphone_muted;
                    if runner_for_mute.set_microphone_muted(muted) {
                        model_for_mute.update(cx, |model, cx| {
                            if matches!(model.state, SessionState::Recording { .. }) {
                                model.microphone_muted = muted;
                                cx.notify();
                            }
                        });
                    }
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
/// The session row is allocated before the worker starts. `Stopped` writes
/// finalised segments and stamps `ended_at`; `Error` removes a row whose
/// audio session never started.
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
                if !view.app.read(cx).needs_live_ui_tick() {
                    return;
                }
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
    storage: &SharedStorage,
    recordings_dir: &std::path::Path,
    cx: &mut gpui::App,
) {
    let (state, pending_persistence, setup_complete, config) = {
        let app = model.read(cx);
        (
            app.state,
            app.has_pending_persistence(),
            app.setup_complete(),
            AppModel::session_config("ja-JP"),
        )
    };
    if pending_persistence {
        model.update(cx, |m, cx| {
            if session_updates::retry_pending_persistence(m, storage) {
                session_updates::recover_pending_sessions(m, storage, recordings_dir);
            }
            cx.notify();
        });
        return;
    }
    match state {
        SessionState::Idle | SessionState::Failed => {
            if !setup_complete {
                return;
            }
            // Per-session subdirectory so each recording's Ogg files stay
            // grouped and we can show them as a single library row.
            let started_at = Utc::now();
            let dir_name = library::session_dir_name(started_at);
            let session_dir = recordings_dir.join(&dir_name);
            let session_id =
                match storage
                    .lock()
                    .map_err(|error| error.to_string())
                    .and_then(|store| {
                        library::create_session(&store, started_at, &dir_name)
                            .map_err(|error| error.to_string())
                    }) {
                    Ok(session_id) => session_id,
                    Err(error) => {
                        model.update(cx, |m, cx| {
                            m.begin_session();
                            m.fail(AppError::Persistence(format!(
                                "could not create the session record: {error}"
                            )));
                            cx.notify();
                        });
                        return;
                    },
                };
            let did_begin = model.update(cx, |m, cx| {
                let did_begin = m.begin_session_start();
                if did_begin {
                    m.current_session_id = Some(session_id);
                    m.linked_session_id = Some(session_id);
                    m.current_session_started_at = Some(started_at);
                    m.current_session_dir_name = Some(dir_name.clone());
                    m.current_output_dir = Some(session_dir.clone());
                    cx.notify();
                }
                did_begin
            });
            if !did_begin {
                if let Ok(store) = storage.lock() {
                    let _ = store.sessions().delete(session_id);
                }
                return;
            }
            let session = SessionStart {
                session_id,
                started_at,
                dir_name,
            };
            if !runner.start(session_dir, config, session) {
                model.update(cx, |m, cx| {
                    if m.current_session_id == Some(session_id) {
                        m.pending_session_write = Some(PendingSessionWrite::Delete);
                        if session_updates::retry_pending_persistence(m, storage) {
                            m.fail(SessionError::Start(
                                "session runner is no longer available".into(),
                            ));
                        }
                        cx.notify();
                    }
                });
            }
        },
        SessionState::Recording { .. } => {
            model.update(cx, |m, cx| {
                m.set_state(m.state.request_stop());
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
    // Recording against an in-memory fallback would look successful and then
    // discard the transcript at process exit. Fail closed before the user can
    // start a session when durable storage is unavailable.
    let storage = match Storage::open(data_dir) {
        Ok(storage) => storage,
        Err(error) => {
            eprintln!(
                "wisp: cannot open durable storage at {}: {error}",
                data_dir.display()
            );
            std::process::exit(1);
        },
    };
    Arc::new(Mutex::new(storage))
}

fn default_data_directory() -> PathBuf {
    if let Ok(dir) = std::env::var("WISP_DATA_DIR") {
        return PathBuf::from(dir);
    }
    // ~/Library/Application Support/dev.mokmok.wisp/ on macOS. An ephemeral
    // temp directory is not a safe persistence fallback, so require HOME (or
    // an explicit WISP_DATA_DIR) before recording can be enabled.
    let Some(home) = std::env::var_os("HOME") else {
        eprintln!("wisp: HOME is unavailable; set WISP_DATA_DIR to durable storage");
        std::process::exit(1);
    };
    let mut p = PathBuf::from(home);
    p.push("Library");
    p.push("Application Support");
    p.push("dev.mokmok.wisp");
    p
}
