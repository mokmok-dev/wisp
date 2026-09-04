//! Serialization of UI state pushed into the web UI, and the [`UiBridge`]
//! entity that diffs + delivers those events.
//!
//! Rust → JS: events are JSON payloads broadcast on the [`EventBus`] and
//! delivered to the UI over the loopback server's SSE stream
//! (`GET /events`). The payload shapes are mirrored by
//! `apps/wisp-desktop/ui/src/types.ts`.

use std::hash::{Hash, Hasher};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};

use gpui::{App, Entity};
use serde::Serialize;
use wisp_audiokit::{PermissionStatus, SourceLabel};

use crate::app::{AppModel, Permissions, SessionState, View};

/// Fan-out hub for JSON events pushed from the host to connected web UIs
/// (one subscriber per SSE connection).
#[derive(Clone, Default)]
pub struct EventBus {
    inner: Arc<EventBusInner>,
}

#[derive(Default)]
struct EventBusInner {
    subscribers: Mutex<Vec<mpsc::Sender<String>>>,
}

impl EventBus {
    pub fn new() -> Self {
        Self::default()
    }

    /// Deliver one JSON payload to every live subscriber. Disconnected
    /// receivers are pruned.
    pub fn broadcast(
        &self,
        payload: &str,
    ) {
        let Ok(mut subscribers) = self.inner.subscribers.lock() else {
            return;
        };
        let payload = payload.to_owned();
        subscribers.retain(|tx| tx.send(payload.clone()).is_ok());
    }

    /// Register a new event stream. The returned channel receives every
    /// payload broadcast afterwards.
    pub fn subscribe(&self) -> mpsc::Receiver<String> {
        let (tx, rx) = mpsc::channel();
        if let Ok(mut subscribers) = self.inner.subscribers.lock() {
            subscribers.push(tx);
        }
        rx
    }
}

/// Static fallback so a serialization gap can never stall the UI.
const EMPTY_JSON: &str = "{}";

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct PermissionsDto {
    microphone: &'static str,
    speech: &'static str,
    pending: Option<&'static str>,
}

impl PermissionsDto {
    fn snapshot(permissions: Permissions) -> Self {
        Self {
            microphone: status_str(permissions.microphone),
            speech: status_str(permissions.speech),
            pending: permissions.pending.map(permission_str),
        }
    }
}

fn permission_str(perm: wisp_audiokit::Permission) -> &'static str {
    match perm {
        wisp_audiokit::Permission::Microphone => "microphone",
        wisp_audiokit::Permission::SpeechRecognition => "speech",
    }
}

