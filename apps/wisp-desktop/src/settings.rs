//! User preferences persisted as `settings.json` in the Wisp data directory.
//!
//! The file holds the light/dark appearance, the transcription language, and
//! the meeting-detection policy per tool:
//!
//! - [`ThemePreference::System`] (the default) follows the macOS appearance
//!   and updates live when the OS switches between light and dark.
//! - [`LanguageCode::DEFAULT`] stays `ja-JP` so an existing install keeps the
//!   language it recorded with. Unknown codes are kept verbatim, since a
//!   newer build may have written them.
//! - Meeting detection is off until the user turns it on. Each tool left out
//!   of the policy map follows [`MeetingPolicy::Ask`].
//!
//! Everything is stored in a GPUI global so menus, the title-bar button, and
//! the settings page share one source of truth. Every write persists the
//! whole document.

use std::path::{Path, PathBuf};

use gpui::{App, Global, Window, WindowAppearance};
use gpui_kit::component::{ActiveTheme as _, Theme, ThemeMode};
use serde::{Deserialize, Deserializer, Serialize};
use wisp_core::{MeetingDetectionSettings, MeetingPolicy, MeetingTool};

/// Name of the settings file inside the Wisp data directory.
const FILE_NAME: &str = "settings.json";
/// Temporary file used to replace `settings.json` atomically.
const TEMP_FILE_NAME: &str = "settings.json.tmp";

/// Light/dark appearance preference.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ThemePreference {
    /// Follow the macOS appearance (default).
    #[default]
    System,
    /// Always use the light theme.
    Light,
    /// Always use the dark theme.
    Dark,
}

impl ThemePreference {
    /// The explicit theme mode, or `None` when following the system.
    const fn explicit_mode(self) -> Option<ThemeMode> {
        match self {
            Self::System => None,
            Self::Light => Some(ThemeMode::Light),
            Self::Dark => Some(ThemeMode::Dark),
        }
    }
}

impl<'de> Deserialize<'de> for ThemePreference {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = String::deserialize(deserializer)?;
        Ok(match value.as_str() {
            "light" => Self::Light,
            "dark" => Self::Dark,
            _ => Self::System,
        })
    }
}

/// BCP-47 identifier handed to the speech engine.
///
/// Stored verbatim so a code this build does not offer survives a save, and
/// so the Swift layer can map it with `supportedLocale(equivalentTo:)`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct LanguageCode(String);

impl LanguageCode {
    /// The locale Wisp recorded with before this setting existed. Changing
    /// the default would silently re-language every existing user.
    pub const DEFAULT: &'static str = "ja-JP";

    /// Build a code from any BCP-47 string.
    #[must_use]
    pub fn new(code: impl Into<String>) -> Self {
        Self(code.into())
    }

    /// The stored code.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Whether the settings page offers this code.
    #[must_use]
    pub fn is_known(&self) -> bool {
        KNOWN_LANGUAGES.iter().any(|(code, _)| *code == self.0)
    }

    /// The name shown in the picker, or the raw code when unknown.
    #[must_use]
    pub fn label(&self) -> &str {
        KNOWN_LANGUAGES
            .iter()
            .find(|(code, _)| *code == self.0)
            .map_or(self.0.as_str(), |(_, label)| *label)
    }
}

impl Default for LanguageCode {
    fn default() -> Self {
        Self(Self::DEFAULT.to_owned())
    }
}

/// Languages the settings page offers. `LanguageCode` accepts any string, so
/// adding an entry here is all a new language needs.
pub const KNOWN_LANGUAGES: &[(&str, &str)] = &[
    ("ja-JP", "Japanese"),
    ("en-US", "English (US)"),
    ("en-GB", "English (UK)"),
    ("zh-CN", "Chinese (Simplified)"),
    ("zh-TW", "Chinese (Traditional)"),
    ("ko-KR", "Korean"),
    ("es-ES", "Spanish (Spain)"),
    ("es-MX", "Spanish (Mexico)"),
    ("fr-FR", "French"),
    ("de-DE", "German"),
    ("it-IT", "Italian"),
    ("pt-BR", "Portuguese (Brazil)"),
    ("pt-PT", "Portuguese (Portugal)"),
    ("ru-RU", "Russian"),
    ("nl-NL", "Dutch"),
    ("sv-SE", "Swedish"),
    ("da-DK", "Danish"),
    ("nb-NO", "Norwegian"),
    ("fi-FI", "Finnish"),
    ("pl-PL", "Polish"),
    ("tr-TR", "Turkish"),
    ("ar-SA", "Arabic"),
    ("hi-IN", "Hindi"),
    ("th-TH", "Thai"),
    ("vi-VN", "Vietnamese"),
    ("id-ID", "Indonesian"),
    ("ms-MY", "Malay"),
    ("uk-UA", "Ukrainian"),
];

