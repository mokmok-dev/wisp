//! Wisp desktop app — `GPUI` shell.
//!
//! Wires together the three building blocks defined in the sibling modules:
//!
//!   * `app::AppModel` — transcript + lifecycle state. UI reads, the
//!     session-runner bridge writes.
//!   * `session_runner::SessionRunner` — background OS thread that owns the
//!     Swift `wisp_audiokit::Session` and surfaces events via a channel.
//!   * `transcript_view::TranscriptView` — the GPUI render target.
//!
//! Two `cx.spawn` async tasks plumb everything together:
//!
//!   1. Drain `SessionRunner` updates into `AppModel` every ~33ms.
//!   2. Toggle the ghost-text cursor on the view every 500ms and refresh
//!      the status bar's elapsed counter at 250ms so it stays smooth.

// We deliberately panic loudly on window-setup failures (clearer than a
// silently-dropped Result hidden behind `?` in `main`).
#![allow(clippy::expect_used)]

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use gpui::{
    App, AppContext, Application, AsyncApp, Bounds, Entity, Timer, TitlebarOptions, WindowBounds,
    WindowHandle, WindowOptions, px, size,
};

mod app;
mod permissions;
mod session_runner;
mod transcript_view;

use app::{AppModel, SessionState};
use session_runner::{SessionRunner, Update};
use transcript_view::{TranscriptView, cursor_blink_period, now, ui_tick_period};

/// How often we re-poll permission status from the OS while the
/// onboarding screen is up. The user might flip the toggle in System
/// Settings; without periodic re-checks we'd stay stuck on "Denied" until
/// they manually re-focus our window. 1.5s is unhurried but still feels
/// responsive when they come back.
const PERMISSION_REFRESH_INTERVAL: Duration = Duration::from_millis(1500);

fn main() {
    Application::new().run(|cx| {
        cx.activate(true);

        let output_dir = default_output_directory();
        let runner = Arc::new(SessionRunner::spawn());
        let model = cx.new(|_| AppModel::new());

        let bounds = Bounds::centered(None, size(px(900.0), px(640.0)), cx);
        let window_options = WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(bounds)),
            titlebar: Some(TitlebarOptions::default()),
            ..Default::default()
        };

        // Populate the model with the initial permission state so the
        // window opens straight onto onboarding or the record screen,
        // without a flash of the wrong content.
        permissions::refresh(&model, cx);

        let window = open_main_window(
            cx,
            window_options,
            runner.clone(),
            model.clone(),
            output_dir,
        );

        spawn_session_update_pump(cx, runner, model.clone());
        spawn_cursor_blink(cx, window);
        spawn_permission_refresh(cx, model);
    });
}

fn open_main_window(
    cx: &mut App,
    window_options: WindowOptions,
    runner: Arc<SessionRunner>,
    model: Entity<AppModel>,
    output_dir: PathBuf,
) -> WindowHandle<TranscriptView> {
    cx.open_window(window_options, move |_, cx| {
        cx.new(|cx| {
            let model_for_toggle = model.clone();
            let model_for_request = model.clone();
            let view = TranscriptView {
                app: model.clone(),
                cursor_visible: true,
                scroll_handle: gpui::ScrollHandle::new(),
                last_signature: (0, 0),
                on_toggle_record: Arc::new(move |_window, cx| {
                    toggle_recording(&runner, &model_for_toggle, &output_dir, cx);
                }),
                on_request_permission: Arc::new(move |perm, _window, cx| {
                    permissions::request(perm, model_for_request.clone(), cx);
                }),
                on_open_settings: Arc::new(move |perm, _window, _cx| {
                    permissions::open_settings(perm);
                    // The next periodic permission refresh picks up the
                    // toggle once the user flips it in System Settings.
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
fn spawn_session_update_pump(
    cx: &mut App,
    runner: Arc<SessionRunner>,
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
                    match u {
                        Update::Started => {
                            model.set_state(SessionState::Recording { started_at: now() });
                        },
                        Update::Event(e) => model.ingest(e),
                        Update::Stopped => {
                            // Lock in whatever the analyzer last had — without
                            // this the trailing partial stays grey forever.
                            model.finalize_all_segments();
                            model.set_state(SessionState::Idle);
                        },
                        Update::Error(msg) => model.fail(msg),
                    }
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

fn toggle_recording(
    runner: &SessionRunner,
    model: &gpui::Entity<AppModel>,
    output_dir: &std::path::Path,
    cx: &mut gpui::App,
) {
    let state = model.read(cx).state;
    match state {
        SessionState::Idle | SessionState::Failed => {
            model.update(cx, |m, cx| {
                m.set_state(SessionState::Starting);
                cx.notify();
            });
            runner.start(output_dir.to_path_buf(), "ja-JP".to_string());
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

fn default_output_directory() -> PathBuf {
    if let Ok(dir) = std::env::var("WISP_OUTPUT_DIR") {
        return PathBuf::from(dir);
    }
    // ~/Library/Application Support/dev.mokmok.wisp/recordings on macOS, or
    // a temp dir if we can't resolve $HOME.
    let mut p = std::env::var_os("HOME").map_or_else(std::env::temp_dir, PathBuf::from);
    p.push("Library");
    p.push("Application Support");
    p.push("dev.mokmok.wisp");
    p.push("recordings");
    p
}
