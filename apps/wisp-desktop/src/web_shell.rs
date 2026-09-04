//! The web shell: hosts the Kumo/React UI inside a wry webview pointing at
//! the loopback server (`web_server.rs`).
//!
//! The webview fills the whole window content area (see `crates/wisp-webview`
//! for the z-order rationale). Assets and the command/event bridge are served
//! by the loopback HTTP server; `WISP_UI_DEV_URL` overrides the URL for Vite
//! hot reload (the dev origin receives the loopback root via a `wisp` query
//! parameter).

#![allow(clippy::expect_used)]
// Window-setup failures panic loudly by design (same policy as `main.rs`):
// a silently missing webview would be far harder to diagnose than a crash.

use std::cell::RefCell;
use std::rc::Rc;

use gpui::{App, AppContext, Entity, IntoElement, ParentElement, Render, Styled, Window, div};
use serde::Deserialize;
use wisp_audiokit::Permission;
use wisp_core::SessionId;
use wisp_webview::wry::WebViewBuilder;
use wisp_webview::{WebView, WebViewHandle};

use crate::app::{AppModel, SessionState, View};
use crate::library::SharedStorage;
use crate::session_runner::SessionRunner;
use crate::transcript_export::{self, suggested_export_name};
use crate::web_bridge::{EventBus, UiBridge};

/// Commands the web UI can send to the host.
// `rename_all` only renames the *variants*; `rename_all_fields` (serde
// 1.0.186+) is required so `session_id` matches the camelCase the UI sends.
#[derive(Debug, Deserialize)]
#[serde(tag = "cmd", rename_all = "camelCase", rename_all_fields = "camelCase")]
pub enum UiCommand {
    Ready,
    ToggleRecord,
    ToggleMute,
    NewSession,
    OpenHistory {
        session_id: i64,
    },
    BackToLibrary,
    SetLiveTitle {
        title: String,
    },
    RenameSession {
        session_id: i64,
        title: String,
    },
    RequestPermission {
        permission: String,
    },
    OpenSettings {
        permission: String,
    },
    CopyTranscript,
    ExportTranscript,
    /// Uncaught JS errors forwarded from the UI, surfaced on stderr.
    #[serde(rename = "__debugJsError")]
    DebugJsError {
        message: String,
    },
}

fn permission_from_name(name: &str) -> Option<Permission> {
    match name {
        "microphone" => Some(Permission::Microphone),
        "speech" => Some(Permission::SpeechRecognition),
        _ => None,
    }
}

/// Everything a [`UiCommand`] handler needs from the host.
pub struct CommandContext<'a> {
    pub runner: &'a SessionRunner,
    pub model: &'a Entity<AppModel>,
    pub storage: &'a SharedStorage,
    pub recordings_dir: &'a std::path::Path,
    pub bridge: &'a Entity<UiBridge>,
}

/// Dispatch one parsed command from the web UI.
pub fn handle_command(
    command: UiCommand,
    context: &CommandContext<'_>,
    cx: &mut App,
) {
    let CommandContext {
        runner,
        model,
        storage,
        recordings_dir,
        bridge,
    } = context;

    match command {
        UiCommand::Ready => {
            bridge.update(cx, |b, cx| b.push_full_snapshot(model, cx));
        },
        UiCommand::ToggleRecord => {
            crate::toggle_recording(runner, model, storage, recordings_dir, cx);
        },
        UiCommand::ToggleMute => {
            let muted = !model.read(cx).microphone_muted;
            if runner.set_microphone_muted(muted) {
                model.update(cx, |m, cx| {
                    if matches!(m.state, SessionState::Recording { .. }) {
                        m.microphone_muted = muted;
                        cx.notify();
                    }
                });
            }
        },
        UiCommand::NewSession => {
            model.update(cx, |m, cx| {
                m.show_new_session();
                cx.notify();
            });
        },
        UiCommand::OpenHistory { session_id } => {
            crate::open_history(storage, model, SessionId::from(session_id), cx);
        },
        UiCommand::BackToLibrary => {
            model.update(cx, |m, cx| {
                m.show_library();
                cx.notify();
            });
        },
        UiCommand::SetLiveTitle { title } => {
            crate::change_live_title(model, storage, cx, &title);
        },
        UiCommand::RenameSession { session_id, title } => {
            crate::rename_session(model, storage, SessionId::from(session_id), &title, cx);
        },
        UiCommand::RequestPermission { permission } => {
            if let Some(perm) = permission_from_name(&permission) {
                crate::permissions::request(perm, (**model).clone(), cx);
            }
        },
        UiCommand::OpenSettings { permission } => {
            if let Some(perm) = permission_from_name(&permission) {
                crate::permissions::open_settings(perm);
            }
        },
        UiCommand::CopyTranscript => {
            let app = model.read(cx);
            if !matches!(app.view, View::LiveSession | View::History { .. }) {
                return;
            }
            let copied = transcript_export::copy_transcript_to_clipboard(&app.segments, cx);
            bridge.update(cx, |b, _| {
                if copied {
                    b.notify("success", "Transcript copied to the clipboard.");
                } else {
                    b.notify("error", "There is no transcript to copy yet.");
                }
            });
        },
        UiCommand::ExportTranscript => {
            let app = model.read(cx);
            if !matches!(app.view, View::LiveSession | View::History { .. }) {
                return;
            }
            let session = app.viewed_session.as_ref();
            let text = transcript_export::format_transcript_markdown(session, &app.segments);
            let name = suggested_export_name(session.map(|s| s.title.as_str()), "transcript");
            transcript_export::export_transcript(text, &name, cx);
            bridge.update(cx, |b, _| {
                b.notify(
                    "success",
                    "Export started — choose a location in the save dialog.",
                );
            });
        },
        UiCommand::DebugJsError { message } => {
            eprintln!("wisp-webview: js error: {message}");
        },
    }
}

