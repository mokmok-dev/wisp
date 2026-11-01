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

pub(crate) fn render(
    view: &AppView,
    cx: &mut Context<AppView>,
) -> AnyElement {
    let app = view.model.read(cx);
    let phase = app.state;
    let active = matches!(phase, SessionState::Recording { .. });
    let pending = app.has_pending_persistence();
    let muted = app.microphone_muted;
    let elapsed = app_view::elapsed_clock(phase);
    let title_editable = matches!(phase, SessionState::Idle | SessionState::Recording { .. });

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
                        .child(phase_tag(phase, pending))
                        .child(
                            div()
                                .text_lg()
                                .font_family(cx.theme().mono_font_family.clone())
                                .child(elapsed),
                        )
                        .child(record_button(phase, pending, cx))
                        .when(active, |this| this.child(mute_button(muted, cx))),
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

fn phase_label(
    phase: SessionState,
    pending: bool,
) -> String {
    if pending {
        return "Unsaved".to_string();
    }
    match phase {
        SessionState::Idle => "Ready",
        SessionState::Starting => "Starting…",
        SessionState::Recording { .. } => "Recording",
        SessionState::Stopping => "Finishing…",
        SessionState::Failed => "Failed",
    }
    .to_string()
}

fn phase_tag(
    phase: SessionState,
    pending: bool,
) -> Tag {
    let label = phase_label(phase, pending);
    let tag = if pending || matches!(phase, SessionState::Recording { .. } | SessionState::Failed) {
        Tag::danger()
    } else if matches!(phase, SessionState::Starting | SessionState::Stopping) {
        Tag::info()
    } else {
        Tag::secondary()
    };
    tag.small()
        .rounded_full()
        .child(div().text_xs().child(label))
}

fn record_control(phase: SessionState) -> (&'static str, AppIcon) {
    match phase {
        SessionState::Failed => ("Retry", AppIcon::RefreshCw),
        SessionState::Recording { .. } => ("Stop", AppIcon::Square),
        SessionState::Starting | SessionState::Stopping => ("…", AppIcon::Mic),
        SessionState::Idle => ("Record", AppIcon::Mic),
    }
}

fn record_button(
    phase: SessionState,
    pending: bool,
    cx: &mut Context<AppView>,
) -> Button {
    let (label, icon) = record_control(phase);
    let active = matches!(phase, SessionState::Recording { .. });
    let busy = matches!(phase, SessionState::Starting | SessionState::Stopping);
    let can_start = matches!(phase, SessionState::Idle) && !pending;
    let failed = matches!(phase, SessionState::Failed);
    let button = if active {
        Button::new("live-record").secondary()
    } else {
        Button::new("live-record")
            .primary()
            .tooltip(if can_start || failed {
                "Start recording (⌘R)"
            } else {
                "Recording controls (⌘R)"
            })
    };
    button
        .icon(icon)
        .label(label)
        .loading(busy)
        .on_click(cx.listener(|this, _, _, cx| this.toggle_record(cx)))
}

fn mute_button(
    muted: bool,
    cx: &mut Context<AppView>,
) -> Button {
    let button = if muted {
        Button::new("live-mute")
            .primary()
            .label("Unmute")
            .tooltip("Unmute microphone")
    } else {
        Button::new("live-mute")
            .secondary()
            .label("Mute")
            .tooltip("Mute microphone")
    };
    button
        .icon(AppIcon::Mic)
        .on_click(cx.listener(|this, _, _, cx| this.toggle_mute(cx)))
}
