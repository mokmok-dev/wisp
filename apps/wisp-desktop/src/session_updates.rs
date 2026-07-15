//! Apply `SessionRunner` updates to `AppModel` and persist at session boundaries.

#[cfg(unix)]
use std::fs::File;
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use wisp_audiokit::SourceLabel;
use wisp_core::SessionId;
use wisp_storage::Storage;

use crate::app::{
    AppError, AppModel, PendingSessionWrite, Segment, SessionState, View, break_on_sentence_end,
};
use crate::library;
use crate::library::SharedStorage;
use crate::session_runner::Update;
use crate::session_runner::Update::{Error, Event, StartFailed, Started, Stopped};
use crate::transcript_view::now;

pub(crate) const RECOVERY_FILE_NAME: &str = "transcript-recovery.json";

#[derive(Deserialize, Serialize)]
struct RecoverySnapshot {
    version: u32,
    session_id: Option<i64>,
    started_at: Option<String>,
    directory_name: Option<String>,
    pending_write: RecoveryPendingWrite,
    segments: Vec<RecoverySegment>,
}

#[derive(Deserialize, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum RecoveryPendingWrite {
    Finalise { ended_at: String },
    Delete,
}

#[derive(Deserialize, Serialize)]
struct RecoverySegment {
    source: String,
    id: u64,
    text: String,
    start_seconds: f64,
    end_seconds: f64,
    is_final: bool,
}

enum RecoveryLoadError {
    AlreadyPersisted,
    Invalid(String),
    Retryable {
        message: String,
        model: Box<AppModel>,
    },
}

