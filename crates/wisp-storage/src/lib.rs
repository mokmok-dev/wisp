//! Persistence layer for Wisp sessions.
//!
//! Owns the `SQLite` schema for sessions, transcript segments, and the
//! filesystem layout that pairs each session with its source Ogg/Opus files.
//!
//! Connection model: a single, owned `rusqlite::Connection` lives inside
//! [`Storage`]. `SQLite` serializes writers internally; the desktop app is
//! the only consumer, so we don't need a pool. Repositories ([`Sessions`]
//! and [`Segments`]) borrow the connection through `&Storage`.

mod error;
mod migrations;
mod segments;
mod sessions;

use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use rusqlite::{Connection, params};
use wisp_core::{NewSegment, SessionId};

pub use crate::error::{Result, StorageError};
pub use crate::segments::Segments;
pub use crate::sessions::Sessions;

/// Owns the database connection and the on-disk root that holds the `SQLite`
/// file plus session Ogg/Opus directories.
pub struct Storage {
    conn: Connection,
    root: PathBuf,
}

impl std::fmt::Debug for Storage {
    fn fmt(
        &self,
        f: &mut std::fmt::Formatter<'_>,
    ) -> std::fmt::Result {
        f.debug_struct("Storage")
            .field("root", &self.root)
            .finish_non_exhaustive()
    }
}

impl Storage {
    /// Open (or create) the `SQLite` database at `<root>/sessions.db`.
    ///
    /// Creates the root directory if it doesn't exist, then runs any
    /// pending migrations.
    ///
    /// # Errors
    /// Returns [`StorageError`] if the root directory can't be created,
    /// the database can't be opened, or a migration fails.
    pub fn open(root: impl AsRef<Path>) -> Result<Self> {
        let root = root.as_ref().to_path_buf();
        std::fs::create_dir_all(&root)?;
        let db_path = root.join("sessions.db");
        let conn = Connection::open(&db_path)?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        migrations::run(&conn)?;
        Ok(Self { conn, root })
    }

    /// Open an in-memory database. Used by tests and one-shot tools where
    /// persistence is undesired.
    ///
    /// # Errors
    /// Returns [`StorageError`] if the in-memory connection can't be opened
    /// or migrations fail.
    pub fn open_in_memory() -> Result<Self> {
        let conn = Connection::open_in_memory()?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        migrations::run(&conn)?;
        Ok(Self {
            conn,
            root: PathBuf::from(":memory:"),
        })
    }

