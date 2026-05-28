use rusqlite::Connection;

use crate::error::{Result, StorageError};

/// Each entry is the SQL that takes the schema from version `i` to `i+1`.
/// Append new entries; never edit existing ones — production databases
/// have already applied them.
const MIGRATIONS: &[&str] = &[
    // v0 -> v1: initial schema (sessions, segments, FTS5 mirror of segment text)
    r"
    CREATE TABLE sessions (
        id              INTEGER PRIMARY KEY AUTOINCREMENT,
        started_at      TEXT NOT NULL,
        ended_at        TEXT,
        title           TEXT NOT NULL,
        mic_wav_path    TEXT NOT NULL,
        system_wav_path TEXT NOT NULL,
        notes           TEXT NOT NULL DEFAULT ''
    );
    CREATE INDEX idx_sessions_started_at ON sessions (started_at DESC);

    CREATE TABLE segments (
        id              INTEGER PRIMARY KEY AUTOINCREMENT,
        session_id      INTEGER NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
        source          TEXT NOT NULL,
        segment_index   INTEGER NOT NULL,
        start_seconds   REAL NOT NULL,
        end_seconds     REAL NOT NULL,
        text            TEXT NOT NULL,
        speaker_label   TEXT,
        created_at      TEXT NOT NULL,
        UNIQUE (session_id, source, segment_index)
    );
    CREATE INDEX idx_segments_session ON segments (session_id);

    CREATE VIRTUAL TABLE segments_fts USING fts5(
        text,
        content='segments',
        content_rowid='id'
    );
    CREATE TRIGGER segments_ai AFTER INSERT ON segments BEGIN
        INSERT INTO segments_fts (rowid, text) VALUES (new.id, new.text);
    END;
    CREATE TRIGGER segments_ad AFTER DELETE ON segments BEGIN
        INSERT INTO segments_fts (segments_fts, rowid, text)
            VALUES ('delete', old.id, old.text);
    END;
    CREATE TRIGGER segments_au AFTER UPDATE ON segments BEGIN
        INSERT INTO segments_fts (segments_fts, rowid, text)
            VALUES ('delete', old.id, old.text);
        INSERT INTO segments_fts (rowid, text) VALUES (new.id, new.text);
    END;
    ",
];

/// Apply any pending migrations, advancing `PRAGMA user_version` as each
/// step succeeds. Idempotent: running on an already-current database is a
/// no-op.
///
/// # Errors
/// Returns [`StorageError::Migration`] if any step fails. The transaction
/// is rolled back so the database stays at its previous version.
pub(crate) fn run(conn: &Connection) -> Result<()> {
    let current: u32 = conn.query_row("PRAGMA user_version", [], |row| row.get(0))?;

    for (i, sql) in MIGRATIONS.iter().enumerate().skip(current as usize) {
        // i+1 <= MIGRATIONS.len(), a compile-time constant of small size,
        // so this conversion can't overflow u32.
        let Ok(target) = u32::try_from(i + 1) else {
            continue;
        };
        let tx_result = (|| -> std::result::Result<(), rusqlite::Error> {
            conn.execute_batch("BEGIN;")?;
            conn.execute_batch(sql)?;
            conn.pragma_update(None, "user_version", target)?;
            conn.execute_batch("COMMIT;")?;
            Ok(())
        })();
        if let Err(source) = tx_result {
            let _ = conn.execute_batch("ROLLBACK;");
            return Err(StorageError::Migration { target, source });
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::run;
    use rusqlite::Connection;

    #[test]
    fn applies_all_migrations_on_fresh_db() {
        let conn = Connection::open_in_memory().expect("in-memory db");
        run(&conn).expect("migrations");
        let version: u32 = conn
            .query_row("PRAGMA user_version", [], |row| row.get(0))
            .expect("read user_version");
        let expected = u32::try_from(super::MIGRATIONS.len()).expect("migrations fit in u32");
        assert_eq!(version, expected);
    }

    #[test]
    fn idempotent_when_already_current() {
        let conn = Connection::open_in_memory().expect("in-memory db");
        run(&conn).expect("first migration pass");
        run(&conn).expect("second migration pass should be a no-op");
    }

    #[test]
    fn creates_expected_tables() {
        let conn = Connection::open_in_memory().expect("in-memory db");
        run(&conn).expect("migrations");
        for table in ["sessions", "segments", "segments_fts"] {
            let count: i64 = conn
                .query_row(
                    "SELECT count(*) FROM sqlite_master WHERE name = ?1",
                    [table],
                    |row| row.get(0),
                )
                .expect("query sqlite_master");
            assert_eq!(count, 1, "table {table} should exist");
        }
    }
}
