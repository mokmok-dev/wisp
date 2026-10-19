//! The native application view.
//!
//! `AppView` is the window's first-level view (wrapped in a `Root` by
//! `gpui_kit::open_window`, which hosts dialogs, sheets, and notifications).
//! It renders the custom title bar, the collapsible sidebar, and one of the
//! onboarding, library, live-session, or history screens.
//!
//! The view owns no recording state of its own: `AppModel` remains the single
//! source of truth, and the buttons here call the same host actions the web
//! UI used to send over IPC.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use gpui::{
    App, AppContext as _, Context, Entity, InteractiveElement as _, IntoElement,
    ParentElement as _, Render, StatefulInteractiveElement as _, Styled as _, Window, div,
    prelude::FluentBuilder as _, px,
};
use gpui_kit::ScrollHandle;
use gpui_kit::assets::IconName as AppIcon;
use gpui_kit::component::{
    ActiveTheme as _, Icon, Sizable as _, TitleBar, WindowExt as _,
    button::{Button, ButtonVariants as _},
    h_flex,
    input::{InputEvent, InputState},
    notification::Notification,
    sidebar::{
        Sidebar, SidebarCollapsible, SidebarFooter, SidebarGroup, SidebarHeader, SidebarMenu,
        SidebarMenuItem, SidebarToggleButton,
    },
    tag::Tag,
    v_flex,
};
use wisp_audiokit::{Permission, PermissionStatus};
use wisp_core::SessionId;

use crate::app::{AppModel, Permissions, SessionState, View};
use crate::library::SharedStorage;
use crate::permissions;
use crate::session_runner::SessionRunner;
use crate::settings;
use crate::transcript_export::{self, suggested_export_name};
use crate::{history_view, live_view, transcript_view};

/// How often the live-session clock repaints while a recording runs.
const CLOCK_INTERVAL: Duration = Duration::from_millis(250);

/// Host services the native views need. `main` builds one of these after
/// opening storage, spawning the session runner, and populating the model.
pub(crate) struct AppDeps {
    pub model: Entity<AppModel>,
    pub runner: Arc<SessionRunner>,
    pub storage: SharedStorage,
    pub recordings_dir: PathBuf,
}

/// Root view for the Wisp window.
pub(crate) struct AppView {
    pub(crate) model: Entity<AppModel>,
    pub(crate) runner: Arc<SessionRunner>,
    pub(crate) storage: SharedStorage,
    pub(crate) recordings_dir: PathBuf,
    /// Live-session title field. Kept in sync with `AppModel::live_title`.
    pub(crate) live_title_input: Entity<InputState>,
    /// History rename field. Committed on Enter or blur.
    pub(crate) history_title_input: Entity<InputState>,
    /// Scroll position of the transcript list, shared by live and history.
    pub(crate) transcript_scroll: ScrollHandle,
    /// Sidebar collapse state.
    sidebar_collapsed: bool,
    /// Last view the title inputs were synchronised with.
    synced_view: Option<u8>,
    /// Last live title written into the input, so observer syncs do not fight
    /// the user's IME composition.
    synced_live_title: String,
    /// Last history title written into the input.
    synced_history_title: String,
}

