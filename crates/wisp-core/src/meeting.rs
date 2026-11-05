//! Meeting tools, their detection policies, and the persisted settings shape.
//!
//! The catalog is platform data: each native tool lists the macOS bundle
//! identifiers that hold the microphone while a call runs. Browser meetings
//! share one entry because the microphone alone cannot say which site a
//! browser tab is on.

use std::collections::BTreeMap;
use std::time::Duration;

use serde::{Deserialize, Deserializer, Serialize};

/// A meeting tool Wisp can detect.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum MeetingTool {
    Zoom,
    MicrosoftTeams,
    Slack,
    Webex,
    FaceTime,
    Discord,
    BrowserMeeting,
}

impl MeetingTool {
    /// Every tool the settings page lists.
    pub const ALL: [Self; 7] = [
        Self::Zoom,
        Self::MicrosoftTeams,
        Self::Slack,
        Self::Webex,
        Self::FaceTime,
        Self::Discord,
        Self::BrowserMeeting,
    ];

    /// Stable key used in `settings.json` and by the detector. This is the
    /// persisted identity: renaming it resets every user's choice for the
    /// tool to the default.
    #[must_use]
    pub const fn key(self) -> &'static str {
        match self {
            Self::Zoom => "zoom",
            Self::MicrosoftTeams => "microsoft-teams",
            Self::Slack => "slack",
            Self::Webex => "webex",
            Self::FaceTime => "face-time",
            Self::Discord => "discord",
            Self::BrowserMeeting => "browser-meeting",
        }
    }

    /// Human-readable name shown in the prompt and settings.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Zoom => "Zoom",
            Self::MicrosoftTeams => "Microsoft Teams",
            Self::Slack => "Slack huddles",
            Self::Webex => "Webex",
            Self::FaceTime => "FaceTime",
            Self::Discord => "Discord",
            Self::BrowserMeeting => "Browser meetings",
        }
    }

    /// macOS bundle identifiers that use the microphone during a call.
    #[must_use]
    pub const fn bundle_ids(self) -> &'static [&'static str] {
        match self {
            Self::Zoom => &["us.zoom.xos"],
            Self::MicrosoftTeams => &["com.microsoft.teams2", "com.microsoft.teams"],
            Self::Slack => &["com.tinyspeck.slackmacgap"],
            Self::Webex => &["com.cisco.webexmeetingsapp"],
            Self::FaceTime => &["com.apple.FaceTime"],
            Self::Discord => &["com.hnc.Discord"],
            Self::BrowserMeeting => &[
                "com.google.Chrome",
                "com.apple.Safari",
                "com.apple.SafariTechnologyPreview",
                "com.microsoft.edgemac",
                "org.mozilla.firefox",
                "com.brave.Browser",
                "company.thebrowser.Browser",
                "com.vivaldi.Vivaldi",
            ],
        }
    }

    /// Whether this entry covers any browser rather than one native app.
    #[must_use]
    pub const fn is_browser(self) -> bool {
        matches!(self, Self::BrowserMeeting)
    }

    /// How long the microphone must stay held before this tool counts as a
    /// call. A known app is trusted quickly; a browser also hosts voice
    /// notes and dictation fields, which release the microphone after a few
    /// seconds, so it needs to hold it for a while.
    #[must_use]
    pub const fn detection_delay(self) -> Duration {
        if self.is_browser() {
            Duration::from_secs(20)
        } else {
            Duration::from_secs(2)
        }
    }

    /// Parse a [`Self::key`]. Unknown keys are `None`.
    #[must_use]
    pub fn from_key(key: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|tool| tool.key() == key)
    }

    /// The tool whose bundle list contains `bundle_id`.
    ///
    /// Helper processes carry a suffix (`us.zoom.xos.helper`), and they are
    /// the processes that usually open the microphone for a native app, so a
    /// dotted extension of a listed identifier names the same tool.
    #[must_use]
    pub fn from_bundle_id(bundle_id: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|tool| {
            tool.bundle_ids().iter().any(|known| {
                bundle_id == *known
                    || bundle_id
                        .strip_prefix(known)
                        .is_some_and(|suffix| suffix.starts_with('.'))
            })
        })
    }
}

