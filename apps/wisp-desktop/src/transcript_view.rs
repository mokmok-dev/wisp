//! Rendering of the mic/system transcript shared by the live and history
//! views.
//!
//! Partial (`is_final == false`) segments keep the ghost-text styling: the
//! speech engine revises them in place until it locks the utterance in.

use gpui::{
    AnyElement, App, InteractiveElement as _, IntoElement, ParentElement as _,
    StatefulInteractiveElement as _, Styled as _, div, prelude::FluentBuilder as _, px, relative,
};
use gpui_kit::ScrollHandle;
use gpui_kit::component::{ActiveTheme as _, Sizable as _, h_flex, tag::Tag, v_flex};
use wisp_audiokit::SourceLabel;

use crate::app::{AppModel, Segment};

/// Render the scrollable transcript for `app`.
///
/// `scroll` must be the same handle the caller attaches with `track_scroll`.
/// In the live view (`follow == true`) the list keeps the newest segment in
/// view unless the user has scrolled away from the bottom.
pub(crate) fn render(
    app: &AppModel,
    scroll: &ScrollHandle,
    follow: bool,
    cx: &App,
) -> AnyElement {
    if app.segments.is_empty() {
        return v_flex()
            .size_full()
            .items_center()
            .justify_center()
            .child(
                div()
                    .text_sm()
                    .text_color(cx.theme().muted_foreground)
                    .child("Transcript will appear here as it is recognized."),
            )
            .into_any_element();
    }

    // Jump to the newest segment when this frame lays the list out. Doing it
    // during render keeps the Web-UI behaviour without a second render pass.
    if follow && is_near_bottom(scroll) {
        scroll.scroll_to_bottom();
    }

    let mut list = v_flex().w_full().gap_2().pb_4();
    for (index, segment) in app.segments.iter().enumerate() {
        let is_latest = index + 1 == app.segments.len();
        list = list.child(segment_card(segment, is_latest, cx));
    }

    div()
        .id("transcript-scroll")
        .size_full()
        .overflow_y_scroll()
        .track_scroll(scroll)
        .child(list)
        .into_any_element()
}

fn is_near_bottom(scroll: &ScrollHandle) -> bool {
    let offset = scroll.offset().y;
    let max = scroll.max_offset().y;
    -offset >= max - px(48.0)
}

fn segment_card(
    segment: &Segment,
    is_latest: bool,
    cx: &App,
) -> impl IntoElement {
    let (label, tag) = match segment.source {
        SourceLabel::Mic => ("MIC", Tag::info()),
        SourceLabel::System => ("SYS", Tag::warning()),
    };

    let text = if !segment.is_final && is_latest {
        format!("{}▍", segment.display_text)
    } else {
        segment.display_text.clone()
    };

    v_flex()
        .w_full()
        .gap_1()
        .px_4()
        .py_3()
        .rounded_md()
        .border_1()
        .border_color(cx.theme().border)
        .bg(cx.theme().background)
        .child(
            h_flex()
                .gap_2()
                .items_center()
                .child(
                    tag.small()
                        .rounded_full()
                        .child(div().text_xs().child(label)),
                )
                .when(!segment.is_final, |this| {
                    this.child(
                        div()
                            .text_xs()
                            .text_color(cx.theme().muted_foreground)
                            .child("transcribing…"),
                    )
                }),
        )
        .child(
            div()
                .text_size(px(15.0))
                .line_height(relative(1.5))
                .text_color(cx.theme().foreground)
                .child(text),
        )
}

#[cfg(test)]
mod tests {
    use super::is_near_bottom;
    use gpui_kit::ScrollHandle;

    #[test]
    fn empty_handle_counts_as_near_bottom() {
        assert!(is_near_bottom(&ScrollHandle::new()));
    }
}