impl AppView {
    pub(crate) fn new(
        window: &mut Window,
        cx: &mut Context<Self>,
        deps: AppDeps,
    ) -> Self {
        let AppDeps {
            model,
            runner,
            storage,
            recordings_dir,
        } = deps;

        // Apply the persisted appearance (or the window's current system
        // appearance) before the first paint, so the window never flashes the
        // wrong theme.
        settings::apply_current(Some(&mut *window), cx);

        let live_title_input =
            cx.new(|cx| InputState::new(window, cx).placeholder("Untitled session"));
        let history_title_input =
            cx.new(|cx| InputState::new(window, cx).placeholder("Untitled session"));
        let transcript_scroll = ScrollHandle::new();

        // Keep the model's tree in sync when either title field changes. The
        // model persists edits immediately, exactly like the former web UI's
        // per-keystroke `setLiveTitle` / blur-time `renameSession` commands.
        cx.subscribe_in(
            &live_title_input,
            window,
            |this: &mut Self, input, event: &InputEvent, _window, cx| {
                if matches!(event, InputEvent::Change) {
                    let title = input.read(cx).value().to_string();
                    // Remember what the field already shows so the model
                    // observer does not rewrite it (and reset the caret or an
                    // in-flight IME composition) after every keystroke.
                    this.synced_live_title.clone_from(&title);
                    this.set_live_title(&title, cx);
                }
            },
        )
        .detach();

        cx.subscribe_in(
            &history_title_input,
            window,
            |this: &mut Self, _input, event: &InputEvent, window, cx| match event {
                InputEvent::PressEnter { .. } | InputEvent::Blur => {
                    this.commit_history_rename(window, cx);
                },
                InputEvent::Change | InputEvent::Focus => {},
            },
        )
        .detach();

        cx.observe_in(&model, window, |this, model, window, cx| {
            this.sync_inputs(&model, window, cx);
            cx.notify();
        })
        .detach();

        // While the preference is `System`, follow the OS when macOS switches
        // between light and dark. Pinned preferences ignore this event.
        window
            .observe_window_appearance(|window, cx| {
                if settings::follows_system(cx) {
                    settings::apply_current(Some(window), cx);
                }
            })
            .detach();

        // Repaint the elapsed clock while a recording is running. The model
        // only notifies on state changes, so without this the timer would
        // freeze between transcript updates.
        cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(CLOCK_INTERVAL).await;
                let Ok(()) = this.update(cx, |this, cx| {
                    if matches!(this.model.read(cx).state, SessionState::Recording { .. }) {
                        cx.notify();
                    }
                }) else {
                    break;
                };
            }
        })
        .detach();

        // Seed the title fields from the model before the first paint so a
        // window restored into a session (or the live view) never shows a
        // stale placeholder until the next model notification.
        let (initial_view, live_title, history_title) = {
            let app = model.read(cx);
            (
                view_key(&app.view),
                app.live_title.clone(),
                app.viewed_session
                    .as_ref()
                    .map_or_else(String::new, |session| session.title.clone()),
            )
        };
        Self::set_input_value(&live_title_input, &live_title, window, cx);
        Self::set_input_value(&history_title_input, &history_title, window, cx);

        Self {
            model,
            runner,
            storage,
            recordings_dir,
            live_title_input,
            history_title_input,
            transcript_scroll,
            sidebar_collapsed: false,
            synced_view: Some(initial_view),
            synced_live_title: live_title,
            synced_history_title: history_title,
        }
    }

    // ------------------------------------------------------------------
    // Host actions (called from the native controls and the app menu)
    // ------------------------------------------------------------------

    pub(crate) fn toggle_record(
        &mut self,
        cx: &mut Context<Self>,
    ) {
        crate::toggle_recording(
            &self.runner,
            &self.model,
            &self.storage,
            &self.recordings_dir,
            cx,
        );
    }

    pub(crate) fn toggle_mute(
        &mut self,
        cx: &mut Context<Self>,
    ) {
        let muted = !self.model.read(cx).microphone_muted;
        if self.runner.set_microphone_muted(muted) {
            self.model.update(cx, |model, cx| {
                if matches!(model.state, SessionState::Recording { .. }) {
                    model.microphone_muted = muted;
                    cx.notify();
                }
            });
        }
    }

    pub(crate) fn new_session(
        &mut self,
        cx: &mut Context<Self>,
    ) {
        self.model.update(cx, |model, cx| {
            model.show_new_session();
            cx.notify();
        });
    }

    pub(crate) fn show_library(
        &mut self,
        cx: &mut Context<Self>,
    ) {
        self.model.update(cx, |model, cx| {
            model.show_library();
            cx.notify();
        });
    }

    pub(crate) fn open_history(
        &mut self,
        session_id: SessionId,
        cx: &mut Context<Self>,
    ) {
        crate::open_history(&self.storage, &self.model, session_id, cx);
    }

    pub(crate) fn set_live_title(
        &mut self,
        title: &str,
        cx: &mut Context<Self>,
    ) {
        crate::change_live_title(&self.model, &self.storage, cx, title);
    }

    pub(crate) fn commit_history_rename(
        &mut self,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let session_id = match self.model.read(cx).view {
            View::History { session_id } => session_id,
            View::Library | View::LiveSession => return,
        };
        let title = self.history_title_input.read(cx).value().to_string();
        crate::rename_session(&self.model, &self.storage, session_id, &title, cx);
        self.synced_history_title.clone_from(&title);
    }

    pub(crate) fn copy_transcript(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let copied = {
            let app = self.model.read(cx);
            if !matches!(app.view, View::LiveSession | View::History { .. }) {
                return;
            }
            transcript_export::copy_transcript_to_clipboard(&app.segments, cx)
        };
        let notification = if copied {
            Notification::success("Transcript copied to the clipboard.")
        } else {
            Notification::error("There is no transcript to copy yet.")
        };
        window.push_notification(notification, cx);
    }

    pub(crate) fn export_transcript(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let (text, name) = {
            let app = self.model.read(cx);
            if !matches!(app.view, View::LiveSession | View::History { .. }) {
                return;
            }
            let session = app.viewed_session.as_ref();
            (
                transcript_export::format_transcript_markdown(session, &app.segments),
                suggested_export_name(session.map(|s| s.title.as_str()), "transcript"),
            )
        };
        transcript_export::export_transcript(text, &name, cx);
        window.push_notification(
            Notification::success("Export started — choose a location in the save dialog."),
            cx,
        );
    }

    pub(crate) fn request_permission(
        &mut self,
        permission: Permission,
        cx: &mut Context<Self>,
    ) {
        permissions::request(permission, self.model.clone(), cx);
    }

    pub(crate) fn open_permission_settings(permission: Permission) {
        permissions::open_settings(permission);
    }

    /// Copy model titles into the title inputs when the screen or its data
    /// changes. A no-op for values already on screen, so typing never gets
    /// reset mid-composition.
    fn sync_inputs(
        &mut self,
        model: &Entity<AppModel>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let (current_view, live_title, history_title) = {
            let app = model.read(cx);
            let history_title = app
                .viewed_session
                .as_ref()
                .map_or_else(String::new, |session| session.title.clone());
            (view_key(&app.view), app.live_title.clone(), history_title)
        };

        if self.synced_view != Some(current_view) {
            self.synced_view = Some(current_view);
            Self::set_input_value(&self.live_title_input, &live_title, window, cx);
            Self::set_input_value(&self.history_title_input, &history_title, window, cx);
            self.synced_live_title.clone_from(&live_title);
            self.synced_history_title.clone_from(&history_title);
            return;
        }

        if live_title != self.synced_live_title {
            Self::set_input_value(&self.live_title_input, &live_title, window, cx);
            self.synced_live_title = live_title;
        }
        if history_title != self.synced_history_title {
            Self::set_input_value(&self.history_title_input, &history_title, window, cx);
            self.synced_history_title = history_title;
        }
    }

    fn set_input_value(
        input: &Entity<InputState>,
        value: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        input.update(cx, |state, cx| state.set_value(value, window, cx));
    }

    // ------------------------------------------------------------------
    // Rendering
    // ------------------------------------------------------------------

    fn render_title_bar(
        &self,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let app = self.model.read(cx);
        let recording = app.state.is_active();
        let pending = app.has_pending_persistence();
        let dark = cx.theme().is_dark();
        let (theme_icon, theme_tooltip) = if dark {
            (AppIcon::Sun, "Switch to light appearance (⌘⇧D)")
        } else {
            (AppIcon::Moon, "Switch to dark appearance (⌘⇧D)")
        };

        TitleBar::new()
            .child(
                h_flex()
                    .gap_2()
                    .items_center()
                    .px_3()
                    .child(
                        div()
                            .text_sm()
                            .font_weight(gpui::FontWeight::SEMIBOLD)
                            .child("Wisp"),
                    )
                    .child(if pending {
                        Tag::warning()
                            .small()
                            .rounded_full()
                            .child(div().text_xs().child("Unsaved"))
                    } else if recording {
                        Tag::danger()
                            .small()
                            .rounded_full()
                            .child(div().text_xs().child("REC"))
                    } else {
                        Tag::secondary()
                            .small()
                            .rounded_full()
                            .child(div().text_xs().child("idle"))
                    }),
            )
            .child(
                h_flex().items_center().px_3().child(
                    Button::new("theme-toggle")
                        .ghost()
                        .small()
                        .icon(theme_icon)
                        .tooltip(theme_tooltip)
                        .on_click(cx.listener(|_this, _, _window, cx| settings::toggle(cx))),
                ),
            )
    }

    fn render_sidebar(
        &self,
        active_view: &View,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let live = matches!(active_view, View::LiveSession);
        let collapsed = self.sidebar_collapsed;

        Sidebar::new("wisp-sidebar")
            .collapsible(SidebarCollapsible::Icon)
            .collapsed(collapsed)
            .header(
                SidebarHeader::new()
                    .child(
                        div()
                            .text_sm()
                            .font_weight(gpui::FontWeight::SEMIBOLD)
                            .child("Wisp"),
                    )
                    .child(
                        div()
                            .text_xs()
                            .text_color(cx.theme().muted_foreground)
                            .child("on-device"),
                    ),
            )
            .child(
                SidebarGroup::new("Menu").child(
                    SidebarMenu::new()
                        .child(
                            SidebarMenuItem::new("New Session")
                                .icon(AppIcon::Mic)
                                .active(live)
                                .on_click(cx.listener(|this, _, _, cx| this.new_session(cx))),
                        )
                        .child(
                            SidebarMenuItem::new("Session History")
                                .icon(AppIcon::RotateCcw)
                                .active(!live)
                                .on_click(cx.listener(|this, _, _, cx| this.show_library(cx))),
                        ),
                ),
            )
            .footer(
                SidebarFooter::new()
                    .child(
                        SidebarToggleButton::new()
                            .collapsed(collapsed)
                            .accessibility_label("Toggle sidebar")
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.sidebar_collapsed = !this.sidebar_collapsed;
                                cx.notify();
                            })),
                    )
                    .when(!collapsed, |this| {
                        this.child(
                            div()
                                .text_xs()
                                .text_color(cx.theme().muted_foreground)
                                .child("On-device · private by default"),
                        )
                    }),
            )
    }

    fn render_onboarding(
        &self,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let permissions = self.model.read(cx).permissions;

        v_flex()
            .flex_1()
            .w_full()
            .items_center()
            .justify_center()
            .p_8()
            .child(
                v_flex()
                    .w_full()
                    .max_w(px(620.0))
                    .gap_6()
                    .child(
                        v_flex()
                            .gap_1()
                            .child(
                                div()
                                    .text_xl()
                                    .font_weight(gpui::FontWeight::SEMIBOLD)
                                    .child("Welcome to Wisp"),
                            )
                            .child(
                                div()
                                    .text_sm()
                                    .text_color(cx.theme().muted_foreground)
                                    .child(
                                        "Wisp records your microphone and system audio at the same \
                                         time and transcribes both on-device. Nothing ever leaves \
                                         your Mac.",
                                    ),
                            ),
                    )
                    .child(
                        v_flex()
                            .gap_3()
                            .child(Self::render_permission_row(
                                Permission::Microphone,
                                AppIcon::Mic,
                                "Microphone",
                                "Capture your voice for on-device transcription.",
                                permissions,
                                cx,
                            ))
                            .child(Self::render_permission_row(
                                Permission::SpeechRecognition,
                                AppIcon::AudioLines,
                                "Speech Recognition",
                                "Run Apple's on-device speech model on captured audio.",
                                permissions,
                                cx,
                            )),
                    )
                    .child(
                        div()
                            .text_sm()
                            .text_color(cx.theme().muted_foreground)
                            .child(
                                "You can change these later in System Settings → Privacy & \
                                 Security. This screen updates automatically once permissions \
                                 are granted.",
                            ),
                    ),
            )
    }

    fn render_permission_row(
        permission: Permission,
        icon: AppIcon,
        title: &'static str,
        rationale: &'static str,
        permissions: Permissions,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let status = match permission {
            Permission::Microphone => permissions.microphone,
            Permission::SpeechRecognition => permissions.speech,
        };
        let pending = permissions.pending == Some(permission);

        let status_tag = if pending {
            Tag::info()
                .small()
                .rounded_full()
                .child(div().text_xs().child("Waiting…"))
        } else {
            match status {
                PermissionStatus::Granted => Tag::success()
                    .small()
                    .rounded_full()
                    .child(div().text_xs().child("Granted")),
                PermissionStatus::Denied => Tag::danger()
                    .small()
                    .rounded_full()
                    .child(div().text_xs().child("Denied")),
                PermissionStatus::Restricted => Tag::warning()
                    .small()
                    .rounded_full()
                    .child(div().text_xs().child("Restricted")),
                PermissionStatus::Undetermined => Tag::secondary()
                    .small()
                    .rounded_full()
                    .child(div().text_xs().child("Not requested")),
            }
        };

        let action = match status {
            PermissionStatus::Granted => None,
            PermissionStatus::Denied | PermissionStatus::Restricted => Some(
                Button::new(format!("open-settings-{}", permission_label(permission)))
                    .secondary()
                    .label("Open Settings")
                    .on_click(move |_, _, _| {
                        AppView::open_permission_settings(permission);
                    }),
            ),
            PermissionStatus::Undetermined => Some(
                Button::new(format!("grant-{}", permission_label(permission)))
                    .primary()
                    .loading(pending)
                    .label("Grant")
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.request_permission(permission, cx);
                    })),
            ),
        };

        let row = h_flex()
            .gap_4()
            .items_center()
            .p_4()
            .rounded_lg()
            .border_1()
            .border_color(cx.theme().border)
            .bg(cx.theme().secondary)
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_center()
                    .size(px(40.0))
                    .rounded_lg()
                    .bg(cx.theme().muted)
                    .child(Icon::new(icon).with_size(px(20.0))),
            )
            .child(
                v_flex()
                    .flex_1()
                    .min_w_0()
                    .gap_1()
                    .child(
                        h_flex()
                            .gap_2()
                            .items_center()
                            .child(div().font_weight(gpui::FontWeight::SEMIBOLD).child(title))
                            .child(status_tag),
                    )
                    .child(
                        div()
                            .text_sm()
                            .text_color(cx.theme().muted_foreground)
                            .child(rationale),
                    ),
            );

        match action {
            Some(action) => row.child(action).into_any_element(),
            None => row.into_any_element(),
        }
    }

    #[allow(clippy::too_many_lines)] // one declarative screen tree
    fn render_library(
        &self,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let app = self.model.read(cx);
        let sessions = &app.library;
        let pending = app.has_pending_persistence();
        let count = sessions.len();

        let list = if sessions.is_empty() {
            v_flex()
                .flex_1()
                .items_center()
                .justify_center()
                .gap_4()
                .child(
                    Icon::new(AppIcon::Mic)
                        .with_size(px(40.0))
                        .text_color(cx.theme().muted_foreground),
                )
                .child(
                    v_flex()
                        .gap_1()
                        .items_center()
                        .child(
                            div()
                                .font_weight(gpui::FontWeight::SEMIBOLD)
                                .child("No sessions yet"),
                        )
                        .child(
                            div()
                                .text_sm()
                                .text_color(cx.theme().muted_foreground)
                                .child(
                                    "Start a new session to capture a meeting, a call, or a voice \
                                     memo — transcribed entirely on-device.",
                                ),
                        ),
                )
                .child(
                    Button::new("library-new-session")
                        .primary()
                        .icon(AppIcon::Plus)
                        .label("New Session")
                        .on_click(cx.listener(|this, _, _, cx| this.new_session(cx))),
                )
                .into_any_element()
        } else {
            let mut rows = v_flex().w_full().gap_1();
            for session in sessions {
                let duration = session
                    .ended_at
                    .map(|ended| (ended - session.started_at).num_seconds());
                let live = session.ended_at.is_none();
                let session_id = session.id;
                rows = rows.child(
                    div()
                        .id(format!("session-row-{}", session.id.as_i64()))
                        .w_full()
                        .cursor_pointer()
                        .rounded_md()
                        .px_3()
                        .py_2()
                        .hover(|this| this.bg(cx.theme().accent))
                        .child(
                            h_flex()
                                .w_full()
                                .gap_3()
                                .items_center()
                                .child(
                                    v_flex()
                                        .flex_1()
                                        .min_w_0()
                                        .gap_0p5()
                                        .child(
                                            div()
                                                .font_weight(gpui::FontWeight::MEDIUM)
                                                .text_ellipsis()
                                                .child(session.title.clone()),
                                        )
                                        .child(
                                            div()
                                                .text_xs()
                                                .text_color(cx.theme().muted_foreground)
                                                .child(crate::format::started_at(
                                                    session.started_at,
                                                )),
                                        ),
                                )
                                .child(
                                    div()
                                        .text_sm()
                                        .text_color(cx.theme().muted_foreground)
                                        .child(crate::format::duration(duration)),
                                )
                                .when(live, |this| {
                                    this.child(
                                        Tag::danger()
                                            .small()
                                            .rounded_full()
                                            .child(div().text_xs().child("live")),
                                    )
                                }),
                        )
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.open_history(session_id, cx);
                        })),
                );
            }
            v_flex()
                .id("library-list")
                .flex_1()
                .min_h_0()
                .w_full()
                .gap_2()
                .overflow_y_scroll()
                .child(rows)
                .into_any_element()
        };

        v_flex()
            .flex_1()
            .min_w_0()
            .h_full()
            .overflow_hidden()
            .child(
                h_flex()
                    .w_full()
                    .justify_between()
                    .items_center()
                    .px_6()
                    .py_4()
                    .border_b_1()
                    .border_color(cx.theme().border)
                    .child(
                        v_flex()
                            .gap_0p5()
                            .child(
                                div()
                                    .text_lg()
                                    .font_weight(gpui::FontWeight::SEMIBOLD)
                                    .child("Sessions"),
                            )
                            .child(
                                div()
                                    .text_sm()
                                    .text_color(cx.theme().muted_foreground)
                                    .child(if count == 0 {
                                        "Recordings and transcripts stay on this Mac.".to_string()
                                    } else if count == 1 {
                                        "1 session saved locally.".to_string()
                                    } else {
                                        format!("{count} sessions saved locally.")
                                    }),
                            ),
                    )
                    .child(
                        Button::new("library-record")
                            .primary()
                            .icon(AppIcon::Plus)
                            .label("New Session")
                            .on_click(cx.listener(|this, _, _, cx| this.new_session(cx))),
                    ),
            )
            .child(v_flex().flex_1().min_h_0().px_6().py_4().child(list))
            .when(pending, |this| {
                this.child(
                    div()
                        .w_full()
                        .border_t_1()
                        .border_color(cx.theme().border)
                        .px_6()
                        .py_3()
                        .text_sm()
                        .text_color(cx.theme().danger)
                        .child(
                            "A session still needs to be saved. Press ⌘R or use the menu to \
                             retry before starting a new recording.",
                        ),
                )
            })
            .child(
                div()
                    .w_full()
                    .border_t_1()
                    .border_color(cx.theme().border)
                    .px_6()
                    .py_2()
                    .text_xs()
                    .text_color(cx.theme().muted_foreground)
                    .child("stored locally as Ogg/Opus + SQLite"),
            )
    }
}

