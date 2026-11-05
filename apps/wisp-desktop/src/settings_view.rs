//! The Settings page: appearance, transcription language, permissions, and
//! per-tool meeting detection.

use gpui::{
    AnyElement, App, Context, FontWeight, InteractiveElement as _, IntoElement, ParentElement as _,
    SharedString, StatefulInteractiveElement as _, Styled as _, div, prelude::FluentBuilder as _,
    px,
};
use gpui_kit::assets::IconName as AppIcon;
use gpui_kit::component::{
    ActiveTheme as _, Disableable as _, Selectable as _, Sizable as _,
    button::{Button, ButtonGroup, ButtonVariants as _},
    h_flex,
    searchable_list::SearchableListItem,
    select::Select,
    switch::Switch,
    v_flex,
};
use wisp_core::{MeetingPolicy, MeetingTool};

use crate::app_view::{AppView, screen_footer};
use crate::settings::{self, KNOWN_LANGUAGES, ThemePreference};

/// One language row in the picker. The value is the BCP-47 code, so an
/// unknown stored code has no selected row and shows as the placeholder.
#[derive(Clone)]
pub(crate) struct LanguageOption {
    pub(crate) code: SharedString,
    pub(crate) label: SharedString,
}

impl SearchableListItem for LanguageOption {
    type Value = SharedString;

    fn title(&self) -> SharedString {
        self.label.clone()
    }

    fn value(&self) -> &Self::Value {
        &self.code
    }
}

/// Every language the picker offers.
pub(crate) fn language_options() -> Vec<LanguageOption> {
    KNOWN_LANGUAGES
        .iter()
        .map(|(code, label)| LanguageOption {
            code: SharedString::from(*code),
            label: SharedString::from(*label),
        })
        .collect()
}

#[allow(clippy::too_many_lines)] // one declarative page tree
pub(crate) fn render(
    app: &AppView,
    cx: &mut Context<AppView>,
) -> AnyElement {
    let stored = settings::snapshot(cx);
    let detection = stored.meetings;
    let language = &stored.transcription_language;
    let language_detail = if language.is_known() {
        format!(
            "{} · the on-device model checks support when the recording starts.",
            language.label()
        )
    } else {
        format!(
            "{} is not in this build's language list. Pick one before your next recording.",
            language.as_str()
        )
    };

    let appearance = row(
        "settings-appearance",
        "Appearance",
        "Follow the macOS setting, or pin light or dark.".to_owned(),
        theme_control(stored.theme, cx),
        true,
        cx,
    );
    let language_row = row(
        "settings-language",
        "Language",
        language_detail,
        // Select's root is `size_full()`, so it has to be constrained by a
        // fixed-size wrapper or it eats the row and leaves the text at zero.
        div()
            .w(px(240.0))
            .h(px(32.0))
            .flex_shrink_0()
            .child(Select::new(&app.language_select).placeholder("Choose a language"))
            .into_any_element(),
        true,
        cx,
    );

    let permissions = app.model.read(cx).permissions;
    let permission_rows: Vec<AnyElement> = vec![
        AppView::render_permission_row(
            wisp_audiokit::Permission::Microphone,
            AppIcon::Mic,
            "Microphone",
            "Capture your voice for on-device transcription.",
            permissions,
            cx,
        ),
        AppView::render_permission_row(
            wisp_audiokit::Permission::SpeechRecognition,
            AppIcon::AudioLines,
            "Speech Recognition",
            "Transcribe both tracks with Apple's on-device speech model.",
            permissions,
            cx,
        ),
    ];

    let mut meeting_rows = vec![row(
        "settings-meeting-detection",
        "Detect meeting apps",
        "Watch which apps hold the microphone and offer to record. Off by default.".to_owned(),
        Switch::new("settings-meeting-detection-switch")
            .checked(detection.enabled)
            .accessibility_label("Detect meeting apps")
            .on_change(cx.listener(|this, &enabled, _, cx| {
                this.set_meeting_detection(enabled, cx);
            }))
            .into_any_element(),
        false,
        cx,
    )];
    let tool_count = MeetingTool::ALL.len();
    for (index, tool) in MeetingTool::ALL.into_iter().enumerate() {
        let policy = detection.policy(tool);
        meeting_rows.push(row(
            format!("settings-tool-{}", tool.key()),
            tool.label(),
            policy_detail(policy).to_owned(),
            policy_control(tool, policy, detection.enabled, cx),
            index + 1 == tool_count,
            cx,
        ));
    }

    v_flex()
        .flex_1()
        .min_w_0()
        .h_full()
        .overflow_hidden()
        .child(header(cx))
        .child(
            div()
                .id("settings-scroll")
                .flex_1()
                .min_h_0()
                .min_w_0()
                .w_full()
                .overflow_y_scroll()
                .child(
                    v_flex()
                        .w_full()
                        .max_w(px(760.0))
                        .mx_auto()
                        .min_w(px(320.0))
                        .p_6()
                        .gap_6()
                        .child(section("Appearance", card(vec![appearance], cx)))
                        .child(section("Transcription", card(vec![language_row], cx)))
                        .child(section(
                            "Permissions",
                            v_flex().gap_3().children(permission_rows),
                        ))
                        .child(section("Meeting detection", card(meeting_rows, cx)))
                        .child(
                            div()
                                .text_xs()
                                .text_color(cx.theme().muted_foreground)
                                .child(
                                    "Detection reads app bundle identifiers from Core Audio. \
                                     Audio is captured only by a recording you start or \
                                     auto-record.",
                                ),
                        ),
                ),
        )
        .child(screen_footer(cx).child("stored locally as settings.json"))
        .into_any_element()
}

