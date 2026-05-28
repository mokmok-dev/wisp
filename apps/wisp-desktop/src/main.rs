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

use gpui::{
    AppContext, Application, AsyncApp, Bounds, Timer, TitlebarOptions, WindowBounds, WindowOptions,
    px, size,
};

mod app;
mod session_runner;
mod transcript_view;

use app::{AppModel, SessionState};
use session_runner::{SessionRunner, Update};
use transcript_view::{TranscriptView, cursor_blink_period, now, ui_tick_period};

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

        let window = {
            let runner = runner.clone();
            let model_ref = model.clone();
            let output_dir = output_dir.clone();
            cx.open_window(window_options, move |_, cx| {
                cx.new(|cx| {
                    let runner = runner.clone();
                    let model_ref = model_ref.clone();
                    let output_dir = output_dir.clone();
                    let view = TranscriptView {
                        app: model_ref.clone(),
                        cursor_visible: true,
                        scroll_handle: gpui::ScrollHandle::new(),
                        last_signature: (0, 0),
                        on_toggle_record: Arc::new(move |_window, cx| {
                            toggle_recording(&runner, &model_ref, &output_dir, cx);
                        }),
                    };
                    // Re-render whenever the underlying model changes.
                    cx.observe(&view.app, |_, _, cx| cx.notify()).detach();
                    view
                })
            })
            .expect("failed to open Wisp window")
        };

        // 1) Pump session-runner updates into the model.
        {
            let runner = runner.clone();
            let model = model.clone();
            cx.spawn(async move |cx: &mut AsyncApp| {
                loop {
                    Timer::after(std::time::Duration::from_millis(33)).await;
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
                                    // Lock in whatever the analyzer last
                                    // had — without this the trailing
                                    // partial stays grey forever.
                                    model.finalize_all_segments();
                                    model.set_state(SessionState::Idle);
                                },
                                Update::Error(msg) => model.fail(msg),
                            }
                        }
                        cx.notify();
                    });
                    if result.is_err() {
                        // Window / app gone; stop pumping.
                        break;
                    }
                }
            })
            .detach();
        }

        // 2) Cursor blink + status-bar tick.
        cx.spawn(async move |cx: &mut AsyncApp| {
            let mut elapsed = std::time::Duration::ZERO;
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
    });
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