/// The window's root view: nothing but the webview.
pub struct WebShellView {
    webview: Entity<WebView>,
}

impl Render for WebShellView {
    fn render(
        &mut self,
        _window: &mut Window,
        _cx: &mut gpui::Context<Self>,
    ) -> impl IntoElement {
        div().size_full().child(self.webview.clone())
    }
}

fn allow_navigation(
    url: &str,
    allowed_prefix: &str,
    dev_url: Option<&str>,
) -> bool {
    url.starts_with(allowed_prefix) || dev_url.is_some_and(|base| url.starts_with(base))
}

fn build_webview(
    window: &mut Window,
    cx: &mut App,
    url: String,
    allowed_prefix: String,
    dev_url: Option<String>,
) -> Entity<WebView> {
    let mut builder = WebViewBuilder::new()
        .with_navigation_handler(move |candidate| {
            allow_navigation(&candidate, &allowed_prefix, dev_url.as_deref())
        })
        .with_accept_first_mouse(true)
        .with_url(url);
    if cfg!(debug_assertions) {
        builder = builder.with_devtools(true);
    }

    cx.new(|cx| {
        let webview = builder
            .build_as_child(window)
            .expect("failed to create webview");
        WebView::new(webview, window, cx)
    })
}

/// Open the main window hosting the web shell.
///
/// `server_url` is the loopback root (including its token); `dev_url` is the
/// optional `WISP_UI_DEV_URL`. Returns the window handle plus the
/// [`UiBridge`] entity wired to the event bus; the caller attaches observers
/// and the command pump.
pub fn open(
    cx: &mut App,
    window_options: gpui::WindowOptions,
    server_url: &str,
    dev_url: Option<String>,
    bus: EventBus,
) -> (gpui::WindowHandle<WebShellView>, Entity<UiBridge>) {
    let handle_slot = Rc::new(RefCell::new(None::<WebViewHandle>));
    let slot_for_view = handle_slot.clone();

    // Allowed origins for in-page navigation: the loopback root and, in dev
    // mode, the Vite dev server.
    let rest = server_url.strip_prefix("http://").unwrap_or(server_url);
    let authority = rest.split('/').next().unwrap_or(rest);
    let nav_prefix = format!("http://{authority}/");

    let target_url = dev_url.clone().map_or_else(
        || server_url.to_owned(),
        |dev| format!("{dev}?wisp={}", urlencoding_lite(server_url)),
    );

    let window = cx
        .open_window(window_options, move |window, cx| {
            let webview = build_webview(window, cx, target_url, nav_prefix, dev_url);
            *slot_for_view.borrow_mut() = Some(webview.read(cx).handle());
            cx.new(|_| WebShellView { webview })
        })
        .expect("failed to open Wisp window");

    let _handle = handle_slot.borrow_mut().take().expect("wry webview handle");
    let bridge = cx.new(|_| UiBridge::new(bus));
    (window, bridge)
}

/// Percent-encode the characters that must not appear raw in a query value.
fn urlencoding_lite(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z'
            | b'a'..=b'z'
            | b'0'..=b'9'
            | b'-'
            | b'.'
            | b'_'
            | b'~'
            | b'/'
            | b':'
            | b'?'
            | b'='
            | b'&' => out.push(byte as char),
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}
