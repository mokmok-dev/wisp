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
use gpui_component::button::{Button, ButtonCustomVariant, ButtonRounded, ButtonVariants};
use gpui_component::{Colorize, IconName, Sizable};
use wisp_audiokit::{Permission, PermissionStatus, SourceLabel};
use wisp_core::{Session as StoredSession, SessionId};

use crate::app::{AppError, AppModel, Permissions, Segment, SessionState, View};
use crate::permissions as perms;
use crate::title_input::{TitleInput, TitleInputState, TitleInputStyle};
use crate::transcript_export::{self, suggested_export_name};

/// Editing state for one active inline rename (library row or history header).
type RenamingState = (SessionId, Rc<RefCell<TitleInputState>>);

pub struct TranscriptView {
    pub app: gpui::Entity<AppModel>,
    pub on_toggle_record: std::sync::Arc<dyn Fn(&mut Window, &mut gpui::App) + 'static>,
    pub on_toggle_microphone_mute: std::sync::Arc<dyn Fn(&mut Window, &mut gpui::App) + 'static>,
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
    /// Persist a live-session title edited in the recording top bar.
    pub on_live_title: std::sync::Arc<dyn Fn(&str, &mut Window, &mut gpui::App) + 'static>,
    /// Persist an inline rename from the library or history header.
    pub on_rename_session:
        std::sync::Arc<dyn Fn(SessionId, &str, &mut Window, &mut gpui::App) + 'static>,
    /// Shared editing state for the always-visible live top-bar field.
    pub live_title_state: Rc<RefCell<Option<Rc<RefCell<TitleInputState>>>>>,
    /// Active inline rename (library row or history header), if any.
    pub renaming: Rc<RefCell<Option<RenamingState>>>,
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

        // Gate the main UI on having both required permissions. Until then,
        // we show an onboarding screen with per-permission rows the user
        // can act on. This avoids the previous failure mode where the user
        // presses Record and only then learns the app needs permissions
        // they may or may not be able to grant.
        if !app.setup_complete() {
            return self.render_onboarding(permissions).into_any_element();
        }

        let view = app.view.clone();
        let segment_count = app.segments.len();
        let text_len_sum: usize = app.segments.iter().map(|s| s.text.len()).sum();
        let active_idx = app.active_segment_index();
        let active_text_len = active_idx.map(|i| app.segments[i].text.len());
        let state = app.state;
        let microphone_muted = app.microphone_muted;
        let log_count = app.recent_log.len();
        let last_error = app.last_error.clone();
        let viewed_session = app.viewed_session.clone();
        let linked_session_id = app.linked_session_id;
        let library = app.library.clone();
        let model = self.app.clone();

        match view {
            View::Library => self.render_library(&library, cx).into_any_element(),
            View::LiveSession => {
                self.sync_transcript_list(&view, segment_count, active_idx, active_text_len);
                self.update_scroll_signature(segment_count, text_len_sum);
                let live_export_title = linked_session_id
                    .and_then(|id| library.iter().find(|s| s.id == id).map(|s| s.title.clone()));
                self.render_live_session(
                    LiveSessionSnapshot {
                        state,
                        microphone_muted,
                        model,
                        segment_count,
                        log_count,
                        last_error: last_error.as_ref(),
                        export_title: live_export_title.as_deref(),
                    },
                    cx,
                )
                .into_any_element()
            },
            View::History { .. } => {
                self.sync_transcript_list(&view, segment_count, active_idx, active_text_len);
                self.render_history(viewed_session.as_ref(), &model, segment_count, cx)
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
        snapshot: LiveSessionSnapshot<'_>,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let LiveSessionSnapshot {
            state,
            microphone_muted,
            model,
            segment_count,
            log_count,
            last_error,
            export_title,
        } = snapshot;
        let export_name = suggested_export_name(export_title, "transcript");
        let status = LiveStatus {
            state,
            microphone_muted,
            segment_count,
            log_count,
            last_error,
        };
        div()
            .flex()
            .flex_col()
            .size_full()
            .bg(theme::bg())
            .text_color(theme::text_primary())
            .child(self.render_live_top_bar(state, microphone_muted, &model, &export_name, cx))
            .child(render_transcript(
                self.transcript_list.clone(),
                &model,
                segment_count,
                self.cursor_visible,
            ))
            .child(render_status_bar(status))
    }

