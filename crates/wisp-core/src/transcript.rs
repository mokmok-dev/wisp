use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::{SegmentId, SessionId, SourceLabel};

/// One recording session = one meeting. Tracks lifecycle timestamps, the
/// user-editable title, and the on-disk paths to the captured WAV files.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Session {
    pub id: SessionId,
    pub started_at: DateTime<Utc>,
    /// `None` while the session is still recording.
    pub ended_at: Option<DateTime<Utc>>,
    pub title: String,
    /// Path relative to the storage root, never absolute. Lets the user
    /// move their library between machines without DB rewrites.
    pub mic_wav_path: String,
    pub system_wav_path: String,
    pub notes: String,
}

/// Fields required to create a new session. The storage layer fills in
/// `id`, `ended_at`, and `notes`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NewSession {
    pub started_at: DateTime<Utc>,
    pub title: String,
    pub mic_wav_path: String,
    pub system_wav_path: String,
}

/// One finalized transcript chunk inside a session.
///
/// Each `SpeechAnalyzer` finalization (mic or system) appends one segment.
/// Partial / volatile updates are not persisted — only the final text is.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Segment {
    pub id: SegmentId,
    pub session_id: SessionId,
    pub source: SourceLabel,
    /// 0-based index within (session, source). Stable across reads, lets
    /// the UI re-order or re-render without timestamp comparisons.
    pub segment_index: u32,
    pub start_seconds: f64,
    pub end_seconds: f64,
    pub text: String,
    /// User-editable speaker name. Defaults to the source label string
    /// (`"mic"` / `"system"`) on insert and can be renamed later.
    pub speaker_label: Option<String>,
    pub created_at: DateTime<Utc>,
}

/// Fields required to append a new segment. The storage layer fills in
/// `id` and `created_at`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NewSegment {
    pub session_id: SessionId,
    pub source: SourceLabel,
    pub segment_index: u32,
    pub start_seconds: f64,
    pub end_seconds: f64,
    pub text: String,
    pub speaker_label: Option<String>,
}