pub fn apply_update(
    update: Update,
    model: &mut AppModel,
    storage: &SharedStorage,
) {
    match update {
        Started(session) => {
            if model.current_session_id != Some(session.session_id) || !model.state.is_active() {
                return;
            }
            model.linked_session_id = Some(session.session_id);
            model.current_session_started_at = Some(session.started_at);
            model.current_session_dir_name = Some(session.dir_name);
            // A quit request can move Starting -> Stopping before the worker
            // reports Started. Do not regress the UI back to Recording; the
            // queued Stop still owns the transition.
            if !matches!(model.state, SessionState::Stopping) {
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
            model.pending_session_write = Some(PendingSessionWrite::Finalise {
                ended_at: Utc::now(),
            });
            let _ = retry_pending_persistence(model, storage);
        },
        StartFailed { session_id, error } => {
            if model.current_session_id != Some(session_id) || !model.state.is_active() {
                return;
            }
            model.finalize_all_segments();
            model.pending_session_write = Some(PendingSessionWrite::Finalise {
                ended_at: Utc::now(),
            });
            if retry_pending_persistence(model, storage) {
                model.fail(error);
            } else if let Some(AppError::Persistence(message)) = model.last_error.as_mut() {
                message.push_str("; audio startup also failed: ");
                message.push_str(&error.to_string());
            }
        },
        Error { session_id, error } => {
            if model.current_session_id != Some(session_id) || !model.state.is_active() {
                return;
            }
            model.pending_session_write = Some(PendingSessionWrite::Delete);
            if retry_pending_persistence(model, storage) {
                model.fail(error);
            } else if let Some(AppError::Persistence(message)) = model.last_error.as_mut() {
                message.push_str("; audio construction also failed: ");
                message.push_str(&error.to_string());
            }
        },
    }
}

fn persist_finished_session(
    model: &mut AppModel,
    storage: &SharedStorage,
) -> bool {
    let Some(PendingSessionWrite::Finalise { ended_at }) = model.pending_session_write else {
        return fail_persistence(model, "the session is not awaiting finalisation");
    };
    let Ok(store) = storage.lock() else {
        return fail_persistence(model, "storage lock is unavailable");
    };
    let session_id = match resolve_session_id(model, &store) {
        Ok(session_id) => session_id,
        Err(error) => return fail_persistence(model, &error),
    };
    model.current_session_id = Some(session_id);
    model.linked_session_id = Some(session_id);
    if let Err(error) = library::finalise_session(&store, session_id, &model.segments, ended_at) {
        // The storage transaction has rolled back, so keep the handle to
        // the still-open row. Navigation/start guards prevent this live
        // transcript from being replaced after a persistence failure.
        return fail_persistence(model, &error.to_string());
    }
    model.current_session_id = None;
    model.pending_session_write = None;
    model.current_session_started_at = None;
    model.current_session_dir_name = None;
    if let Some(output_dir) = model.current_output_dir.take() {
        let _ = fs::remove_file(output_dir.join(RECOVERY_FILE_NAME));
    }
    model.set_state(SessionState::Idle);
    model.last_error = None;
    if let Ok(list) = store.sessions().list() {
        model.set_library(list);
    }
    true
}

/// Resolve a retained database handle before every retry. A sidecar can
/// outlive a rolled-back or externally removed row, while a transient lookup
/// failure can leave us uncertain whether that row still exists. Rechecking
/// here makes retry safe in both cases: reuse the matching row, recreate a
/// missing one, and refuse to write through an ID owned by another recording.
fn resolve_session_id(
    model: &AppModel,
    storage: &Storage,
) -> Result<SessionId, String> {
    let (Some(started_at), Some(dir_name)) = (
        model.current_session_started_at,
        model.current_session_dir_name.as_deref(),
    ) else {
        return Err("session launch metadata is unavailable".into());
    };

    if let Some(session_id) = model.current_session_id
        && let Some(session) = storage
            .sessions()
            .get(session_id)
            .map_err(|error| format!("could not validate the session row: {error}"))?
    {
        let expected_mic = format!("recordings/{dir_name}/mic.wav");
        let expected_system = format!("recordings/{dir_name}/system.wav");
        if session.mic_wav_path != expected_mic || session.system_wav_path != expected_system {
            return Err("the retained session id belongs to a different recording".into());
        }
        return Ok(session_id);
    }

    library::create_session(storage, started_at, dir_name)
        .map_err(|error| format!("could not create the session row: {error}"))
}

/// Retry a transaction that previously rolled back. Returns true once no
/// current database handle remains and a new session may safely start.
pub(crate) fn retry_pending_persistence(
    model: &mut AppModel,
    storage: &SharedStorage,
) -> bool {
    let Some(pending_write) = model.pending_session_write else {
        return !model.has_unsettled_session();
    };
    match pending_write {
        PendingSessionWrite::Finalise { .. } => persist_finished_session(model, storage),
        PendingSessionWrite::Delete => delete_unstarted_session(model, storage),
    }
}

fn delete_unstarted_session(
    model: &mut AppModel,
    storage: &SharedStorage,
) -> bool {
    let Some(session_id) = model.current_session_id else {
        return fail_persistence(model, "the pending cleanup has no database id");
    };
    let Ok(store) = storage.lock() else {
        return fail_persistence(model, "storage lock is unavailable");
    };
    if let Err(error) = store.sessions().delete(session_id) {
        return fail_persistence(
            model,
            &format!("could not remove failed session row: {error}"),
        );
    }
    model.current_session_id = None;
    model.linked_session_id = None;
    model.pending_session_write = None;
    model.current_session_started_at = None;
    model.current_session_dir_name = None;
    if let Some(output_dir) = model.current_output_dir.take() {
        let _ = fs::remove_file(output_dir.join(RECOVERY_FILE_NAME));
    }
    model.set_state(SessionState::Idle);
    model.last_error = None;
    if let Ok(list) = store.sessions().list() {
        model.set_library(list);
    }
    true
}

/// Write the in-memory transcript beside its WAV files using an atomic
/// replace. This is the durable fallback used before quit and whenever the
/// database transaction rolls back.
pub(crate) fn write_recovery_snapshot(model: &AppModel) -> io::Result<PathBuf> {
    let output_dir = model.current_output_dir.as_ref().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            "session output directory is unavailable",
        )
    })?;
    fs::create_dir_all(output_dir)?;
    let path = output_dir.join(RECOVERY_FILE_NAME);
    let temporary_path = output_dir.join(format!(".{RECOVERY_FILE_NAME}.tmp"));
    let pending_write = match model.pending_session_write {
        Some(PendingSessionWrite::Finalise { ended_at }) => RecoveryPendingWrite::Finalise {
            ended_at: ended_at.to_rfc3339(),
        },
        Some(PendingSessionWrite::Delete) => RecoveryPendingWrite::Delete,
        None => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "session has no pending persistence operation",
            ));
        },
    };
    let snapshot = RecoverySnapshot {
        version: 1,
        session_id: model.current_session_id.map(wisp_core::SessionId::as_i64),
        started_at: model
            .current_session_started_at
            .map(|started_at| started_at.to_rfc3339()),
        directory_name: model.current_session_dir_name.clone(),
        pending_write,
        segments: model
            .segments
            .iter()
            .map(|segment| RecoverySegment {
                source: segment.source.as_str().to_owned(),
                id: segment.id,
                text: segment.text.clone(),
                start_seconds: segment.start_seconds,
                end_seconds: segment.end_seconds,
                is_final: segment.is_final,
            })
            .collect(),
    };
    let mut file = OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .open(&temporary_path)?;
    serde_json::to_writer_pretty(&mut file, &snapshot).map_err(io::Error::other)?;
    file.write_all(b"\n")?;
    file.sync_all()?;
    drop(file);
    fs::rename(temporary_path, &path)?;
    // `sync_all` on the file flushes its contents; syncing the containing
    // directory also makes the atomic rename durable across a crash on the
    // Unix platforms where the desktop app records audio.
    #[cfg(unix)]
    File::open(output_dir)?.sync_all()?;
    Ok(path)
}