    fn render_history(
        &self,
        session: Option<&StoredSession>,
        model: &Entity<AppModel>,
        segment_count: usize,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let title = session.map_or_else(|| "Session".to_string(), |s| s.title.clone());
        let export_name = suggested_export_name(Some(&title), "transcript");
        let is_renaming = self
            .renaming
            .borrow()
            .as_ref()
            .is_some_and(|(rid, _)| session.is_some_and(|s| *rid == s.id));

        div()
            .flex()
            .flex_col()
            .size_full()
            .bg(theme::bg())
            .text_color(theme::text_primary())
            .child(self.render_history_top_bar(session, model, &export_name, is_renaming, cx))
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
        cx: &mut Context<Self>,
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
            .child(render_new_session_button(on_new, cx));

        let body = div()
            .flex()
            .flex_col()
            .flex_grow()
            .min_h_0()
            .child(render_session_list(
                sessions,
                self.on_open_history.clone(),
                self.renaming.clone(),
                self.on_rename_session.clone(),
                cx.entity_id(),
                cx,
            ));

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
        microphone_muted: bool,
        model: &Entity<AppModel>,
        export_name: &str,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let toggle = self.on_toggle_record.clone();
        let toggle_mute = self.on_toggle_microphone_mute.clone();
        let on_back = self.on_back_to_library.clone();
        let (has_unsettled_session, pending_persistence) = {
            let app = model.read(cx);
            (app.has_unsettled_session(), app.has_pending_persistence())
        };
        let mut leading = div()
            .flex()
            .flex_grow()
            .min_w_0()
            .items_center()
            .gap(px(12.0));
        if !has_unsettled_session {
            leading = leading.child(render_back_button("library-back-live", on_back, cx));
        }
        leading = leading.child(render_brand_compact());
        leading = leading.child(self.render_live_title_field(model, cx));
        let mut actions = div()
            .flex()
            .items_center()
            .gap(px(8.0))
            .child(render_transcript_actions(model, export_name, cx));
        if matches!(state, SessionState::Recording { .. }) {
            actions = actions.child(render_microphone_mute_button(
                microphone_muted,
                toggle_mute,
                cx,
            ));
        }
        actions = actions.child(render_record_button(state, pending_persistence, toggle, cx));

        div()
            .h(px(56.0))
            .flex()
            .items_center()
            .justify_between()
            .px(px(20.0))
            .border_b_1()
            .border_color(theme::border())
            .child(leading)
            .child(actions)
    }

    /// The always-editable session-name field in the live top bar. Seeded
    /// from `model.live_title`; edits flow back through `on_live_title`.
    fn render_live_title_field(
        &self,
        model: &Entity<AppModel>,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let state = self.ensure_live_title_state(model, cx);
        let field = TitleInput::new(
            ElementId::Name("title-input-live".into()),
            state,
            cx.entity_id(),
            "Name this session",
            live_input_style(),
            theme::text_primary().into(),
            theme::text_tertiary().into(),
            theme::text_primary().into(),
            selection_fill(),
            {
                let on_live = self.on_live_title.clone();
                move |text, window, cx| on_live(text, window, cx)
            },
            {
                let on_live = self.on_live_title.clone();
                move |text, window, cx| on_live(text, window, cx)
            },
            move |_window, _cx| {},
        );
        div().flex_grow().min_w_0().child(render_input_pill(field))
    }

    /// Create (or refresh from the model) the shared live-title editor state.
    ///
    /// While the IME is composing (marked text present) the editor owns the
    /// display and the model holds the last committed value, so the sync is
    /// skipped to avoid wiping the in-progress composition.
    fn ensure_live_title_state(
        &self,
        model: &Entity<AppModel>,
        cx: &mut Context<Self>,
    ) -> Rc<RefCell<TitleInputState>> {
        let mut slot = self.live_title_state.borrow_mut();
        let current = model.read(cx).live_title.clone();
        if let Some(state) = slot.as_ref() {
            let mut inner = state.borrow_mut();
            if inner.editor.marked().is_none() && inner.editor.text() != current {
                inner.set_text(&current);
            }
            state.clone()
        } else {
            let state = Rc::new(RefCell::new(TitleInputState::new(cx, &current)));
            *slot = Some(state.clone());
            state
        }
    }