/// Persisted application settings.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    /// Light/dark appearance preference.
    pub theme: ThemePreference,
    /// Speech language used for the next recording.
    pub transcription_language: LanguageCode,
    /// Meeting detection and per-tool policies.
    pub meetings: MeetingDetectionSettings,
}

impl Settings {
    /// Read `settings.json` from `data_dir`, falling back to defaults when the
    /// file is missing or unreadable. A corrupt file is reported on stderr but
    /// never prevents the app from starting.
    #[must_use]
    pub fn load(data_dir: &Path) -> Self {
        let path = Self::path(data_dir);
        let Ok(bytes) = std::fs::read(&path) else {
            return Self::default();
        };
        serde_json::from_slice(&bytes).unwrap_or_else(|error| {
            eprintln!("wisp: ignoring unreadable {}: {error}", path.display());
            Self::default()
        })
    }

    /// Write the settings back to `settings.json` through a temporary file,
    /// so a crash mid-write keeps the previous preferences.
    pub fn save(
        &self,
        data_dir: &Path,
    ) {
        let path = Self::path(data_dir);
        let temp = data_dir.join(TEMP_FILE_NAME);
        match serde_json::to_vec_pretty(self) {
            Ok(json) => {
                if let Err(error) =
                    std::fs::write(&temp, json).and_then(|()| std::fs::rename(&temp, &path))
                {
                    eprintln!("wisp: could not write {}: {error}", path.display());
                }
            },
            Err(error) => eprintln!("wisp: could not serialize settings: {error}"),
        }
    }

    fn path(data_dir: &Path) -> PathBuf {
        data_dir.join(FILE_NAME)
    }
}

struct SettingsState {
    settings: Settings,
    data_dir: PathBuf,
}

impl Global for SettingsState {}

/// Apply `edit` to the live settings and persist the whole document when
/// anything changed. A write always carries every field, so changing the
/// theme cannot erase the language or the meeting policies.
fn update(
    cx: &mut App,
    edit: impl FnOnce(&mut Settings),
) -> bool {
    let (changed, data_dir, settings) = {
        let state = cx.global_mut::<SettingsState>();
        let before = state.settings.clone();
        edit(&mut state.settings);
        (
            state.settings != before,
            state.data_dir.clone(),
            state.settings.clone(),
        )
    };
    if changed {
        settings.save(&data_dir);
    }
    changed
}

/// Load the persisted settings and apply the stored appearance. Must run after
/// `gpui_kit::init` and before the first window opens.
pub fn init(
    cx: &mut App,
    data_dir: &Path,
) {
    let settings = Settings::load(data_dir);
    cx.set_global(SettingsState {
        settings: settings.clone(),
        data_dir: data_dir.to_path_buf(),
    });
    apply(settings.theme, None, cx);
}

/// A copy of the live settings.
#[must_use]
pub fn snapshot(cx: &App) -> Settings {
    cx.global::<SettingsState>().settings.clone()
}

/// Re-apply the current preference. Pass the window when one exists so the
/// system-following mode reads that window's appearance.
pub fn apply_current(
    window: Option<&mut Window>,
    cx: &mut App,
) {
    let preference = cx.global::<SettingsState>().settings.theme;
    apply(preference, window, cx);
}

/// The stored appearance preference.
#[must_use]
pub fn preference(cx: &App) -> ThemePreference {
    cx.global::<SettingsState>().settings.theme
}

/// Whether the theme currently follows the OS rather than a pinned mode.
#[must_use]
pub fn follows_system(cx: &App) -> bool {
    preference(cx) == ThemePreference::System
}

