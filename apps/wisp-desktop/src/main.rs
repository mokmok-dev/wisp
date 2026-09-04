//! Wisp desktop app — GPUI shell with an embedded web UI.
//!
//! Wires together the building blocks defined in the sibling modules:
//!
//!   * `app::AppModel` — transcript + lifecycle + library state. UI reads,
//!     the session-runner bridge writes.
//!   * `session_runner::SessionRunner` — background OS thread that owns the
//!     Swift `wisp_audiokit::Session` and surfaces events via a channel.
//!   * `web_shell` — the wry webview hosting the Kumo/React UI
//!     (`apps/wisp-desktop/ui`), served offline over the `wisp://` scheme.
//!   * `web_bridge` — serializes model state into webview events.
//!   * `library` — bridges the in-memory transcript with `wisp_storage`
//!     so sessions persist across restarts and can be reviewed later.
//!
//! Two `cx.spawn` async tasks plumb everything together:
//!
//!   1. Drain web-UI commands (`window.ipc.postMessage`) and `SessionRunner`
//!      updates into `AppModel` every ~33ms, finalising the pre-allocated DB
//!      row when the worker stops.
//!   2. Re-poll permission status periodically.

// We deliberately panic loudly on window-setup failures (clearer than a
// silently-dropped Result hidden behind `?` in `main`).
#![allow(clippy::expect_used)]

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use chrono::Utc;
use gpui::{
    App, AppContext, Application, AsyncApp, Bounds, Entity, Timer, TitlebarOptions, WindowBounds,
    WindowOptions, px, size,
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
mod web_bridge;
mod web_server;
mod web_shell;

use app::{AppError, AppModel, PendingSessionWrite, SessionState};
use app_menu::configure as configure_app_menu;
use library::SharedStorage;
use session_runner::{SessionRunner, SessionStart};
use session_updates::apply_update;
use web_bridge::{EventBus, UiBridge};
use web_shell::{CommandContext, UiCommand};

/// How often we re-poll permission status from the OS while the
/// onboarding screen is up. The user might flip the toggle in System
/// Settings; without periodic re-checks we'd stay stuck on "Denied" until
/// they manually re-focus our window. 1.5s is unhurried but still feels
/// responsive when they come back.
const PERMISSION_REFRESH_INTERVAL: Duration = Duration::from_millis(1500);

/// Cadence of the main pump that drains web-UI commands and session
/// updates into the model. 33ms ≈ one frame at 30fps; well under any
/// perceptible threshold for button presses and transcript updates.
const PUMP_INTERVAL: Duration = Duration::from_millis(33);

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

        let (cmd_tx, cmd_rx) = std::sync::mpsc::channel::<UiCommand>();
        let bus = EventBus::new();
        let server = web_server::spawn(cmd_tx, bus.clone(), ui_dev_url().is_some())
            .expect("failed to start loopback web server");
        // Handy while developing: the same UI can be opened in a browser.
        if cfg!(debug_assertions) {
            eprintln!("wisp-webview: UI at {}", server.url);
        }
        let (_window, bridge) = web_shell::open(cx, window_options, &server.url, ui_dev_url(), bus);

        // Push model changes into the webview. The bridge diffs state so
        // each notify only sends what actually changed.
        let bridge_for_observe = bridge.clone();
        cx.observe(&model, move |model, cx| {
            bridge_for_observe.update(cx, |b, cx| b.push_changes(&model, cx));
        })
        .detach();

        configure_app_menu(
            cx,
            runner.clone(),
            storage.clone(),
            model.clone(),
            recordings_dir.clone(),
        );

        spawn_ui_pump(
            cx,
            PumpDeps {
                runner,
                storage,
                model: model.clone(),
                recordings_dir,
                bridge,
                cmd_rx,
            },
        );
        spawn_permission_refresh(cx, model.clone());
    });
}

/// Base URL for the web UI when hot-reloading against a Vite dev server.
fn ui_dev_url() -> Option<String> {
    std::env::var("WISP_UI_DEV_URL")
        .ok()
        .filter(|url| !url.trim().is_empty())
}

struct PumpDeps {
    runner: Arc<SessionRunner>,
    storage: SharedStorage,
    model: Entity<AppModel>,
    recordings_dir: PathBuf,
    bridge: Entity<UiBridge>,
    cmd_rx: std::sync::mpsc::Receiver<UiCommand>,
}

