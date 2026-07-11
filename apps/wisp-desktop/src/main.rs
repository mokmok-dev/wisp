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
mod ipc_server;
mod library;
mod mcp_setup_view;
mod permissions;
mod session_runner;
mod session_updates;
mod settings;
mod setup;
mod transcript_export;
mod transcript_view;

use app::{AppModel, LocalMcpBridge, SessionState};
use app_menu::configure as configure_app_menu;
use library::SharedStorage;
use session_runner::SessionRunner;
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
        let mut app_settings = settings::load(&data_dir);
        if let Some(addr) = ipc_server::env_addr_override() {
            app_settings.local_mcp.addr = addr;
        }
        if ipc_server::env_enabled() {
            app_settings.local_mcp.enabled = true;
        }
        let local_mcp = LocalMcpBridge::new(
            app_settings.local_mcp.enabled,
            app_settings.local_mcp.addr.clone(),
            bundled_mcp_command_path(),
        );
        let model = cx.new(|_| AppModel::new_with_data_dir_and_local_mcp(&data_dir, local_mcp));
        let ipc_snapshot = ipc_server::new_shared_snapshot();
        let ipc_handle = Arc::new(Mutex::new(None));

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
        setup::refresh(&model, &data_dir, cx);
        permissions::refresh(&model, cx);
        ipc_server::refresh_snapshot(&ipc_snapshot, model.read(cx));
        if model.read(cx).local_mcp.enabled {
            start_local_mcp_bridge(&ipc_handle, &ipc_snapshot, &model, cx);
        }

        let on_set_local_mcp_enabled: Arc<dyn Fn(bool, &mut App)> = {
            let ipc_handle = ipc_handle.clone();
            let ipc_snapshot = ipc_snapshot.clone();
            let model = model.clone();
            let data_dir = data_dir.clone();
            Arc::new(move |enabled, cx| {
                set_local_mcp_bridge_enabled(
                    enabled,
                    &ipc_handle,
                    &ipc_snapshot,
                    &model,
                    &data_dir,
                    cx,
                );
            })
        };

        let window = open_main_window(
            cx,
            window_options,
            MainWindowDeps {
                runner: runner.clone(),
                storage: storage.clone(),
                model: model.clone(),
                on_set_local_mcp_enabled: on_set_local_mcp_enabled.clone(),
                data_dir: data_dir.clone(),
                recordings_dir: recordings_dir.clone(),
            },
        );

        configure_app_menu(
            cx,
            runner.clone(),
            storage.clone(),
            model.clone(),
            on_set_local_mcp_enabled,
            data_dir.clone(),
            recordings_dir,
        );

        spawn_session_update_pump(cx, runner, storage, model.clone());
        spawn_cursor_blink(cx, window);
        spawn_permission_refresh(cx, model.clone());
        spawn_ipc_snapshot_sync(cx, ipc_snapshot, model);
    });
}

struct MainWindowDeps {
    runner: Arc<SessionRunner>,
    storage: SharedStorage,
    model: Entity<AppModel>,
    on_set_local_mcp_enabled: Arc<dyn Fn(bool, &mut App)>,
    data_dir: PathBuf,
    recordings_dir: PathBuf,
}