/// The speech language used for the next recording.
#[must_use]
pub fn language(cx: &App) -> LanguageCode {
    cx.global::<SettingsState>()
        .settings
        .transcription_language
        .clone()
}

/// The persisted meeting-detection preferences.
#[must_use]
pub fn meeting_detection(cx: &App) -> MeetingDetectionSettings {
    cx.global::<SettingsState>().settings.meetings.clone()
}

/// Follow the macOS appearance again.
pub fn set_system(cx: &mut App) {
    set_preference(ThemePreference::System, cx);
}

/// Pin the light theme.
pub fn set_light(cx: &mut App) {
    set_preference(ThemePreference::Light, cx);
}

/// Pin the dark theme.
pub fn set_dark(cx: &mut App) {
    set_preference(ThemePreference::Dark, cx);
}

/// Switch to the inverse explicit appearance and persist the choice.
///
/// From [`ThemePreference::System`] this pins whichever mode is *not* in
/// effect right now; afterwards each press flips light ↔ dark.
pub fn toggle(cx: &mut App) {
    set_preference(next_after_toggle(cx.theme().is_dark()), cx);
}

/// The preference a toggle picks from the current effective appearance.
const fn next_after_toggle(dark: bool) -> ThemePreference {
    if dark {
        ThemePreference::Light
    } else {
        ThemePreference::Dark
    }
}

/// Replace the transcription language for the next recording.
pub fn set_language(
    language: LanguageCode,
    cx: &mut App,
) {
    update(cx, |settings| {
        settings.transcription_language = language;
    });
}

/// Turn microphone-activity detection on or off.
pub fn set_meeting_detection_enabled(
    enabled: bool,
    cx: &mut App,
) -> MeetingDetectionSettings {
    update(cx, |settings| settings.meetings.enabled = enabled);
    meeting_detection(cx)
}

/// Replace one tool's detection policy.
pub fn set_meeting_policy(
    tool: MeetingTool,
    policy: MeetingPolicy,
    cx: &mut App,
) -> MeetingDetectionSettings {
    update(cx, |settings| {
        let _ = settings.meetings.set_policy(tool, policy);
    });
    meeting_detection(cx)
}

fn set_preference(
    preference: ThemePreference,
    cx: &mut App,
) {
    update(cx, |settings| settings.theme = preference);
    apply(preference, None, cx);
}

