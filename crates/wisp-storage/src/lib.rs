//! Persistence layer for Wisp sessions.
//!
//! Owns the `SQLite` schema for sessions, transcript segments, and the
//! filesystem layout that pairs each session with its source WAV files.
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

use rusqlite::Connection;

pub use crate::error::{Result, StorageError};
pub use crate::segments::Segments;
pub use crate::sessions::Sessions;

/// Owns the database connection and the on-disk root that holds the `SQLite`
/// file plus session WAV directories.
pub struct Storage {
    conn: Connection,
    root: PathBuf,
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
    pub fn sessions(&self) -> Sessions<'_> {
        Sessions::new(&self.conn)
    }

    /// Segments repository.
    #[must_use]
    pub fn segments(&self) -> Segments<'_> {
        Segments::new(&self.conn)
    }
}

#[cfg(test)]
mod tests {
    use super::Storage;

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
}