impl Render for AppView {
    fn render(
        &mut self,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let onboarding = !self.model.read(cx).setup_complete();
        let content = if onboarding {
            self.render_onboarding(cx).into_any_element()
        } else {
            let view = self.model.read(cx).view.clone();
            h_flex()
                .flex_1()
                .min_h_0()
                .w_full()
                .overflow_hidden()
                .child(self.render_sidebar(&view, cx))
                .child(match view {
                    View::Library => self.render_library(cx).into_any_element(),
                    View::LiveSession => live_view::render(self, cx),
                    View::History { .. } => history_view::render(self, cx),
                })
                .into_any_element()
        };

        v_flex()
            .size_full()
            .bg(cx.theme().background)
            .text_color(cx.theme().foreground)
            .child(self.render_title_bar(cx))
            .child(content)
    }
}

/// Stable, comparable key for the active screen.
const fn view_key(view: &View) -> u8 {
    match view {
        View::Library => 0,
        View::LiveSession => 1,
        View::History { .. } => 2,
    }
}

/// Stable element-id fragment for a permission row's buttons.
fn permission_label(permission: Permission) -> &'static str {
    match permission {
        Permission::Microphone => "microphone",
        Permission::SpeechRecognition => "speech",
    }
}

/// Format the elapsed recording time for the live header. Kept here so both
/// the live view and future status bar share one definition.
pub(crate) fn elapsed_clock(state: SessionState) -> String {
    match state {
        SessionState::Recording { started_at } => {
            let millis = u64::try_from(started_at.elapsed().as_millis()).unwrap_or(u64::MAX);
            crate::format::clock(millis)
        },
        _ => crate::format::clock(0),
    }
}

/// Base styling for the content area below a screen header.
pub(crate) fn screen_body() -> gpui::Div {
    v_flex()
        .flex_1()
        .min_h_0()
        .w_full()
        .px_6()
        .py_4()
        .overflow_hidden()
}

/// Base styling for a screen footer.
pub(crate) fn screen_footer(cx: &App) -> gpui::Div {
    h_flex()
        .w_full()
        .justify_between()
        .border_t_1()
        .border_color(cx.theme().border)
        .px_6()
        .py_2()
        .text_xs()
        .text_color(cx.theme().muted_foreground)
}

/// Transcript renderer re-exported for the screen modules.
pub(crate) fn render_transcript(
    view: &AppView,
    follow: bool,
    cx: &App,
) -> gpui::AnyElement {
    let app = view.model.read(cx);
    transcript_view::render(app, &view.transcript_scroll, follow, cx)
}