fn apply(
    preference: ThemePreference,
    window: Option<&mut Window>,
    cx: &mut App,
) {
    if let Some(mode) = preference.explicit_mode() {
        Theme::change(mode, None, cx);
        let appearance = match mode {
            ThemeMode::Dark => WindowAppearance::Dark,
            ThemeMode::Light => WindowAppearance::Light,
        };
        cx.set_window_appearance(Some(appearance));
    } else {
        cx.set_window_appearance(None);
        Theme::sync_system_appearance(window, cx);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_follow_the_system_appearance_and_record_japanese() {
        let settings = Settings::default();
        assert_eq!(settings.theme, ThemePreference::System);
        assert_eq!(settings.theme.explicit_mode(), None);
        assert_eq!(settings.transcription_language.as_str(), "ja-JP");
        assert!(!settings.meetings.enabled);
    }

    #[test]
    fn settings_round_trip_as_lowercase_json() {
        let dir = tempfile::tempdir().expect("temp dir");
        let settings = Settings {
            theme: ThemePreference::Dark,
            transcription_language: LanguageCode::new("en-US"),
            meetings: MeetingDetectionSettings {
                enabled: true,
                ..MeetingDetectionSettings::default()
            },
        };
        settings.save(dir.path());

        let raw = std::fs::read_to_string(dir.path().join(FILE_NAME)).expect("settings file");
        assert!(raw.contains("\"dark\""), "unexpected settings JSON: {raw}");
        assert!(raw.contains("\"en-US\""), "unexpected settings JSON: {raw}");

        let loaded = Settings::load(dir.path());
        assert_eq!(loaded.theme, ThemePreference::Dark);
        assert_eq!(loaded.transcription_language.as_str(), "en-US");
        assert!(loaded.meetings.enabled);
    }

    #[test]
    fn an_unknown_theme_keeps_the_other_preferences() {
        let raw =
            r#"{"theme":"sepia","transcription_language":"en-US","meetings":{"enabled":true}}"#;
        let settings: Settings = serde_json::from_str(raw).expect("settings");
        assert_eq!(settings.theme, ThemePreference::System);
        assert_eq!(settings.transcription_language.as_str(), "en-US");
        assert!(settings.meetings.enabled);
    }

    #[test]
    fn an_unknown_language_is_preserved_verbatim() {
        let raw = r#"{"theme":"light","transcription_language":"xx-YY"}"#;
        let settings: Settings = serde_json::from_str(raw).expect("settings");
        assert_eq!(settings.transcription_language.as_str(), "xx-YY");
        assert!(!settings.transcription_language.is_known());
        assert_eq!(settings.transcription_language.label(), "xx-YY");
        assert_eq!(settings.theme, ThemePreference::Light);
    }

    #[test]
    fn an_unknown_tool_keeps_the_other_preferences() {
        let raw = r#"{
            "theme": "dark",
            "transcription_language": "en-US",
            "meetings": {"enabled": true, "policies": {"unknown-tool": "off", "zoom": "auto-record"}}
        }"#;
        let settings: Settings = serde_json::from_str(raw).expect("settings");
        assert_eq!(settings.theme, ThemePreference::Dark);
        assert_eq!(settings.transcription_language.as_str(), "en-US");
        assert!(settings.meetings.enabled);
        assert_eq!(
            settings.meetings.policy(MeetingTool::Zoom),
            MeetingPolicy::AutoRecord
        );
        assert_eq!(
            settings.meetings.policy(MeetingTool::Discord),
            MeetingPolicy::Ask
        );
    }

    #[test]
    fn a_theme_change_keeps_the_language_and_policies() {
        let dir = tempfile::tempdir().expect("temp dir");
        let mut settings = Settings {
            transcription_language: LanguageCode::new("fr-FR"),
            ..Settings::default()
        };
        let _ = settings
            .meetings
            .set_policy(MeetingTool::Zoom, MeetingPolicy::AutoRecord);
        settings.save(dir.path());

        let mut loaded = Settings::load(dir.path());
        loaded.theme = ThemePreference::Dark;
        loaded.save(dir.path());

        let reloaded = Settings::load(dir.path());
        assert_eq!(reloaded.theme, ThemePreference::Dark);
        assert_eq!(reloaded.transcription_language.as_str(), "fr-FR");
        assert_eq!(
            reloaded.meetings.policy(MeetingTool::Zoom),
            MeetingPolicy::AutoRecord
        );
    }

    #[test]
    fn known_languages_have_unique_codes_and_labels() {
        let mut codes: Vec<_> = KNOWN_LANGUAGES.iter().map(|(code, _)| *code).collect();
        codes.sort_unstable();
        codes.dedup();
        assert_eq!(codes.len(), KNOWN_LANGUAGES.len());

        let mut labels: Vec<_> = KNOWN_LANGUAGES.iter().map(|(_, label)| *label).collect();
        labels.sort_unstable();
        labels.dedup();
        assert_eq!(labels.len(), KNOWN_LANGUAGES.len());
        assert!(
            KNOWN_LANGUAGES
                .iter()
                .any(|(code, _)| *code == LanguageCode::DEFAULT)
        );
    }

    #[test]
    fn toggle_pins_the_opposite_explicit_mode() {
        assert_eq!(next_after_toggle(false), ThemePreference::Dark);
        assert_eq!(next_after_toggle(true), ThemePreference::Light);
    }

    #[test]
    fn missing_settings_file_uses_defaults() {
        let dir = tempfile::tempdir().expect("temp dir");
        assert_eq!(Settings::load(dir.path()), Settings::default());
    }

    #[test]
    fn corrupt_settings_file_uses_defaults() {
        let dir = tempfile::tempdir().expect("temp dir");
        std::fs::write(dir.path().join(FILE_NAME), b"not json").expect("write corrupt file");
        assert_eq!(Settings::load(dir.path()), Settings::default());
    }
}
