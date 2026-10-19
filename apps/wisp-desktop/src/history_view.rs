//! The history screen: a saved session's transcript with inline rename,
//! clipboard copy, and Markdown export.

use gpui::{AnyElement, Context, IntoElement, ParentElement as _, Styled as _, div, px};
use gpui_kit::assets::IconName as AppIcon;
use gpui_kit::component::{
    ActiveTheme as _, Sizable as _,
    button::{Button, ButtonVariants as _},
    h_flex,
    input::Input,
    tag::Tag,
    v_flex,
};

use crate::app_view::{self, AppView};
use crate::format;

pub(crate) fn render(
    view: &AppView,
    cx: &mut Context<AppView>,
) -> AnyElement {
    let app = view.model.read(cx);
    let (started, duration) = match &app.viewed_session {
        Some(session) => (
            format::started_at(session.started_at),
            format::duration(
                session
                    .ended_at
                    .map(|ended| (ended - session.started_at).num_seconds()),
            ),
        ),
        None => (String::new(), format::duration(None)),
    };

    let transcript = app_view::render_transcript(view, false, cx);
    let title_input = Input::new(&view.history_title_input);

    v_flex()
        .flex_1()
        .min_w_0()
        .h_full()
        .overflow_hidden()
        .child(
            h_flex()
                .w_full()
                .gap_3()
                .items_center()
                .px_6()
                .py_4()
                .border_b_1()
                .border_color(cx.theme().border)
                .child(
                    Button::new("history-back")
                        .secondary()
                        .icon(AppIcon::ArrowLeft)
                        .tooltip("Back to library")
                        .on_click(cx.listener(|this, _, _, cx| this.show_library(cx))),
                )
                .child(div().flex_1().min_w_0().max_w(px(420.0)).child(title_input))
                .child(
                    h_flex()
                        .gap_2()
                        .items_center()
                        .child(
                            div()
                                .text_sm()
                                .text_color(cx.theme().muted_foreground)
                                .child(started),
                        )
                        .child(
                            Tag::secondary()
                                .small()
                                .rounded_full()
                                .child(div().text_xs().child(duration)),
                        )
                        .child(
                            Button::new("history-copy")
                                .secondary()
                                .icon(AppIcon::Copy)
                                .label("Copy")
                                .tooltip("Copy transcript (⌘⇧C)")
                                .on_click(cx.listener(|this, _, window, cx| {
                                    this.copy_transcript(window, cx);
                                })),
                        )
                        .child(
                            Button::new("history-export")
                                .primary()
                                .icon(AppIcon::Download)
                                .label("Export")
                                .tooltip("Export as Markdown (⌘⇧E)")
                                .on_click(cx.listener(|this, _, window, cx| {
                                    this.export_transcript(window, cx);
                                })),
                        ),
                ),
        )
        .child(app_view::screen_body().child(transcript))
        .child(
            app_view::screen_footer(cx)
                .child("stored locally as Ogg/Opus + SQLite")
                .child("⌘⇧C copy · ⌘⇧E export"),
        )
        .into_any_element()
}