fn status_str(status: PermissionStatus) -> &'static str {
    match status {
        PermissionStatus::Undetermined => "undetermined",
        PermissionStatus::Denied => "denied",
        PermissionStatus::Granted => "granted",
        PermissionStatus::Restricted => "restricted",
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct StateEvent<'a> {
    r#type: &'static str,
    view: &'static str,
    phase: &'static str,
    elapsed_ms: u64,
    microphone_muted: bool,
    permissions: PermissionsDto,
    live_title: &'a str,
    history_title: &'a str,
    history_session_id: Option<i64>,
    history_started_at: Option<String>,
    history_duration_seconds: Option<i64>,
    pending_persistence: bool,
    error: Option<String>,
    can_record: bool,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct SegmentDto {
    source: &'static str,
    id: u64,
    text: String,
    display_text: String,
    start_seconds: f64,
    end_seconds: f64,
    is_final: bool,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct TranscriptEvent {
    r#type: &'static str,
    segments: Vec<SegmentDto>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct SessionDto {
    id: i64,
    title: String,
    started_at: String,
    ended_at: Option<String>,
    duration_seconds: Option<i64>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct LibraryEvent {
    r#type: &'static str,
    sessions: Vec<SessionDto>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct NoticeEvent {
    r#type: &'static str,
    kind: &'static str,
    message: String,
}

fn view_str(view: &View) -> &'static str {
    match view {
        View::Library => "library",
        View::LiveSession => "live",
        View::History { .. } => "history",
    }
}

fn phase_str(state: SessionState) -> &'static str {
    match state {
        SessionState::Idle => "idle",
        SessionState::Starting => "starting",
        SessionState::Recording { .. } => "recording",
        SessionState::Stopping => "stopping",
        SessionState::Failed => "failed",
    }
}

fn elapsed_ms(state: SessionState) -> u64 {
    match state {
        SessionState::Recording { started_at } => {
            u64::try_from(started_at.elapsed().as_millis()).unwrap_or(u64::MAX)
        },
        _ => 0,
    }
}

fn source_str(source: SourceLabel) -> &'static str {
    match source {
        SourceLabel::Mic => "mic",
        SourceLabel::System => "system",
    }
}

fn state_event(app: &AppModel) -> StateEvent<'_> {
    let (history_session_id, history_started_at, history_duration_seconds) =
        match &app.viewed_session {
            Some(session) => {
                let duration = session
                    .ended_at
                    .map(|ended| (ended - session.started_at).num_seconds());
                (
                    Some(session.id.as_i64()),
                    Some(session.started_at.to_rfc3339()),
                    duration,
                )
            },
            None => (None, None, None),
        };
    StateEvent {
        r#type: "state",
        view: view_str(&app.view),
        phase: phase_str(app.state),
        elapsed_ms: elapsed_ms(app.state),
        microphone_muted: app.microphone_muted,
        permissions: PermissionsDto::snapshot(app.permissions),
        live_title: &app.live_title,
        history_title: app
            .viewed_session
            .as_ref()
            .map_or("", |session| session.title.as_str()),
        history_session_id,
        history_started_at,
        history_duration_seconds,
        pending_persistence: app.has_pending_persistence(),
        error: app.last_error.as_ref().map(ToString::to_string),
        can_record: app.setup_complete(),
    }
}

fn transcript_event(app: &AppModel) -> TranscriptEvent {
    TranscriptEvent {
        r#type: "transcript",
        segments: app
            .segments
            .iter()
            .map(|segment| SegmentDto {
                source: source_str(segment.source),
                id: segment.id,
                text: segment.text.clone(),
                display_text: segment.display_text.clone(),
                start_seconds: segment.start_seconds,
                end_seconds: segment.end_seconds,
                is_final: segment.is_final,
            })
            .collect(),
    }
}

fn library_event(app: &AppModel) -> LibraryEvent {
    LibraryEvent {
        r#type: "library",
        sessions: app
            .library
            .iter()
            .map(|session| SessionDto {
                id: session.id.as_i64(),
                title: session.title.clone(),
                started_at: session.started_at.to_rfc3339(),
                ended_at: session.ended_at.map(|ended| ended.to_rfc3339()),
                duration_seconds: session
                    .ended_at
                    .map(|ended| (ended - session.started_at).num_seconds()),
            })
            .collect(),
    }
}

/// Cheap content signature of the transcript; a change means the web UI
/// should receive a fresh `transcript` event.
fn transcript_signature(app: &AppModel) -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    app.segments.len().hash(&mut hasher);
    for segment in &app.segments {
        segment.source.as_str().hash(&mut hasher);
        segment.id.hash(&mut hasher);
        segment.text.hash(&mut hasher);
        segment.is_final.hash(&mut hasher);
    }
    hasher.finish()
}

/// Cheap content signature of the library list.
fn library_signature(app: &AppModel) -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    app.library.len().hash(&mut hasher);
    for session in &app.library {
        session.id.as_i64().hash(&mut hasher);
        session.title.hash(&mut hasher);
        session.ended_at.hash(&mut hasher);
    }
    hasher.finish()
}

/// Bridges `AppModel` state into the web UI. Lives on the main thread as a
/// GPUI entity; payloads are fanned out over the [`EventBus`] to the
/// loopback server's SSE connections.
pub struct UiBridge {
    events: EventBus,
    transcript_signature: u64,
    library_signature: u64,
    booted: bool,
}

impl UiBridge {
    pub fn new(events: EventBus) -> Self {
        Self {
            events,
            transcript_signature: 0,
            library_signature: 0,
            booted: false,
        }
    }

    /// Send everything the UI needs after its event stream connects.
    /// Establishes the diff baselines, so later `push_changes` calls only
    /// send deltas.
    pub fn push_full_snapshot(
        &mut self,
        model: &Entity<AppModel>,
        cx: &mut App,
    ) {
        let app = model.read(cx);
        self.booted = true;
        self.transcript_signature = transcript_signature(app);
        self.library_signature = library_signature(app);
        self.emit(&state_event(app));
        self.emit(&transcript_event(app));
        self.emit(&library_event(app));
    }

    /// Push only what changed since the last call. No-ops until the web UI
    /// has booted; the full snapshot covers the pre-boot window.
    pub fn push_changes(
        &mut self,
        model: &Entity<AppModel>,
        cx: &mut App,
    ) {
        if !self.booted {
            return;
        }
        let app = model.read(cx);
        self.emit(&state_event(app));

        let transcript = transcript_signature(app);
        if transcript != self.transcript_signature {
            self.transcript_signature = transcript;
            self.emit(&transcript_event(app));
        }

        let library = library_signature(app);
        if library != self.library_signature {
            self.library_signature = library;
            self.emit(&library_event(app));
        }
    }

    /// One-off user-facing notice (copy results, export hints, …).
    pub fn notify(
        &self,
        kind: &'static str,
        message: impl Into<String>,
    ) {
        self.emit(&NoticeEvent {
            r#type: "notice",
            kind,
            message: message.into(),
        });
    }

    fn emit<E: Serialize>(
        &self,
        event: &E,
    ) {
        let json = serde_json::to_string(event).unwrap_or_else(|_| EMPTY_JSON.to_owned());
        self.events.broadcast(&json);
    }
}
