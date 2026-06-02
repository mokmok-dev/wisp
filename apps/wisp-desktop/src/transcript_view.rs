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
use wisp_audiokit::{Permission, PermissionStatus, SessionError, SourceLabel};
use wisp_core::{Session as StoredSession, SessionId};

use crate::app::{AppModel, Permissions, Segment, SessionState, View};
use crate::permissions as perms;
use crate::transcript_export::{self, suggested_export_name};

pub struct TranscriptView {
    pub app: gpui::Entity<AppModel>,
    pub on_toggle_record: std::sync::Arc<dyn Fn(&mut Window, &mut gpui::App) + 'static>,
    /// Request a permission. Fires the OS prompt asynchronously; the
    /// resulting status flows back into the model.
    pub on_request_permission:
        std::sync::Arc<dyn Fn(Permission, &mut Window, &mut gpui::App) + 'static>,
    /// Open the System Settings privacy pane for a permission. Used when
    /// the permission is already denied and only the user can re-enable it.
    pub on_open_settings: std::sync::Arc<dyn Fn(Permission, &mut Window, &mut gpui::App) + 'static>,
    /// Switch from the library screen to the empty recording screen.
    pub on_new_session: std::sync::Arc<dyn Fn(&mut Window, &mut gpui::App) + 'static>,
    /// Load a session's transcript from storage and switch to history view.
    pub on_open_history: std::sync::Arc<dyn Fn(SessionId, &mut Window, &mut gpui::App) + 'static>,
    /// Return to the library screen from a live or historical session view.
    pub on_back_to_library: std::sync::Arc<dyn Fn(&mut Window, &mut gpui::App) + 'static>,
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
        let permissions = app.permissions;

        // Gate the main UI on having both required permissions. Until then,
        // we show an onboarding screen with per-permission rows the user
        // can act on. This avoids the previous failure mode where the user
        // presses Record and only then learns the app needs permissions
        // they may or may not be able to grant.
        if !permissions.all_granted() {
            return self.render_onboarding(permissions).into_any_element();
        }

        let view = app.view.clone();
        let segments = app.segments.clone();
        let active_idx = app.active_segment_index();
        let state = app.state;
        let log_count = app.recent_log.len();
        let last_error = app.last_error.clone();
        let viewed_session = app.viewed_session.clone();
        let current_session_id = app.current_session_id;
        let library = app.library.clone();

        match view {
            View::Library => self.render_library(&library).into_any_element(),
            View::LiveSession => {
                self.update_scroll_signature(&segments);
                let live_export_title = current_session_id
                    .and_then(|id| library.iter().find(|s| s.id == id).map(|s| s.title.clone()));
                self.render_live_session(
                    state,
                    &segments,
                    active_idx,
                    log_count,
                    last_error.as_ref(),
                    live_export_title.as_deref(),
                )
                .into_any_element()
            },
            View::History { .. } => self
                .render_history(viewed_session.as_ref(), &segments)
                .into_any_element(),
        }
    }
}

impl TranscriptView {
    /// Refresh `last_signature` and pin scroll to bottom on transcript
    /// growth. Only the live-session view calls this — library and history
    /// don't have a streaming partial to follow.
    fn update_scroll_signature(
        &mut self,
        segments: &[Segment],
    ) {
        let signature = (segments.len(), segments.iter().map(|s| s.text.len()).sum());
        if signature != self.last_signature {
            if is_at_bottom(&self.scroll_handle) {
                self.scroll_handle.scroll_to_bottom();
            }
            self.last_signature = signature;
        }
    }

    fn render_live_session(
        &self,
        state: SessionState,
        segments: &[Segment],
        active_idx: Option<usize>,
        log_count: usize,
        last_error: Option<&SessionError>,
        export_title: Option<&str>,
    ) -> impl IntoElement {
        let export_name = suggested_export_name(export_title, "transcript");
        div()
            .flex()
            .flex_col()
            .size_full()
            .bg(theme::bg())
            .text_color(theme::text_primary())
            .child(self.render_live_top_bar(state, segments, &export_name))
            .child(render_transcript(
                segments,
                active_idx,
                self.cursor_visible,
                &self.scroll_handle,
            ))
            .child(render_status_bar(
                state,
                segments.len(),
                log_count,
                last_error,
            ))
    }

