//! Wisp desktop app — native GPUI + GPUI Component shell.
//!
//! Wires together the building blocks defined in the sibling modules:
//!
//!   * `app::AppModel` — transcript + lifecycle + library state. Views read,
//!     the session-runner bridge writes.
//!   * `app_view::AppView` — the native UI (title bar, sidebar, onboarding,
//!     library, live session, and history screens) built with GPUI Component
//!     from `longbridge/gpui-kit`.
//!   * `session_runner::SessionRunner` — background OS thread that owns the
//!     Swift `wisp_audiokit::Session` and surfaces events via a channel.
//!   * `library` — bridges the in-memory transcript with `wisp_storage`
//!     so sessions persist across restarts and can be reviewed later.
//!
//! Two `cx.spawn` async tasks plumb everything together:
//!
//!   1. Drain `SessionRunner` updates into `AppModel` every ~33ms,
//!      finalising the pre-allocated DB row when the worker stops.
//!   2. Re-poll permission status periodically.

// We deliberately panic loudly on window-setup failures (clearer than a
// silently-dropped Result hidden behind `?` in `main`).
#![allow(clippy::expect_used)]

// GPUI Kit is the single application-facing dependency: alias it as `gpui` so
// the GPUI APIs already used across the crate resolve through the Kit, exactly
// as the Kit's documentation recommends.
extern crate gpui_kit as gpui;

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use chrono::{DateTime, Utc};
use gpui::{App, AppContext as _, AsyncApp, Bounds, Entity, WindowBounds, WindowOptions, px, size};
use wisp_audiokit::{SessionConfig, SessionError};
use wisp_core::SessionId;
use wisp_storage::Storage;

mod about_view;
mod app;
mod app_menu;
mod app_view;
mod format;
mod history_view;
mod library;
mod live_view;
mod meeting_watch;
mod permissions;
mod session_runner;
mod session_updates;
mod settings;
mod settings_view;
mod transcript_export;
mod transcript_view;

use app::{AppError, AppModel, MeetingPrompt, PendingSessionWrite, SessionState, View};
use app_menu::configure as configure_app_menu;
use app_view::{AppDeps, AppView};
use library::SharedStorage;
use meeting_watch::{MeetingEvent, MeetingWatcher};
use session_runner::{SessionRunner, SessionStart};
use session_updates::apply_update;

/// How often we re-poll permission status from the OS while the
/// onboarding screen is up. The user might flip the toggle in System
/// Settings; without periodic re-checks we'd stay stuck on "Denied" until
/// they manually re-focus our window. 1.5s is unhurried but still feels
/// responsive when they come back.
const PERMISSION_REFRESH_INTERVAL: Duration = Duration::from_millis(1500);

/// Cadence of the main pump that drains session updates into the model.
/// 33ms ≈ one frame at 30fps; well under any perceptible threshold for
/// button presses and transcript updates.
const PUMP_INTERVAL: Duration = Duration::from_millis(33);

fn main() {
    gpui_kit::application()
        .with_assets(gpui_kit::assets::AllAssets)
        .run(|cx| {
            // Must run before any GPUI Component view is constructed.
            gpui_kit::init(cx);
            cx.activate(true);

            let data_dir = default_data_directory();
            // Load persisted preferences (appearance, language, meeting
            // policies) and apply the stored theme before the first window
            // paints.
            settings::init(cx, &data_dir);
            let meeting = Arc::new(MeetingWatcher::spawn(settings::meeting_detection(cx)));
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
                ..gpui_kit::component::TitleBar::window_options()
            };

            // Populate the model with the initial permission state so the
            // window opens straight onto onboarding or the library screen,
            // without a flash of the wrong content.
            permissions::refresh(&model, cx);

            let deps = AppDeps {
                model: model.clone(),
                runner: runner.clone(),
                storage: storage.clone(),
                recordings_dir: recordings_dir.clone(),
                meeting: meeting.clone(),
            };
            let (_window, _view) = gpui_kit::open_window(window_options, cx, move |window, cx| {
                cx.new(|cx| AppView::new(window, cx, deps))
            })
            .expect("failed to open the Wisp window");

            configure_app_menu(
                cx,
                runner.clone(),
                storage.clone(),
                model.clone(),
                recordings_dir.clone(),
            );

            spawn_session_pump(
                cx,
                SessionPump::new(
                    runner,
                    storage,
                    model.clone(),
                    recordings_dir.clone(),
                    meeting,
                ),
            );
            spawn_permission_refresh(cx, model);
        });
}

