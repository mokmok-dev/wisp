//! Wisp desktop app — `GPUI` shell that drives audio capture, transcription
//! display, and session management.
//!
//! This is the skeleton: it just opens a window with a placeholder. The
//! audio + transcription wiring lands in subsequent PRs once the Swift FFI
//! session API is wired in.

// `Application::run`-style entry-point boilerplate uses `.expect` to fail
// loudly on the open_window setup — clearer than a panic-from-result hidden
// behind a `?` in `main`.
#![allow(clippy::expect_used)]

use gpui::{
    AppContext, Application, Bounds, Context, IntoElement, ParentElement, Render, Styled, Window,
    WindowBounds, WindowOptions, div, prelude::FluentBuilder, px, rgb, size,
};

/// Top-level Wisp window. Will grow to host the recording controls,
/// transcript view, and session sidebar.
struct WispWindow;

impl Render for WispWindow {
    fn render(
        &mut self,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> impl IntoElement {
        div()
            .flex()
            .flex_col()
            .size_full()
            .bg(rgb(0x0011_1418))
            .text_color(rgb(0x00e6_e8eb))
            .justify_center()
            .items_center()
            .gap_2()
            .child(div().text_2xl().child("Wisp"))
            .child(
                div()
                    .text_sm()
                    .text_color(rgb(0x008a_8f98))
                    .child(format!("WispAudioKit {}", wisp_audiokit::version())),
            )
            // Keep `FluentBuilder` referenced; we'll use `.when(...)` for
            // conditional UI in the next PR.
            .when(false, |this| this)
    }
}

fn main() {
    Application::new().run(|cx| {
        cx.activate(true);
        let bounds = Bounds::centered(None, size(px(720.0), px(480.0)), cx);
        cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                ..Default::default()
            },
            |_, cx| cx.new(|_| WispWindow),
        )
        .expect("failed to open Wisp window");
    });
}
