use serde::{Deserialize, Serialize};

use crate::{SourceKind, TrackDescriptor, TrackId};

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

    /// Stable track identity used by the backend-neutral capture contracts.
    #[must_use]
    pub const fn track_id(self) -> TrackId {
        match self {
            Self::Mic => TrackId::MICROPHONE,
            Self::System => TrackId::SYSTEM,
        }
    }

    /// Extensible source kind corresponding to this legacy storage label.
    #[must_use]
    pub const fn source_kind(self) -> SourceKind {
        match self {
            Self::Mic => SourceKind::Microphone,
            Self::System => SourceKind::SystemAudio,
        }
    }

    #[must_use]
    pub fn track_descriptor(self) -> TrackDescriptor {
        TrackDescriptor {
            id: self.track_id(),
            source: self.source_kind(),
            name: self.as_str().to_owned(),
        }
    }

    /// Parse the stable string form. Returns `None` for unknown values.
    #[must_use]
    pub fn parse(s: &str) -> Option<Self> {
        s.parse().ok()
    }
}

impl std::str::FromStr for SourceLabel {
    type Err = crate::SourceLabelError;

    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        match s {
            "mic" => Ok(Self::Mic),
            "system" => Ok(Self::System),
            _ => Err(crate::SourceLabelError(s.to_owned())),
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

    #[test]
    fn legacy_sources_have_stable_track_contracts() {
        assert_eq!(SourceLabel::Mic.track_id(), crate::TrackId::MICROPHONE);
        assert_eq!(SourceLabel::System.track_id(), crate::TrackId::SYSTEM);
        assert_eq!(
            SourceLabel::Mic.source_kind(),
            crate::SourceKind::Microphone
        );
        assert_eq!(
            SourceLabel::System.source_kind(),
            crate::SourceKind::SystemAudio
        );
    }
}
