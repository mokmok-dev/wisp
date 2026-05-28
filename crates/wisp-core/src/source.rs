use serde::{Deserialize, Serialize};

/// Which audio source produced a segment.
///
/// Wisp captures the microphone and the system audio as two independent
/// streams, transcribes each separately, and uses the source as a free
/// speaker label (mic = self, system = others) — no ML diarization required.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SourceLabel {
    /// The user's microphone.
    Mic,
    /// System audio (other meeting participants, media playback).
    System,
}

impl SourceLabel {
    /// Stable string form used in the `SQLite` schema and exports.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Mic => "mic",
            Self::System => "system",
        }
    }

    /// Parse the stable string form. Returns `None` for unknown values.
    #[must_use]
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "mic" => Some(Self::Mic),
            "system" => Some(Self::System),
            _ => None,
        }
    }
}

impl std::fmt::Display for SourceLabel {
    fn fmt(
        &self,
        f: &mut std::fmt::Formatter<'_>,
    ) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::SourceLabel;

    #[test]
    fn roundtrip_str() {
        for src in [SourceLabel::Mic, SourceLabel::System] {
            assert_eq!(SourceLabel::parse(src.as_str()), Some(src));
        }
    }

    #[test]
    fn parse_unknown_is_none() {
        assert_eq!(SourceLabel::parse("speaker"), None);
        assert_eq!(SourceLabel::parse(""), None);
    }
}
