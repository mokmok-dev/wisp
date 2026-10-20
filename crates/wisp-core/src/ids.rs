use serde::{Deserialize, Serialize};

/// Identifier for one recording session (one meeting).
///
/// Newtype around the `SQLite` rowid so it can't be mixed up with other
/// integer IDs (segment IDs, indices, etc.).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct SessionId(i64);

impl SessionId {
    /// Returns the underlying rowid. Use when interacting with `SQLite` or
    /// when an integer key needs to cross an FFI boundary.
    #[must_use]
    pub const fn as_i64(self) -> i64 {
        self.0
    }
}

impl From<i64> for SessionId {
    fn from(value: i64) -> Self {
        Self(value)
    }
}

impl std::fmt::Display for SessionId {
    fn fmt(
        &self,
        f: &mut std::fmt::Formatter<'_>,
    ) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// Identifier for one transcript segment.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct SegmentId(i64);

impl SegmentId {
    /// Returns the underlying rowid.
    #[must_use]
    pub const fn as_i64(self) -> i64 {
        self.0
    }
}

impl From<i64> for SegmentId {
    fn from(value: i64) -> Self {
        Self(value)
    }
}

impl std::fmt::Display for SegmentId {
    fn fmt(
        &self,
        f: &mut std::fmt::Formatter<'_>,
    ) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::{SegmentId, SessionId};

    /// Covers every integer shape the `SQLite` layer can hand back, including
    /// the values mutation testing substitutes for dedicated accessors.
    const RAW_IDS: [i64; 6] = [0, 1, 42, -3, i64::MAX, i64::MIN];

    #[test]
    fn session_ids_roundtrip_and_display_decimal() {
        for raw in RAW_IDS {
            let id = SessionId::from(raw);
            assert_eq!(id.as_i64(), raw);
            assert_eq!(id.to_string(), raw.to_string());
        }
    }

    #[test]
    fn segment_ids_roundtrip_and_display_decimal() {
        for raw in RAW_IDS {
            let id = SegmentId::from(raw);
            assert_eq!(id.as_i64(), raw);
            assert_eq!(id.to_string(), raw.to_string());
        }
    }
}
