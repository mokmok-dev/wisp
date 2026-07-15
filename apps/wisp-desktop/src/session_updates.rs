//! Apply `SessionRunner` updates to `AppModel` and persist at session boundaries.

use chrono::Utc;

use crate::app::{AppError, AppModel, PendingSessionWrite, SessionState};
use crate::library;
use crate::library::SharedStorage;
use crate::session_runner::Update;
use crate::session_runner::Update::{Error, Event, StartFailed, Started, Stopped};
use crate::transcript_view::now;

pub fn apply_update(
    update: Update,
    model: &mut AppModel,
    storage: &SharedStorage,
) {
    match update {
        Started(session) => {
            if model.current_session_id == Some(session.session_id)
                && model.state == SessionState::Starting
            {
                model.set_state(SessionState::Recording { started_at: now() });
            }
        },
        Event { session_id, event } => {
            if model.current_session_id == Some(session_id) && model.state.is_active() {
                model.ingest(event);
            }
        },
        Stopped { session_id } => {
            if model.current_session_id != Some(session_id) || !model.state.is_active() {
                return;
            }
            model.finalize_all_segments();
            match persist_finished_session(model, storage) {
                Ok(()) => model.set_state(SessionState::Idle),
                Err(error) => model.fail(AppError::Persistence(error)),
            }
        },
        StartFailed { session_id, error } => {
            if model.current_session_id != Some(session_id) || !model.state.is_active() {
                return;
            }
            model.finalize_all_segments();
            match persist_finished_session(model, storage) {
                Ok(()) => model.fail(error),
                Err(persistence_error) => model.fail(AppError::Persistence(format!(
                    "{persistence_error}; audio error: {error}"
                ))),
            }
        },
        Error { session_id, error } => {
            if model.current_session_id != Some(session_id) || !model.state.is_active() {
                return;
            }
            model.pending_session_write = Some(PendingSessionWrite::Delete);
            match retry_pending_session_write(model, storage) {
                Ok(()) => model.fail(error),
                Err(deletion_error) => model.fail(AppError::Persistence(format!(
                    "could not remove failed session {session_id}: {deletion_error}; audio error: {error}"
                ))),
            }
        },
    }
}

pub(crate) fn persist_finished_session(
    model: &mut AppModel,
    storage: &SharedStorage,
) -> Result<(), String> {
    match model.pending_session_write {
        None => {
            model.pending_session_write = Some(PendingSessionWrite::Finalise {
                ended_at: Utc::now(),
            });
        },
        Some(PendingSessionWrite::Finalise { .. }) => {},
        Some(PendingSessionWrite::Delete) => {
            return Err("the session is awaiting cleanup, not finalisation".into());
        },
    }
    retry_pending_session_write(model, storage)
}