    fn render_history(
        &self,
        session: Option<&StoredSession>,
        segments: &[Segment],
    ) -> impl IntoElement {
        let title = session.map_or_else(|| "Session".to_string(), |s| s.title.clone());
        let subtitle = session.map(history_subtitle);
        let export_name = suggested_export_name(Some(&title), "transcript");

        div()
            .flex()
            .flex_col()
            .size_full()
            .bg(theme::bg())
            .text_color(theme::text_primary())
            .child(self.render_history_top_bar(&title, subtitle.as_deref(), segments, &export_name))
            .child(render_transcript(
                segments,
                None,
                false,
                &self.scroll_handle,
            ))
            .child(render_count_status_bar(format!(
                "{} segments",
                segments.len()
            )))
    }

    fn render_library(
        &self,
        sessions: &[StoredSession],
    ) -> impl IntoElement {
        let on_new = self.on_new_session.clone();
        let header = div()
            .h(px(56.0))
            .flex()
            .items_center()
            .justify_between()
            .px(px(20.0))
            .border_b_1()
            .border_color(theme::border())
            .child(render_brand())
            .child(render_new_session_button(on_new));

        let body = if sessions.is_empty() {
            render_empty_library().into_any_element()
        } else {
            render_session_list(sessions, self.on_open_history.clone()).into_any_element()
        };

        div()
            .flex()
            .flex_col()
            .size_full()
            .bg(theme::bg())
            .text_color(theme::text_primary())
            .child(header)
            .child(body)
            .child(render_count_status_bar(format!(
                "{} sessions",
                sessions.len()
            )))
    }

