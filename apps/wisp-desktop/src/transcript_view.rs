//! The main wisp-desktop view. Three rows top to bottom:
//!
//!   - Top bar  (48px) — title left, record/stop button right
//!   - Transcript area (flex 1) — scrollable list of segments with the
//!     ghost-text styling for the active partial
//!   - Status bar (28px) — recording dot, elapsed time, segment count
//!
//! Color palette (see `theme` mod) is a deep-slate dark mode with warm
//! mic/system accents.

use std::cell::RefCell;
use std::rc::Rc;
use std::time::Instant;

use gpui::{
    App, Context, ElementId, Entity, FontWeight, InteractiveElement, IntoElement, ListAlignment,
    ListState, ParentElement, Render, StatefulInteractiveElement, Styled, Window, div, list, px,
    rgb,
};
use wisp_audiokit::{
    LocalModelStatus, Permission, PermissionStatus, RecognizerBackend, SourceLabel,
};
use wisp_core::{Session as StoredSession, SessionId};

use crate::app::{
    AppError, AppModel, LocalMcpBridge, ModelDownloadState, Permissions, Segment, SessionState,
    Setup, View,
};
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
    /// Select the transcription backend used for new sessions.
    pub on_select_recognizer:
        std::sync::Arc<dyn Fn(RecognizerBackend, &mut Window, &mut gpui::App) + 'static>,
    /// Download the local transcription model on a background thread.
    pub on_download_local_model: std::sync::Arc<dyn Fn(&mut Window, &mut gpui::App) + 'static>,
    /// Switch from the library screen to the empty recording screen.
    pub on_new_session: std::sync::Arc<dyn Fn(&mut Window, &mut gpui::App) + 'static>,
    /// Load a session's transcript from storage and switch to history view.
    pub on_open_history: std::sync::Arc<dyn Fn(SessionId, &mut Window, &mut gpui::App) + 'static>,
    /// Return to the library screen from a live or historical session view.
    pub on_back_to_library: std::sync::Arc<dyn Fn(&mut Window, &mut gpui::App) + 'static>,
    /// Enable/disable the local MCP bridge from the library settings card.
    pub on_toggle_local_mcp: std::sync::Arc<dyn Fn(&mut Window, &mut gpui::App) + 'static>,
    /// Toggled by the cursor-blink animation timer in main.rs so the
    /// ghost-text caret pulses.
    pub cursor_visible: bool,
    /// Virtualized transcript list — only visible rows are laid out.
    pub transcript_list: ListState,
    pub(crate) transcript_list_count: usize,
    pub(crate) transcript_active_len: usize,
    pub(crate) transcript_list_view: View,
    /// When true, new transcript lines keep the viewport pinned to the bottom.
    pub follow_transcript: Rc<RefCell<bool>>,
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
        let setup = app.setup.clone();

        // Gate the main UI on having both required permissions. Until then,
        // we show an onboarding screen with per-permission rows the user
        // can act on. This avoids the previous failure mode where the user
        // presses Record and only then learns the app needs permissions
        // they may or may not be able to grant.
        if !app.setup_complete() {
            return self
                .render_onboarding(permissions, &setup)
                .into_any_element();
        }

        let view = app.view.clone();
        let segment_count = app.segments.len();
        let text_len_sum: usize = app.segments.iter().map(|s| s.text.len()).sum();
        let active_idx = app.active_segment_index();
        let active_text_len = active_idx.map(|i| app.segments[i].text.len());
        let state = app.state;
        let log_count = app.recent_log.len();
        let last_error = app.last_error.clone();
        let viewed_session = app.viewed_session.clone();
        let current_session_id = app.current_session_id;
        let library = app.library.clone();
        let local_mcp = app.local_mcp.clone();
        let model = self.app.clone();

        match view {
            View::Library => self.render_library(&library, &local_mcp).into_any_element(),
            View::LiveSession => {
                self.sync_transcript_list(&view, segment_count, active_idx, active_text_len);
                self.update_scroll_signature(segment_count, text_len_sum);
                let live_export_title = current_session_id
                    .and_then(|id| library.iter().find(|s| s.id == id).map(|s| s.title.clone()));
                self.render_live_session(
                    state,
                    model,
                    segment_count,
                    log_count,
                    last_error.as_ref(),
                    live_export_title.as_deref(),
                    cx,
                )
                .into_any_element()
            },
            View::History { .. } => {
                self.sync_transcript_list(&view, segment_count, active_idx, active_text_len);
                self.render_history(viewed_session.as_ref(), model, segment_count, cx)
                    .into_any_element()
            },
        }
    }
}

