import Foundation

/// Each entry is the SQL that takes the schema from version `i` to `i+1`.
/// Append new entries; never edit existing ones — production databases have
/// already applied them.
private let migrations: [String] = [
  // v0 -> v1: initial schema (sessions, segments, FTS5 mirror of segment text)
  """
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
  """,
  // v1 -> v2: the default unicode61 tokenizer splits on whitespace, so a
  // Japanese sentence becomes one token and a word inside it never matches.
  // The app records ja-JP, and trigram indexes substrings without spaces.
  // The triggers belong to `segments`, so dropping `segments_fts` leaves
  // them behind and they must be dropped explicitly before recreation.
  """
  DROP TABLE segments_fts;
  DROP TRIGGER IF EXISTS segments_ai;
  DROP TRIGGER IF EXISTS segments_ad;
  DROP TRIGGER IF EXISTS segments_au;

  CREATE VIRTUAL TABLE segments_fts USING fts5(
      text,
      content='segments',
      content_rowid='id',
      tokenize='trigram'
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

  INSERT INTO segments_fts (rowid, text) SELECT id, text FROM segments;
  """,
]

/// The schema version a fully migrated database reports.
let currentSchemaVersion = UInt32(migrations.count)

/// SQL for the migration that takes the schema from `index` to `index + 1`.
/// Tests replay an individual step to build a database at an older version.
func migrationSQL(at index: Int) -> String {
  migrations[index]
}

/// Apply any pending migrations, advancing `PRAGMA user_version` as each step
/// succeeds. Idempotent: running on an already-current database is a no-op.
///
/// Each step runs in its own transaction, so a failure rolls back to the
/// previous version.
func runMigrations(on connection: Connection) throws {
  let current = try readUserVersion(connection)
  guard current <= currentSchemaVersion else {
    // A database written by a newer build. Migrating it would mean guessing at
    // a schema this build cannot read, so refuse before touching anything.
    throw StorageError.migration(
      target: current,
      message: "database schema is newer than this build"
    )
  }
  for index in Int(current)..<migrations.count {
    let target = UInt32(index + 1)
    do {
      try connection.execute("BEGIN IMMEDIATE;")
      try connection.execute(migrations[index])
      try connection.execute("PRAGMA user_version = \(target);")
      try connection.execute("COMMIT;")
    } catch {
      try? connection.execute("ROLLBACK;")
      throw StorageError.migration(target: target, message: "\(error)")
    }
  }
}

private func readUserVersion(_ connection: Connection) throws -> UInt32 {
  let rows = try connection.query("PRAGMA user_version;") { row in
    row.integer(at: 0)
  }
  guard let raw = rows.first else { return 0 }
  // `user_version` is a signed 32-bit value. A negative one is not a version
  // this build ever wrote, and clamping it to 0 would re-run every migration
  // over live tables.
  guard raw >= 0, raw <= Int64(UInt32.max) else {
    throw StorageError.migration(
      target: 0,
      message: "database user_version is not a valid schema version: \(raw)"
    )
  }
  return UInt32(raw)
}