/// What Wisp does when it detects a call from a tool.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum MeetingPolicy {
    /// Ignore calls from this tool.
    Off,
    /// Offer to record when a call starts.
    #[default]
    Ask,
    /// Start recording as soon as a call starts.
    AutoRecord,
}

impl MeetingPolicy {
    /// Every policy the settings UI offers.
    pub const ALL: [Self; 3] = [Self::Off, Self::Ask, Self::AutoRecord];

    /// Stable key used in `settings.json`.
    #[must_use]
    pub const fn key(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Ask => "ask",
            Self::AutoRecord => "auto-record",
        }
    }

    /// Parse a [`Self::key`]. Unknown keys are `None`.
    #[must_use]
    pub fn from_key(key: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|policy| policy.key() == key)
    }

    /// Human-readable name shown in settings.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Off => "Off",
            Self::Ask => "Ask",
            Self::AutoRecord => "Auto-record",
        }
    }
}

impl<'de> Deserialize<'de> for MeetingPolicy {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let key = String::deserialize(deserializer)?;
        Ok(Self::from_key(&key).unwrap_or_default())
    }
}

/// One policy per tool. Tools left out follow [`MeetingPolicy::Ask`], and
/// keys this build does not know are dropped so a newer settings file keeps
/// the user's other preferences.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
#[serde(transparent)]
pub struct MeetingPolicies(BTreeMap<MeetingTool, MeetingPolicy>);

impl MeetingPolicies {
    /// The policy for `tool`, or the default when it has none.
    #[must_use]
    pub fn policy(
        &self,
        tool: MeetingTool,
    ) -> MeetingPolicy {
        self.0.get(&tool).copied().unwrap_or_default()
    }

    /// Replace one tool's policy. `Ask` is the default, so it leaves no
    /// entry behind.
    pub fn set(
        &mut self,
        tool: MeetingTool,
        policy: MeetingPolicy,
    ) {
        if policy == MeetingPolicy::Ask {
            self.0.remove(&tool);
        } else {
            self.0.insert(tool, policy);
        }
    }

    /// Number of tools that differ from the default.
    #[must_use]
    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// Whether every tool follows the default.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl<'de> Deserialize<'de> for MeetingPolicies {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let raw = Option::<BTreeMap<String, MeetingPolicy>>::deserialize(deserializer)?
            .unwrap_or_default();
        Ok(Self(
            raw.into_iter()
                .filter(|(_, policy)| *policy != MeetingPolicy::Ask)
                .filter_map(|(key, policy)| MeetingTool::from_key(&key).map(|tool| (tool, policy)))
                .collect(),
        ))
    }
}

/// Persisted meeting-detection preferences.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct MeetingDetectionSettings {
    /// Whether Wisp watches which apps hold the microphone.
    pub enabled: bool,
    /// The user's choice for each tool.
    pub policies: MeetingPolicies,
}

impl MeetingDetectionSettings {
    /// The policy for `tool`, or the default when it has none.
    #[must_use]
    pub fn policy(
        &self,
        tool: MeetingTool,
    ) -> MeetingPolicy {
        self.policies.policy(tool)
    }

    /// Replace one tool's policy. Returns whether anything changed.
    pub fn set_policy(
        &mut self,
        tool: MeetingTool,
        policy: MeetingPolicy,
    ) -> bool {
        if self.policies.policy(tool) == policy {
            return false;
        }
        self.policies.set(tool, policy);
        true
    }
}

#[cfg(test)]
mod tests {
    use super::{MeetingDetectionSettings, MeetingPolicies, MeetingPolicy, MeetingTool};

