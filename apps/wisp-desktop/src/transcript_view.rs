//! The main wisp-desktop view. Three rows top to bottom:
//!
//!   - Top bar  (48px) — title left, record/stop button right
//!   - Transcript area (flex 1) — scrollable list of segments with the
//!     ghost-text styling for the active partial
//!   - Status bar (28px) — recording dot, elapsed time, segment count
//!
//! Color palette (see `theme` mod) is a deep-slate dark mode with warm
//! mic/system accents.

use std::time::Instant;

use gpui::{
    Context, ElementId, FontWeight, InteractiveElement, IntoElement, ParentElement, Render,
    ScrollHandle, StatefulInteractiveElement, Styled, Window, div, px, rgb,
};
use wisp_audiokit::SourceLabel;

use crate::app::{AppModel, Segment, SessionState};

pub struct TranscriptView {
    pub app: gpui::Entity<AppModel>,
    pub on_toggle_record: std::sync::Arc<dyn Fn(&mut Window, &mut gpui::App) + 'static>,
    /// Toggled by the cursor-blink animation timer in main.rs so the
    /// ghost-text caret pulses.
    pub cursor_visible: bool,
    /// Persistent scroll position for the transcript list.
    pub scroll_handle: ScrollHandle,
    /// Cheap fingerprint of the transcript on the previous render. When it
    /// changes between renders we know an event landed (new segment or
    /// partial text grew) and pin the scroll position to the bottom — but
    /// not on cursor-blink ticks, which would otherwise yank the viewport
    /// back down every 500ms when the user scrolls up to read history.
    pub last_signature: (usize, usize),
}

mod theme {
    use gpui::rgb;
    pub fn bg() -> gpui::Rgba {
        rgb(0x0b_0e13)
    }
    pub fn surface() -> gpui::Rgba {
        rgb(0x13_171f)
    }
    pub fn border() -> gpui::Rgba {
        rgb(0x1f_242e)
    }
    pub fn text_primary() -> gpui::Rgba {
        rgb(0xe8_eaed)
    }
    pub fn text_secondary() -> gpui::Rgba {
        rgb(0x8a_8f98)
    }
    pub fn text_tertiary() -> gpui::Rgba {
        rgb(0x5c_606b)
    }
    pub fn mic_accent() -> gpui::Rgba {
        rgb(0x74_b9ff)
    }
    pub fn system_accent() -> gpui::Rgba {
        rgb(0xff_9472)
    }
    pub fn record_red() -> gpui::Rgba {
        rgb(0xff_5959)
    }
    pub fn record_idle() -> gpui::Rgba {
        rgb(0x33_3942)
    }
}

impl Render for TranscriptView {
    fn render(
        &mut self,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let app = self.app.read(cx);
        let segments = app.segments.clone();
        let active_idx = app.active_segment_index();
        let state = app.state;
        let log_count = app.recent_log.len();
        let last_error = app.last_error.clone();

        // Pin the viewport to the bottom whenever the transcript actually
        // changed (new segment landed, or the current partial got longer).
        // Cursor blinks don't shift the signature so they don't fight the
        // user when they scroll up to read history.
        let signature = (segments.len(), segments.iter().map(|s| s.text.len()).sum());
        if signature != self.last_signature {
            self.scroll_handle.scroll_to_bottom();
            self.last_signature = signature;
        }

        div()
            .flex()
            .flex_col()
            .size_full()
            .bg(theme::bg())
            .text_color(theme::text_primary())
            .child(self.render_top_bar(state))
            .child(render_transcript(
                &segments,
                active_idx,
                self.cursor_visible,
                &self.scroll_handle,
            ))
            .child(render_status_bar(
                state,
                segments.len(),
                log_count,
                last_error.as_deref(),
            ))
    }
}

impl TranscriptView {
    fn render_top_bar(
        &self,
        state: SessionState,
    ) -> impl IntoElement {
        let toggle = self.on_toggle_record.clone();
        div()
            .h(px(56.0))
            .flex()
            .items_center()
            .justify_between()
            .px(px(20.0))
            .border_b_1()
            .border_color(theme::border())
            .child(render_brand())
            .child(render_record_button(state, toggle))
    }
}

