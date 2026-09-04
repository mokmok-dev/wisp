//! The web shell: hosts the Kumo/React UI inside a wry webview.
//!
//! The webview fills the whole window content area (see `crates/wisp-webview`
//! for the z-order rationale). Assets are served from the embedded
//! `ui/dist` bundle over the custom `wisp://` scheme so the app works fully
//! offline; `WISP_UI_DEV_URL` overrides the URL for Vite hot reload.
//!
//! JS → Rust commands arrive via wry IPC. The wry callbacks carry no GPUI
//! context, so commands flow through a channel drained by the main pump in
//! `main.rs`.

#![allow(clippy::expect_used)]
// Window-setup failures panic loudly by design (same policy as `main.rs`):
// a silently missing webview would be far harder to diagnose than a crash.

use std::borrow::Cow;
use std::cell::RefCell;
use std::path::Path;
use std::rc::Rc;
use std::sync::mpsc::Sender;

use gpui::{App, AppContext, Entity, IntoElement, ParentElement, Render, Styled, Window, div};
use serde::Deserialize;
use wisp_audiokit::Permission;
use wisp_core::SessionId;
use wisp_webview::wry::WebViewBuilder;
use wisp_webview::wry::http::{self, Request, Response};
use wisp_webview::{WebView, WebViewHandle};

use crate::app::{AppModel, SessionState, View};
use crate::library::SharedStorage;
use crate::session_runner::SessionRunner;
use crate::transcript_export::{self, suggested_export_name};
use crate::web_bridge::UiBridge;

/// The embedded UI bundle, built by `apps/wisp-desktop/ui` (`npm run build`).
/// A fresh checkout without a UI build falls back to a placeholder page.
static UI_ASSETS: include_dir::Dir<'_> = include_dir::include_dir!("$CARGO_MANIFEST_DIR/ui/dist");

const FALLBACK_PAGE: &str = r#"<!doctype html>
<html lang="en"><head><meta charset="utf-8"><title>Wisp</title></head>
<body style="font-family: system-ui; background:#0b0e13; color:#e8eaed; display:flex; height:100vh; align-items:center; justify-content:center;">
  <div style="text-align:center;">
    <h1>Wisp</h1>
    <p>The web UI bundle is missing. Build it with:</p>
    <pre><code>cd apps/wisp-desktop/ui &amp;&amp; npm install &amp;&amp; npm run build</code></pre>
    <p>then restart the app.</p>
  </div>
</body></html>
"#;

/// Commands the web UI can send to the host.
#[derive(Debug, Deserialize)]
#[serde(tag = "cmd", rename_all = "camelCase")]
pub enum UiCommand {
    Ready,
    ToggleRecord,
    ToggleMute,
    NewSession,
    OpenHistory { session_id: i64 },
    BackToLibrary,
    SetLiveTitle { title: String },
    RenameSession { session_id: i64, title: String },
    RequestPermission { permission: String },
    OpenSettings { permission: String },
    CopyTranscript,
    ExportTranscript,
}

/// Parse one JSON-encoded [`UiCommand`] from the IPC channel.
#[must_use]
pub fn parse_command(body: &str) -> Option<UiCommand> {
    serde_json::from_str(body).ok()
}

fn mime_for(path: &str) -> &'static str {
    match path.rsplit('.').next() {
        Some("html") => "text/html; charset=utf-8",
        Some("js" | "mjs") => "text/javascript; charset=utf-8",
        Some("css") => "text/css; charset=utf-8",
        Some("json" | "map") => "application/json",
        Some("svg") => "image/svg+xml",
        Some("png") => "image/png",
        Some("ico") => "image/x-icon",
        Some("woff") => "font/woff",
        Some("woff2") => "font/woff2",
        _ => "application/octet-stream",
    }
}

fn response(
    status: u16,
    mime: &'static str,
    bytes: &'static [u8],
) -> Response<Cow<'static, [u8]>> {
    http::Response::builder()
        .status(status)
        .header(http::header::CONTENT_TYPE, mime)
        .body(Cow::Borrowed(bytes))
        .unwrap_or_else(|_| http::Response::new(Cow::Borrowed(&[])))
}

/// Serve the embedded UI bundle over `wisp://`.
fn serve_ui(request: &Request<Vec<u8>>) -> Response<Cow<'static, [u8]>> {
    let raw = request.uri().path().trim_start_matches('/');
    let path = if raw.is_empty() { "index.html" } else { raw };

    if let Some(file) = UI_ASSETS.get_file(path) {
        return response(200, mime_for(path), file.contents());
    }

    // SPA-style fallback: route-like paths get the app shell; anything that
    // looks like a missing asset 404s so build errors surface clearly.
    if !path.contains('.') {
        return match UI_ASSETS.get_file("index.html") {
            Some(file) => response(200, mime_for("index.html"), file.contents()),
            None => response(200, mime_for("index.html"), FALLBACK_PAGE.as_bytes()),
        };
    }
    response(404, "text/plain; charset=utf-8", b"not found\n")
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
    pub recordings_dir: &'a Path,
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
    dev_url: Option<&str>,
) -> bool {
    url.starts_with("wisp://") || dev_url.is_some_and(|base| url.starts_with(base))
}

fn build_webview(
    window: &mut Window,
    cx: &mut App,
    ipc_tx: Sender<UiCommand>,
    dev_url: Option<&str>,
) -> Entity<WebView> {
    let navigation_url = dev_url.map_or_else(|| "wisp://app/index.html".to_owned(), str::to_owned);

    let mut builder = WebViewBuilder::new()
        .with_custom_protocol("wisp".into(), |_, request| serve_ui(&request))
        .with_ipc_handler(move |request| {
            if let Some(command) = parse_command(request.body()) {
                let _ = ipc_tx.send(command);
            }
        })
        .with_navigation_handler({
            let dev_url = dev_url.map(str::to_owned);
            move |url| allow_navigation(&url, dev_url.as_deref())
        })
        .with_accept_first_mouse(true)
        .with_url(navigation_url);
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
/// Returns the window handle plus the [`UiBridge`] entity wired to the
/// webview; the caller attaches observers and the command pump.
pub fn open(
    cx: &mut App,
    window_options: gpui::WindowOptions,
    ipc_tx: Sender<UiCommand>,
    dev_url: Option<String>,
) -> (gpui::WindowHandle<WebShellView>, Entity<UiBridge>) {
    let handle_slot = Rc::new(RefCell::new(None::<WebViewHandle>));
    let slot_for_view = handle_slot.clone();

    let window = cx
        .open_window(window_options, move |window, cx| {
            let webview = build_webview(window, cx, ipc_tx, dev_url.as_deref());
            *slot_for_view.borrow_mut() = Some(webview.read(cx).handle());
            cx.new(|_| WebShellView { webview })
        })
        .expect("failed to open Wisp window");

    let handle = handle_slot.borrow_mut().take().expect("wry webview handle");
    let bridge = cx.new(|_| UiBridge::new(handle));
    (window, bridge)
}