impl TranscriptView {
    /// Keep `ListState` in sync with the model — append/splice rows instead
    /// of rebuilding the whole list each frame.
    fn sync_transcript_list(
        &mut self,
        view: &View,
        segment_count: usize,
        active_idx: Option<usize>,
        active_text_len: Option<usize>,
    ) {
        if *view != self.transcript_list_view {
            self.transcript_list.reset(segment_count);
            self.transcript_list_count = segment_count;
            self.transcript_active_len = 0;
            self.transcript_list_view = view.clone();
            *self.follow_transcript.borrow_mut() = matches!(view, View::LiveSession);
            return;
        }

        if segment_count != self.transcript_list_count {
            let old = self.transcript_list_count;
            if segment_count > old {
                self.transcript_list.splice(old..old, segment_count - old);
            } else {
                self.transcript_list.reset(segment_count);
            }
            self.transcript_list_count = segment_count;
            self.transcript_active_len = 0;
        }

        if let (Some(idx), Some(len)) = (active_idx, active_text_len) {
            if len != self.transcript_active_len {
                self.transcript_list.splice(idx..idx + 1, 1);
                self.transcript_active_len = len;
            }
        } else {
            self.transcript_active_len = 0;
        }
    }

    /// Refresh `last_signature` and pin scroll to bottom on transcript
    /// growth. Only the live-session view calls this — library and history
    /// don't have a streaming partial to follow.
    fn update_scroll_signature(
        &mut self,
        segment_count: usize,
        text_len_sum: usize,
    ) {
        let signature = (segment_count, text_len_sum);
        if signature != self.last_signature {
            if *self.follow_transcript.borrow() && segment_count > 0 {
                self.transcript_list
                    .scroll_to_reveal_item(segment_count - 1);
            }
            self.last_signature = signature;
        }
    }

    fn render_live_session(
        &self,
        state: SessionState,
        model: Entity<AppModel>,
        segment_count: usize,
        log_count: usize,
        last_error: Option<&AppError>,
        export_title: Option<&str>,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let export_name = suggested_export_name(export_title, "transcript");
        div()
            .flex()
            .flex_col()
            .size_full()
            .bg(theme::bg())
            .text_color(theme::text_primary())
            .child(self.render_live_top_bar(state, model.clone(), &export_name, cx))
            .child(render_transcript(
                self.transcript_list.clone(),
                model,
                segment_count,
                self.cursor_visible,
            ))
            .child(render_status_bar(
                state,
                segment_count,
                log_count,
                last_error,
            ))
    }

    fn render_history(
        &self,
        session: Option<&StoredSession>,
        model: Entity<AppModel>,
        segment_count: usize,
        cx: &mut Context<Self>,
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
            .child(self.render_history_top_bar(
                &title,
                subtitle.as_deref(),
                model.clone(),
                &export_name,
                cx,
            ))
            .child(render_transcript(
                self.transcript_list.clone(),
                model,
                segment_count,
                false,
            ))
            .child(render_count_status_bar(format!("{segment_count} segments")))
    }