    fn render_history_top_bar(
        &self,
        session: Option<&StoredSession>,
        model: &Entity<AppModel>,
        export_name: &str,
        is_renaming: bool,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let on_back = self.on_back_to_library.clone();
        let mut title_block = div().flex().flex_col().gap(px(2.0)).min_w_0();
        if is_renaming {
            // Keep the pill from swallowing the whole bar width.
            let pill = div()
                .flex()
                .flex_none()
                .child(render_input_pill(self.render_rename_input(session, cx)));
            title_block = title_block.child(pill);
        } else {
            let title = session.map_or_else(|| "Session".to_string(), |s| s.title.clone());
            title_block = title_block.child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(8.0))
                    .child(
                        div()
                            .text_color(theme::text_primary())
                            .font_weight(FontWeight::SEMIBOLD)
                            .child(title),
                    )
                    .child(self.render_rename_button(session, cx)),
            );
            if let Some(session) = session {
                let subtitle = history_subtitle(session);
                title_block = title_block.child(
                    div()
                        .text_xs()
                        .text_color(theme::text_tertiary())
                        .child(subtitle),
                );
            }
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
                    .min_w_0()
                    .child(render_back_button("library-back-history", on_back, cx))
                    .child(title_block),
            )
            .child(render_transcript_actions(model, export_name, cx))
    }

    /// Small "Rename" button next to a history title.
    fn render_rename_button(
        &self,
        session: Option<&StoredSession>,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let Some(session) = session else {
            return div().into_any_element();
        };
        let session_id = session.id;
        let renaming = self.renaming.clone();
        let view_entity_id = cx.entity_id();
        let title = session.title.clone();
        render_toolbar_button(
            "history-rename",
            "Rename",
            {
                let renaming = renaming.clone();
                move |_window, cx| {
                    start_rename(&renaming, session_id, &title, view_entity_id, cx);
                }
            },
            cx,
        )
        .into_any_element()
    }

    /// The active inline rename input for a library row or history header.
    /// Only called while a rename for `session` id is in flight.
    fn render_rename_input(
        &self,
        session: Option<&StoredSession>,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let renaming = self.renaming.clone();
        let view_entity_id = cx.entity_id();
        let Some((session_id, state)) = session.map(|s| s.id).and_then(|id| {
            renaming
                .borrow()
                .as_ref()
                .map(|(_, state)| (id, state.clone()))
        }) else {
            let placeholder = Rc::new(RefCell::new(TitleInputState::new(cx, "")));
            return TitleInput::new(
                ElementId::Name("title-input-rename-empty".into()),
                placeholder,
                view_entity_id,
                "Session name",
                rename_input_style(),
                theme::text_primary().into(),
                theme::text_tertiary().into(),
                theme::text_primary().into(),
                selection_fill(),
                |_text, _window, _cx| {},
                |_text, _window, _cx| {},
                |_window, _cx| {},
            );
        };
        let on_rename = self.on_rename_session.clone();
        rename_title_input(
            ElementId::Name(format!("title-input-rename-{}", session_id.as_i64()).into()),
            state,
            view_entity_id,
            session_id,
            on_rename,
            renaming,
        )
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

        div()
            .flex()
            .flex_col()
            .items_center()
            .justify_center()
            .size_full()
            .bg(theme::bg())
            .text_color(theme::text_primary())
            .child(
                div()
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
                    .child(div().text_xs().text_color(theme::text_secondary()).child(
                        "These run entirely on-device. Wisp doesn't send your audio anywhere.",
                    ))
                    .child(row_mic)
                    .child(row_speech),
            )
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

/// A `ButtonCustomVariant` matching the app's flat pill buttons: the given
/// fill with the standard light foreground, plus lighten/darken hover and
/// active states derived from the fill.
fn button_variant(
    cx: &App,
    fill: gpui::Rgba,
) -> ButtonCustomVariant {
    let fill: gpui::Hsla = fill.into();
    ButtonCustomVariant::new(cx)
        .color(fill)
        .foreground(theme::text_primary().into())
        .border(theme::border().into())
        .hover(fill.lighten(0.12))
        .active(fill.darken(0.08))
}

/// A `Button` using the app's flat pill style: custom fill and a fully
/// rounded ("pill") corner radius, matching the original hand-rolled buttons.
fn pill_button(
    id: impl Into<ElementId>,
    cx: &App,
    fill: gpui::Rgba,
) -> Button {
    Button::new(id)
        .custom(button_variant(cx, fill))
        .rounded(ButtonRounded::Size(px(999.0)))
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

/// Brand mark without the tagline — used in the live top bar next to the
/// session-name field so it reads as a compact header.
fn render_brand_compact() -> impl IntoElement {
    div()
        .flex()
        .items_center()
        .gap_2()
        .flex_none()
        .child(div().size(px(8.0)).rounded_full().bg(theme::mic_accent()))
        .child(
            div()
                .text_color(theme::text_primary())
                .font_weight(FontWeight::SEMIBOLD)
                .child("Wisp"),
        )
}

fn render_record_button(
    state: SessionState,
    pending_persistence: bool,
    on_click: std::sync::Arc<dyn Fn(&mut Window, &mut gpui::App) + 'static>,
    cx: &App,
) -> impl IntoElement {
    let (label, fill, dot_color) = if pending_persistence {
        ("Retry Save", theme::record_idle(), theme::system_accent())
    } else {
        match state {
            SessionState::Idle | SessionState::Failed => {
                ("Record", theme::record_idle(), theme::record_red())
            },
            SessionState::Recording { .. } => ("Stop", theme::record_red(), rgb(0xff_ffff)),
            SessionState::Starting => ("Starting…", theme::record_idle(), theme::text_tertiary()),
            SessionState::Stopping => ("Stopping…", theme::record_idle(), theme::text_tertiary()),
        }
    };
    let interactive = matches!(
        state,
        SessionState::Idle | SessionState::Recording { .. } | SessionState::Failed
    );
    let mut button = pill_button("record-button", cx, fill)
        .label(label)
        .child(div().size(px(8.0)).rounded_full().bg(dot_color));
    if interactive {
        button = button.on_click(move |_event, window, cx| on_click(window, cx));
    }
    button
}

fn render_microphone_mute_button(
    muted: bool,
    on_click: std::sync::Arc<dyn Fn(&mut Window, &mut gpui::App) + 'static>,
    cx: &App,
) -> impl IntoElement {
    let (label, dot_color) = if muted {
        ("Unmute mic", theme::record_red())
    } else {
        ("Mute mic", theme::mic_accent())
    };
    pill_button("microphone-mute-button", cx, theme::record_idle())
        .label(label)
        .child(div().size(px(8.0)).rounded_full().bg(dot_color))
        .on_click(move |_event, window, cx| on_click(window, cx))
}

fn render_transcript(
    list_state: ListState,
    model: &Entity<AppModel>,
    segment_count: usize,
    cursor_visible: bool,
) -> impl IntoElement {
    let mut container = div()
        .debug_selector(|| "transcript-scroll".into())
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
        .debug_selector(move || format!("segment-row-{index}"))
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
    on_click: std::sync::Arc<dyn Fn(&mut Window, &mut gpui::App) + 'static>,
    cx: &App,
) -> impl IntoElement {
    pill_button("new-session-button", cx, theme::record_idle())
        .icon(IconName::Plus)
        .label("New Session")
        .on_click(move |_event, window, cx| on_click(window, cx))
}

fn render_transcript_actions(
    model: &Entity<AppModel>,
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
            cx,
        ))
        .child(render_toolbar_button(
            "transcript-export",
            "Export",
            {
                let export_name = export_name.clone();
                let model = (*model).clone();
                move |_window, cx| {
                    let app = model.read(cx);
                    let text = transcript_export::format_transcript_markdown(
                        app.viewed_session.as_ref(),
                        &app.segments,
                    );
                    transcript_export::export_transcript(text, &export_name, cx);
                }
            },
            cx,
        ))
        .into_any_element()
}