/// Reconcile durable recovery snapshots left by a previous process before a
/// new recording can start. Valid snapshots are retried automatically against
/// `SQLite`. If storage is still unavailable, the first pending transcript is
/// restored into `AppModel`'s guarded Failed state so the existing Retry Save
/// action remains available; its sidecar stays beside the WAV files.
pub(crate) fn recover_pending_sessions(
    model: &mut AppModel,
    storage: &SharedStorage,
    recordings_dir: &Path,
) -> usize {
    let Ok(entries) = fs::read_dir(recordings_dir) else {
        return 0;
    };
    let mut recovery_paths = entries
        .filter_map(Result::ok)
        .filter_map(|entry| {
            entry
                .file_type()
                .ok()
                .filter(std::fs::FileType::is_dir)
                .map(|_| entry.path().join(RECOVERY_FILE_NAME))
        })
        .filter(|path| path.is_file())
        .collect::<Vec<_>>();
    recovery_paths.sort();

    let mut recovered_count = 0;
    for recovery_path in recovery_paths {
        let mut recovered = match load_recovery_model(&recovery_path, storage) {
            Ok(recovered) => recovered,
            Err(RecoveryLoadError::AlreadyPersisted) => {
                // The database transaction committed and only the best-effort
                // sidecar unlink was interrupted. Never reinstall an Ended
                // row as a live handle; retry just the idempotent cleanup.
                match fs::remove_file(&recovery_path) {
                    Ok(()) => recovered_count += 1,
                    Err(error) => eprintln!(
                        "wisp: cannot remove already-persisted recovery snapshot {}: {error}",
                        recovery_path.display()
                    ),
                }
                continue;
            },
            Err(RecoveryLoadError::Invalid(error)) => {
                eprintln!(
                    "wisp: cannot load recovery snapshot {}: {error}",
                    recovery_path.display()
                );
                continue;
            },
            Err(RecoveryLoadError::Retryable {
                message,
                model: mut recovered,
            }) => {
                recovered.last_error = Some(AppError::Persistence(format!(
                    "cannot reconcile pending recovery {}: {message}",
                    recovery_path.display()
                )));
                install_recovered_session(model, *recovered);
                break;
            },
        };
        recovered.finalize_all_segments();
        if retry_pending_persistence(&mut recovered, storage) {
            recovered_count += 1;
            continue;
        }

        install_recovered_session(model, recovered);
        break;
    }
    recovered_count
}

/// Keep setup/MCP state owned by the running process and replace only the
/// transcript ownership fields. The installed Failed state blocks navigation
/// and new recordings while exposing the ordinary Retry Save action.
fn install_recovered_session(
    model: &mut AppModel,
    recovered: AppModel,
) {
    model.state = SessionState::Failed;
    model.view = View::LiveSession;
    model.segments = recovered.segments;
    model.current_session_id = recovered.current_session_id;
    model.linked_session_id = recovered.linked_session_id;
    model.pending_session_write = recovered.pending_session_write;
    model.current_session_started_at = recovered.current_session_started_at;
    model.current_session_dir_name = recovered.current_session_dir_name;
    model.current_output_dir = recovered.current_output_dir;
    model.viewed_session = None;
    model.last_error = recovered.last_error;
}

fn load_recovery_model(
    recovery_path: &Path,
    storage: &SharedStorage,
) -> Result<AppModel, RecoveryLoadError> {
    let invalid = |message: String| RecoveryLoadError::Invalid(message);
    let bytes = fs::read(recovery_path).map_err(|error| invalid(error.to_string()))?;
    let snapshot: RecoverySnapshot =
        serde_json::from_slice(&bytes).map_err(|error| invalid(error.to_string()))?;
    if snapshot.version != 1 {
        return Err(invalid(format!(
            "unsupported recovery version {}",
            snapshot.version
        )));
    }
    let output_dir = recovery_path
        .parent()
        .ok_or_else(|| invalid("recovery path has no parent directory".into()))?
        .to_path_buf();
    let directory_name = output_dir
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| invalid("recording directory is not valid UTF-8".into()))?
        .to_owned();
    if snapshot.directory_name.as_deref() != Some(directory_name.as_str()) {
        return Err(invalid(
            "snapshot directory metadata does not match its location".into(),
        ));
    }
    let started_at = snapshot
        .started_at
        .as_deref()
        .ok_or_else(|| invalid("snapshot has no launch timestamp".into()))?
        .parse::<DateTime<Utc>>()
        .map_err(|error| invalid(format!("invalid launch timestamp: {error}")))?;
    let retained_session_id = snapshot.session_id.map(SessionId::from);
    let pending_write = parse_recovery_pending_write(snapshot.pending_write)?;
    let segments = parse_recovery_segments(snapshot.segments)?;

    let mut model = AppModel::new();
    model.state = SessionState::Failed;
    model.view = View::LiveSession;
    model.segments = segments;
    model.current_session_id = retained_session_id;
    model.linked_session_id = retained_session_id;
    model.pending_session_write = Some(pending_write);
    model.current_session_started_at = Some(started_at);
    model.current_session_dir_name = Some(directory_name);
    model.current_output_dir = Some(output_dir);
    model.last_error = Some(AppError::Persistence(format!(
        "pending recovery loaded from {}",
        recovery_path.display()
    )));

    if let Some(session_id) = retained_session_id {
        let store = match storage.lock() {
            Ok(store) => store,
            Err(_) => {
                return Err(RecoveryLoadError::Retryable {
                    message: "storage lock is unavailable".into(),
                    model: Box::new(model),
                });
            },
        };
        match store.sessions().get(session_id) {
            Ok(Some(session)) => {
                let dir_name = model
                    .current_session_dir_name
                    .as_deref()
                    .expect("validated directory metadata");
                let expected_mic = format!("recordings/{dir_name}/mic.wav");
                let expected_system = format!("recordings/{dir_name}/system.wav");
                if session.mic_wav_path != expected_mic
                    || session.system_wav_path != expected_system
                {
                    return Err(invalid(
                        "snapshot session id points at a different recording".into(),
                    ));
                }
                match pending_write {
                    PendingSessionWrite::Finalise { .. } if session.ended_at.is_some() => {
                        return Err(RecoveryLoadError::AlreadyPersisted);
                    },
                    PendingSessionWrite::Delete if session.ended_at.is_some() => {
                        return Err(invalid(
                            "cleanup snapshot points at an already-finalised session".into(),
                        ));
                    },
                    PendingSessionWrite::Finalise { .. } | PendingSessionWrite::Delete => {},
                }
            },
            Ok(None) => {
                if pending_write == PendingSessionWrite::Delete {
                    return Err(RecoveryLoadError::AlreadyPersisted);
                }
                // A finalisation row may have been lost while the sidecar
                // remained. Launch metadata lets retry recreate it safely.
                model.current_session_id = None;
                model.linked_session_id = None;
            },
            Err(error) => {
                return Err(RecoveryLoadError::Retryable {
                    message: error.to_string(),
                    model: Box::new(model),
                });
            },
        }
    }
    Ok(model)
}

