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
use std::path::PathBuf;
use std::time::Instant;

use chrono::{DateTime, Utc};
use wisp_audiokit::{
    Event, Permission, PermissionStatus, SessionConfig, SessionError, SessionResult, SourceLabel,
    TranscriptionPolicy,
};
use wisp_core::{MeetingTool, Session as StoredSession, SessionId};
use wisp_lifecycle::{Phase, UpdateContext, ViewOwner, WorkerUpdate, can_replace_transcript};

#[derive(Debug, Clone)]
pub enum AppError {
    Audio(SessionError),
    Persistence(String),
}

impl From<SessionError> for AppError {
    fn from(error: SessionError) -> Self {
        Self::Audio(error)
    }
}

impl std::fmt::Display for AppError {
    fn fmt(
        &self,
        formatter: &mut std::fmt::Formatter<'_>,
    ) -> std::fmt::Result {
        match self {
            Self::Audio(error) => std::fmt::Display::fmt(error, formatter),
            Self::Persistence(error) => {
                write!(formatter, "session history persistence failed: {error}")
            },
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionState {
    Idle,
    Starting,
    Recording { started_at: Instant },
    Stopping,
    Failed,
}

impl SessionState {
    #[must_use]
    pub const fn phase(self) -> Phase {
        match self {
            Self::Idle => Phase::Idle,
            Self::Starting => Phase::Starting,
            Self::Recording { .. } => Phase::Recording,
            Self::Stopping => Phase::Stopping,
            Self::Failed => Phase::Failed,
        }
    }

    /// Whether an audio session is running or changing lifecycle state.
    /// While this is true the live transcript is the persistence source and
    /// must not be replaced by another view's segments.
    #[must_use]
    pub const fn is_active(self) -> bool {
        self.phase().is_active()
    }

    #[must_use]
    pub fn with_phase(
        phase: Phase,
        recording_started_at: Instant,
    ) -> Self {
        match phase {
            Phase::Idle => Self::Idle,
            Phase::Starting => Self::Starting,
            Phase::Recording => Self::Recording {
                started_at: recording_started_at,
            },
            Phase::Stopping => Self::Stopping,
            Phase::Failed => Self::Failed,
        }
    }

    #[must_use]
    pub const fn request_stop(self) -> Self {
        match self.phase().request_stop() {
            Phase::Stopping => Self::Stopping,
            Phase::Idle => Self::Idle,
            Phase::Starting => Self::Starting,
            Phase::Recording => self,
            Phase::Failed => Self::Failed,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PendingSessionWrite {
    Finalise { ended_at: DateTime<Utc> },
    Delete,
}

/// Which top-level screen the desktop UI is currently showing.
///
///   - `Library`: list of past sessions with a "New Session" button.
///   - `LiveSession`: the recording UI (idle, recording, or just-stopped
///     state). New sessions land here before a record press.
///   - `History`: read-only view of a past session loaded from storage.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum View {
    Library,
    LiveSession,
    History {
        session_id: SessionId,
    },
    /// Preferences. Opening it never discards a settling session, and
    /// `view_owner` keeps a live recording's updates flowing while it is
    /// shown.
    Settings,
}

/// A detected meeting waiting for the user's answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MeetingPrompt {
    pub tool: MeetingTool,
}

/// Snapshot of every permission Wisp gates Record on.
///
/// The UI is allowed to enter the main transcript view once both fields
/// are `Granted`. While `pending` is `Some(p)`, a previous
/// `request_permission(p)` call is still waiting on the OS dialog — the
/// onboarding row for that permission shows a spinner instead of a button.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Permissions {
    pub microphone: PermissionStatus,
    pub speech: PermissionStatus,
    pub pending: Option<Permission>,
}

impl Permissions {
    pub fn unknown() -> Self {
        Self {
            microphone: PermissionStatus::Undetermined,
            speech: PermissionStatus::Undetermined,
            pending: None,
        }
    }

    /// Whether the minimum permissions required by this session policy are
    /// available. macOS may record after Speech Recognition is explicitly
    /// denied/restricted only when policy permits record-only fallback.
    pub fn satisfies(
        self,
        policy: TranscriptionPolicy,
    ) -> bool {
        if !self.microphone.is_granted() {
            return false;
        }
        if self.speech.is_granted() {
            return true;
        }
        cfg!(target_os = "macos")
            && policy.allow_record_only
            && matches!(
                self.speech,
                PermissionStatus::Denied | PermissionStatus::Restricted
            )
    }

    pub fn set_status(
        &mut self,
        perm: Permission,
        status: PermissionStatus,
    ) {
        match perm {
            Permission::Microphone => self.microphone = status,
            Permission::SpeechRecognition => self.speech = status,
        }
    }
}

#[derive(Debug, Clone)]
pub struct Segment {
    pub source: SourceLabel,
    /// Monotonic-per-source identifier emitted by the Swift transcription
    /// pipeline. Same id while the segment is being revised; a new id
    /// means the previous one is finalised.
    pub id: u64,
    pub text: String,
    /// Pre-rendered transcript body (sentence breaks applied). Kept in
    /// sync with `text` so long history views don't re-walk every segment
    /// on each frame.
    pub display_text: String,
    pub start_seconds: f64,
    pub end_seconds: f64,
    /// True once the speech engine reports that it has stopped revising this
    /// segment.
    pub is_final: bool,
}

/// Insert a `\n` after each sentence-ending 。 *except* the trailing
/// one — that way the partial line doesn't visibly break the moment the
/// punctuation is recognised; the break only appears once the next
/// sentence starts arriving.
pub fn break_on_sentence_end(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 8);
    let mut iter = text.chars().peekable();
    while let Some(c) = iter.next() {
        out.push(c);
        if c == '。' && iter.peek().is_some() {
            out.push('\n');
        }
    }
    out
}

impl Segment {
    fn refresh_display(&mut self) {
        self.display_text = break_on_sentence_end(&self.text);
    }
}

#[derive(Debug)]
pub struct AppModel {
    pub state: SessionState,
    /// Whether live microphone samples are currently replaced with silence.
    pub microphone_muted: bool,
    pub view: View,
    pub segments: Vec<Segment>,
    /// All persisted sessions, newest first. Populated on launch and
    /// refreshed whenever a recording finishes or the user returns to the
    /// library.
    pub library: Vec<StoredSession>,
    /// Open database row owned by an active or not-yet-persisted recording.
    /// Cleared only when finalisation commits or an unstarted row is deleted.
    pub current_session_id: Option<SessionId>,
    /// Persisted row associated with the transcript currently shown in the
    /// live view. Unlike the open handle, this remains after a successful
    /// finalisation so IPC/export metadata cannot drift to another session.
    pub linked_session_id: Option<SessionId>,
    /// A failed storage operation that must be retried before the live
    /// transcript can be discarded or another session can start.
    pub pending_session_write: Option<PendingSessionWrite>,
    /// Launch metadata retained until the transcript transaction commits.
    /// It lets a stop/retry create the database row if the initial `Started`
    /// update could not acquire or write storage.
    pub current_session_started_at: Option<DateTime<Utc>>,
    pub current_session_dir_name: Option<String>,
    /// Per-run audio directory. Retained with the transcript after a storage
    /// failure so a durable recovery snapshot can be written beside the Ogg files.
    pub current_output_dir: Option<PathBuf>,
    /// The session being viewed in `View::History`, kept around so the
    /// header can render its title without re-querying.
    pub viewed_session: Option<StoredSession>,
    /// Working title for the live session. Edited in the live top bar (a
    /// `title_input::TitleInput`) before and during recording; the empty
    /// string means "fall back to the recorded-at timestamp" when the row is
    /// created.
    pub live_title: String,
    pub recent_log: VecDeque<String>,
    pub last_error: Option<AppError>,
    pub permissions: Permissions,
    /// User-typed transcript search. Empty shows every session.
    pub library_query: String,
    /// Session ids matching [`library_query`], in relevance order. `None`
    /// means no query is active and the library is unfiltered.
    pub library_hits: Option<Vec<SessionId>>,
    /// Session waiting for the second click on Delete.
    pub pending_delete: Option<SessionId>,
    /// A detected meeting waiting for the user's answer.
    pub meeting_prompt: Option<MeetingPrompt>,
    /// The screen Settings was opened from, so closing returns there without
    /// touching a live session.
    pub pre_settings_view: Option<View>,
}

impl AppModel {
    fn view_owner(&self) -> ViewOwner {
        match self.view {
            View::Library => ViewOwner::Library,
            // Settings owns no session; a live recording keeps ingesting while
            // the user reads preferences.
            View::LiveSession | View::Settings => ViewOwner::Live,
            View::History { .. } => ViewOwner::History,
        }
    }

    #[must_use]
    pub fn accepts_worker_update(
        &self,
        session_id: SessionId,
    ) -> bool {
        UpdateContext {
            phase: self.state.phase(),
            view: self.view_owner(),
            current_session_id: self.current_session_id.map(SessionId::as_i64),
        }
        .accepts(session_id.as_i64())
    }

    #[must_use]
    pub fn worker_update_phase(
        &self,
        session_id: SessionId,
        update: WorkerUpdate,
    ) -> Option<Phase> {
        UpdateContext {
            phase: self.state.phase(),
            view: self.view_owner(),
            current_session_id: self.current_session_id.map(SessionId::as_i64),
        }
        .next_phase(session_id.as_i64(), update)
    }

    pub fn new() -> Self {
        Self {
            state: SessionState::Idle,
            microphone_muted: false,
            view: View::Library,
            segments: Vec::new(),
            library: Vec::new(),
            current_session_id: None,
            linked_session_id: None,
            pending_session_write: None,
            current_session_started_at: None,
            current_session_dir_name: None,
            current_output_dir: None,
            viewed_session: None,
            live_title: String::new(),
            recent_log: VecDeque::new(),
            last_error: None,
            permissions: Permissions::unknown(),
            library_query: String::new(),
            library_hits: None,
            pending_delete: None,
            meeting_prompt: None,
            pre_settings_view: None,
        }
    }

    /// Whether the live transcript still owns worker or persistence state.
    /// A retained database handle after a failed finalization is deliberately
    /// treated as unsettled so navigation cannot silently discard it.
    pub fn has_unsettled_session(&self) -> bool {
        !can_replace_transcript(
            self.state.phase(),
            self.pending_session_write.is_some(),
            self.current_output_dir.is_some(),
        )
    }

    /// A stopped worker whose transcript transaction needs to be retried.
    pub fn has_pending_persistence(&self) -> bool {
        matches!(self.state, SessionState::Failed) && self.pending_session_write.is_some()
    }

    /// Replace the cached library list. Called after storage reads (launch,
    /// recording end, post-delete).
    pub fn set_library(
        &mut self,
        sessions: Vec<StoredSession>,
    ) {
        self.library = sessions;
    }

    /// Replace the active transcript search. An empty query clears the
    /// filter so the library shows every session again.
    pub fn set_library_search(
        &mut self,
        query: String,
        hits: Option<Vec<SessionId>>,
    ) {
        self.library_query = query;
        self.library_hits = hits;
    }

    /// Drop the last failure so its banner disappears. The session state is
    /// left untouched; a failed live view still offers Retry.
    pub fn clear_error(&mut self) {
        self.last_error = None;
    }

    /// Arm the inline delete confirmation for one library row.
    pub fn request_delete(
        &mut self,
        session_id: SessionId,
    ) {
        self.pending_delete = Some(session_id);
    }

    pub fn cancel_delete(&mut self) {
        self.pending_delete = None;
    }

    /// Show the preferences page. Unlike the other `show_*` methods this never
    /// consults [`Self::has_unsettled_session`]: opening settings discards
    /// nothing, and closing returns to the screen it was opened from.
    pub fn show_settings(&mut self) {
        if !matches!(self.view, View::Settings) {
            self.pre_settings_view = Some(self.view.clone());
            self.view = View::Settings;
        }
    }

    /// Return to the screen Settings was opened from. This bypasses the
    /// `show_*` guards so a user who opened Settings during a recording can
    /// always get back to it.
    pub fn close_settings(&mut self) {
        self.view = self.pre_settings_view.take().unwrap_or(View::Library);
    }

    /// Drop a detected-meeting prompt without changing detection state; the
    /// detector's snooze already prevents this call from being offered again.
    pub fn dismiss_meeting_prompt(&mut self) {
        self.meeting_prompt = None;
    }

    /// Move to the library screen and drop any live/historical segments so
    /// the next view enter starts from a clean slate. Navigation is ignored
    /// while a worker session is active; otherwise its future events could be
    /// attached to the library or to a historical transcript.
    pub fn show_library(&mut self) {
        if self.has_unsettled_session() {
            return;
        }
        self.view = View::Library;
        self.microphone_muted = false;
        self.segments.clear();
        self.viewed_session = None;
        self.live_title = String::new();
        self.current_session_id = None;
        self.linked_session_id = None;
        self.pending_session_write = None;
        self.current_session_started_at = None;
        self.current_session_dir_name = None;
        self.current_output_dir = None;
        self.last_error = None;
        self.pending_delete = None;
        self.pre_settings_view = None;
    }

    /// Move to the live recording screen in idle state. Used by the
    /// library's "New Session" button. An active session owns this view and
    /// cannot be replaced with a new one until it settles.
    pub fn show_new_session(&mut self) {
        if self.has_unsettled_session() {
            return;
        }
        self.view = View::LiveSession;
        self.state = SessionState::Idle;
        self.microphone_muted = false;
        self.segments.clear();
        self.viewed_session = None;
        self.live_title = String::new();
        self.current_session_id = None;
        self.linked_session_id = None;
        self.pending_session_write = None;
        self.current_session_started_at = None;
        self.current_session_dir_name = None;
        self.current_output_dir = None;
        self.last_error = None;
        self.pending_delete = None;
        self.pre_settings_view = None;
    }

    /// Move to a historical session's read-only transcript view. The
    /// caller is responsible for populating `segments` from storage.
    pub fn show_history(
        &mut self,
        session: StoredSession,
        segments: Vec<Segment>,
    ) {
        if self.has_unsettled_session() {
            return;
        }
        self.view = View::History {
            session_id: session.id,
        };
        self.microphone_muted = false;
        self.segments = segments;
        self.viewed_session = Some(session);
        self.current_session_id = None;
        self.linked_session_id = None;
        self.pending_session_write = None;
        self.current_session_started_at = None;
        self.current_session_dir_name = None;
        self.current_output_dir = None;
        self.pending_delete = None;
        self.pre_settings_view = None;
        // Historical segments are already finalized.
        self.finalize_all_segments();
        for seg in &mut self.segments {
            seg.refresh_display();
        }
    }

    /// Normalize any terminal screen to a fresh live transcript, then enter
    /// the Starting phase. Global shortcuts call this before enqueueing Start
    /// so Library/History can never remain the owner of live worker events.
    #[must_use]
    pub fn begin_session_start(&mut self) -> bool {
        if self.has_unsettled_session() {
            return false;
        }
        self.show_new_session();
        self.state = SessionState::Starting;
        self.microphone_muted = false;
        self.meeting_prompt = None;
        true
    }

    /// Compatibility wrapper for callers that do not need the acceptance
    /// result. Prefer `begin_session_start` before enqueueing worker commands.
    pub fn begin_session(&mut self) {
        let _ = self.begin_session_start();
    }

    pub fn set_state(
        &mut self,
        state: SessionState,
    ) {
        self.state = state;
        if state.is_active() {
            self.meeting_prompt = None;
        }
        if matches!(state, SessionState::Idle | SessionState::Failed) {
            self.microphone_muted = false;
        }
    }

    pub fn fail<E>(
        &mut self,
        error: E,
    ) where
        E: Into<AppError>,
    {
        self.last_error = Some(error.into());
        self.state = SessionState::Failed;
        self.microphone_muted = false;
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
                seg.is_final = result.is_final;
                seg.refresh_display();
                return;
            }
            // Different segment ID for the same source. If the text looks
            // like a continuation/refinement of the previous segment,
            // overwrite in place instead of creating a visually duplicated
            // row. Otherwise the previous segment is locked in and we push
            // a fresh one.
            if !seg.is_final && looks_like_continuation(&seg.text, &result.text) {
                seg.id = result.segment_id;
                seg.text = result.text;
                seg.start_seconds = result.start_seconds;
                seg.end_seconds = result.end_seconds;
                seg.is_final = result.is_final;
                seg.refresh_display();
                return;
            }
            seg.is_final = true;
            break;
        }
        let mut segment = Segment {
            source: result.source,
            id: result.segment_id,
            text: result.text,
            display_text: String::new(),
            start_seconds: result.start_seconds,
            end_seconds: result.end_seconds,
            is_final: result.is_final,
        };
        segment.refresh_display();
        self.segments.push(segment);
    }