fn render_toolbar_button(
    id: &'static str,
    label: &'static str,
    on_click: impl Fn(&mut Window, &mut gpui::App) + 'static,
    cx: &App,
) -> impl IntoElement {
    pill_button(id, cx, theme::record_idle())
        .small()
        .label(label)
        .on_click(move |_event, window, cx| on_click(window, cx))
}

fn render_back_button(
    id: &'static str,
    on_click: std::sync::Arc<dyn Fn(&mut Window, &mut gpui::App) + 'static>,
    cx: &App,
) -> impl IntoElement {
    pill_button(id, cx, theme::record_idle())
        .small()
        .icon(IconName::ArrowLeft)
        .label("Library")
        .on_click(move |_event, window, cx| on_click(window, cx))
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
    renaming: Rc<RefCell<Option<RenamingState>>>,
    on_rename: std::sync::Arc<dyn Fn(SessionId, &str, &mut Window, &mut gpui::App) + 'static>,
    view_entity_id: gpui::EntityId,
    cx: &App,
) -> impl IntoElement {
    let mut list = div()
        .debug_selector(|| "library-scroll".into())
        .id(ElementId::Name("library-scroll".into()))
        .flex()
        .flex_col()
        .flex_grow()
        .overflow_y_scroll()
        .px(px(20.0))
        .py(px(16.0))
        .gap(px(8.0));
    if sessions.is_empty() {
        return list.child(render_empty_library());
    }
    for s in sessions {
        list = list.child(render_session_row(
            s,
            on_open.clone(),
            renaming.clone(),
            on_rename.clone(),
            view_entity_id,
            cx,
        ));
    }
    list
}