fn render_brand() -> impl IntoElement {
    div()
        .flex()
        .items_center()
        .gap_3()
        .child(div().size(px(8.0)).rounded_full().bg(theme::mic_accent()))
        .child(
            div()
                .text_color(theme::text_primary())
                .font_weight(FontWeight::SEMIBOLD)
                .child("Wisp"),
        )
        .child(
            div()
                .text_xs()
                .text_color(theme::text_tertiary())
                .child("on-device transcription"),
        )
}

fn render_record_button(
    state: SessionState,
    on_click: std::sync::Arc<dyn Fn(&mut Window, &mut gpui::App) + 'static>,
) -> impl IntoElement {
    let (label, fill, dot_color) = match state {
        SessionState::Idle | SessionState::Failed => {
            ("Record", theme::record_idle(), theme::record_red())
        },
        SessionState::Recording { .. } => ("Stop", theme::record_red(), rgb(0xff_ffff)),
        SessionState::Starting => ("Starting…", theme::record_idle(), theme::text_tertiary()),
        SessionState::Stopping => ("Stopping…", theme::record_idle(), theme::text_tertiary()),
    };
    let interactive = matches!(
        state,
        SessionState::Idle | SessionState::Recording { .. } | SessionState::Failed
    );
    let id = ElementId::Name("record-button".into());
    let mut button = div()
        .id(id)
        .flex()
        .items_center()
        .gap_2()
        .px(px(14.0))
        .py(px(7.0))
        .rounded_full()
        .bg(fill)
        .text_color(theme::text_primary())
        .text_sm()
        .font_weight(FontWeight::MEDIUM)
        .child(div().size(px(8.0)).rounded_full().bg(dot_color))
        .child(label);
    if interactive {
        button = button.cursor_pointer().on_click(move |_event, window, cx| {
            on_click(window, cx);
        });
    }
    button
}

fn render_transcript(
    segments: &[Segment],
    active_idx: Option<usize>,
    cursor_visible: bool,
    scroll_handle: &ScrollHandle,
) -> impl IntoElement {
    let mut container = div()
        .id(ElementId::Name("transcript-scroll".into()))
        .track_scroll(scroll_handle)
        .flex()
        .flex_col()
        .flex_grow()
        .overflow_y_scroll()
        .px(px(20.0))
        .py(px(16.0))
        .gap(px(10.0));

    if segments.is_empty() {
        container = container.child(render_empty_state());
    } else {
        for (i, seg) in segments.iter().enumerate() {
            container = container.child(render_segment(
                seg,
                Some(i) == active_idx && cursor_visible,
                !seg.is_final && Some(i) == active_idx,
            ));
        }
    }
    container
}

fn render_empty_state() -> impl IntoElement {
    div()
        .flex()
        .flex_col()
        .items_center()
        .justify_center()
        .size_full()
        .gap_2()
        .child(
            div()
                .text_color(theme::text_secondary())
                .font_weight(FontWeight::MEDIUM)
                .child("Ready when you are."),
        )
        .child(
            div()
                .text_xs()
                .text_color(theme::text_tertiary())
                .child("Press Record to start capturing mic + system audio."),
        )
}

fn render_segment(
    seg: &Segment,
    show_cursor: bool,
    is_active: bool,
) -> impl IntoElement {
    let accent = match seg.source {
        SourceLabel::Mic => theme::mic_accent(),
        SourceLabel::System => theme::system_accent(),
    };
    let label = match seg.source {
        SourceLabel::Mic => "MIC",
        SourceLabel::System => "SYS",
    };
    let text_color = if is_active {
        theme::text_secondary()
    } else {
        theme::text_primary()
    };

    // Break the displayed text after each sentence-ending 。 so the row
    // doesn't read as one giant paragraph. Trailing 。 isn't broken — when
    // the next sentence comes in the line break appears naturally, giving
    // the partial a typewriter feel.
    //
    // The caret is then appended to the text string so it sits inline; the
    // blink is driven by a timer in main.rs toggling `cursor_visible` and
    // re-rendering.
    let mut display = break_on_sentence_end(&seg.text);
    if is_active {
        display.push(if show_cursor { '▊' } else { ' ' });
    }
    // `min_w_0` is the CSS dance that lets a flex item shrink below its
    // intrinsic content width. Without it, long Japanese strings (no
    // whitespace, so no implicit break points) just blow past the right
    // edge of the window. `whitespace_normal` keeps wrapping enabled even
    // when content is wider than the box.
    let body = div()
        .flex_grow()
        .min_w_0()
        .whitespace_normal()
        .text_color(text_color)
        .line_height(px(22.0))
        .child(display);

    div()
        .flex()
        .items_start()
        .w_full()
        .gap(px(12.0))
        .py(px(8.0))
        .px(px(12.0))
        .bg(theme::surface())
        .rounded(px(8.0))
        .border_l_2()
        .border_color(accent)
        .child(
            div()
                .w(px(36.0))
                .flex_none()
                .text_xs()
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(accent)
                .child(label),
        )
        .child(body)
}