    fn render_live_top_bar(
        &self,
        state: SessionState,
        segments: &[Segment],
        export_name: &str,
    ) -> impl IntoElement {
        let toggle = self.on_toggle_record.clone();
        let on_back = self.on_back_to_library.clone();
        div()
            .h(px(56.0))
            .flex()
            .items_center()
            .justify_between()
            .px(px(20.0))
            .border_b_1()
            .border_color(theme::border())
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(12.0))
                    .child(render_back_button("library-back-live", on_back))
                    .child(render_brand()),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(8.0))
                    .child(render_transcript_actions(segments, export_name))
                    .child(render_record_button(state, toggle)),
            )
    }

    fn render_history_top_bar(
        &self,
        title: &str,
        subtitle: Option<&str>,
        segments: &[Segment],
        export_name: &str,
    ) -> impl IntoElement {
        let on_back = self.on_back_to_library.clone();
        let mut title_block = div().flex().flex_col().gap(px(2.0)).child(
            div()
                .text_color(theme::text_primary())
                .font_weight(FontWeight::SEMIBOLD)
                .child(title.to_string()),
        );
        if let Some(sub) = subtitle {
            title_block = title_block.child(
                div()
                    .text_xs()
                    .text_color(theme::text_tertiary())
                    .child(sub.to_string()),
            );
        }
        div()
            .h(px(56.0))
            .flex()
            .items_center()
            .justify_between()
            .px(px(20.0))
            .border_b_1()
            .border_color(theme::border())
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(12.0))
                    .child(render_back_button("library-back-history", on_back))
                    .child(title_block),
            )
            .child(render_transcript_actions(segments, export_name))
    }

    fn render_onboarding(
        &self,
        permissions: Permissions,
    ) -> impl IntoElement {
        let pending = permissions.pending;
        let row_mic = self.render_permission_row(
            Permission::Microphone,
            permissions.microphone,
            pending == Some(Permission::Microphone),
        );
        let row_speech = self.render_permission_row(
            Permission::SpeechRecognition,
            permissions.speech,
            pending == Some(Permission::SpeechRecognition),
        );

        let card = div()
            .flex()
            .flex_col()
            .gap(px(16.0))
            .w(px(520.0))
            .p(px(24.0))
            .bg(theme::surface())
            .rounded(px(12.0))
            .border_1()
            .border_color(theme::border())
            .child(
                div()
                    .text_color(theme::text_primary())
                    .font_weight(FontWeight::SEMIBOLD)
                    .child("Wisp needs a couple of permissions"),
            )
            .child(
                div()
                    .text_xs()
                    .text_color(theme::text_secondary())
                    .child("These run entirely on-device. Wisp doesn't send your audio anywhere."),
            )
            .child(row_mic)
            .child(row_speech);

        div()
            .flex()
            .flex_col()
            .items_center()
            .justify_center()
            .size_full()
            .bg(theme::bg())
            .text_color(theme::text_primary())
            .child(card)
    }

    fn render_permission_row(
        &self,
        perm: Permission,
        status: PermissionStatus,
        is_pending: bool,
    ) -> impl IntoElement {
        let title_text = perms::label(perm);
        let rationale_text = perms::rationale(perm);
        let status_text = perms::status_label(status);

        let info = div()
            .flex()
            .flex_col()
            .gap(px(4.0))
            .flex_grow()
            .min_w_0()
            .child(
                div()
                    .text_color(theme::text_primary())
                    .font_weight(FontWeight::MEDIUM)
                    .child(title_text),
            )
            .child(
                div()
                    .text_xs()
                    .text_color(theme::text_tertiary())
                    .child(rationale_text),
            )
            .child(
                div()
                    .text_xs()
                    .text_color(status_color(status))
                    .child(status_text),
            );

        let action = self.render_permission_action(perm, status, is_pending);

        div()
            .flex()
            .items_center()
            .gap(px(12.0))
            .py(px(12.0))
            .px(px(12.0))
            .bg(theme::bg())
            .rounded(px(8.0))
            .border_l_2()
            .border_color(status_color(status))
            .child(info)
            .child(action)
    }

    fn render_permission_action(
        &self,
        perm: Permission,
        status: PermissionStatus,
        is_pending: bool,
    ) -> gpui::AnyElement {
        // Already granted — nothing to do; render a static check label so
        // the row stays balanced.
        if status == PermissionStatus::Granted {
            return div()
                .px(px(14.0))
                .py(px(7.0))
                .text_sm()
                .text_color(theme::text_tertiary())
                .child("Allowed")
                .into_any_element();
        }
        // Restricted means a system policy is preventing this; there is no
        // user-facing toggle. Just label it.
        if status == PermissionStatus::Restricted {
            return div()
                .px(px(14.0))
                .py(px(7.0))
                .text_sm()
                .text_color(theme::text_tertiary())
                .child("Restricted")
                .into_any_element();
        }
        // A request is already in flight — show a non-interactive label.
        if is_pending {
            return div()
                .px(px(14.0))
                .py(px(7.0))
                .text_sm()
                .text_color(theme::text_tertiary())
                .child("Waiting…")
                .into_any_element();
        }

        // Undetermined → can re-trigger the OS prompt.
        // Denied → can't, OS won't prompt again; jump straight to Settings.
        let (label, action_kind) = match status {
            PermissionStatus::Denied => ("Open Settings", ActionKind::OpenSettings),
            _ => ("Allow", ActionKind::Request),
        };
        let on_request = self.on_request_permission.clone();
        let on_open = self.on_open_settings.clone();
        let id_label = match action_kind {
            ActionKind::Request => "permission-allow",
            ActionKind::OpenSettings => "permission-open-settings",
        };
        // Element IDs must be unique per render tree; suffix with the
        // permission discriminant so the two rows don't collide.
        let suffix = match perm {
            Permission::Microphone => "mic",
            Permission::SpeechRecognition => "speech",
        };
        let id = ElementId::Name(format!("{id_label}-{suffix}").into());
        div()
            .id(id)
            .px(px(14.0))
            .py(px(7.0))
            .rounded_full()
            .bg(theme::record_idle())
            .text_color(theme::text_primary())
            .text_sm()
            .font_weight(FontWeight::MEDIUM)
            .cursor_pointer()
            .on_click(move |_event, window, cx| match action_kind {
                ActionKind::Request => on_request(perm, window, cx),
                ActionKind::OpenSettings => on_open(perm, window, cx),
            })
            .child(label)
            .into_any_element()
    }
}