#[allow(clippy::needless_pass_by_value)]
#[allow(clippy::too_many_arguments)]
fn render_session_row(
    session: &StoredSession,
    on_open: std::sync::Arc<dyn Fn(SessionId, &mut Window, &mut gpui::App) + 'static>,
    renaming: Rc<RefCell<Option<RenamingState>>>,
    on_rename: std::sync::Arc<dyn Fn(SessionId, &str, &mut Window, &mut gpui::App) + 'static>,
    view_entity_id: gpui::EntityId,
    cx: &App,
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

    let renaming_state = renaming
        .borrow()
        .as_ref()
        .and_then(|(rid, state)| (*rid == id).then(|| state.clone()));

    let mut row = div()
        .debug_selector(move || format!("session-row-{}", id.as_i64()))
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
        .border_color(theme::mic_accent());

    if let Some(state) = renaming_state {
        let input = rename_title_input(
            ElementId::Name(format!("title-input-rename-{}", id.as_i64()).into()),
            state,
            view_entity_id,
            id,
            on_rename,
            renaming,
        );
        row = row.child(render_input_pill(input));
    } else {
        let rename_button = render_toolbar_button(
            "session-rename",
            "Rename",
            {
                let renaming = renaming.clone();
                let title = session.title.clone();
                move |_window, cx| {
                    cx.stop_propagation();
                    start_rename(&renaming, id, &title, view_entity_id, cx);
                }
            },
            cx,
        );
        row = row
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
            .child(rename_button)
            .child(
                div()
                    .text_xs()
                    .text_color(theme::text_secondary())
                    .child(duration_text),
            )
            .cursor_pointer()
            .on_click(move |_event, window, cx| {
                on_open(id, window, cx);
            });
    }

    row
}

/// Wraps a [`TitleInput`] in the pill-style field background shared by the
/// live top bar, library rows, and the history header.
fn render_input_pill(input: impl IntoElement) -> impl IntoElement {
    div()
        .min_w_0()
        .h(px(32.0))
        .flex()
        .items_center()
        .bg(theme::surface())
        .rounded(px(8.0))
        .border_1()
        .border_color(theme::border())
        .px(px(4.0))
        .child(input)
}