    #[test]
    fn every_tool_key_roundtrips() {
        for tool in MeetingTool::ALL {
            assert_eq!(MeetingTool::from_key(tool.key()), Some(tool));
        }
        assert_eq!(MeetingTool::from_key("google-meet"), None);
        assert_eq!(MeetingTool::from_key(""), None);
    }

    #[test]
    fn catalog_matches_the_shipped_names_and_bundle_ids() {
        let expected: [(MeetingTool, &str, &str, &[&str]); 7] = [
            (MeetingTool::Zoom, "zoom", "Zoom", &["us.zoom.xos"]),
            (
                MeetingTool::MicrosoftTeams,
                "microsoft-teams",
                "Microsoft Teams",
                &["com.microsoft.teams2", "com.microsoft.teams"],
            ),
            (
                MeetingTool::Slack,
                "slack",
                "Slack huddles",
                &["com.tinyspeck.slackmacgap"],
            ),
            (
                MeetingTool::Webex,
                "webex",
                "Webex",
                &["com.cisco.webexmeetingsapp"],
            ),
            (
                MeetingTool::FaceTime,
                "face-time",
                "FaceTime",
                &["com.apple.FaceTime"],
            ),
            (
                MeetingTool::Discord,
                "discord",
                "Discord",
                &["com.hnc.Discord"],
            ),
            (
                MeetingTool::BrowserMeeting,
                "browser-meeting",
                "Browser meetings",
                &[
                    "com.google.Chrome",
                    "com.apple.Safari",
                    "com.apple.SafariTechnologyPreview",
                    "com.microsoft.edgemac",
                    "org.mozilla.firefox",
                    "com.brave.Browser",
                    "company.thebrowser.Browser",
                    "com.vivaldi.Vivaldi",
                ],
            ),
        ];
        assert_eq!(MeetingTool::ALL.len(), expected.len());
        for (tool, key, label, bundle_ids) in expected {
            assert_eq!(tool.key(), key);
            assert_eq!(tool.label(), label);
            assert_eq!(tool.bundle_ids(), bundle_ids);
        }
    }

    #[test]
    fn every_bundle_id_names_its_tool() {
        for tool in MeetingTool::ALL {
            for bundle_id in tool.bundle_ids() {
                assert_eq!(MeetingTool::from_bundle_id(bundle_id), Some(tool));
            }
        }
        assert_eq!(MeetingTool::from_bundle_id("com.example.Nope"), None);
        assert_eq!(
            MeetingTool::from_bundle_id("us.zoom.xos.helper"),
            Some(MeetingTool::Zoom)
        );
        assert_eq!(
            MeetingTool::from_bundle_id("com.google.Chrome.helper.GPU"),
            Some(MeetingTool::BrowserMeeting)
        );
        assert_eq!(MeetingTool::from_bundle_id("us.zoom.xosish"), None);
    }

    #[test]
    fn tools_have_distinct_keys_labels_and_bundle_ids() {
        let mut keys = MeetingTool::ALL.map(MeetingTool::key).to_vec();
        keys.sort_unstable();
        keys.dedup();
        assert_eq!(keys.len(), MeetingTool::ALL.len());

        let mut labels = MeetingTool::ALL.map(MeetingTool::label).to_vec();
        labels.sort_unstable();
        labels.dedup();
        assert_eq!(labels.len(), MeetingTool::ALL.len());

        let mut bundle_ids: Vec<_> = MeetingTool::ALL
            .iter()
            .flat_map(|tool| tool.bundle_ids().iter())
            .collect();
        let count = bundle_ids.len();
        bundle_ids.sort_unstable();
        bundle_ids.dedup();
        assert_eq!(bundle_ids.len(), count);
    }

    #[test]
    fn only_browser_meetings_wait_out_the_voice_note_window() {
        for tool in MeetingTool::ALL {
            let expected = if tool == MeetingTool::BrowserMeeting {
                std::time::Duration::from_secs(20)
            } else {
                std::time::Duration::from_secs(2)
            };
            assert_eq!(tool.detection_delay(), expected);
        }
    }

