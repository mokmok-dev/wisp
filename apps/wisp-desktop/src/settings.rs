//! User preferences persisted as `settings.json` in the Wisp data directory.
//!
//! Today the file only holds the light/dark appearance preference:
//!
//! - [`ThemePreference::System`] (the default) follows the macOS appearance
//!   and updates live when the OS switches between light and dark.
//! - [`ThemePreference::Light`] / [`ThemePreference::Dark`] pin the theme, and
//!   also override the native window chrome so the title bar and borders match.
//!
//! The preference is stored in a GPUI global so the title-bar button and the
//! application-menu action share one source of truth.

use std::path::{Path, PathBuf};

use gpui::{App, Global, Window, WindowAppearance};
use gpui_kit::component::{ActiveTheme as _, Theme, ThemeMode};
use serde::{Deserialize, Serialize};

/// Name of the settings file inside the Wisp data directory.
const FILE_NAME: &str = "settings.json";

/// Light/dark appearance preference.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
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

/// Persisted application settings.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    /// Light/dark appearance preference.
    pub theme: ThemePreference,
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

    /// Write the settings back to `settings.json`.
    pub fn save(
        &self,
        data_dir: &Path,
    ) {
        let path = Self::path(data_dir);
        match serde_json::to_vec_pretty(self) {
            Ok(json) => {
                if let Err(error) = std::fs::write(&path, json) {
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

struct ThemeState {
    preference: ThemePreference,
    data_dir: PathBuf,
}

impl Global for ThemeState {}

/// Load the persisted settings and apply the stored appearance. Must run after
/// `gpui_kit::init` and before the first window opens.
pub fn init(
    cx: &mut App,
    data_dir: &Path,
) {
    let settings = Settings::load(data_dir);
    cx.set_global(ThemeState {
        preference: settings.theme,
        data_dir: data_dir.to_path_buf(),
    });
    apply(settings.theme, None, cx);
}

/// Re-apply the current preference. Pass the window when one exists so the
/// system-following mode reads that window's appearance.
pub fn apply_current(
    window: Option<&mut Window>,
    cx: &mut App,
) {
    let preference = cx.global::<ThemeState>().preference;
    apply(preference, window, cx);
}

/// The stored appearance preference.
#[must_use]
pub fn preference(cx: &App) -> ThemePreference {
    cx.global::<ThemeState>().preference
}

/// Whether the theme currently follows the OS rather than a pinned mode.
#[must_use]
pub fn follows_system(cx: &App) -> bool {
    preference(cx) == ThemePreference::System
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

fn set_preference(
    preference: ThemePreference,
    cx: &mut App,
) {
    let data_dir = {
        let state = cx.global_mut::<ThemeState>();
        state.preference = preference;
        state.data_dir.clone()
    };
    apply(preference, None, cx);
    Settings { theme: preference }.save(&data_dir);
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
    fn defaults_follow_the_system_appearance() {
        let settings = Settings::default();
        assert_eq!(settings.theme, ThemePreference::System);
        assert_eq!(settings.theme.explicit_mode(), None);
    }

    #[test]
    fn settings_round_trip_as_lowercase_json() {
        let dir = tempfile::tempdir().expect("temp dir");
        let settings = Settings {
            theme: ThemePreference::Dark,
        };
        settings.save(dir.path());

        let raw = std::fs::read_to_string(dir.path().join(FILE_NAME)).expect("settings file");
        assert!(raw.contains("\"dark\""), "unexpected settings JSON: {raw}");

        let loaded = Settings::load(dir.path());
        assert_eq!(loaded.theme, ThemePreference::Dark);
    }

    #[test]
    fn toggle_pins_the_opposite_explicit_mode() {
        assert_eq!(next_after_toggle(false), ThemePreference::Dark);
        assert_eq!(next_after_toggle(true), ThemePreference::Light);
    }

    #[test]
    fn missing_settings_file_uses_defaults() {
        let dir = tempfile::tempdir().expect("temp dir");
        assert_eq!(Settings::load(dir.path()).theme, ThemePreference::System);
    }

    #[test]
    fn corrupt_settings_file_uses_defaults() {
        let dir = tempfile::tempdir().expect("temp dir");
        std::fs::write(dir.path().join(FILE_NAME), b"not json").expect("write corrupt file");
        assert_eq!(Settings::load(dir.path()).theme, ThemePreference::System);
    }
}
