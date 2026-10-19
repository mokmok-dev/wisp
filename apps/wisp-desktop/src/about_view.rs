//! Small "About Wisp" window opened from the application menu.
//!
//! Rendered with GPUI Component primitives so the window shares the app's
//! theme tokens instead of hard-coded colours.

use gpui::{
    App, AppContext as _, Bounds, Context, InteractiveElement as _, IntoElement,
    ParentElement as _, Render, Styled as _, TitlebarOptions, Window, WindowBounds, WindowOptions,
    actions, div, px, size,
};
use gpui_kit::component::{
    ActiveTheme as _,
    button::{Button, ButtonVariants as _},
    h_flex, v_flex,
};

actions!(wisp_desktop, [CloseAbout]);

pub struct AboutView;

impl Render for AboutView {
    fn render(
        &mut self,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let app_version = env!("CARGO_PKG_VERSION");
        let audiokit_version = wisp_audiokit::version();
        let muted = cx.theme().muted_foreground;

        v_flex()
            .size_full()
            .p_6()
            .gap_3()
            .bg(cx.theme().background)
            .text_color(cx.theme().foreground)
            .child(
                div()
                    .text_xl()
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    .child("Wisp"),
            )
            .child(
                div()
                    .text_sm()
                    .text_color(muted)
                    .child(format!("Version {app_version}")),
            )
            .child(
                div()
                    .text_sm()
                    .text_color(muted)
                    .child(format!("WispAudioKit {audiokit_version}")),
            )
            .child(
                div()
                    .text_sm()
                    .text_color(muted)
                    .child("On-device transcription on macOS; local Windows recording preview."),
            )
            .child(div().flex_grow(1.0))
            .child(
                h_flex().w_full().justify_end().child(
                    Button::new("about-ok")
                        .primary()
                        .label("OK")
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
    gpui_kit::open_window(
        WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(bounds)),
            titlebar: Some(TitlebarOptions {
                title: Some("About Wisp".into()),
                ..Default::default()
            }),
            ..Default::default()
        },
        cx,
        |_, cx| cx.new(|_| AboutView),
    )
    .expect("failed to open About window");
}