    /// Returns the storage root directory.
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Sessions repository.
    #[must_use]
    pub const fn sessions(&self) -> Sessions<'_> {
        Sessions::new(&self.conn)
    }

    /// Segments repository.
    #[must_use]
    pub const fn segments(&self) -> Segments<'_> {
        Segments::new(&self.conn)
    }

    /// Persist all final transcript segments and mark their session ended in
    /// one transaction.
    ///
    /// If any segment fails to insert, or the session update fails, every
    /// write made by this call is rolled back.
    ///
    /// # Errors
    /// Returns [`StorageError`] if the transaction cannot be started or
    /// committed, a segment cannot be inserted, or the session cannot be
    /// updated.
    pub fn finalise_session(
        &self,
        session_id: SessionId,
        segments: &[NewSegment],
        ended_at: DateTime<Utc>,
    ) -> Result<()> {
        if let Some(segment) = segments
            .iter()
            .find(|segment| segment.session_id != session_id)
        {
            return Err(StorageError::SessionMismatch {
                expected: session_id,
                actual: segment.session_id,
            });
        }
        let tx = self.conn.unchecked_transaction()?;
        let exists = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM sessions WHERE id = ?1)",
            [session_id.as_i64()],
            |row| row.get::<_, bool>(0),
        )?;
        if !exists {
            return Err(StorageError::SessionNotFound(session_id));
        }
        // The in-memory transcript is the complete authoritative snapshot.
        // Replacing the session's rows makes retries idempotent and repairs
        // partial data left by older non-transactional versions.
        tx.execute(
            "DELETE FROM segments WHERE session_id = ?1",
            params![session_id.as_i64()],
        )?;
        let segment_store = Segments::new(&tx);
        for segment in segments {
            segment_store.append(segment)?;
        }
        Sessions::new(&tx).mark_ended(session_id, ended_at)?;
        tx.commit()?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use chrono::{TimeZone, Utc};
    use proptest::prelude::*;
    use wisp_core::{NewSegment, NewSession, SourceLabel};

    use super::Storage;

    fn new_session() -> NewSession {
        NewSession {
            started_at: Utc.with_ymd_and_hms(2026, 7, 15, 10, 0, 0).unwrap(),
            title: "transaction test".into(),
            mic_wav_path: "transaction-test/mic.ogg".into(),
            system_wav_path: "transaction-test/system.ogg".into(),
        }
    }

    fn new_segment(
        session_id: wisp_core::SessionId,
        segment_index: u32,
        text: &str,
    ) -> NewSegment {
        NewSegment {
            session_id,
            source: SourceLabel::Mic,
            segment_index,
            start_seconds: f64::from(segment_index),
            end_seconds: f64::from(segment_index) + 1.0,
            text: text.into(),
            speaker_label: None,
        }
    }

    #[test]
    fn open_in_memory_runs_migrations() {
        let storage = Storage::open_in_memory().expect("open in-memory storage");
        // Trivially: the repos should be constructible against the migrated schema.
        let _sessions = storage.sessions();
        let _segments = storage.segments();
    }

    #[test]
    fn open_creates_root_and_db_file() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path().join("wisp-store");
        let storage = Storage::open(&root).expect("open storage");
        assert_eq!(storage.root(), root.as_path());
        assert!(
            root.join("sessions.db").exists(),
            "sessions.db should be created"
        );
    }

    #[test]
    fn finalise_session_persists_segments_and_end_time_atomically() {
        let storage = Storage::open_in_memory().expect("open");
        let session_id = storage.sessions().create(&new_session()).expect("create");
        let ended_at = Utc.with_ymd_and_hms(2026, 7, 15, 11, 0, 0).unwrap();
        let segments = [
            new_segment(session_id, 0, "first"),
            new_segment(session_id, 1, "second"),
        ];

        storage
            .finalise_session(session_id, &segments, ended_at)
            .expect("finalise");

        let stored = storage
            .segments()
            .list_by_session(session_id)
            .expect("list segments");
        assert_eq!(stored.len(), 2);
        assert_eq!(stored[0].text, "first");
        assert_eq!(stored[1].text, "second");
        assert_eq!(
            storage
                .sessions()
                .get(session_id)
                .unwrap()
                .unwrap()
                .ended_at,
            Some(ended_at)
        );
    }

    #[test]
    fn finalise_session_rolls_back_all_writes_when_a_segment_fails() {
        let storage = Storage::open_in_memory().expect("open");
        let session_id = storage.sessions().create(&new_session()).expect("create");
        storage
            .segments()
            .append(&new_segment(session_id, 1, "pre-existing"))
            .expect("seed conflicting segment");
        let ended_at = Utc.with_ymd_and_hms(2026, 7, 15, 11, 0, 0).unwrap();
        let segments = [
            new_segment(session_id, 0, "must roll back"),
            new_segment(session_id, 0, "duplicate index"),
        ];

        let err = storage
            .finalise_session(session_id, &segments, ended_at)
            .expect_err("duplicate segment should abort finalisation");
        assert!(matches!(err, super::StorageError::Sqlite(_)), "got {err:?}");

        let stored = storage
            .segments()
            .list_by_session(session_id)
            .expect("list segments");
        assert_eq!(stored.len(), 1);
        assert_eq!(stored[0].text, "pre-existing");
        assert!(
            storage
                .segments()
                .search("must", 10)
                .expect("search FTS mirror")
                .is_empty(),
            "the FTS trigger write should roll back with the segment"
        );
        assert!(
            storage
                .sessions()
                .get(session_id)
                .unwrap()
                .unwrap()
                .ended_at
                .is_none(),
            "ended_at should remain unchanged"
        );
    }

    #[test]
    fn finalise_session_rejects_segments_from_another_session() {
        let storage = Storage::open_in_memory().expect("open");
        let session_id = storage.sessions().create(&new_session()).expect("create");
        let other_id = storage
            .sessions()
            .create(&new_session())
            .expect("create other");
        let ended_at = Utc.with_ymd_and_hms(2026, 7, 15, 11, 0, 0).unwrap();

        let error = storage
            .finalise_session(session_id, &[new_segment(other_id, 0, "wrong")], ended_at)
            .expect_err("mismatched session must be rejected");

        assert!(matches!(
            error,
            super::StorageError::SessionMismatch {
                expected,
                actual
            } if expected == session_id && actual == other_id
        ));
        assert_eq!(
            storage
                .segments()
                .list_by_session(other_id)
                .expect("list other segments"),
            Vec::new()
        );
        assert!(
            storage
                .sessions()
                .get(session_id)
                .expect("get session")
                .expect("session exists")
                .ended_at
                .is_none()
        );
    }

    #[test]
    fn finalise_session_can_retry_with_a_complete_snapshot() {
        let storage = Storage::open_in_memory().expect("open");
        let session_id = storage.sessions().create(&new_session()).expect("create");
        let first_end = Utc.with_ymd_and_hms(2026, 7, 15, 11, 0, 0).unwrap();
        storage
            .finalise_session(
                session_id,
                &[new_segment(session_id, 0, "old snapshot")],
                first_end,
            )
            .expect("first finalise");
        let retry_end = Utc.with_ymd_and_hms(2026, 7, 15, 11, 1, 0).unwrap();

        storage
            .finalise_session(
                session_id,
                &[
                    new_segment(session_id, 0, "replacement"),
                    new_segment(session_id, 1, "new tail"),
                ],
                retry_end,
            )
            .expect("retry finalise");

        let stored = storage
            .segments()
            .list_by_session(session_id)
            .expect("list segments");
        assert_eq!(stored.len(), 2);
        assert_eq!(stored[0].text, "replacement");
        assert_eq!(stored[1].text, "new tail");
        assert_eq!(
            storage
                .segments()
                .search("old", 10)
                .expect("search replaced snapshot"),
            Vec::new()
        );
        assert_eq!(
            storage
                .sessions()
                .get(session_id)
                .expect("get session")
                .expect("session exists")
                .ended_at,
            Some(retry_end)
        );
    }

    #[test]
    fn finalise_session_rejects_a_missing_session_even_when_empty() {
        let storage = Storage::open_in_memory().expect("open");
        let missing_id = wisp_core::SessionId::from(404);
        let ended_at = Utc.with_ymd_and_hms(2026, 7, 15, 11, 0, 0).unwrap();

        let error = storage
            .finalise_session(missing_id, &[], ended_at)
            .expect_err("missing session must fail");

        assert!(matches!(
            error,
            super::StorageError::SessionNotFound(id) if id == missing_id
        ));
    }

    #[test]
    fn debug_rendering_names_the_storage_root() {
        let storage = Storage::open_in_memory().expect("open");
        let rendered = format!("{storage:?}");
        assert!(
            rendered.contains(":memory:"),
            "unexpected debug output: {rendered}"
        );
    }

    proptest! {
        #[test]
        fn sessions_roundtrip_arbitrary_text(
            title in "[a-zA-Z0-9 _\\-]{0,80}",
            notes in "[a-zA-Z0-9 _\\-]{0,120}",
        ) {
            let storage = Storage::open_in_memory().expect("open");
            let sessions = storage.sessions();
            let mut new = new_session();
            new.title = title.clone();
            let id = sessions.create(&new).expect("create");
            sessions.update_notes(id, &notes).expect("update notes");

            let stored = sessions.get(id).expect("get").expect("found");
            prop_assert_eq!(&stored.title, &title);
            prop_assert_eq!(&stored.notes, &notes);
            prop_assert_eq!(sessions.list().expect("list").len(), 1);
        }

        #[test]
        fn appended_segments_roundtrip_arbitrary_text(
            texts in prop::collection::vec("[a-zA-Z0-9 _\\-]{0,60}", 1..=8),
        ) {
            let storage = Storage::open_in_memory().expect("open");
            let session_id = storage
                .sessions()
                .create(&new_session())
                .expect("create session");
            let segments = storage.segments();
            for (index, text) in texts.iter().enumerate() {
                let index = u32::try_from(index).expect("test vector fits u32");
                segments
                    .append(&new_segment(session_id, index, text))
                    .expect("append segment");
            }

            let stored = segments.list_by_session(session_id).expect("list");
            prop_assert_eq!(stored.len(), texts.len());
            for (stored, expected) in stored.iter().zip(texts.iter()) {
                prop_assert_eq!(&stored.text, expected);
            }
        }

        #[test]
        fn finalise_replaces_the_snapshot_with_arbitrary_text(
            texts in prop::collection::vec("[a-zA-Z0-9 _\\-]{0,60}", 1..=8),
            replacement in "[a-zA-Z0-9 _\\-]{0,60}",
        ) {
            let storage = Storage::open_in_memory().expect("open");
            let session_id = storage
                .sessions()
                .create(&new_session())
                .expect("create session");
            storage
                .segments()
                .append(&new_segment(session_id, 0, &replacement))
                .expect("seed stale segment");
            let ended_at = Utc.with_ymd_and_hms(2026, 7, 15, 11, 0, 0).unwrap();
            let snapshot = texts
                .iter()
                .enumerate()
                .map(|(index, text)| {
                    new_segment(
                        session_id,
                        u32::try_from(index).expect("test vector fits u32"),
                        text,
                    )
                })
                .collect::<Vec<_>>();

            storage
                .finalise_session(session_id, &snapshot, ended_at)
                .expect("finalise session");
            let stored = storage
                .segments()
                .list_by_session(session_id)
                .expect("list segments");
            prop_assert_eq!(stored.len(), texts.len());
            for (stored, expected) in stored.iter().zip(texts.iter()) {
                prop_assert_eq!(&stored.text, expected);
            }
            prop_assert_eq!(
                storage
                    .sessions()
                    .get(session_id)
                    .expect("get session")
                    .expect("session exists")
                    .ended_at,
                Some(ended_at)
            );
        }
    }
}
