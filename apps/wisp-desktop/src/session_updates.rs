//! Apply `SessionRunner` updates to `AppModel` and persist at session boundaries.

use chrono::Utc;

use crate::app::{AppModel, SessionState};
use crate::library;
use crate::library::SharedStorage;
use crate::session_runner::Update;
use crate::session_runner::Update::{Error, Event, Started, Stopped};
use crate::transcript_view::now;

pub fn apply_update(
    update: Update,
    model: &mut AppModel,
    storage: &SharedStorage,
) {
    match update {
        Started => {
            let started_at = Utc::now();
            model.set_state(SessionState::Recording { started_at: now() });
            if let Ok(store) = storage.lock() {
                let dir_name = library::session_dir_name(started_at);
                if let Ok(session_id) = library::create_session(&store, started_at, &dir_name) {
                    model.current_session_id = Some(session_id);
                }
            }
        },
        Event(e) => model.ingest(e),
        Stopped => {
            model.finalize_all_segments();
            model.set_state(SessionState::Idle);
            persist_finished_session(model, storage);
        },
        Error(msg) => {
            if let Some(id) = model.current_session_id.take()
                && let Ok(store) = storage.lock()
            {
                let _ = store.sessions().delete(id);
            }
            model.fail(msg);
        },
    }
}

fn persist_finished_session(
    model: &mut AppModel,
    storage: &SharedStorage,
) {
    let Some(session_id) = model.current_session_id.take() else {
        return;
    };
    let Ok(store) = storage.lock() else {
        return;
    };
    let ended_at = Utc::now();
    let _ = library::finalise_session(&store, session_id, &model.segments, ended_at);
    if let Ok(list) = store.sessions().list() {
        model.set_library(list);
    }
}
