use serde::{Deserialize, Serialize};

/// Identifier for one recording session (one meeting).
///
/// Newtype around the `SQLite` rowid so it can't be mixed up with other
/// integer IDs (segment IDs, indices, etc.).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct SessionId(pub i64);

impl SessionId {
    /// Returns the underlying rowid. Use when interacting with `SQLite` or
    /// when an integer key needs to cross an FFI boundary.
    #[must_use]
    pub const fn as_i64(self) -> i64 {
        self.0
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
pub struct SegmentId(pub i64);

impl SegmentId {
    /// Returns the underlying rowid.
    #[must_use]
    pub const fn as_i64(self) -> i64 {
        self.0
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