#[derive(Debug, Clone, Copy)]
enum ActionKind {
    Request,
    OpenSettings,
}

fn status_color(status: PermissionStatus) -> gpui::Rgba {
    match status {
        PermissionStatus::Granted => theme::mic_accent(),
        PermissionStatus::Denied | PermissionStatus::Restricted => theme::record_red(),
        PermissionStatus::Undetermined => theme::text_tertiary(),
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

fn render_new_session_button(
    on_click: std::sync::Arc<dyn Fn(&mut Window, &mut gpui::App) + 'static>
) -> impl IntoElement {
    div()
        .id(ElementId::Name("new-session-button".into()))
        .flex()
        .items_center()
        .gap_2()
        .px(px(14.0))
        .py(px(7.0))
        .rounded_full()
        .bg(theme::record_idle())
        .text_color(theme::text_primary())
        .text_sm()
        .font_weight(FontWeight::MEDIUM)
        .cursor_pointer()
        .child(div().size(px(8.0)).rounded_full().bg(theme::mic_accent()))
        .child("New Session")
        .on_click(move |_event, window, cx| {
            on_click(window, cx);
        })
}

fn render_transcript_actions(
    segments: &[Segment],
    export_name: &str,
) -> gpui::AnyElement {
    if transcript_export::format_transcript_plain(segments).is_empty() {
        return div().into_any_element();
    }

    let segments_copy = segments.to_vec();
    let segments_export = segments.to_vec();
    let export_name = export_name.to_string();

    div()
        .flex()
        .items_center()
        .gap(px(6.0))
        .child(render_toolbar_button(
            "transcript-copy",
            "Copy",
            move |_window, cx| {
                transcript_export::copy_transcript_to_clipboard(&segments_copy, cx);
            },
        ))
        .child(render_toolbar_button("transcript-export", "Export", {
            let segments_export = segments_export.clone();
            let export_name = export_name.clone();
            move |_window, cx| {
                transcript_export::export_transcript(segments_export.clone(), &export_name, cx);
            }
        }))
        .into_any_element()
}

fn render_toolbar_button(
    id: &'static str,
    label: &'static str,
    on_click: impl Fn(&mut Window, &mut gpui::App) + 'static,
) -> impl IntoElement {
    div()
        .id(ElementId::Name(id.into()))
        .px(px(12.0))
        .py(px(6.0))
        .rounded_full()
        .bg(theme::record_idle())
        .text_color(theme::text_primary())
        .text_xs()
        .font_weight(FontWeight::MEDIUM)
        .cursor_pointer()
        .child(label)
        .on_click(move |_event, window, cx| on_click(window, cx))
}

fn render_back_button(
    id: &'static str,
    on_click: std::sync::Arc<dyn Fn(&mut Window, &mut gpui::App) + 'static>,
) -> impl IntoElement {
    div()
        .id(ElementId::Name(id.into()))
        .px(px(10.0))
        .py(px(5.0))
        .rounded_full()
        .bg(theme::record_idle())
        .text_color(theme::text_primary())
        .text_xs()
        .font_weight(FontWeight::MEDIUM)
        .cursor_pointer()
        .child("← Library")
        .on_click(move |_event, window, cx| {
            on_click(window, cx);
        })
}

fn render_empty_library() -> impl IntoElement {
    div()
        .flex()
        .flex_col()
        .items_center()
        .justify_center()
        .flex_grow()
        .gap_2()
        .child(
            div()
                .text_color(theme::text_secondary())
                .font_weight(FontWeight::MEDIUM)
                .child("No sessions yet."),
        )
        .child(
            div()
                .text_xs()
                .text_color(theme::text_tertiary())
                .child("Click New Session to record your first one."),
        )
}

#[allow(clippy::needless_pass_by_value)]
fn render_session_list(
    sessions: &[StoredSession],
    on_open: std::sync::Arc<dyn Fn(SessionId, &mut Window, &mut gpui::App) + 'static>,
) -> impl IntoElement {
    let mut list = div()
        .id(ElementId::Name("library-scroll".into()))
        .flex()
        .flex_col()
        .flex_grow()
        .overflow_y_scroll()
        .px(px(20.0))
        .py(px(16.0))
        .gap(px(8.0));
    for s in sessions {
        list = list.child(render_session_row(s, on_open.clone()));
    }
    list
}

fn render_session_row(
    session: &StoredSession,
    on_open: std::sync::Arc<dyn Fn(SessionId, &mut Window, &mut gpui::App) + 'static>,
) -> impl IntoElement {
    let id = session.id;
    // Unique element id per row — GPUI requires every interactive child
    // to carry a distinct one within its parent.
    let element_id = ElementId::Name(format!("session-row-{}", id.as_i64()).into());

    let started_local = session.started_at.with_timezone(&chrono::Local);
    let when = started_local.format("%Y-%m-%d %H:%M").to_string();
    let duration_text = session.ended_at.map_or_else(
        || "in progress".to_string(),
        |end| format_duration(end.signed_duration_since(session.started_at)),
    );

    div()
        .id(element_id)
        .flex()
        .items_center()
        .justify_between()
        .gap(px(12.0))
        .py(px(12.0))
        .px(px(14.0))
        .bg(theme::surface())
        .rounded(px(8.0))
        .border_l_2()
        .border_color(theme::mic_accent())
        .cursor_pointer()
        .child(
            div()
                .flex()
                .flex_col()
                .gap(px(4.0))
                .min_w_0()
                .flex_grow()
                .child(
                    div()
                        .text_color(theme::text_primary())
                        .font_weight(FontWeight::MEDIUM)
                        .child(session.title.clone()),
                )
                .child(
                    div()
                        .text_xs()
                        .text_color(theme::text_tertiary())
                        .child(when),
                ),
        )
        .child(
            div()
                .text_xs()
                .text_color(theme::text_secondary())
                .child(duration_text),
        )
        .on_click(move |_event, window, cx| {
            on_open(id, window, cx);
        })
}

/// Format a `chrono::Duration` as `MM:SS` or `H:MM:SS` for the library
/// row's right-hand label.
fn format_duration(d: chrono::Duration) -> String {
    let total = d.num_seconds().max(0);
    let h = total / 3600;
    let m = (total % 3600) / 60;
    let s = total % 60;
    if h > 0 {
        format!("{h}:{m:02}:{s:02}")
    } else {
        format!("{m:02}:{s:02}")
    }
}

fn history_subtitle(session: &StoredSession) -> String {
    let started = session.started_at.with_timezone(&chrono::Local);
    let when = started.format("%Y-%m-%d %H:%M").to_string();
    match session.ended_at {
        Some(end) => {
            let dur = format_duration(end.signed_duration_since(session.started_at));
            format!("{when} · {dur}")
        },
        None => format!("{when} · in progress"),
    }
}

/// A minimal status bar showing a single left-aligned count label. Shared
/// by the library ("N sessions") and history ("N segments") screens.
fn render_count_status_bar(text: String) -> impl IntoElement {
    div()
        .h(px(32.0))
        .flex()
        .items_center()
        .px(px(20.0))
        .border_t_1()
        .border_color(theme::border())
        .child(
            div()
                .text_xs()
                .text_color(theme::text_secondary())
                .child(text),
        )
}

fn render_status_bar(
    state: SessionState,
    segment_count: usize,
    log_count: usize,
    last_error: Option<&SessionError>,
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

/// Is the scroll handle close enough to the bottom that we should
/// continue auto-following new content?
///
/// GPUI's vertical scroll offset is in `[-max_offset.height, 0]` — `0`
/// is the top of the content, `-max_offset.height` is the bottom. We
/// allow a few pixels of slack so wheel inertia / one-pixel rounding
/// don't accidentally disable auto-follow. On the very first render
/// (`max_offset.height == 0`) this also returns true, so the initial
/// arrival of segments gets scrolled into view.
fn is_at_bottom(handle: &ScrollHandle) -> bool {
    let slack = px(8.0);
    let bottom = -handle.max_offset().height;
    handle.offset().y <= bottom + slack
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
