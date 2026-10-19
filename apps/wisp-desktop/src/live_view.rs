//! The live recording screen: title, recorder controls, and the streaming
//! mic/system transcript.

use gpui::{
    AnyElement, Context, IntoElement, ParentElement as _, Styled as _, div,
    prelude::FluentBuilder as _, px,
};
use gpui_kit::assets::IconName as AppIcon;
use gpui_kit::component::{
    ActiveTheme as _, Sizable as _,
    button::{Button, ButtonVariants as _},
    h_flex,
    input::Input,
    tag::Tag,
    v_flex,
};

use crate::app::SessionState;
use crate::app_view::{self, AppView};

#[allow(clippy::too_many_lines)] // one declarative screen tree
pub(crate) fn render(
    view: &AppView,
    cx: &mut Context<AppView>,
) -> AnyElement {
    let app = view.model.read(cx);
    let phase = app.state;
    let active = matches!(phase, SessionState::Recording { .. });
    let busy = matches!(phase, SessionState::Starting | SessionState::Stopping);
    let pending = app.has_pending_persistence();
    let failed = matches!(phase, SessionState::Failed);
    let can_start = matches!(phase, SessionState::Idle) && !pending;
    let muted = app.microphone_muted;
    let elapsed = app_view::elapsed_clock(phase);
    let title_editable = matches!(phase, SessionState::Idle | SessionState::Recording { .. });

    let phase_label = if pending {
        "Unsaved".to_string()
    } else {
        match phase {
            SessionState::Idle => "Ready",
            SessionState::Starting => "Starting…",
            SessionState::Recording { .. } => "Recording",
            SessionState::Stopping => "Finishing…",
            SessionState::Failed => "Failed",
        }
        .to_string()
    };

    let phase_tag = if pending || failed || active {
        Tag::danger()
            .small()
            .rounded_full()
            .child(div().text_xs().child(phase_label))
    } else if busy {
        Tag::info()
            .small()
            .rounded_full()
            .child(div().text_xs().child(phase_label))
    } else {
        Tag::secondary()
            .small()
            .rounded_full()
            .child(div().text_xs().child(phase_label))
    };

    let record_label = if failed {
        "Retry"
    } else if active {
        "Stop"
    } else if busy {
        "…"
    } else {
        "Record"
    };
    let record_icon = if failed {
        AppIcon::RefreshCw
    } else if active {
        AppIcon::Square
    } else {
        AppIcon::Mic
    };
    let record_button = if active {
        Button::new("live-record")
            .secondary()
            .icon(record_icon)
            .label(record_label)
            .loading(busy)
            .on_click(cx.listener(|this, _, _, cx| this.toggle_record(cx)))
    } else {
        Button::new("live-record")
            .primary()
            .icon(record_icon)
            .label(record_label)
            .loading(busy)
            .tooltip(if can_start || failed {
                "Start recording (⌘R)"
            } else {
                "Recording controls (⌘R)"
            })
            .on_click(cx.listener(|this, _, _, cx| this.toggle_record(cx)))
    };

    let mute_button = if muted {
        Button::new("live-mute")
            .primary()
            .icon(AppIcon::Mic)
            .label("Unmute")
            .tooltip("Unmute microphone")
            .on_click(cx.listener(|this, _, _, cx| this.toggle_mute(cx)))
    } else {
        Button::new("live-mute")
            .secondary()
            .icon(AppIcon::Mic)
            .label("Mute")
            .tooltip("Mute microphone")
            .on_click(cx.listener(|this, _, _, cx| this.toggle_mute(cx)))
    };

    let transcript = app_view::render_transcript(view, true, cx);
    let title_input = Input::new(&view.live_title_input).disabled(!title_editable);

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
                .child(div().flex_1().min_w_0().max_w(px(420.0)).child(title_input))
                .child(
                    h_flex()
                        .gap_3()
                        .items_center()
                        .when(active, |this| {
                            this.child(div().size(px(8.0)).rounded_full().bg(cx.theme().danger))
                        })
                        .child(phase_tag)
                        .child(
                            div()
                                .text_lg()
                                .font_family(cx.theme().mono_font_family.clone())
                                .child(elapsed),
                        )
                        .child(record_button)
                        .when(active, |this| this.child(mute_button)),
                ),
        )
        .child(app_view::screen_body().child(transcript))
        .child(
            app_view::screen_footer(cx)
                .child("Transcribed on-device · mic and system audio are kept as separate tracks")
                .child("⌘R to start / stop"),
        )
        .into_any_element()
}
