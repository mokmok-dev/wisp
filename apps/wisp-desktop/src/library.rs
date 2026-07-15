//! Glue between [`wisp_storage::Storage`] and the GPUI [`AppModel`].
//!
//! Owns the helpers that:
//!   * load the saved-session list for the library screen,
//!   * create a session row at recording start,
//!   * persist finalised segments and stamp `ended_at` at recording stop,
//!   * load a session's transcript for the history view.
//!
//! All public functions take a `&Storage` (already locked) and either
//! mutate the model directly or return a value the caller writes back —
//! none of them touch the GPUI context, so they're safe to call from
//! background tasks.

use std::sync::{Arc, Mutex};

use chrono::{DateTime, Utc};
use wisp_audiokit::SourceLabel;
use wisp_core::{NewSegment, NewSession, SessionId};
use wisp_storage::{Storage, StorageError};

use crate::app::{Segment as UiSegment, break_on_sentence_end};

/// Format a `started_at` timestamp into the default session title:
/// `2026-05-29 14:30` in the user's local timezone. Users can rename
/// later (the storage layer supports it; UI hook is TODO).
pub fn default_title(started_at: DateTime<Utc>) -> String {
    started_at
        .with_timezone(&chrono::Local)
        .format("%Y-%m-%d %H:%M")
        .to_string()
}

/// Format the directory name we hand to the Swift audio session so each
/// recording lands in its own subfolder of the recordings root. Uses a
/// filesystem-safe ISO-ish form: `2026-05-29T143000123456789Z` (always UTC,
/// no punctuation that needs escaping on any platform). Nanosecond precision
/// keeps separate recordings started within the same second in distinct
/// directories.
pub fn session_dir_name(started_at: DateTime<Utc>) -> String {
    format!(
        "{}{:09}Z",
        started_at.format("%Y-%m-%dT%H%M%S"),
        started_at.timestamp_subsec_nanos()
    )
}

/// Create a new session row. `dir_name` is the per-session subdirectory
/// passed to the Swift audio kit beneath the `recordings` directory. WAV
/// paths are stored relative to the storage root, as required by
/// `wisp_core::Session`.
pub fn create_session(
    storage: &Storage,
    started_at: DateTime<Utc>,
    dir_name: &str,
) -> Result<SessionId, StorageError> {
    let mic_rel = format!("recordings/{dir_name}/mic.wav");
    let sys_rel = format!("recordings/{dir_name}/system.wav");
    storage.sessions().create(&NewSession {
        started_at,
        title: default_title(started_at),
        mic_wav_path: mic_rel,
        system_wav_path: sys_rel,
    })
}

/// Save the in-memory live transcript into storage at the end of a
/// recording, then mark the session as ended.
///
/// `segments` is expected to be the model's full list at stop time — all
/// already-final segments plus the trailing partial, which the caller
/// finalises with `AppModel::finalize_all_segments()` before invoking
/// this. We assign per-source `segment_index` values by walking the slice
/// in order, matching the playback ordering the UI uses.
pub fn finalise_session(
    storage: &Storage,
    session_id: SessionId,
    segments: &[UiSegment],
    ended_at: DateTime<Utc>,
) -> Result<(), StorageError> {
    let mut stored_segments = Vec::with_capacity(segments.len());
    let mut mic_idx: u32 = 0;
    let mut sys_idx: u32 = 0;
    for seg in segments {
        let (idx, source) = match seg.source {
            SourceLabel::Mic => {
                let i = mic_idx;
                mic_idx = mic_idx.saturating_add(1);
                (i, SourceLabel::Mic)
            },
            SourceLabel::System => {
                let i = sys_idx;
                sys_idx = sys_idx.saturating_add(1);
                (i, SourceLabel::System)
            },
        };
        // Skip empties — `SpeechAnalyzer` occasionally emits a zero-length
        // result during silence. Persisting them just clutters the history.
        if seg.text.trim().is_empty() {
            continue;
        }
        stored_segments.push(NewSegment {
            session_id,
            source,
            segment_index: idx,
            start_seconds: seg.start_seconds,
            end_seconds: seg.end_seconds,
            text: seg.text.clone(),
            speaker_label: None,
        });
    }
    storage.finalise_session(session_id, &stored_segments, ended_at)
}

/// Load a session's full transcript and return it in the UI segment
/// representation. All segments come back finalised — the history view
/// doesn't show partial/ghost text.
pub fn load_history(
    storage: &Storage,
    session_id: SessionId,
) -> Result<Vec<UiSegment>, StorageError> {
    let segs = storage.segments().list_by_session(session_id)?;
    Ok(segs
        .into_iter()
        .map(|s| {
            let display_text = break_on_sentence_end(&s.text);
            UiSegment {
                source: s.source,
                id: u64::from(s.segment_index),
                text: s.text,
                display_text,
                start_seconds: s.start_seconds,
                end_seconds: s.end_seconds,
                is_final: true,
            }
        })
        .collect())
}

/// Convenience: shared, locked storage handle the rest of the app
/// passes around. `SQLite` serialises writers internally, so a plain
/// `Mutex` is enough; we don't need a connection pool.
pub type SharedStorage = Arc<Mutex<Storage>>;

#[cfg(test)]
mod tests {
    use chrono::{TimeDelta, TimeZone, Utc};

    use super::session_dir_name;

    #[test]
    fn session_dir_name_distinguishes_starts_within_the_same_second() {
        let base = Utc.with_ymd_and_hms(2026, 5, 29, 14, 30, 0).unwrap();
        let first = base + TimeDelta::nanoseconds(1);
        let second = base + TimeDelta::nanoseconds(2);

        assert_eq!(session_dir_name(first), "2026-05-29T143000000000001Z");
        assert_eq!(session_dir_name(second), "2026-05-29T143000000000002Z");
        assert_ne!(session_dir_name(first), session_dir_name(second));
    }
}
