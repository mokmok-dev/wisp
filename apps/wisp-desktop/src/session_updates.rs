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
        Started {
            started_at,
            dir_name,
        } => {
            model.set_state(SessionState::Recording { started_at: now() });
            if let Ok(store) = storage.lock()
                && let Ok(session_id) = library::create_session(&store, started_at, &dir_name)
            {
                model.current_session_id = Some(session_id);
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

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use chrono::TimeZone;
    use wisp_storage::Storage;

    use super::*;

    #[test]
    fn started_uses_supplied_dir_name_for_session_paths() {
        let storage = Arc::new(Mutex::new(Storage::open_in_memory().expect("open")));
        let mut model = AppModel::new();
        let started_at = Utc.with_ymd_and_hms(2026, 7, 15, 12, 34, 56).unwrap();

        apply_update(
            Update::Started {
                started_at,
                dir_name: "actual-recording-dir".into(),
            },
            &mut model,
            &storage,
        );

        let session_id = model.current_session_id.expect("session id");
        let store = storage.lock().expect("storage lock");
        let session = store
            .sessions()
            .get(session_id)
            .expect("get session")
            .expect("session");

        assert_eq!(session.started_at, started_at);
        assert_eq!(session.mic_wav_path, "actual-recording-dir/mic.wav");
        assert_eq!(session.system_wav_path, "actual-recording-dir/system.wav");
    }
}