    #[test]
    fn policy_keys_roundtrip_and_unknown_policies_ask() {
        let expected = [
            (MeetingPolicy::Off, "off", "Off"),
            (MeetingPolicy::Ask, "ask", "Ask"),
            (MeetingPolicy::AutoRecord, "auto-record", "Auto-record"),
        ];
        for (policy, key, label) in expected {
            assert_eq!(policy.key(), key);
            assert_eq!(policy.label(), label);
            assert_eq!(MeetingPolicy::from_key(key), Some(policy));
            let encoded = format!("\"{}\"", policy.key());
            assert_eq!(
                serde_json::from_str::<MeetingPolicy>(&encoded).expect("policy"),
                policy
            );
        }
        assert_eq!(MeetingPolicy::from_key("record"), None);
        assert_eq!(
            serde_json::from_str::<MeetingPolicy>("\"record\"").expect("fallback"),
            MeetingPolicy::Ask
        );
    }

    #[test]
    fn policies_drop_unknown_tools_and_defaults() {
        let raw = r#"{"zoom":"auto-record","unknown-tool":"off","slack":"ask"}"#;
        let policies: MeetingPolicies = serde_json::from_str(raw).expect("policies");
        assert_eq!(policies.len(), 1);
        assert_eq!(
            policies.policy(MeetingTool::Zoom),
            MeetingPolicy::AutoRecord
        );
        assert_eq!(policies.policy(MeetingTool::Slack), MeetingPolicy::Ask);
        assert_eq!(policies.policy(MeetingTool::Discord), MeetingPolicy::Ask);

        let empty: MeetingPolicies = serde_json::from_str("null").expect("null policies");
        assert!(empty.is_empty());
    }

    #[test]
    fn policies_have_distinct_labels() {
        let mut labels = MeetingPolicy::ALL.map(MeetingPolicy::label).to_vec();
        labels.sort_unstable();
        labels.dedup();
        assert_eq!(labels.len(), MeetingPolicy::ALL.len());
    }

    #[test]
    fn setting_the_default_policy_removes_the_entry() {
        let mut policies = MeetingPolicies::default();
        assert!(policies.is_empty());
        policies.set(MeetingTool::Zoom, MeetingPolicy::Off);
        assert_eq!(policies.len(), 1);
        assert!(!policies.is_empty());
        policies.set(MeetingTool::Discord, MeetingPolicy::AutoRecord);
        assert_eq!(policies.len(), 2);
        policies.set(MeetingTool::Zoom, MeetingPolicy::Ask);
        assert_eq!(policies.len(), 1);
        policies.set(MeetingTool::Discord, MeetingPolicy::Ask);
        assert!(policies.is_empty());
    }

    #[test]
    fn settings_roundtrip_as_json_with_tool_keys() {
        let mut settings = MeetingDetectionSettings {
            enabled: true,
            ..MeetingDetectionSettings::default()
        };
        assert!(settings.set_policy(MeetingTool::MicrosoftTeams, MeetingPolicy::AutoRecord));
        assert!(!settings.set_policy(MeetingTool::MicrosoftTeams, MeetingPolicy::AutoRecord));

        let json = serde_json::to_string(&settings).expect("settings json");
        assert!(
            json.contains("\"microsoft-teams\":\"auto-record\""),
            "{json}"
        );
        let parsed: MeetingDetectionSettings = serde_json::from_str(&json).expect("settings");
        assert_eq!(parsed, settings);
    }

    #[test]
    fn default_settings_detect_nothing_and_ask_for_every_tool() {
        let settings = MeetingDetectionSettings::default();
        assert!(!settings.enabled);
        for tool in MeetingTool::ALL {
            assert_eq!(settings.policy(tool), MeetingPolicy::Ask);
        }
    }
}