fn header(cx: &mut Context<AppView>) -> AnyElement {
    h_flex()
        .w_full()
        .gap_3()
        .items_center()
        .px_6()
        .py_4()
        .border_b_1()
        .border_color(cx.theme().border)
        .child(
            Button::new("settings-back")
                .ghost()
                .small()
                .icon(AppIcon::ArrowLeft)
                .label("Back")
                .tooltip("Back to the previous screen")
                .on_click(cx.listener(|this, _, _, cx| this.close_settings(cx))),
        )
        .child(
            v_flex()
                .flex_1()
                .min_w_0()
                .gap_0p5()
                .child(
                    div()
                        .text_xl()
                        .font_weight(FontWeight::SEMIBOLD)
                        .child("Settings"),
                )
                .child(
                    div()
                        .text_sm()
                        .text_color(cx.theme().muted_foreground)
                        .child("Preferences are saved on this Mac as you change them."),
                ),
        )
        .into_any_element()
}

fn section(
    title: &'static str,
    body: impl IntoElement,
) -> AnyElement {
    v_flex()
        .gap_2()
        .child(
            div()
                .text_sm()
                .font_weight(FontWeight::SEMIBOLD)
                .child(title),
        )
        .child(body)
        .into_any_element()
}

fn card(
    rows: Vec<AnyElement>,
    cx: &App,
) -> AnyElement {
    v_flex()
        .w_full()
        .min_w_0()
        .rounded_lg()
        .border_1()
        .border_color(cx.theme().border)
        .bg(cx.theme().secondary)
        .overflow_hidden()
        .children(rows)
        .into_any_element()
}

fn row(
    id: impl Into<gpui::ElementId>,
    title: impl Into<SharedString>,
    detail: String,
    control: AnyElement,
    last: bool,
    cx: &App,
) -> AnyElement {
    let title: SharedString = title.into();
    h_flex()
        .id(id)
        .min_h(px(56.0))
        .gap_4()
        .items_center()
        .px_4()
        .py_3()
        .when(!last, |this| {
            this.border_b_1().border_color(cx.theme().border)
        })
        .child(
            v_flex()
                .flex_1()
                .min_w_0()
                .gap_0p5()
                .child(
                    div()
                        .text_sm()
                        .font_weight(FontWeight::SEMIBOLD)
                        .child(title),
                )
                .child(
                    div()
                        .text_xs()
                        .text_color(cx.theme().muted_foreground)
                        .child(detail),
                ),
        )
        .child(control)
        .into_any_element()
}

fn theme_control(
    current: ThemePreference,
    cx: &mut Context<AppView>,
) -> AnyElement {
    ButtonGroup::new("settings-theme")
        .children(vec![
            Button::new("settings-theme-system")
                .label("Follow System")
                .selected(current == ThemePreference::System),
            Button::new("settings-theme-light")
                .label("Light")
                .selected(current == ThemePreference::Light),
            Button::new("settings-theme-dark")
                .label("Dark")
                .selected(current == ThemePreference::Dark),
        ])
        .secondary()
        .small()
        .on_click(cx.listener(|this, clicked: &Vec<usize>, _, cx| {
            let preference = match clicked.first().copied() {
                Some(1) => ThemePreference::Light,
                Some(2) => ThemePreference::Dark,
                _ => ThemePreference::System,
            };
            this.choose_appearance(preference, cx);
        }))
        .into_any_element()
}

fn policy_control(
    tool: MeetingTool,
    current: MeetingPolicy,
    enabled: bool,
    cx: &mut Context<AppView>,
) -> AnyElement {
    let buttons = MeetingPolicy::ALL.into_iter().map(|policy| {
        Button::new(format!("settings-policy-{}-{}", tool.key(), policy.key()))
            .label(policy.label())
            .selected(policy == current)
            .disabled(!enabled)
    });
    ButtonGroup::new(format!("settings-policy-{}", tool.key()))
        .children(buttons)
        .secondary()
        .small()
        .disabled(!enabled)
        .on_click(cx.listener(move |this, clicked: &Vec<usize>, _, cx| {
            let policy = clicked
                .first()
                .and_then(|index| MeetingPolicy::ALL.get(*index))
                .copied()
                .unwrap_or_default();
            this.set_meeting_policy(tool, policy, cx);
        }))
        .into_any_element()
}

const fn policy_detail(policy: MeetingPolicy) -> &'static str {
    match policy {
        MeetingPolicy::Off => "Calls from this app are ignored.",
        MeetingPolicy::Ask => "Wisp asks before recording.",
        MeetingPolicy::AutoRecord => "Recording starts on its own.",
    }
}
