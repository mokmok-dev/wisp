//! In-memory transcript model + the ingestion logic that turns streaming
//! `wisp_audiokit::Event`s into a tidy `Vec<Segment>` the UI can render.
//!
//! Ghost-text semantics:
//!   - Each `(source, segment_id)` pair maps to one row.
//!   - While a segment is the latest one for its source, it's a *partial* —
//!     the text gets revised in place as the `SpeechAnalyzer` refines it.
//!   - When the next segment for that source arrives, the previous one is
//!     marked `final` (the speech engine has locked it in).

use std::collections::VecDeque;
use std::time::Instant;

use wisp_audiokit::{Event, SessionResult, SourceLabel};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionState {
    Idle,
    Starting,
    Recording { started_at: Instant },
    Stopping,
    Failed,
}

#[derive(Debug, Clone)]
pub struct Segment {
    pub source: SourceLabel,
    /// Monotonic-per-source identifier emitted by the Swift transcription
    /// pipeline. Same id while the segment is being revised; a new id
    /// means the previous one is finalised.
    pub id: u64,
    pub text: String,
    pub start_seconds: f64,
    pub end_seconds: f64,
    /// True once a later segment from the same source has appeared, which
    /// means the speech engine has stopped revising this one.
    pub is_final: bool,
}

#[derive(Debug)]
pub struct AppModel {
    pub state: SessionState,
    pub segments: Vec<Segment>,
    pub recent_log: VecDeque<String>,
    pub last_error: Option<String>,
}

impl AppModel {
    pub fn new() -> Self {
        Self {
            state: SessionState::Idle,
            segments: Vec::new(),
            recent_log: VecDeque::new(),
            last_error: None,
        }
    }

    pub fn set_state(
        &mut self,
        state: SessionState,
    ) {
        self.state = state;
    }

    pub fn fail(
        &mut self,
        message: impl Into<String>,
    ) {
        self.last_error = Some(message.into());
        self.state = SessionState::Failed;
        self.finalize_all_segments();
    }

    /// Mark every segment as final. Called when recording stops so the
    /// most recent (formerly active) row drops the ghost-text styling
    /// and reads as locked-in alongside the rest.
    pub fn finalize_all_segments(&mut self) {
        for seg in &mut self.segments {
            seg.is_final = true;
        }
    }

    pub fn ingest(
        &mut self,
        event: Event,
    ) {
        match event {
            Event::Result(result) => self.upsert_segment(result),
            Event::Log(line) => {
                self.recent_log.push_back(line);
                while self.recent_log.len() > 200 {
                    self.recent_log.pop_front();
                }
            },
        }
    }

    /// Either revise the active partial for this source or start a new
    /// segment, finalising the previous one for the same source.
    ///
    /// `SpeechAnalyzer` sometimes emits the same utterance under two
    /// distinct segment IDs — once when it ratifies the previous range,
    /// once when it starts the next one — even though both carry the
    /// (nearly) same text. We dedupe those by checking text similarity
    /// against the latest segment for the same source: if it looks like a
    /// continuation, we adopt the new ID + text in place instead of
    /// pushing a duplicate row.
    fn upsert_segment(
        &mut self,
        result: SessionResult,
    ) {
        // Walk newest → oldest to find the latest entry for this source.
        for seg in self.segments.iter_mut().rev() {
            if seg.source != result.source {
                continue;
            }
            if seg.id == result.segment_id {
                seg.text = result.text;
                seg.start_seconds = result.start_seconds;
                seg.end_seconds = result.end_seconds;
                return;
            }
            // Different segment ID for the same source. If the text looks
            // like a continuation/refinement of the previous segment,
            // overwrite in place instead of creating a visually duplicated
            // row. Otherwise the previous segment is locked in and we push
            // a fresh one.
            if looks_like_continuation(&seg.text, &result.text) {
                seg.id = result.segment_id;
                seg.text = result.text;
                seg.start_seconds = result.start_seconds;
                seg.end_seconds = result.end_seconds;
                return;
            }
            seg.is_final = true;
            break;
        }
        self.segments.push(Segment {
            source: result.source,
            id: result.segment_id,
            text: result.text,
            start_seconds: result.start_seconds,
            end_seconds: result.end_seconds,
            is_final: false,
        });
    }

    /// The most recent non-final segment, if any (used by the renderer to
    /// draw the blinking ghost-text cursor on the active line).
    pub fn active_segment_index(&self) -> Option<usize> {
        self.segments
            .iter()
            .enumerate()
            .rev()
            .find_map(|(i, s)| (!s.is_final).then_some(i))
    }
}

impl Default for AppModel {
    fn default() -> Self {
        Self::new()
    }
}

/// Heuristic: do these two strings look like the same utterance being
/// emitted under two segment IDs?
///
/// Combines a common-prefix and common-suffix match against the shorter
/// length of the two strings, so that small in-the-middle revisions (like
/// `MaOS → MacOS`) still register as a continuation. Returns true when
/// the matched chars cover at least 60% of the shorter string (with a
/// floor of 6 characters). Tuned to catch `SpeechAnalyzer`'s "ratify
/// previous + begin next" double-emit and small wording revisions, while
/// not collapsing genuinely separate short utterances.
fn looks_like_continuation(
    prev: &str,
    new: &str,
) -> bool {
    if prev == new {
        return true;
    }
    if prev.is_empty() || new.is_empty() {
        return false;
    }
    let common_prefix = prev
        .chars()
        .zip(new.chars())
        .take_while(|(a, b)| a == b)
        .count();
    let common_suffix = prev
        .chars()
        .rev()
        .zip(new.chars().rev())
        .take_while(|(a, b)| a == b)
        .count();
    let shorter = std::cmp::min(prev.chars().count(), new.chars().count());
    // Cap so prefix + suffix can't overlap on tiny strings.
    let matched = std::cmp::min(common_prefix + common_suffix, shorter);
    let threshold = std::cmp::max(6, shorter * 6 / 10);
    matched >= threshold
}