fn parse_recovery_pending_write(
    pending_write: RecoveryPendingWrite
) -> Result<PendingSessionWrite, RecoveryLoadError> {
    match pending_write {
        RecoveryPendingWrite::Finalise { ended_at } => ended_at
            .parse::<DateTime<Utc>>()
            .map(|ended_at| PendingSessionWrite::Finalise { ended_at })
            .map_err(|error| {
                RecoveryLoadError::Invalid(format!("invalid stop timestamp: {error}"))
            }),
        RecoveryPendingWrite::Delete => Ok(PendingSessionWrite::Delete),
    }
}

fn parse_recovery_segments(
    recovered_segments: Vec<RecoverySegment>
) -> Result<Vec<Segment>, RecoveryLoadError> {
    let mut segments = Vec::with_capacity(recovered_segments.len());
    for recovered in recovered_segments {
        if !recovered.start_seconds.is_finite()
            || !recovered.end_seconds.is_finite()
            || recovered.start_seconds < 0.0
            || recovered.end_seconds < recovered.start_seconds
        {
            return Err(RecoveryLoadError::Invalid(
                "snapshot contains invalid segment timing".into(),
            ));
        }
        let source = recovered
            .source
            .parse::<SourceLabel>()
            .map_err(|error| RecoveryLoadError::Invalid(error.to_string()))?;
        let display_text = break_on_sentence_end(&recovered.text);
        segments.push(Segment {
            source,
            id: recovered.id,
            text: recovered.text,
            display_text,
            start_seconds: recovered.start_seconds,
            end_seconds: recovered.end_seconds,
            // A previous process has stopped; no engine can revise this row.
            is_final: true,
        });
    }
    Ok(segments)
}