struct SessionPump {
    runner: Arc<SessionRunner>,
    storage: SharedStorage,
    model: Entity<AppModel>,
    recordings_dir: PathBuf,
    meeting: Arc<MeetingWatcher>,
}

impl SessionPump {
    fn new(
        runner: Arc<SessionRunner>,
        storage: SharedStorage,
        model: Entity<AppModel>,
        recordings_dir: PathBuf,
        meeting: Arc<MeetingWatcher>,
    ) -> Self {
        Self {
            runner,
            storage,
            model,
            recordings_dir,
            meeting,
        }
    }
}

/// Drain session-runner updates (transcript events, lifecycle) into the
/// model. The native views re-render from the model observer in `AppView`.
fn spawn_session_pump(
    cx: &mut App,
    deps: SessionPump,
) {
    cx.spawn(async move |cx: &mut AsyncApp| {
        loop {
            cx.background_executor().timer(PUMP_INTERVAL).await;
            cx.update(|cx| {
                let updates = deps.runner.drain_updates();
                if !updates.is_empty() {
                    deps.model.update(cx, |model, cx| {
                        for update in updates {
                            apply_update(update, model, &deps.storage);
                        }
                        cx.notify();
                    });
                }
                for event in deps.meeting.drain_events() {
                    apply_meeting_event(event, &deps, cx);
                }
            });
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
            cx.background_executor()
                .timer(PERMISSION_REFRESH_INTERVAL)
                .await;
            cx.update(|cx| permissions::refresh(&model, cx));
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
    let language = settings::language(cx);
    let (state, pending_persistence, setup_complete, config, live_title) = {
        let app = model.read(cx);
        (
            app.state,
            app.has_pending_persistence(),
            app.setup_complete(),
            AppModel::session_config(language.as_str()),
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
            if setup_complete {
                start_recording(
                    runner,
                    model,
                    storage,
                    recordings_dir,
                    config,
                    &live_title,
                    cx,
                );
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

/// A detected call may start a recording, but only from an idle app. Unlike
/// [`toggle_recording`] this never stops a session that is already running.
fn start_meeting_recording(
    tool: wisp_core::MeetingTool,
    runner: &SessionRunner,
    model: &gpui::Entity<AppModel>,
    storage: &SharedStorage,
    recordings_dir: &std::path::Path,
    cx: &mut gpui::App,
) {
    let (state, pending_persistence, setup_complete) = {
        let app = model.read(cx);
        (
            app.state,
            app.has_pending_persistence(),
            app.setup_complete(),
        )
    };
    if pending_persistence
        || !matches!(state, SessionState::Idle | SessionState::Failed)
        || !setup_complete
    {
        return;
    }
    let config = AppModel::session_config(settings::language(cx).as_str());
    start_recording(
        runner,
        model,
        storage,
        recordings_dir,
        config,
        tool.label(),
        cx,
    );
}

fn apply_meeting_event(
    event: MeetingEvent,
    deps: &SessionPump,
    cx: &mut gpui::App,
) {
    match event {
        MeetingEvent::Detected { tool, auto_record } => {
            let (active, ready) = {
                let model = deps.model.read(cx);
                (model.state.is_active(), model.setup_complete())
            };
            if active || !ready {
                return;
            }
            if auto_record {
                start_meeting_recording(
                    tool,
                    &deps.runner,
                    &deps.model,
                    &deps.storage,
                    &deps.recordings_dir,
                    cx,
                );
                return;
            }
            deps.model.update(cx, |model, cx| {
                model.meeting_prompt = Some(MeetingPrompt { tool });
                cx.notify();
            });
        },
        MeetingEvent::Ended { tool } => {
            deps.model.update(cx, |model, cx| {
                if model
                    .meeting_prompt
                    .is_some_and(|prompt| prompt.tool == tool)
                {
                    model.meeting_prompt = None;
                    cx.notify();
                }
            });
        },
        MeetingEvent::SamplerUnavailable => {
            deps.model.update(cx, |model, cx| {
                model
                    .recent_log
                    .push_back("meeting detection: microphone activity is unavailable".to_owned());
                while model.recent_log.len() > 200 {
                    model.recent_log.pop_front();
                }
                cx.notify();
            });
        },
    }
}

fn start_recording(
    runner: &SessionRunner,
    model: &gpui::Entity<AppModel>,
    storage: &SharedStorage,
    recordings_dir: &std::path::Path,
    config: SessionConfig,
    live_title: &str,
    cx: &mut gpui::App,
) {
    // Per-session subdirectory so each recording's Ogg files stay
    // grouped and we can show them as a single library row.
    let started_at = Utc::now();
    let dir_name = library::session_dir_name(started_at);
    let session_dir = recordings_dir.join(&dir_name);
    let title = session_title(live_title, started_at);
    let session_id = match create_session_row(storage, started_at, &dir_name, &title) {
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
        abandon_unstarted_recording(model, storage, session_id, cx);
    }
}

fn session_title(
    live_title: &str,
    started_at: DateTime<Utc>,
) -> String {
    let trimmed = live_title.trim();
    if trimmed.is_empty() {
        library::default_title(started_at)
    } else {
        trimmed.to_owned()
    }
}

fn create_session_row(
    storage: &SharedStorage,
    started_at: DateTime<Utc>,
    dir_name: &str,
    title: &str,
) -> Result<SessionId, String> {
    storage
        .lock()
        .map_err(|error| error.to_string())
        .and_then(|store| {
            library::create_session(&store, started_at, dir_name, title)
                .map_err(|error| error.to_string())
        })
}

fn abandon_unstarted_recording(
    model: &gpui::Entity<AppModel>,
    storage: &SharedStorage,
    session_id: SessionId,
    cx: &mut gpui::App,
) {
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
    let query = model.read(cx).library_query.clone();
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
    if !query.trim().is_empty() {
        search_library(storage, model, &query, cx);
    }
}

/// Run a transcript search and replace the library filter.
///
/// The query is stored verbatim so the field and the filter agree. Only the
/// storage call sees the trimmed form.
pub(crate) fn search_library(
    storage: &SharedStorage,
    model: &Entity<AppModel>,
    query: &str,
    cx: &mut App,
) {
    let trimmed = query.trim();
    if trimmed.is_empty() {
        model.update(cx, |m, cx| {
            m.set_library_search(query.to_string(), None);
            cx.notify();
        });
        return;
    }

    let mut hits = Vec::new();
    let mut failure = None;
    if let Ok(store) = storage.lock() {
        match store.segments().search_text(trimmed, 200) {
            Ok(segments) => {
                for segment in segments {
                    if !hits.contains(&segment.session_id) {
                        hits.push(segment.session_id);
                    }
                }
            },
            Err(source) => failure = Some(source.to_string()),
        }
    }
    model.update(cx, |m, cx| {
        m.set_library_search(query.to_string(), Some(hits));
        if let Some(failure) = failure {
            m.last_error = Some(AppError::Persistence(format!(
                "transcript search failed: {failure}"
            )));
        }
        cx.notify();
    });
}

/// Delete a saved session and its recording directory.
///
/// The database row goes first. A file-removal failure leaves an orphaned
/// directory, which costs disk but cannot leave a library row pointing at
/// missing audio.
pub(crate) fn delete_session(
    storage: &SharedStorage,
    recordings_dir: &std::path::Path,
    model: &Entity<AppModel>,
    session_id: SessionId,
    cx: &mut App,
) {
    let Ok(store) = storage.lock() else {
        model.update(cx, |m, cx| {
            m.cancel_delete();
            m.last_error = Some(AppError::Persistence(
                "storage is unavailable; the session was not deleted".into(),
            ));
            cx.notify();
        });
        return;
    };
    let session = match store.sessions().get(session_id) {
        Ok(Some(session)) => session,
        Ok(None) => {
            drop(store);
            refresh_library(storage, model, cx);
            model.update(cx, |m, cx| {
                m.cancel_delete();
                cx.notify();
            });
            return;
        },
        Err(source) => {
            drop(store);
            model.update(cx, |m, cx| {
                m.cancel_delete();
                m.last_error = Some(AppError::Persistence(format!(
                    "could not read the session before deleting it: {source}"
                )));
                cx.notify();
            });
            return;
        },
    };
    let storage_root = store.root().to_path_buf();
    let delete_result = store.sessions().delete(session_id);
    drop(store);

    match delete_result {
        Ok(()) => {
            let file_error =
                library::remove_session_recordings(&storage_root, recordings_dir, &session).err();
            refresh_library(storage, model, cx);
            model.update(cx, |m, cx| {
                if m.view == (View::History { session_id }) {
                    m.show_library();
                }
                m.cancel_delete();
                if let Some(error) = file_error {
                    m.last_error = Some(AppError::Persistence(format!(
                        "session deleted, but its recordings could not be removed: {error}"
                    )));
                }
                cx.notify();
            });
        },
        Err(source) => {
            model.update(cx, |m, cx| {
                m.cancel_delete();
                m.last_error = Some(AppError::Persistence(format!(
                    "could not delete the session: {source}"
                )));
                cx.notify();
            });
        },
    }
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
