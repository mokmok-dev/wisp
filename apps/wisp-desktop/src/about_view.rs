//! Small "About Wisp" window opened from the application menu.

use gpui::{
    App, Bounds, Context, IntoElement, ParentElement, Render, Styled, TitlebarOptions, Window,
    WindowBounds, WindowOptions, actions, div, prelude::*, px, rgb, size,
};

actions!(wisp_desktop, [CloseAbout]);

pub struct AboutView;

impl Render for AboutView {
    fn render(
        &mut self,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let app_version = env!("CARGO_PKG_VERSION");
        let audiokit_version = wisp_audiokit::version();

        div()
            .flex()
            .flex_col()
            .size_full()
            .bg(rgb(0x0b_0e13))
            .text_color(rgb(0xe8_eaed))
            .p_6()
            .gap_3()
            .child(
                div()
                    .text_xl()
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    .child("Wisp"),
            )
            .child(
                div()
                    .text_sm()
                    .text_color(rgb(0x8a_8f98))
                    .child(format!("Version {app_version}")),
            )
            .child(
                div()
                    .text_sm()
                    .text_color(rgb(0x8a_8f98))
                    .child(format!("WispAudioKit {audiokit_version}")),
            )
            .child(
                div()
                    .text_sm()
                    .text_color(rgb(0x5c_606b))
                    .child("Fully offline meeting transcription for macOS and Windows preview."),
            )
            .child(div().flex_grow())
            .child(
                div().flex().justify_end().child(
                    div()
                        .id("about-ok")
                        .px_3()
                        .py_1p5()
                        .bg(rgb(0x13_171f))
                        .border_1()
                        .border_color(rgb(0x1f_242e))
                        .rounded_md()
                        .cursor_pointer()
                        .child("OK")
                        .on_click(|_, window, _| {
                            window.remove_window();
                        }),
                ),
            )
            .on_action(|_: &CloseAbout, window, _| {
                window.remove_window();
            })
    }
}

pub fn open(cx: &mut App) {
    let bounds = Bounds::centered(None, size(px(360.0), px(240.0)), cx);
    cx.open_window(
        WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(bounds)),
            titlebar: Some(TitlebarOptions {
                title: Some("About Wisp".into()),
                ..Default::default()
            }),
            ..Default::default()
        },
        |_, cx| cx.new(|_| AboutView),
    )
    .expect("failed to open About window");
}
