use chrono::{DateTime, Utc};
use rusqlite::{Connection, OptionalExtension, params};
use wisp_core::{NewSession, Session, SessionId};

use crate::error::Result;

/// Read/write operations for the `sessions` table.
pub struct Sessions<'a> {
    conn: &'a Connection,
}

impl<'a> Sessions<'a> {
    pub(crate) fn new(conn: &'a Connection) -> Self {
        Self { conn }
    }

    /// Insert a new session and return its assigned [`SessionId`].
    ///
    /// # Errors
    /// Returns [`crate::StorageError::Sqlite`] on insertion failure.
    pub fn create(
        &self,
        new: &NewSession,
    ) -> Result<SessionId> {
        self.conn.execute(
            "INSERT INTO sessions \
             (started_at, ended_at, title, mic_wav_path, system_wav_path, notes) \
             VALUES (?1, NULL, ?2, ?3, ?4, '')",
            params![
                new.started_at,
                new.title,
                new.mic_wav_path,
                new.system_wav_path,
            ],
        )?;
        Ok(SessionId::from(self.conn.last_insert_rowid()))
    }

    /// Look up a session by ID.
    ///
    /// # Errors
    /// Returns [`crate::StorageError::Sqlite`] on query failure.
    pub fn get(
        &self,
        id: SessionId,
    ) -> Result<Option<Session>> {
        let session = self
            .conn
            .query_row(
                "SELECT id, started_at, ended_at, title, mic_wav_path, system_wav_path, notes \
                 FROM sessions WHERE id = ?1",
                [id.as_i64()],
                row_to_session,
            )
            .optional()?;
        Ok(session)
    }

    /// List all sessions, newest first.
    ///
    /// # Errors
    /// Returns [`crate::StorageError::Sqlite`] on query failure.
    pub fn list(&self) -> Result<Vec<Session>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, started_at, ended_at, title, mic_wav_path, system_wav_path, notes \
             FROM sessions ORDER BY started_at DESC, id DESC",
        )?;
        let rows = stmt.query_map([], row_to_session)?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// Mark a session as ended at `ended_at`. No-op if `id` doesn't exist.
    ///
    /// # Errors
    /// Returns [`crate::StorageError::Sqlite`] on update failure.
    pub fn mark_ended(
        &self,
        id: SessionId,
        ended_at: DateTime<Utc>,
    ) -> Result<()> {
        self.conn.execute(
            "UPDATE sessions SET ended_at = ?1 WHERE id = ?2",
            params![ended_at, id.as_i64()],
        )?;
        Ok(())
    }

    /// Replace the title of a session. No-op if `id` doesn't exist.
    ///
    /// # Errors
    /// Returns [`crate::StorageError::Sqlite`] on update failure.
    pub fn update_title(
        &self,
        id: SessionId,
        title: &str,
    ) -> Result<()> {
        self.conn.execute(
            "UPDATE sessions SET title = ?1 WHERE id = ?2",
            params![title, id.as_i64()],
        )?;
        Ok(())
    }

    /// Replace the freeform notes of a session.
    ///
    /// # Errors
    /// Returns [`crate::StorageError::Sqlite`] on update failure.
    pub fn update_notes(
        &self,
        id: SessionId,
        notes: &str,
    ) -> Result<()> {
        self.conn.execute(
            "UPDATE sessions SET notes = ?1 WHERE id = ?2",
            params![notes, id.as_i64()],
        )?;
        Ok(())
    }

    /// Delete a session. Cascades to its segments via the FK.
    ///
    /// # Errors
    /// Returns [`crate::StorageError::Sqlite`] on delete failure.
    pub fn delete(
        &self,
        id: SessionId,
    ) -> Result<()> {
        self.conn
            .execute("DELETE FROM sessions WHERE id = ?1", [id.as_i64()])?;
        Ok(())
    }
}

fn row_to_session(row: &rusqlite::Row<'_>) -> rusqlite::Result<Session> {
    Ok(Session {
        id: SessionId::from(row.get::<_, i64>(0)?),
        started_at: row.get(1)?,
        ended_at: row.get(2)?,
        title: row.get(3)?,
        mic_wav_path: row.get(4)?,
        system_wav_path: row.get(5)?,
        notes: row.get(6)?,
    })
}

#[cfg(test)]
mod tests {
    use chrono::{TimeZone, Utc};
    use wisp_core::NewSession;

    use crate::Storage;

    fn sample(title: &str) -> NewSession {
        NewSession {
            started_at: Utc.with_ymd_and_hms(2026, 5, 28, 10, 30, 0).unwrap(),
            title: title.into(),
            mic_wav_path: "session-1/mic.wav".into(),
            system_wav_path: "session-1/system.wav".into(),
        }
    }

    #[test]
    fn create_then_get_roundtrips() {
        let storage = Storage::open_in_memory().expect("open");
        let sessions = storage.sessions();
        let id = sessions.create(&sample("standup")).expect("create");
        let got = sessions.get(id).expect("get").expect("found");
        assert_eq!(got.id, id);
        assert_eq!(got.title, "standup");
        assert!(got.ended_at.is_none());
        assert_eq!(got.notes, "");
    }

    #[test]
    fn list_returns_newest_first() {
        let storage = Storage::open_in_memory().expect("open");
        let sessions = storage.sessions();
        let mut a = sample("a");
        a.started_at = Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap();
        let mut b = sample("b");
        b.started_at = Utc.with_ymd_and_hms(2026, 6, 1, 0, 0, 0).unwrap();
        sessions.create(&a).unwrap();
        sessions.create(&b).unwrap();
        let all = sessions.list().expect("list");
        assert_eq!(all.len(), 2);
        assert_eq!(all[0].title, "b");
        assert_eq!(all[1].title, "a");
    }

    #[test]
    fn mark_ended_and_update_title_persist() {
        let storage = Storage::open_in_memory().expect("open");
        let sessions = storage.sessions();
        let id = sessions.create(&sample("draft")).unwrap();
        let end = Utc.with_ymd_and_hms(2026, 5, 28, 11, 0, 0).unwrap();
        sessions.mark_ended(id, end).expect("mark_ended");
        sessions
            .update_title(id, "Q2 planning")
            .expect("update_title");
        sessions
            .update_notes(id, "good chat")
            .expect("update_notes");
        let got = sessions.get(id).unwrap().unwrap();
        assert_eq!(got.ended_at, Some(end));
        assert_eq!(got.title, "Q2 planning");
        assert_eq!(got.notes, "good chat");
    }

    #[test]
    fn delete_removes_session() {
        let storage = Storage::open_in_memory().expect("open");
        let sessions = storage.sessions();
        let id = sessions.create(&sample("doomed")).unwrap();
        sessions.delete(id).expect("delete");
        assert!(sessions.get(id).unwrap().is_none());
    }

    #[test]
    fn get_missing_is_none() {
        let storage = Storage::open_in_memory().expect("open");
        let got = storage
            .sessions()
            .get(wisp_core::SessionId::from(9999))
            .unwrap();
        assert!(got.is_none());
    }
}