pub(crate) fn retry_pending_session_write(
    model: &mut AppModel,
    storage: &SharedStorage,
) -> Result<(), String> {
    let Some(pending_write) = model.pending_session_write else {
        return Ok(());
    };
    let session_id = model
        .current_session_id
        .ok_or_else(|| "the pending session has no database id".to_string())?;
    let store = storage.lock().map_err(|error| error.to_string())?;
    match pending_write {
        PendingSessionWrite::Finalise { ended_at } => {
            library::finalise_session(&store, session_id, &model.segments, ended_at)
                .map_err(|error| error.to_string())?;
        },
        PendingSessionWrite::Delete => store
            .sessions()
            .delete(session_id)
            .map_err(|error| error.to_string())?,
    }
    let library = store.sessions().list().ok();
    model.pending_session_write = None;
    if pending_write == PendingSessionWrite::Delete {
        model.current_session_id = None;
    }
    if let Some(list) = library {
        model.set_library(list);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use chrono::{TimeZone, Utc};
    use wisp_audiokit::{Event as AudioEvent, SessionError, SessionResult, SourceLabel};
    use wisp_core::SessionId;
    use wisp_storage::Storage;

    use super::{apply_update, retry_pending_session_write};
    use crate::app::{AppModel, PendingSessionWrite, Segment, SessionState, View};
    use crate::library;
    use crate::session_runner::{SessionStart, Update};

    fn segment(text: &str) -> Segment {
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

    fn result(
        id: u64,
        text: &str,
    ) -> AudioEvent {
        AudioEvent::Result(SessionResult {
            source: SourceLabel::Mic,
            segment_id: id,
            text: text.into(),
            start_seconds: 0.0,
            end_seconds: 1.0,
        })
    }

    fn construction_error(session_id: SessionId) -> Update {
        Update::Error {
            session_id,
            error: SessionError::Construction,
        }
    }

    #[test]
    fn active_transcript_cannot_be_replaced_and_is_persisted_to_its_session() {
        let storage = Arc::new(Mutex::new(Storage::open_in_memory().expect("open storage")));
        let old_started = Utc.with_ymd_and_hms(2026, 7, 15, 10, 0, 0).unwrap();
        let (old_session, old_segments) = {
            let store = storage.lock().expect("lock storage");
            let old_id =
                library::create_session(&store, old_started, "old").expect("create old session");
            library::finalise_session(&store, old_id, &[segment("old transcript")], old_started)
                .expect("finalise old session");
            let old_session = store
                .sessions()
                .get(old_id)
                .expect("get old session")
                .expect("old session exists");
            let old_segments = library::load_history(&store, old_id).expect("load old history");
            (old_session, old_segments)
        };
        let old_id = old_session.id;

        let active_started = Utc.with_ymd_and_hms(2026, 7, 15, 11, 0, 0).unwrap();
        let active_id = {
            let store = storage.lock().expect("lock storage");
            library::create_session(&store, active_started, "active")
                .expect("create active session")
        };
        let mut model = AppModel::new();
        model.begin_session();
        model.current_session_id = Some(active_id);
        apply_update(
            Update::Started(SessionStart {
                session_id: active_id,
            }),
            &mut model,
            &storage,
        );
        apply_update(
            Update::Event {
                session_id: active_id,
                event: result(1, "live head"),
            },
            &mut model,
            &storage,
        );

        // This was the corrupting path: navigation replaced the live buffer
        // with the old history while the runner continued producing events.
        apply_update(
            Update::Event {
                session_id: old_id,
                event: result(99, "stale transcript"),
            },
            &mut model,
            &storage,
        );
        model.show_library();
        model.show_history(old_session, old_segments);
        assert_eq!(model.view, View::LiveSession);
        assert_eq!(model.segments[0].text, "live head");

        model.set_state(SessionState::Stopping);
        model.show_library();
        apply_update(Update::Stopped { session_id: old_id }, &mut model, &storage);
        assert_eq!(model.state, SessionState::Stopping);
        apply_update(
            Update::Event {
                session_id: active_id,
                event: result(2, "live tail"),
            },
            &mut model,
            &storage,
        );
        apply_update(
            Update::Stopped {
                session_id: active_id,
            },
            &mut model,
            &storage,
        );
        apply_update(construction_error(active_id), &mut model, &storage);

        assert_eq!(model.state, SessionState::Idle);
        assert_eq!(model.current_session_id, Some(active_id));
        let store = storage.lock().expect("lock storage");
        let active_text = store
            .segments()
            .list_by_session(active_id)
            .expect("load active transcript")
            .into_iter()
            .map(|segment| segment.text)
            .collect::<Vec<_>>();
        assert_eq!(active_text, ["live head", "live tail"]);
        let old_text = store
            .segments()
            .list_by_session(old_id)
            .expect("reload old transcript")
            .into_iter()
            .map(|segment| segment.text)
            .collect::<Vec<_>>();
        assert_eq!(old_text, ["old transcript"]);

        let stored_active = store
            .sessions()
            .get(active_id)
            .expect("get active session")
            .expect("active session exists");
        assert_eq!(stored_active.started_at, active_started);
        assert_eq!(stored_active.mic_wav_path, "recordings/active/mic.wav");
        assert!(stored_active.ended_at.is_some());
    }

    #[test]
    fn stale_lifecycle_updates_do_not_mutate_the_current_session() {
        let storage = Arc::new(Mutex::new(Storage::open_in_memory().expect("open storage")));
        let started_at = Utc.with_ymd_and_hms(2026, 7, 15, 12, 0, 0).unwrap();
        let (stale_id, current_id) = {
            let store = storage.lock().expect("lock storage");
            let stale_id =
                library::create_session(&store, started_at, "stale").expect("create stale");
            let current_id =
                library::create_session(&store, started_at, "current").expect("create current");
            (stale_id, current_id)
        };
        let mut model = AppModel::new();
        model.begin_session();
        model.current_session_id = Some(current_id);

        apply_update(
            Update::Started(SessionStart {
                session_id: stale_id,
            }),
            &mut model,
            &storage,
        );
        apply_update(construction_error(stale_id), &mut model, &storage);
        apply_update(
            Update::Stopped {
                session_id: stale_id,
            },
            &mut model,
            &storage,
        );

        assert_eq!(model.state, SessionState::Starting);
        assert_eq!(model.current_session_id, Some(current_id));
        assert!(model.segments.is_empty());
        let store = storage.lock().expect("lock storage");
        assert!(
            store
                .sessions()
                .get(stale_id)
                .expect("get stale session")
                .is_some()
        );
        assert!(
            store
                .sessions()
                .get(current_id)
                .expect("get current session")
                .is_some()
        );
    }

    #[test]
    fn partial_start_failure_is_finalised_in_its_preallocated_session() {
        let storage = Arc::new(Mutex::new(Storage::open_in_memory().expect("open storage")));
        let started_at = Utc.with_ymd_and_hms(2026, 7, 15, 13, 0, 0).unwrap();
        let session_id = {
            let store = storage.lock().expect("lock storage");
            library::create_session(&store, started_at, "partial-start").expect("create session")
        };
        let mut model = AppModel::new();
        model.begin_session();
        model.current_session_id = Some(session_id);
        apply_update(
            Update::Event {
                session_id,
                event: result(1, "captured before failure"),
            },
            &mut model,
            &storage,
        );

        apply_update(
            Update::StartFailed {
                session_id,
                error: SessionError::Start("system capture failed".into()),
            },
            &mut model,
            &storage,
        );

        assert_eq!(model.state, SessionState::Failed);
        assert_eq!(model.current_session_id, Some(session_id));
        assert!(model.pending_session_write.is_none());
        let store = storage.lock().expect("lock storage");
        let segments = store
            .segments()
            .list_by_session(session_id)
            .expect("load partial transcript");
        assert_eq!(segments.len(), 1);
        assert_eq!(segments[0].text, "captured before failure");
        assert!(
            store
                .sessions()
                .get(session_id)
                .expect("get session")
                .expect("session exists")
                .ended_at
                .is_some()
        );
    }

    #[test]
    fn pending_delete_removes_unstarted_row_instead_of_finalising_it() {
        let storage = Arc::new(Mutex::new(Storage::open_in_memory().expect("open storage")));
        let started_at = Utc.with_ymd_and_hms(2026, 7, 15, 14, 0, 0).unwrap();
        let session_id = {
            let store = storage.lock().expect("lock storage");
            library::create_session(&store, started_at, "unstarted").expect("create session")
        };
        let mut model = AppModel::new();
        model.state = SessionState::Failed;
        model.current_session_id = Some(session_id);
        model.pending_session_write = Some(PendingSessionWrite::Delete);

        retry_pending_session_write(&mut model, &storage).expect("retry cleanup");

        assert!(model.current_session_id.is_none());
        assert!(model.pending_session_write.is_none());
        let store = storage.lock().expect("lock storage");
        assert!(
            store
                .sessions()
                .get(session_id)
                .expect("get deleted session")
                .is_none()
        );
    }

    #[test]
    fn pending_finalise_retry_uses_the_original_stop_time() {
        let storage = Arc::new(Mutex::new(Storage::open_in_memory().expect("open storage")));
        let started_at = Utc.with_ymd_and_hms(2026, 7, 15, 15, 0, 0).unwrap();
        let ended_at = Utc.with_ymd_and_hms(2026, 7, 15, 15, 30, 0).unwrap();
        let session_id = {
            let store = storage.lock().expect("lock storage");
            library::create_session(&store, started_at, "retry").expect("create session")
        };
        let mut model = AppModel::new();
        model.state = SessionState::Failed;
        model.current_session_id = Some(session_id);
        model.segments.push(segment("retry transcript"));
        model.pending_session_write = Some(PendingSessionWrite::Finalise { ended_at });

        retry_pending_session_write(&mut model, &storage).expect("retry finalisation");

        assert_eq!(model.current_session_id, Some(session_id));
        assert!(model.pending_session_write.is_none());
        let store = storage.lock().expect("lock storage");
        assert_eq!(
            store
                .sessions()
                .get(session_id)
                .expect("get session")
                .expect("session exists")
                .ended_at,
            Some(ended_at)
        );
        assert_eq!(
            store
                .segments()
                .list_by_session(session_id)
                .expect("load transcript")[0]
                .text,
            "retry transcript"
        );
    }
}