    fn render_library(
        &self,
        sessions: &[StoredSession],
        local_mcp: &LocalMcpBridge,
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

        let body = render_session_list(
            sessions,
            self.on_open_history.clone(),
            local_mcp,
            self.on_toggle_local_mcp.clone(),
        );

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
        model: Entity<AppModel>,
        export_name: &str,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let toggle = self.on_toggle_record.clone();
        let on_back = self.on_back_to_library.clone();
        let can_leave_live_session = !model.read(cx).live_session_is_protected();
        let mut navigation = div().flex().items_center().gap(px(12.0));
        if can_leave_live_session {
            navigation = navigation.child(render_back_button("library-back-live", on_back));
        }
        navigation = navigation.child(render_brand());
        div()
            .h(px(56.0))
            .flex()
            .items_center()
            .justify_between()
            .px(px(20.0))
            .border_b_1()
            .border_color(theme::border())
            .child(navigation)
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(8.0))
                    .child(render_transcript_actions(model, export_name, cx))
                    .child(render_record_button(state, toggle)),
            )
    }

    fn render_history_top_bar(
        &self,
        title: &str,
        subtitle: Option<&str>,
        model: Entity<AppModel>,
        export_name: &str,
        cx: &mut Context<Self>,
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
            .child(render_transcript_actions(model, export_name, cx))
    }

    fn render_onboarding(
        &self,
        permissions: Permissions,
        setup: &Setup,
    ) -> impl IntoElement {
        let pending = permissions.pending;
        let setup_title = if wisp_audiokit::requires_recognizer_setup() {
            "Wisp needs a quick setup"
        } else {
            "Wisp needs a couple of permissions"
        };
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

        let mut card = div()
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
                    .child(setup_title),
            )
            .child(
                div()
                    .text_xs()
                    .text_color(theme::text_secondary())
                    .child("These run entirely on-device. Wisp doesn't send your audio anywhere."),
            )
            .child(row_mic)
            .child(row_speech);
        if wisp_audiokit::requires_recognizer_setup() {
            card = card
                .child(self.render_recognizer_row(setup))
                .child(self.render_local_model_row(setup));
        }

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

    fn render_recognizer_row(
        &self,
        setup: &Setup,
    ) -> impl IntoElement {
        let selected = setup.recognizer;
        let platform_action =
            self.render_recognizer_option(RecognizerBackend::Platform, selected, "Platform");
        let local_action =
            self.render_recognizer_option(RecognizerBackend::LocalModel, selected, "Local");
        div()
            .flex()
            .items_center()
            .gap(px(12.0))
            .py(px(12.0))
            .px(px(12.0))
            .bg(theme::bg())
            .rounded(px(8.0))
            .border_l_2()
            .border_color(if setup.is_complete() {
                theme::mic_accent()
            } else {
                theme::text_tertiary()
            })
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(4.0))
                    .flex_grow()
                    .min_w_0()
                    .child(
                        div()
                            .text_color(theme::text_primary())
                            .font_weight(FontWeight::MEDIUM)
                            .child("Transcription backend"),
                    )
                    .child(
                        div()
                            .text_xs()
                            .text_color(theme::text_tertiary())
                            .child("Use Windows speech for the OS mic path, or a local model for WASAPI mic + system audio."),
                    )
                    .child(
                        div()
                            .text_xs()
                            .text_color(theme::text_secondary())
                            .child(selected.label()),
                    ),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .p(px(2.0))
                    .rounded_full()
                    .bg(theme::record_idle())
                    .child(platform_action)
                    .child(local_action),
            )
    }

    fn render_recognizer_option(
        &self,
        recognizer: RecognizerBackend,
        selected: RecognizerBackend,
        label: &'static str,
    ) -> impl IntoElement {
        let is_selected = recognizer == selected;
        let on_select = self.on_select_recognizer.clone();
        let id = match recognizer {
            RecognizerBackend::Platform => "recognizer-platform",
            RecognizerBackend::LocalModel => "recognizer-local",
        };
        let mut button = div()
            .id(ElementId::Name(id.into()))
            .px(px(12.0))
            .py(px(5.0))
            .rounded_full()
            .text_xs()
            .font_weight(FontWeight::MEDIUM)
            .text_color(if is_selected {
                theme::text_primary()
            } else {
                theme::text_secondary()
            })
            .child(label);
        if is_selected {
            button = button.bg(theme::surface());
        } else {
            button = button.cursor_pointer().on_click(move |_event, window, cx| {
                on_select(recognizer, window, cx);
            });
        }
        button
    }

    fn render_local_model_row(
        &self,
        setup: &Setup,
    ) -> impl IntoElement {
        let (status_text, status_color) = match setup.local_model.clone() {
            LocalModelStatus::Ready { bytes, .. } => (
                format!("Ready ({:.0} MB)", bytes as f64 / 1024.0 / 1024.0),
                theme::mic_accent(),
            ),
            LocalModelStatus::Missing { spec, .. } => (
                format!(
                    "Not downloaded ({:.0} MB)",
                    spec.approx_bytes as f64 / 1024.0 / 1024.0
                ),
                if setup.recognizer == RecognizerBackend::LocalModel {
                    theme::record_red()
                } else {
                    theme::text_tertiary()
                },
            ),
        };
        let mut info = div()
            .flex()
            .flex_col()
            .gap(px(4.0))
            .flex_grow()
            .min_w_0()
            .child(
                div()
                    .text_color(theme::text_primary())
                    .font_weight(FontWeight::MEDIUM)
                    .child("Local transcription model"),
            )
            .child(
                div()
                    .text_xs()
                    .text_color(theme::text_tertiary())
                    .child("Downloaded once, stored locally, and used without network access."),
            )
            .child(div().text_xs().text_color(status_color).child(status_text));
        if let Some(error) = &setup.model_error {
            info = info.child(
                div()
                    .text_xs()
                    .text_color(theme::record_red())
                    .child(error.clone()),
            );
        }
        div()
            .flex()
            .items_center()
            .gap(px(12.0))
            .py(px(12.0))
            .px(px(12.0))
            .bg(theme::bg())
            .rounded(px(8.0))
            .border_l_2()
            .border_color(status_color)
            .child(info)
            .child(self.render_local_model_action(setup))
    }

    fn render_local_model_action(
        &self,
        setup: &Setup,
    ) -> gpui::AnyElement {
        if setup.model_download == ModelDownloadState::Downloading {
            return div()
                .px(px(14.0))
                .py(px(7.0))
                .text_sm()
                .text_color(theme::text_tertiary())
                .child("Downloading…")
                .into_any_element();
        }
        if setup.local_model.is_ready() {
            return div()
                .px(px(14.0))
                .py(px(7.0))
                .text_sm()
                .text_color(theme::text_tertiary())
                .child("Installed")
                .into_any_element();
        }
        let on_download = self.on_download_local_model.clone();
        div()
            .id(ElementId::Name("download-local-model".into()))
            .px(px(14.0))
            .py(px(7.0))
            .rounded_full()
            .bg(theme::record_idle())
            .text_color(theme::text_primary())
            .text_sm()
            .font_weight(FontWeight::MEDIUM)
            .cursor_pointer()
            .on_click(move |_event, window, cx| on_download(window, cx))
            .child("Download")
            .into_any_element()
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
    list_state: ListState,
    model: Entity<AppModel>,
    segment_count: usize,
    cursor_visible: bool,
) -> impl IntoElement {
    let mut container = div()
        .id(ElementId::Name("transcript-scroll".into()))
        .flex()
        .flex_col()
        .flex_grow();

    if segment_count == 0 {
        container = container.child(render_empty_state());
    } else {
        let model_for_list = model.clone();
        container = container.px(px(20.0)).py(px(16.0)).child(
            list(list_state, move |ix, _window, cx| {
                let app = model_for_list.read(cx);
                let Some(seg) = app.segments.get(ix) else {
                    return div().into_any_element();
                };
                let active_idx = app.active_segment_index();
                let is_active = Some(ix) == active_idx;
                render_segment_row(
                    ix,
                    seg,
                    is_active && cursor_visible,
                    is_active && !seg.is_final,
                )
                .into_any_element()
            })
            .w_full()
            .h_full(),
        );
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

/// One transcript row in the virtualized list. GPUI's `list` stacks items by
/// measured height and does not leave space for margins between siblings, so
/// the inter-segment gap is applied as top padding on the wrapper (matching
/// the old flex column's `gap(px(10.0))`).
fn render_segment_row(
    index: usize,
    seg: &Segment,
    show_cursor: bool,
    is_active: bool,
) -> impl IntoElement {
    let gap = if index > 0 { px(10.0) } else { px(0.0) };
    div()
        .w_full()
        .pt(gap)
        .child(render_segment_card(seg, show_cursor, is_active))
}

fn render_segment_card(
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

    // `display_text` is kept in sync with `text` on ingest; append the caret
    // inline for the active partial. Blink is driven by main.rs.
    let mut display = seg.display_text.clone();
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
    model: Entity<AppModel>,
    export_name: &str,
    cx: &App,
) -> gpui::AnyElement {
    let has_content = model
        .read(cx)
        .segments
        .iter()
        .any(|seg| !seg.text.trim().is_empty());
    if !has_content {
        return div().into_any_element();
    }

    let segments_copy = model.read(cx).segments.clone();
    let segments_export = segments_copy.clone();
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
    local_mcp: &LocalMcpBridge,
    on_toggle_local_mcp: std::sync::Arc<dyn Fn(&mut Window, &mut gpui::App) + 'static>,
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
    list = list.child(render_local_mcp_bridge_card(local_mcp, on_toggle_local_mcp));
    if sessions.is_empty() {
        return list.child(render_empty_library());
    }
    for s in sessions {
        list = list.child(render_session_row(s, on_open.clone()));
    }
    list
}

fn render_local_mcp_bridge_card(
    local_mcp: &LocalMcpBridge,
    on_toggle: std::sync::Arc<dyn Fn(&mut Window, &mut gpui::App) + 'static>,
) -> impl IntoElement {
    let (status_text, status_color) = if let Some(error) = &local_mcp.error {
        (format!("Failed: {error}"), theme::record_red())
    } else if local_mcp.running {
        ("Running".to_owned(), theme::mic_accent())
    } else if local_mcp.enabled {
        ("Enabled, not running".to_owned(), theme::text_secondary())
    } else {
        ("Off".to_owned(), theme::text_tertiary())
    };
    let action_label = if local_mcp.enabled {
        "Disable"
    } else {
        "Enable"
    };

    div()
        .id(ElementId::Name("local-mcp-bridge-card".into()))
        .flex()
        .items_center()
        .justify_between()
        .gap(px(14.0))
        .py(px(12.0))
        .px(px(14.0))
        .bg(theme::surface())
        .rounded(px(8.0))
        .border_l_2()
        .border_color(if local_mcp.running {
            theme::mic_accent()
        } else {
            theme::border()
        })
        .child(
            div()
                .flex()
                .flex_col()
                .gap(px(5.0))
                .min_w_0()
                .flex_grow()
                .child(
                    div()
                        .text_color(theme::text_primary())
                        .font_weight(FontWeight::MEDIUM)
                        .child("Local MCP Bridge"),
                )
                .child(
                    div()
                        .text_xs()
                        .text_color(theme::text_secondary())
                        .child(format!(
                            "IPC: {} · MCP command: {}",
                            local_mcp.addr, local_mcp.command_path
                        )),
                )
                .child(div().text_xs().text_color(status_color).child(status_text)),
        )
        .child(render_toolbar_button(
            "local-mcp-toggle",
            action_label,
            move |window, cx| on_toggle(window, cx),
        ))
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
    last_error: Option<&AppError>,
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

/// Construct a virtualized transcript list and scroll-follow flag.
pub fn new_transcript_list_state() -> (ListState, Rc<RefCell<bool>>) {
    let follow_transcript = Rc::new(RefCell::new(true));
    let follow_for_scroll = follow_transcript.clone();
    let list = ListState::new(0, ListAlignment::Top, px(100.));
    list.set_scroll_handler(move |event, _, _| {
        let at_bottom = event.visible_range.end >= event.count.saturating_sub(1);
        *follow_for_scroll.borrow_mut() = at_bottom;
    });
    (list, follow_transcript)
}