#[cfg(test)]
mod tests {
    use super::*;

    fn r(
        source: SourceLabel,
        seg: u64,
        text: &str,
    ) -> SessionResult {
        SessionResult {
            source,
            segment_id: seg,
            text: text.into(),
            start_seconds: 0.0,
            end_seconds: 0.0,
        }
    }

    #[test]
    fn partial_revisions_replace_text_in_place() {
        let mut m = AppModel::new();
        m.ingest(Event::Result(r(SourceLabel::Mic, 1, "こん")));
        m.ingest(Event::Result(r(SourceLabel::Mic, 1, "こんにちは")));
        assert_eq!(m.segments.len(), 1);
        assert_eq!(m.segments[0].text, "こんにちは");
        assert!(!m.segments[0].is_final);
    }

    #[test]
    fn new_segment_id_finalises_previous() {
        let mut m = AppModel::new();
        m.ingest(Event::Result(r(SourceLabel::Mic, 1, "前")));
        m.ingest(Event::Result(r(SourceLabel::Mic, 2, "次")));
        assert_eq!(m.segments.len(), 2);
        assert!(m.segments[0].is_final, "first segment should be final");
        assert!(!m.segments[1].is_final, "second should still be partial");
    }

    #[test]
    fn mic_and_system_segments_are_independent() {
        let mut m = AppModel::new();
        m.ingest(Event::Result(r(SourceLabel::Mic, 1, "m1")));
        m.ingest(Event::Result(r(SourceLabel::System, 1, "s1")));
        m.ingest(Event::Result(r(SourceLabel::Mic, 1, "m1-updated")));
        assert_eq!(m.segments.len(), 2);
        assert_eq!(m.segments[0].text, "m1-updated");
        assert_eq!(m.segments[1].text, "s1");
        assert!(!m.segments[0].is_final);
        assert!(!m.segments[1].is_final);
    }

    #[test]
    fn active_segment_index_finds_newest_partial() {
        let mut m = AppModel::new();
        m.ingest(Event::Result(r(SourceLabel::Mic, 1, "a")));
        m.ingest(Event::Result(r(SourceLabel::Mic, 2, "b")));
        m.ingest(Event::Result(r(SourceLabel::System, 1, "c")));
        assert_eq!(m.active_segment_index(), Some(2));
    }

    #[test]
    fn duplicate_text_under_new_segment_id_merges() {
        let mut m = AppModel::new();
        m.ingest(Event::Result(r(
            SourceLabel::Mic,
            1,
            "おはようございます。",
        )));
        // SpeechAnalyzer re-emits the same utterance under segment 2 —
        // should merge into the existing row, not push a duplicate.
        m.ingest(Event::Result(r(
            SourceLabel::Mic,
            2,
            "おはようございます。",
        )));
        assert_eq!(m.segments.len(), 1, "duplicate text should be merged");
        assert_eq!(m.segments[0].id, 2, "merged row should adopt the new id");
        assert!(!m.segments[0].is_final, "still the active partial");
    }

    #[test]
    fn small_typo_revision_merges() {
        let mut m = AppModel::new();
        m.ingest(Event::Result(r(
            SourceLabel::Mic,
            1,
            "MaOSの標準機能を使って",
        )));
        // Same utterance, but SpeechAnalyzer corrects "MaOS" → "MacOS" and
        // routes it through a new segment ID.
        m.ingest(Event::Result(r(
            SourceLabel::Mic,
            2,
            "MacOSの標準機能を使って",
        )));
        assert_eq!(m.segments.len(), 1, "small typo revision should merge");
        assert!(m.segments[0].text.contains("MacOS"));
    }

    #[test]
    fn dissimilar_text_starts_a_new_segment() {
        let mut m = AppModel::new();
        m.ingest(Event::Result(r(SourceLabel::Mic, 1, "おはようございます")));
        m.ingest(Event::Result(r(
            SourceLabel::Mic,
            2,
            "今日はいい天気ですね",
        )));
        assert_eq!(m.segments.len(), 2, "different utterances should not merge");
        assert!(m.segments[0].is_final);
        assert!(!m.segments[1].is_final);
    }

    #[test]
    fn finalize_all_segments_locks_partial() {
        let mut m = AppModel::new();
        m.ingest(Event::Result(r(SourceLabel::Mic, 1, "途中…")));
        assert!(!m.segments[0].is_final);
        m.finalize_all_segments();
        assert!(m.segments[0].is_final);
    }

    #[test]
    fn log_buffer_is_bounded() {
        let mut m = AppModel::new();
        for i in 0..300 {
            m.ingest(Event::Log(format!("line {i}")));
        }
        assert!(m.recent_log.len() <= 200);
        assert!(m.recent_log.back().unwrap().contains("299"));
    }
}