    /// The most recent non-final segment, if any.
    ///
    /// Kept for `formal`/lifecycle reasoning and tests; the web UI renders
    /// partial segments based on `is_final` directly.
    #[cfg(test)]
    pub fn active_segment_index(&self) -> Option<usize> {
        self.segments
            .iter()
            .enumerate()
            .rev()
            .find_map(|(i, s)| (!s.is_final).then_some(i))
    }

    pub fn setup_complete(&self) -> bool {
        self.permissions
            .satisfies(TranscriptionPolicy::platform_default())
    }

    pub fn session_config(locale: impl Into<String>) -> SessionConfig {
        SessionConfig::platform_default(locale)
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
    use super::{break_on_sentence_end, *};

    #[test]
    fn breaks_between_sentences_but_not_at_trailing_period() {
        assert_eq!(
            break_on_sentence_end("一文目。二文目。"),
            "一文目。\n二文目。"
        );
    }

    #[test]
    fn passes_through_text_without_period() {
        assert_eq!(break_on_sentence_end("途中"), "途中");
    }

    #[test]
    fn preserves_text_with_only_a_trailing_period() {
        assert_eq!(break_on_sentence_end("こんにちは。"), "こんにちは。");
    }

    fn r(
        source: SourceLabel,
        seg: u64,
        text: &str,
    ) -> SessionResult {
        SessionResult {
            source,
            segment_id: seg,
            is_final: false,
            text: text.into(),
            start_seconds: 0.0,
            end_seconds: 0.0,
            confidence_mean: None,
            confidence_min: None,
        }
    }

    fn stored_session(id: i64) -> StoredSession {
        let started_at = chrono::Utc::now();
        StoredSession {
            id: SessionId::from(id),
            started_at,
            ended_at: Some(started_at),
            title: format!("session {id}"),
            mic_wav_path: format!("session-{id}/mic.ogg"),
            system_wav_path: format!("session-{id}/system.ogg"),
            notes: String::new(),
        }
    }

    fn historical_segment(text: &str) -> Segment {
        Segment {
            source: SourceLabel::Mic,
            id: 0,
            text: text.into(),
            display_text: text.into(),
            start_seconds: 0.0,
            end_seconds: 1.0,
            is_final: true,
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
    fn speech_engine_final_result_locks_segment_immediately() {
        let mut m = AppModel::new();
        let mut result = r(SourceLabel::Mic, 1, "final transcript");
        result.is_final = true;

        m.ingest(Event::Result(result));

        assert_eq!(m.segments.len(), 1);
        assert!(m.segments[0].is_final);
        assert_eq!(m.active_segment_index(), None);
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
    fn active_recording_cannot_replace_live_transcript_with_history() {
        let mut m = AppModel::new();
        m.show_new_session();
        m.state = SessionState::Recording {
            started_at: Instant::now(),
        };
        m.current_session_id = Some(SessionId::from(10));
        m.ingest(Event::Result(r(SourceLabel::Mic, 1, "live transcript")));

        m.show_library();
        assert_eq!(m.view, View::LiveSession);
        assert_eq!(m.current_session_id, Some(SessionId::from(10)));
        assert_eq!(m.segments.len(), 1);
        assert_eq!(m.segments[0].text, "live transcript");

        m.show_history(
            stored_session(20),
            vec![historical_segment("old transcript")],
        );
        assert_eq!(m.view, View::LiveSession);
        assert_eq!(m.current_session_id, Some(SessionId::from(10)));
        assert_eq!(m.segments.len(), 1);
        assert_eq!(m.segments[0].text, "live transcript");

        m.show_new_session();
        assert!(matches!(m.state, SessionState::Recording { .. }));
        assert_eq!(m.current_session_id, Some(SessionId::from(10)));
        assert_eq!(m.segments[0].text, "live transcript");
    }

    #[test]
    fn stopping_session_keeps_live_transcript_until_persistence() {
        let mut m = AppModel::new();
        m.show_new_session();
        m.state = SessionState::Stopping;
        m.current_session_id = Some(SessionId::from(10));
        m.ingest(Event::Result(r(SourceLabel::Mic, 1, "flushed transcript")));

        m.show_library();

        assert_eq!(m.view, View::LiveSession);
        assert_eq!(m.current_session_id, Some(SessionId::from(10)));
        assert_eq!(m.segments[0].text, "flushed transcript");
    }

    #[test]
    fn failed_persistence_keeps_live_transcript_protected_for_retry() {
        let mut m = AppModel::new();
        m.show_new_session();
        m.state = SessionState::Failed;
        m.current_session_id = Some(SessionId::from(10));
        m.pending_session_write = Some(PendingSessionWrite::Finalise {
            ended_at: chrono::Utc::now(),
        });
        m.segments.push(historical_segment("not persisted yet"));

        m.show_library();
        m.show_history(stored_session(20), vec![historical_segment("old")]);
        m.show_new_session();
        m.begin_session();

        assert_eq!(m.view, View::LiveSession);
        assert_eq!(m.state, SessionState::Failed);
        assert_eq!(m.current_session_id, Some(SessionId::from(10)));
        assert_eq!(m.segments[0].text, "not persisted yet");
    }

    #[test]
    fn begin_session_normalizes_history_to_fresh_live_view() {
        let mut m = AppModel::new();
        m.show_history(
            stored_session(20),
            vec![historical_segment("old transcript")],
        );

        m.begin_session();

        assert_eq!(m.view, View::LiveSession);
        assert_eq!(m.state, SessionState::Starting);
        assert!(
            m.segments.is_empty(),
            "a new live session starts with an empty transcript"
        );
        assert!(m.viewed_session.is_none());
        assert!(m.current_session_id.is_none());
    }

    #[test]
    fn active_session_rejects_top_level_navigation() {
        let active_states = [
            SessionState::Starting,
            SessionState::Recording {
                started_at: std::time::Instant::now(),
            },
            SessionState::Stopping,
        ];

        for state in active_states {
            let mut m = AppModel::new();
            m.show_new_session();
            m.ingest(Event::Result(r(SourceLabel::Mic, 1, "live")));
            m.current_session_id = Some(SessionId::from(42));
            m.state = state;

            m.show_library();
            m.show_new_session();
            m.show_history(stored_session(7), Vec::new());

            assert_eq!(m.view, View::LiveSession);
            assert_eq!(m.state, state);
            assert_eq!(m.current_session_id, Some(SessionId::from(42)));
            assert_eq!(m.segments.len(), 1);
            assert_eq!(m.segments[0].text, "live");
        }
    }

    #[test]
    fn settings_view_keeps_accepting_the_live_session() {
        let mut m = AppModel::new();
        let started_at = Instant::now();
        m.show_new_session();
        m.state = SessionState::Recording { started_at };
        m.current_session_id = Some(SessionId::from(10));

        m.show_settings();

        assert_eq!(m.view, View::Settings);
        assert!(m.accepts_worker_update(SessionId::from(10)));
        assert_eq!(m.state, SessionState::Recording { started_at });
    }

    #[test]
    fn closing_settings_returns_to_the_screen_it_opened_from() {
        let mut m = AppModel::new();
        m.show_history(
            stored_session(20),
            vec![historical_segment("old transcript")],
        );

        m.show_settings();
        m.close_settings();

        assert_eq!(
            m.view,
            View::History {
                session_id: SessionId::from(20)
            }
        );
        assert_eq!(m.segments.len(), 1);
        assert_eq!(m.segments[0].text, "old transcript");
    }

    #[test]
    fn settings_never_discards_a_settling_session() {
        let mut m = AppModel::new();
        m.show_new_session();
        m.state = SessionState::Failed;
        m.current_session_id = Some(SessionId::from(10));
        m.pending_session_write = Some(PendingSessionWrite::Finalise {
            ended_at: chrono::Utc::now(),
        });
        m.segments.push(historical_segment("not persisted yet"));

        m.show_settings();
        m.close_settings();

        assert!(m.has_unsettled_session());
        assert_eq!(m.segments.len(), 1);
        assert_eq!(m.current_session_id, Some(SessionId::from(10)));
    }

    #[test]
    fn opening_settings_twice_keeps_the_first_origin() {
        let mut m = AppModel::new();
        m.show_library();
        m.show_settings();
        m.show_settings();

        m.close_settings();

        assert_eq!(m.view, View::Library);
    }

    #[test]
    fn global_start_normalizes_history_to_a_fresh_live_view() {
        let mut m = AppModel::new();
        m.show_history(stored_session(7), Vec::new());
        assert!(matches!(m.view, View::History { .. }));

        assert!(m.begin_session_start());

        assert_eq!(m.view, View::LiveSession);
        assert_eq!(m.state, SessionState::Starting);
        assert!(
            m.segments.is_empty(),
            "a new live session starts with an empty transcript"
        );
        assert!(m.viewed_session.is_none());
        assert!(m.current_session_id.is_none());
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

    #[test]
    fn navigation_resets_the_live_title() {
        let mut m = AppModel::new();
        m.live_title = "Q3 planning".into();
        m.show_library();
        assert_eq!(m.live_title, "");

        m.live_title = "Q3 planning".into();
        m.show_new_session();
        assert_eq!(m.live_title, "");
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_permission_gate_exposes_explicit_record_only_fallback() {
        let permissions = Permissions {
            microphone: PermissionStatus::Granted,
            speech: PermissionStatus::Denied,
            pending: None,
        };
        assert!(permissions.satisfies(TranscriptionPolicy::platform_default()));
        assert!(!permissions.satisfies(TranscriptionPolicy {
            allow_record_only: false,
            ..TranscriptionPolicy::platform_default()
        }));

        let undetermined = Permissions {
            speech: PermissionStatus::Undetermined,
            ..permissions
        };
        assert!(
            !undetermined.satisfies(TranscriptionPolicy::platform_default()),
            "the onboarding screen should still offer the Speech prompt before denial"
        );
    }
}