/// Style for the always-flexible live-session title field.
fn live_input_style() -> TitleInputStyle {
    TitleInputStyle {
        fill: true,
        min_width: px(160.0),
        max_width: Some(px(420.0)),
        pad_x: px(10.0),
        pad_y: px(5.0),
        font_size: px(15.0),
    }
}

/// Style for a compact inline rename field.
fn rename_input_style() -> TitleInputStyle {
    TitleInputStyle::default()
}

/// Enter rename mode for `session_id`, seeding the editor with `title` and
/// focusing the fresh field on its first paint.
fn start_rename(
    renaming: &Rc<RefCell<Option<RenamingState>>>,
    session_id: SessionId,
    title: &str,
    view_entity_id: gpui::EntityId,
    cx: &mut gpui::App,
) {
    let state = Rc::new(RefCell::new(TitleInputState::new(cx, title)));
    state.borrow_mut().request_focus = true;
    *renaming.borrow_mut() = Some((session_id, state));
    cx.notify(view_entity_id);
}

/// Build the shared rename input element. Enter/blur persists via `on_rename`
/// and closes the rename; Escape closes it without saving.
fn rename_title_input(
    element_id: ElementId,
    state: Rc<RefCell<TitleInputState>>,
    view_entity_id: gpui::EntityId,
    session_id: SessionId,
    on_rename: std::sync::Arc<dyn Fn(SessionId, &str, &mut Window, &mut gpui::App) + 'static>,
    renaming: Rc<RefCell<Option<RenamingState>>>,
) -> TitleInput {
    let renaming_commit = renaming.clone();
    TitleInput::new(
        element_id,
        state,
        view_entity_id,
        "Session name",
        rename_input_style(),
        theme::text_primary().into(),
        theme::text_tertiary().into(),
        theme::text_primary().into(),
        selection_fill(),
        |_text, _window, _cx| {},
        {
            move |text, window, cx| {
                renaming_commit.borrow_mut().take();
                on_rename(session_id, text, window, cx);
            }
        },
        move |_window, cx| {
            renaming.borrow_mut().take();
            cx.notify(view_entity_id);
        },
    )
}

/// Selection highlight fill: the brand accent at low alpha, semi-transparent
/// so the text underneath stays readable.
fn selection_fill() -> gpui::Hsla {
    let mut accent: gpui::Hsla = theme::mic_accent().into();
    accent.fade_out(0.62);
    accent
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

/// Snapshot of everything needed to render the live-session view, so
/// `render_live_session` stays under clippy's argument limit.
struct LiveSessionSnapshot<'a> {
    state: SessionState,
    microphone_muted: bool,
    model: Entity<AppModel>,
    segment_count: usize,
    log_count: usize,
    last_error: Option<&'a AppError>,
    export_title: Option<&'a str>,
}

/// Snapshot of the status-bar display state, bundled so both the view render
/// and `render_status_bar` stay under clippy's argument limit.
#[derive(Clone, Copy)]
struct LiveStatus<'a> {
    state: SessionState,
    microphone_muted: bool,
    segment_count: usize,
    log_count: usize,
    last_error: Option<&'a AppError>,
}