/// Drain web-UI commands and session-runner updates into the model.
fn spawn_ui_pump(
    cx: &mut App,
    deps: PumpDeps,
) {
    let PumpDeps {
        runner,
        storage,
        model,
        recordings_dir,
        bridge,
        cmd_rx,
    } = deps;

    cx.spawn(async move |cx: &mut AsyncApp| {
        loop {
            Timer::after(PUMP_INTERVAL).await;
            let result = cx.update(|cx| {
                // 1. Commands from the web UI arrive on the command
                //    channel (the loopback server thread has no GPUI
                //    context, so they hop through here).
                while let Ok(command) = cmd_rx.try_recv() {
                    let context = CommandContext {
                        runner: &runner,
                        model: &model,
                        storage: &storage,
                        recordings_dir: &recordings_dir,
                        bridge: &bridge,
                    };
                    web_shell::handle_command(command, &context, cx);
                }

                // 2. Session runner updates (transcript events, lifecycle).
                let updates = runner.drain_updates();
                if !updates.is_empty() {
                    model.update(cx, |model, cx| {
                        for u in updates {
                            apply_update(u, model, &storage);
                        }
                        cx.notify();
                    });
                }
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

#[allow(clippy::too_many_lines)] // start/stop state machine, kept linear for clarity
pub(crate) fn toggle_recording(
    runner: &SessionRunner,
    model: &gpui::Entity<AppModel>,
    storage: &SharedStorage,
    recordings_dir: &std::path::Path,
    cx: &mut gpui::App,
) {
    let (state, pending_persistence, setup_complete, config, live_title) = {
        let app = model.read(cx);
        (
            app.state,
            app.has_pending_persistence(),
            app.setup_complete(),
            AppModel::session_config("ja-JP"),
            app.live_title.clone(),
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
            let title = {
                let trimmed = live_title.trim();
                if trimmed.is_empty() {
                    library::default_title(started_at)
                } else {
                    trimmed.to_owned()
                }
            };
            let session_id =
                match storage
                    .lock()
                    .map_err(|error| error.to_string())
                    .and_then(|store| {
                        library::create_session(&store, started_at, &dir_name, &title)
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
                    m.live_title = title;
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

pub(crate) fn open_history(
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

pub(crate) fn refresh_library(
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

/// Track the live-session title as it is edited in the recording top bar.
///
/// The model keeps the raw value so the field stays in sync across IME
/// composition; once a database row exists the trimmed value (or the default
/// timestamp when blank) is persisted so an unnamed session still shows a
/// sensible library title. Mid-composition kana values are overwritten on the
/// next edit or commit and never reach the final transcript.
pub(crate) fn change_live_title(
    model: &Entity<AppModel>,
    storage: &SharedStorage,
    cx: &mut App,
    new_title: &str,
) {
    let trimmed = new_title.trim();
    model.update(cx, |m, cx| {
        m.live_title = new_title.to_string();
        let Some(session_id) = m.current_session_id.or(m.linked_session_id) else {
            cx.notify();
            return;
        };
        let stored = if trimmed.is_empty() {
            m.current_session_started_at
                .map(library::default_title)
                .unwrap_or_default()
        } else {
            trimmed.to_owned()
        };
        if let Ok(store) = storage.lock() {
            let _ = store.sessions().update_title(session_id, &stored);
        }
        cx.notify();
    });
}

/// Apply an inline rename from the library or history header. A blank title
/// keeps the previous value, so clearing the field acts as a cancel.
pub(crate) fn rename_session(
    model: &Entity<AppModel>,
    storage: &SharedStorage,
    session_id: SessionId,
    new_title: &str,
    cx: &mut App,
) {
    let trimmed = new_title.trim();
    let Ok(store) = storage.lock() else {
        return;
    };
    if !trimmed.is_empty() {
        let _ = store.sessions().update_title(session_id, trimmed);
    }
    let Ok(list) = store.sessions().list() else {
        drop(store);
        return;
    };
    drop(store);
    model.update(cx, |m, cx| {
        m.set_library(list);
        if !trimmed.is_empty()
            && let Some(viewed) = &mut m.viewed_session
            && viewed.id == session_id
        {
            trimmed.clone_into(&mut viewed.title);
        }
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