fn open_main_window(
    cx: &mut App,
    window_options: WindowOptions,
    deps: MainWindowDeps,
) -> WindowHandle<TranscriptView> {
    let MainWindowDeps {
        runner,
        storage,
        model,
        on_set_local_mcp_enabled,
        data_dir,
        recordings_dir,
    } = deps;
    cx.open_window(window_options, move |_, cx| {
        cx.new(|cx| {
            let model_for_toggle = model.clone();
            let model_for_request = model.clone();
            let model_for_select = model.clone();
            let model_for_download = model.clone();
            let model_for_new = model.clone();
            let model_for_open_history = model.clone();
            let model_for_back = model.clone();
            let model_for_local_mcp = model.clone();
            let storage_for_open_history = storage.clone();
            let data_for_toggle = data_dir.clone();
            let data_for_download = data_dir.clone();
            let recordings_for_toggle = recordings_dir.clone();
            let runner_for_toggle = runner.clone();
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
                        &data_for_toggle,
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
                on_select_recognizer: Arc::new(move |recognizer, _window, cx| {
                    setup::select_recognizer(recognizer, &model_for_select, cx);
                }),
                on_download_local_model: Arc::new(move |_window, cx| {
                    setup::download_model(
                        model_for_download.clone(),
                        data_for_download.clone(),
                        cx,
                    );
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
                on_toggle_local_mcp: Arc::new(move |_window, cx| {
                    let enabled = !model_for_local_mcp.read(cx).local_mcp.enabled;
                    on_set_local_mcp_enabled(enabled, cx);
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

/// Keep the local IPC endpoint's read-only transcript snapshot fresh without
/// letting the HTTP thread touch GPUI state directly.
fn spawn_ipc_snapshot_sync(
    cx: &mut App,
    snapshot: ipc_server::SharedSnapshot,
    model: Entity<AppModel>,
) {
    cx.spawn(async move |cx: &mut AsyncApp| {
        loop {
            Timer::after(Duration::from_millis(250)).await;
            let snapshot_for_update = snapshot.clone();
            let result = model.update(cx, move |model, _cx| {
                ipc_server::refresh_snapshot(&snapshot_for_update, model);
            });
            if result.is_err() {
                break;
            }
        }
    })
    .detach();
}

fn set_local_mcp_bridge_enabled(
    enabled: bool,
    ipc_handle: &Arc<Mutex<Option<ipc_server::IpcServer>>>,
    snapshot: &ipc_server::SharedSnapshot,
    model: &Entity<AppModel>,
    data_dir: &std::path::Path,
    cx: &mut App,
) {
    if enabled {
        if model.read(cx).local_mcp.running {
            return;
        }
        start_local_mcp_bridge(ipc_handle, snapshot, model, cx);
    } else {
        stop_local_mcp_bridge(ipc_handle, model, cx);
    }
    save_local_mcp_settings(data_dir, model, cx);
}

fn start_local_mcp_bridge(
    ipc_handle: &Arc<Mutex<Option<ipc_server::IpcServer>>>,
    snapshot: &ipc_server::SharedSnapshot,
    model: &Entity<AppModel>,
    cx: &mut App,
) {
    let addr = model.read(cx).local_mcp.addr.clone();
    let config = ipc_server::IpcConfig {
        addr,
        token: ipc_server::env_token(),
    };
    match ipc_server::start(config, snapshot.clone()) {
        Ok(server) => {
            let previous = if let Ok(mut slot) = ipc_handle.lock() {
                let previous = slot.take();
                *slot = Some(server);
                previous
            } else {
                model.update(cx, |m, cx| {
                    m.local_mcp.enabled = true;
                    m.local_mcp.running = false;
                    m.local_mcp.error = Some("failed to lock IPC server handle".into());
                    cx.notify();
                });
                return;
            };
            if let Some(previous) = previous {
                previous.stop();
            }
            model.update(cx, |m, cx| {
                m.local_mcp.enabled = true;
                m.local_mcp.running = true;
                m.local_mcp.error = None;
                cx.notify();
            });
        },
        Err(err) => {
            model.update(cx, |m, cx| {
                m.local_mcp.enabled = true;
                m.local_mcp.running = false;
                m.local_mcp.error = Some(err);
                cx.notify();
            });
        },
    }
}

fn stop_local_mcp_bridge(
    ipc_handle: &Arc<Mutex<Option<ipc_server::IpcServer>>>,
    model: &Entity<AppModel>,
    cx: &mut App,
) {
    let server = ipc_handle.lock().ok().and_then(|mut slot| slot.take());
    if let Some(server) = server {
        server.stop();
    }
    model.update(cx, |m, cx| {
        m.local_mcp.enabled = false;
        m.local_mcp.running = false;
        m.local_mcp.error = None;
        cx.notify();
    });
}

fn save_local_mcp_settings(
    data_dir: &std::path::Path,
    model: &Entity<AppModel>,
    cx: &App,
) {
    let local_mcp = model.read(cx).local_mcp.clone();
    let app_settings = settings::AppSettings {
        local_mcp: settings::LocalMcpSettings {
            enabled: local_mcp.enabled,
            addr: local_mcp.addr,
        },
    };
    if let Err(err) = settings::save(data_dir, &app_settings) {
        eprintln!("wisp: failed to save settings: {err}");
    }
}

pub(crate) fn toggle_recording(
    runner: &SessionRunner,
    model: &gpui::Entity<AppModel>,
    data_dir: &std::path::Path,
    recordings_dir: &std::path::Path,
    cx: &mut gpui::App,
) {
    setup::refresh(model, data_dir, cx);
    let (state, setup_complete, config) = {
        let app = model.read(cx);
        (
            app.state,
            app.setup_complete(),
            app.setup.session_config("ja-JP"),
        )
    };
    match state {
        SessionState::Idle | SessionState::Failed => {
            if !setup_complete {
                return;
            }
            // Per-session subdirectory so each recording's WAVs stay
            // grouped and we can show them as a single library row.
            let session_dir = recordings_dir.join(library::session_dir_name(Utc::now()));
            model.update(cx, |m, cx| {
                m.segments.clear();
                m.last_error = None;
                m.set_state(SessionState::Starting);
                cx.notify();
            });
            runner.start(session_dir, config);
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

fn bundled_mcp_command_path() -> String {
    std::env::current_exe().map_or_else(
        |_| "wisp-mcp".to_owned(),
        |mut path| {
            path.set_file_name("wisp-mcp");
            path.display().to_string()
        },
    )
}