fn fail_persistence(
    model: &mut AppModel,
    error: &str,
) -> bool {
    let message = match write_recovery_snapshot(model) {
        Ok(path) => format!("{error}; recovery transcript saved at {}", path.display()),
        Err(recovery_error) => {
            format!("{error}; recovery transcript could not be saved: {recovery_error}")
        },
    };
    model.fail(AppError::Persistence(message));
    false
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use chrono::{TimeZone, Utc};
    use wisp_audiokit::{Event as AudioEvent, SessionError, SessionResult, SourceLabel};
    use wisp_core::SessionId;
    use wisp_storage::Storage;

    use super::*;
    use crate::app::{Segment, View};
    use crate::session_runner::SessionStart;

    fn in_memory_storage() -> SharedStorage {
        Arc::new(Mutex::new(
            Storage::open_in_memory().expect("in-memory storage"),
        ))
    }

    fn preallocate_session(
        storage: &SharedStorage,
        started_at: DateTime<Utc>,
        dir_name: &str,
    ) -> SessionId {
        storage
            .lock()
            .expect("storage lock")
            .sessions()
            .create(&wisp_core::NewSession {
                started_at,
                title: library::default_title(started_at),
                mic_wav_path: format!("recordings/{dir_name}/mic.wav"),
                system_wav_path: format!("recordings/{dir_name}/system.wav"),
            })
            .expect("preallocate session")
    }

    fn pending_finalise(
        model: &mut AppModel,
        ended_at: DateTime<Utc>,
    ) {
        model.pending_session_write = Some(PendingSessionWrite::Finalise { ended_at });
    }

    #[test]
    fn started_reuses_preallocated_identity_and_launch_metadata() {
        let storage = in_memory_storage();
        let started_at = Utc
            .with_ymd_and_hms(2026, 7, 15, 4, 30, 0)
            .single()
            .expect("valid timestamp");
        let dir_name = library::session_dir_name(started_at);
        let id = preallocate_session(&storage, started_at, &dir_name);
        let mut model = AppModel::new();
        assert!(model.begin_session_start());
        model.current_session_id = Some(id);
        model.linked_session_id = Some(id);
        model.current_session_started_at = Some(started_at);
        model.current_session_dir_name = Some(dir_name.clone());

        apply_update(
            Update::Started(SessionStart {
                session_id: id,
                started_at,
                dir_name: dir_name.clone(),
            }),
            &mut model,
            &storage,
        );

        assert!(matches!(model.state, SessionState::Recording { .. }));
        assert_eq!(model.current_session_id, Some(id));
        let store = storage.lock().expect("storage lock");
        let session = store
            .sessions()
            .get(id)
            .expect("read session")
            .expect("session exists");
        assert_eq!(session.started_at, started_at);
        assert_eq!(
            session.mic_wav_path,
            format!("recordings/{dir_name}/mic.wav")
        );
    }

    #[test]
    fn started_does_not_regress_an_in_flight_stop_to_recording() {
        let storage = in_memory_storage();
        let started_at = Utc
            .with_ymd_and_hms(2026, 7, 15, 4, 30, 0)
            .single()
            .expect("valid timestamp");
        let dir_name = library::session_dir_name(started_at);
        let id = preallocate_session(&storage, started_at, &dir_name);
        let mut model = AppModel::new();
        assert!(model.begin_session_start());
        model.current_session_id = Some(id);
        model.linked_session_id = Some(id);
        model.current_session_started_at = Some(started_at);
        model.current_session_dir_name = Some(dir_name.clone());
        model.set_state(SessionState::Stopping);

        apply_update(
            Update::Started(SessionStart {
                session_id: id,
                started_at,
                dir_name,
            }),
            &mut model,
            &storage,
        );

        assert_eq!(model.state, SessionState::Stopping);
        assert!(model.current_session_id.is_some());
    }

    #[test]
    fn failed_persistence_writes_recovery_and_retry_recreates_a_missing_row() {
        let storage = in_memory_storage();
        let started_at = Utc
            .with_ymd_and_hms(2026, 7, 15, 4, 30, 0)
            .single()
            .expect("valid timestamp");
        let dir_name = library::session_dir_name(started_at);
        let output_dir =
            std::env::temp_dir().join(format!("wisp-recovery-test-{}", uuid::Uuid::new_v4()));
        let mut model = AppModel::new();
        model.show_new_session();
        model.current_session_started_at = Some(started_at);
        model.current_session_dir_name = Some(dir_name.clone());
        model.current_output_dir = Some(output_dir.clone());
        model.segments.push(Segment {
            source: SourceLabel::Mic,
            id: 1,
            text: "live transcript".into(),
            display_text: "live transcript".into(),
            start_seconds: 0.0,
            end_seconds: 1.0,
            is_final: false,
        });
        model.finalize_all_segments();
        let ended_at = started_at + chrono::Duration::seconds(1);
        pending_finalise(&mut model, ended_at);

        assert!(!fail_persistence(&mut model, "simulated rollback"));

        assert_eq!(model.state, SessionState::Failed);
        assert!(model.current_session_id.is_none());
        assert!(model.has_pending_persistence());
        assert!(
            model
                .last_error
                .as_ref()
                .expect("persistence error")
                .to_string()
                .contains("persistence failed")
        );
        let recovery_path = output_dir.join(RECOVERY_FILE_NAME);
        let recovery_json = fs::read_to_string(&recovery_path).expect("read recovery snapshot");
        assert!(recovery_json.contains("live transcript"));
        assert!(recovery_json.contains(&dir_name));

        let original_view = model.view.clone();
        model.show_library();
        assert_eq!(model.view, original_view);
        assert!(!model.begin_session_start());
        assert_eq!(model.view, View::LiveSession);
        assert!(model.current_session_id.is_none());

        assert!(retry_pending_persistence(&mut model, &storage));
        assert_eq!(model.state, SessionState::Idle);
        assert!(!model.has_unsettled_session());
        assert!(!recovery_path.exists());
        assert!(model.current_session_started_at.is_none());
        assert!(model.current_session_dir_name.is_none());

        let store = storage.lock().expect("storage lock");
        let sessions = store.sessions().list().expect("list sessions");
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].started_at, started_at);
        assert_eq!(sessions[0].ended_at, Some(ended_at));
        let stored_segments = store
            .segments()
            .list_by_session(sessions[0].id)
            .expect("list segments");
        assert_eq!(stored_segments.len(), 1);
        assert_eq!(stored_segments[0].text, "live transcript");
        drop(store);

        let _ = fs::remove_dir_all(output_dir);
    }

    #[test]
    fn startup_reconciles_a_durable_recovery_snapshot() {
        let storage = in_memory_storage();
        let started_at = Utc
            .with_ymd_and_hms(2026, 7, 15, 5, 0, 0)
            .single()
            .expect("valid timestamp");
        let dir_name = library::session_dir_name(started_at);
        let recordings_dir = std::env::temp_dir().join(format!(
            "wisp-recovery-startup-test-{}",
            uuid::Uuid::new_v4()
        ));
        let output_dir = recordings_dir.join(&dir_name);
        let mut crashed_process = AppModel::new();
        crashed_process.current_session_started_at = Some(started_at);
        crashed_process.current_session_dir_name = Some(dir_name);
        crashed_process.current_output_dir = Some(output_dir.clone());
        crashed_process.segments.push(Segment {
            source: SourceLabel::System,
            id: 7,
            text: "recovered after restart".into(),
            display_text: "recovered after restart".into(),
            start_seconds: 0.25,
            end_seconds: 1.5,
            is_final: true,
        });
        pending_finalise(
            &mut crashed_process,
            started_at + chrono::Duration::seconds(1),
        );
        assert!(!fail_persistence(
            &mut crashed_process,
            "simulated process exit"
        ));
        assert!(output_dir.join(RECOVERY_FILE_NAME).is_file());

        let mut restarted_process = AppModel::new();
        assert_eq!(
            recover_pending_sessions(&mut restarted_process, &storage, &recordings_dir),
            1
        );
        assert!(!restarted_process.has_unsettled_session());
        assert!(!output_dir.join(RECOVERY_FILE_NAME).exists());

        let store = storage.lock().expect("storage lock");
        let sessions = store.sessions().list().expect("list sessions");
        assert_eq!(sessions.len(), 1);
        assert!(sessions[0].ended_at.is_some());
        let segments = store
            .segments()
            .list_by_session(sessions[0].id)
            .expect("list recovered segments");
        assert_eq!(segments.len(), 1);
        assert_eq!(segments[0].text, "recovered after restart");
        drop(store);

        let _ = fs::remove_dir_all(recordings_dir);
    }

    #[test]
    fn startup_recovery_recreates_a_missing_retained_row() {
        let storage = in_memory_storage();
        let started_at = Utc
            .with_ymd_and_hms(2026, 7, 15, 5, 15, 0)
            .single()
            .expect("valid timestamp");
        let dir_name = library::session_dir_name(started_at);
        let recordings_dir = std::env::temp_dir().join(format!(
            "wisp-recovery-missing-row-test-{}",
            uuid::Uuid::new_v4()
        ));
        let output_dir = recordings_dir.join(&dir_name);
        let session_id = {
            let store = storage.lock().expect("storage lock");
            library::create_session(&store, started_at, &dir_name).expect("session row")
        };
        let mut crashed_process = AppModel::new();
        crashed_process.current_session_id = Some(session_id);
        crashed_process.current_session_started_at = Some(started_at);
        crashed_process.current_session_dir_name = Some(dir_name);
        crashed_process.current_output_dir = Some(output_dir.clone());
        crashed_process.segments.push(Segment {
            source: SourceLabel::Mic,
            id: 3,
            text: "recreated row transcript".into(),
            display_text: "recreated row transcript".into(),
            start_seconds: 0.0,
            end_seconds: 0.5,
            is_final: true,
        });
        pending_finalise(
            &mut crashed_process,
            started_at + chrono::Duration::seconds(1),
        );
        write_recovery_snapshot(&crashed_process).expect("recovery snapshot");
        storage
            .lock()
            .expect("storage lock")
            .sessions()
            .delete(session_id)
            .expect("delete retained row");

        let mut restarted_process = AppModel::new();
        assert_eq!(
            recover_pending_sessions(&mut restarted_process, &storage, &recordings_dir),
            1
        );
        assert!(!restarted_process.has_unsettled_session());
        assert!(!output_dir.join(RECOVERY_FILE_NAME).exists());

        let store = storage.lock().expect("storage lock");
        let sessions = store.sessions().list().expect("list sessions");
        assert_eq!(sessions.len(), 1);
        assert_ne!(sessions[0].id, session_id);
        let segments = store
            .segments()
            .list_by_session(sessions[0].id)
            .expect("list recovered segments");
        assert_eq!(segments.len(), 1);
        assert_eq!(segments[0].text, "recreated row transcript");
        drop(store);

        let _ = fs::remove_dir_all(recordings_dir);
    }

    #[test]
    fn startup_discards_a_stale_sidecar_for_an_ended_row() {
        let storage = in_memory_storage();
        let started_at = Utc
            .with_ymd_and_hms(2026, 7, 15, 5, 18, 0)
            .single()
            .expect("valid timestamp");
        let dir_name = library::session_dir_name(started_at);
        let recordings_dir = std::env::temp_dir().join(format!(
            "wisp-recovery-ended-row-test-{}",
            uuid::Uuid::new_v4()
        ));
        let output_dir = recordings_dir.join(&dir_name);
        let session_id = {
            let store = storage.lock().expect("storage lock");
            library::create_session(&store, started_at, &dir_name).expect("session row")
        };
        let mut completed_process = AppModel::new();
        completed_process.current_session_id = Some(session_id);
        completed_process.current_session_started_at = Some(started_at);
        completed_process.current_session_dir_name = Some(dir_name);
        completed_process.current_output_dir = Some(output_dir.clone());
        completed_process.segments.push(Segment {
            source: SourceLabel::System,
            id: 4,
            text: "already committed".into(),
            display_text: "already committed".into(),
            start_seconds: 0.1,
            end_seconds: 0.6,
            is_final: true,
        });
        pending_finalise(
            &mut completed_process,
            started_at + chrono::Duration::seconds(1),
        );
        let recovery_path = write_recovery_snapshot(&completed_process).expect("recovery snapshot");
        {
            let store = storage.lock().expect("storage lock");
            library::finalise_session(
                &store,
                session_id,
                &completed_process.segments,
                started_at + chrono::Duration::seconds(1),
            )
            .expect("commit transcript before interrupted sidecar cleanup");
        }
        assert!(recovery_path.exists());

        let mut restarted_process = AppModel::new();
        assert_eq!(
            recover_pending_sessions(&mut restarted_process, &storage, &recordings_dir),
            1
        );
        assert!(!restarted_process.has_unsettled_session());
        assert!(!recovery_path.exists());

        let store = storage.lock().expect("storage lock");
        let sessions = store.sessions().list().expect("list sessions");
        assert_eq!(sessions.len(), 1, "stale recovery must not duplicate a row");
        assert!(sessions[0].ended_at.is_some());
        let segments = store
            .segments()
            .list_by_session(session_id)
            .expect("list committed segments");
        assert_eq!(segments.len(), 1);
        assert_eq!(segments[0].text, "already committed");
        drop(store);

        let _ = fs::remove_dir_all(recordings_dir);
    }

    #[test]
    fn startup_storage_lookup_failure_keeps_recovery_guarded() {
        let storage = in_memory_storage();
        let started_at = Utc
            .with_ymd_and_hms(2026, 7, 15, 5, 20, 0)
            .single()
            .expect("valid timestamp");
        let dir_name = library::session_dir_name(started_at);
        let recordings_dir = std::env::temp_dir().join(format!(
            "wisp-recovery-storage-failure-test-{}",
            uuid::Uuid::new_v4()
        ));
        let output_dir = recordings_dir.join(&dir_name);
        let session_id = {
            let store = storage.lock().expect("storage lock");
            library::create_session(&store, started_at, &dir_name).expect("session row")
        };
        let mut crashed_process = AppModel::new();
        crashed_process.current_session_id = Some(session_id);
        crashed_process.current_session_started_at = Some(started_at);
        crashed_process.current_session_dir_name = Some(dir_name.clone());
        crashed_process.current_output_dir = Some(output_dir.clone());
        crashed_process.segments.push(Segment {
            source: SourceLabel::System,
            id: 9,
            text: "must remain guarded".into(),
            display_text: "must remain guarded".into(),
            start_seconds: 0.2,
            end_seconds: 0.8,
            is_final: true,
        });
        pending_finalise(
            &mut crashed_process,
            started_at + chrono::Duration::seconds(1),
        );
        write_recovery_snapshot(&crashed_process).expect("recovery snapshot");

        let storage_to_poison = storage.clone();
        let poisoned = std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || {
            let _guard = storage_to_poison.lock().expect("storage lock");
            panic!("simulate an unavailable storage lock");
        }));
        assert!(poisoned.is_err());

        let mut restarted_process = AppModel::new();
        assert_eq!(
            recover_pending_sessions(&mut restarted_process, &storage, &recordings_dir),
            0
        );
        assert_eq!(restarted_process.state, SessionState::Failed);
        assert_eq!(restarted_process.view, View::LiveSession);
        assert!(restarted_process.has_pending_persistence());
        assert!(!restarted_process.begin_session_start());
        assert_eq!(restarted_process.current_session_id, Some(session_id));
        assert_eq!(
            restarted_process.current_session_dir_name.as_deref(),
            Some(dir_name.as_str())
        );
        assert_eq!(
            restarted_process.current_output_dir,
            Some(output_dir.clone())
        );
        assert_eq!(restarted_process.segments.len(), 1);
        assert_eq!(restarted_process.segments[0].text, "must remain guarded");
        assert!(output_dir.join(RECOVERY_FILE_NAME).exists());

        let _ = fs::remove_dir_all(recordings_dir);
    }

    #[test]
    fn stale_tagged_updates_cannot_mutate_the_current_transcript() {
        let storage = in_memory_storage();
        let started_at = Utc
            .with_ymd_and_hms(2026, 7, 15, 5, 25, 0)
            .single()
            .expect("valid timestamp");
        let stale_dir = library::session_dir_name(started_at);
        let current_dir = library::session_dir_name(started_at);
        let stale_id = preallocate_session(&storage, started_at, &stale_dir);
        let current_id = preallocate_session(&storage, started_at, &current_dir);
        let mut model = AppModel::new();
        assert!(model.begin_session_start());
        model.current_session_id = Some(current_id);
        model.linked_session_id = Some(current_id);
        model.current_session_started_at = Some(started_at);
        model.current_session_dir_name = Some(current_dir);

        apply_update(
            Update::Started(SessionStart {
                session_id: stale_id,
                started_at,
                dir_name: stale_dir,
            }),
            &mut model,
            &storage,
        );
        apply_update(
            Update::Event {
                session_id: stale_id,
                event: AudioEvent::Result(SessionResult {
                    source: SourceLabel::Mic,
                    segment_id: 1,
                    text: "stale transcript".into(),
                    start_seconds: 0.0,
                    end_seconds: 1.0,
                }),
            },
            &mut model,
            &storage,
        );
        apply_update(
            Update::Stopped {
                session_id: stale_id,
            },
            &mut model,
            &storage,
        );

        assert_eq!(model.state, SessionState::Starting);
        assert_eq!(model.current_session_id, Some(current_id));
        assert_eq!(model.linked_session_id, Some(current_id));
        assert!(model.segments.is_empty());
    }

    #[test]
    fn successful_finalisation_releases_open_handle_but_retains_transcript_link() {
        let storage = in_memory_storage();
        let started_at = Utc
            .with_ymd_and_hms(2026, 7, 15, 5, 27, 0)
            .single()
            .expect("valid timestamp");
        let dir_name = library::session_dir_name(started_at);
        let session_id = preallocate_session(&storage, started_at, &dir_name);
        let output_dir =
            std::env::temp_dir().join(format!("wisp-linked-test-{}", uuid::Uuid::new_v4()));
        let mut model = AppModel::new();
        assert!(model.begin_session_start());
        model.current_session_id = Some(session_id);
        model.linked_session_id = Some(session_id);
        model.current_session_started_at = Some(started_at);
        model.current_session_dir_name = Some(dir_name);
        model.current_output_dir = Some(output_dir.clone());
        model.set_state(SessionState::Stopping);
        model.segments.push(Segment {
            source: SourceLabel::Mic,
            id: 1,
            text: "linked transcript".into(),
            display_text: "linked transcript".into(),
            start_seconds: 0.0,
            end_seconds: 1.0,
            is_final: false,
        });

        apply_update(Update::Stopped { session_id }, &mut model, &storage);

        assert_eq!(model.state, SessionState::Idle);
        assert!(model.current_session_id.is_none());
        assert_eq!(model.linked_session_id, Some(session_id));
        assert!(!model.has_unsettled_session());
        assert!(model.current_output_dir.is_none());
        model.show_library();
        assert!(model.linked_session_id.is_none());

        let _ = fs::remove_dir_all(output_dir);
    }

    #[test]
    fn startup_recovery_preserves_pending_delete_semantics() {
        let storage = in_memory_storage();
        let started_at = Utc
            .with_ymd_and_hms(2026, 7, 15, 5, 28, 0)
            .single()
            .expect("valid timestamp");
        let dir_name = library::session_dir_name(started_at);
        let recordings_dir = std::env::temp_dir().join(format!(
            "wisp-recovery-delete-test-{}",
            uuid::Uuid::new_v4()
        ));
        let output_dir = recordings_dir.join(&dir_name);
        let session_id = preallocate_session(&storage, started_at, &dir_name);
        let mut crashed_process = AppModel::new();
        crashed_process.state = SessionState::Failed;
        crashed_process.view = View::LiveSession;
        crashed_process.current_session_id = Some(session_id);
        crashed_process.linked_session_id = Some(session_id);
        crashed_process.current_session_started_at = Some(started_at);
        crashed_process.current_session_dir_name = Some(dir_name);
        crashed_process.current_output_dir = Some(output_dir.clone());
        crashed_process.pending_session_write = Some(PendingSessionWrite::Delete);
        write_recovery_snapshot(&crashed_process).expect("delete recovery snapshot");

        let mut restarted_process = AppModel::new();
        assert_eq!(
            recover_pending_sessions(&mut restarted_process, &storage, &recordings_dir),
            1
        );
        assert!(!output_dir.join(RECOVERY_FILE_NAME).exists());
        assert!(
            storage
                .lock()
                .expect("storage lock")
                .sessions()
                .get(session_id)
                .expect("read deleted row")
                .is_none()
        );

        let _ = fs::remove_dir_all(recordings_dir);
    }

    #[test]
    fn failed_native_start_persists_results_flushed_during_cleanup() {
        let storage = in_memory_storage();
        let started_at = Utc
            .with_ymd_and_hms(2026, 7, 15, 5, 30, 0)
            .single()
            .expect("valid timestamp");
        let dir_name = library::session_dir_name(started_at);
        let output_dir =
            std::env::temp_dir().join(format!("wisp-failed-start-test-{}", uuid::Uuid::new_v4()));
        let mut model = AppModel::new();
        assert!(model.begin_session_start());
        let session_id = preallocate_session(&storage, started_at, &dir_name);
        model.current_session_id = Some(session_id);
        model.linked_session_id = Some(session_id);
        model.current_session_started_at = Some(started_at);
        model.current_session_dir_name = Some(dir_name);
        model.current_output_dir = Some(output_dir.clone());

        apply_update(
            Update::Event {
                session_id,
                event: AudioEvent::Result(SessionResult {
                    source: SourceLabel::Mic,
                    segment_id: 1,
                    text: "mic audio before system setup failed".into(),
                    start_seconds: 0.0,
                    end_seconds: 0.75,
                }),
            },
            &mut model,
            &storage,
        );
        apply_update(
            Update::StartFailed {
                session_id,
                error: SessionError::Start("system capture setup failed".into()),
            },
            &mut model,
            &storage,
        );

        assert_eq!(model.state, SessionState::Failed);
        assert!(!model.has_unsettled_session());
        assert!(
            model
                .last_error
                .as_ref()
                .expect("native start error")
                .to_string()
                .contains("system capture setup failed")
        );
        let store = storage.lock().expect("storage lock");
        let sessions = store.sessions().list().expect("list sessions");
        assert_eq!(sessions.len(), 1);
        assert!(sessions[0].ended_at.is_some());
        let segments = store
            .segments()
            .list_by_session(sessions[0].id)
            .expect("list segments");
        assert_eq!(segments.len(), 1);
        assert_eq!(segments[0].text, "mic audio before system setup failed");
        drop(store);

        let _ = fs::remove_dir_all(output_dir);
    }
}