fn render_status_bar(
    state: SessionState,
    segment_count: usize,
    log_count: usize,
    last_error: Option<&str>,
) -> impl IntoElement {
    let (dot, status_text) = match state {
        SessionState::Idle => (theme::record_idle(), "Idle".to_string()),
        SessionState::Starting => (theme::text_tertiary(), "Starting…".to_string()),
        SessionState::Recording { started_at } => {
            let secs = started_at.elapsed().as_secs();
            (
                theme::record_red(),
                format!("Recording · {:02}:{:02}", secs / 60, secs % 60),
            )
        },
        SessionState::Stopping => (theme::text_tertiary(), "Stopping…".to_string()),
        SessionState::Failed => (
            theme::record_red(),
            last_error.map_or_else(|| "Failed".into(), |e| format!("Failed: {e}")),
        ),
    };
    div()
        .h(px(32.0))
        .flex()
        .items_center()
        .justify_between()
        .px(px(20.0))
        .border_t_1()
        .border_color(theme::border())
        .child(
            div()
                .flex()
                .items_center()
                .gap_2()
                .child(div().size(px(8.0)).rounded_full().bg(dot))
                .child(
                    div()
                        .text_xs()
                        .text_color(theme::text_secondary())
                        .child(status_text),
                ),
        )
        .child(
            div()
                .flex()
                .items_center()
                .gap_4()
                .child(
                    div()
                        .text_xs()
                        .text_color(theme::text_tertiary())
                        .child(format!("{segment_count} segments")),
                )
                .child(
                    div()
                        .text_xs()
                        .text_color(theme::text_tertiary())
                        .child(format!("{log_count} log lines")),
                ),
        )
}

/// Insert a `\n` after each sentence-ending 。 *except* the trailing
/// one — that way the partial line doesn't visibly break the moment the
/// punctuation is recognised; the break only appears once the next
/// sentence starts arriving.
fn break_on_sentence_end(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 8);
    let mut iter = text.chars().peekable();
    while let Some(c) = iter.next() {
        out.push(c);
        if c == '。' && iter.peek().is_some() {
            out.push('\n');
        }
    }
    out
}

/// Public helper used by main.rs to pick a polling interval.
pub fn cursor_blink_period() -> std::time::Duration {
    std::time::Duration::from_millis(500)
}

/// Public helper used by main.rs for the periodic UI tick (status bar
/// elapsed-time updates).
pub fn ui_tick_period() -> std::time::Duration {
    std::time::Duration::from_millis(250)
}

/// Public helper: timestamp `Instant` for "right now".
pub fn now() -> Instant {
    Instant::now()
}

#[cfg(test)]
mod tests {
    use super::break_on_sentence_end;

    #[test]
    fn breaks_between_sentences_but_not_at_trailing_period() {
        assert_eq!(
            break_on_sentence_end("一文目。二文目。"),
            "一文目。\n二文目。"
        );
    }

    #[test]
    fn passes_through_text_without_period() {
        assert_eq!(break_on_sentence_end("途中"), "途中");
    }

    #[test]
    fn preserves_text_with_only_a_trailing_period() {
        assert_eq!(break_on_sentence_end("こんにちは。"), "こんにちは。");
    }
}