fn render_status_bar(status: LiveStatus<'_>) -> impl IntoElement {
    let LiveStatus {
        state,
        microphone_muted,
        segment_count,
        log_count,
        last_error,
    } = status;
    let (dot, status_text) = match state {
        SessionState::Idle => (theme::record_idle(), "Idle".to_string()),
        SessionState::Starting => (theme::text_tertiary(), "Starting…".to_string()),
        SessionState::Recording { started_at } => {
            let secs = started_at.elapsed().as_secs();
            let mic_status = if microphone_muted {
                " · Mic muted"
            } else {
                ""
            };
            (
                theme::record_red(),
                format!("Recording{mic_status} · {:02}:{:02}", secs / 60, secs % 60),
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

#[cfg(test)]
mod layout_tests {
    use super::*;
    use gpui::{AppContext, TestAppContext};
    use wisp_core::SessionId;

    fn test_session(id: i64) -> StoredSession {
        let now = chrono::Utc::now();
        StoredSession {
            id: SessionId::from(id),
            started_at: now,
            ended_at: Some(now),
            title: format!("session {id}"),
            mic_wav_path: format!("session-{id}/mic.ogg"),
            system_wav_path: format!("session-{id}/system.ogg"),
            notes: String::new(),
        }
    }

    fn test_segment(text: &str) -> Segment {
        Segment {
            source: SourceLabel::Mic,
            id: 0,
            text: text.into(),
            display_text: text.into(),
            start_seconds: 0.0,
            end_seconds: 1.0,
            is_final: true,
        }
    }

    fn noop_id() -> std::sync::Arc<dyn Fn(SessionId, &mut Window, &mut gpui::App) + 'static> {
        std::sync::Arc::new(|_, _, _| {})
    }

    fn noop_rename()
    -> std::sync::Arc<dyn Fn(SessionId, &str, &mut Window, &mut gpui::App) + 'static> {
        std::sync::Arc::new(|_, _, _, _| {})
    }

    /// Minimal view that renders the session list (library page content area).
    struct LibraryTestView {
        sessions: Vec<StoredSession>,
    }

    impl Render for LibraryTestView {
        fn render(
            &mut self,
            _: &mut Window,
            cx: &mut Context<Self>,
        ) -> impl IntoElement {
            let body = div()
                .flex()
                .flex_col()
                .flex_grow()
                .min_h_0()
                .child(render_session_list(
                    &self.sessions,
                    noop_id(),
                    Rc::new(RefCell::new(None)),
                    noop_rename(),
                    cx.entity_id(),
                    cx,
                ));
            div()
                .flex()
                .flex_col()
                .size_full()
                .child(div().h(px(56.0)))
                .child(body)
                .child(div().h(px(32.0)))
        }
    }

    /// Minimal view that renders the transcript list (history page content area).
    struct HistoryTestView {
        model: Entity<AppModel>,
        list: ListState,
        count: usize,
    }

    impl Render for HistoryTestView {
        fn render(
            &mut self,
            _: &mut Window,
            _cx: &mut Context<Self>,
        ) -> impl IntoElement {
            div()
                .flex()
                .flex_col()
                .size_full()
                .child(div().h(px(56.0)))
                .child(render_transcript(
                    self.list.clone(),
                    &self.model,
                    self.count,
                    false,
                ))
                .child(div().h(px(32.0)))
        }
    }

    #[gpui::test]
    fn library_and_history_content_tops_align(cx: &mut TestAppContext) {
        cx.update(gpui_component::init);
        let model = cx.new(|_| AppModel::new());
        cx.update(|cx| {
            model.update(cx, |m, _| {
                m.segments = vec![test_segment("こんにちは")];
            });
        });

        let size = gpui::size(px(900.0), px(640.0));
        let origin = gpui::point(px(0.0), px(0.0));

        // History / transcript page content.
        let history_cx = cx.add_empty_window();
        history_cx.draw(origin, size, |_, cx| {
            cx.new(|_| HistoryTestView {
                model: model.clone(),
                list: ListState::new(1, ListAlignment::Top, px(100.0)),
                count: 1,
            })
            .into_any_element()
        });
        let history_first = history_cx.debug_bounds("segment-row-0");
        let history_cont = history_cx.debug_bounds("transcript-scroll");

        // Library page content.
        let library_cx = cx.add_empty_window();
        library_cx.draw(origin, size, |_, cx| {
            cx.new(|_| LibraryTestView {
                sessions: vec![test_session(7)],
            })
            .into_any_element()
        });
        let library_first = library_cx.debug_bounds("session-row-7");
        let library_cont = library_cx.debug_bounds("library-scroll");

        let history_first = history_first.expect("history first row should be laid out");
        let library_first = library_first.expect("library first row should be laid out");
        let history_cont = history_cont.expect("history container should be laid out");
        let library_cont = library_cont.expect("library container should be laid out");

        assert_eq!(
            library_cont.origin.y, history_cont.origin.y,
            "scroll containers should start at the same y"
        );
        assert_eq!(
            library_first.origin.y, history_first.origin.y,
            "first content rows should start at the same y"
        );
        assert_eq!(
            library_first.origin.y,
            library_cont.origin.y + px(16.0),
            "library should have 16px top padding"
        );
    }
}
