import Foundation
import WispCore

/// Read/write operations for the `sessions` table.
public struct Sessions {
  let connection: Connection

  /// Insert a new session and return its assigned `SessionID`.
  public func create(_ new: NewSession) throws -> SessionID {
    try connection.run(
      """
      INSERT INTO sessions \
      (started_at, ended_at, title, mic_wav_path, system_wav_path, notes) \
      VALUES (?1, NULL, ?2, ?3, ?4, '')
      """,
      [
        .text(SQLiteClock.string(from: new.startedAt)),
        .text(new.title),
        .text(new.micWavPath),
        .text(new.systemWavPath),
      ]
    )
    return SessionID(connection.lastInsertRowID)
  }

  /// Look up a session by ID.
  public func get(_ id: SessionID) throws -> Session? {
    let rows = try connection.query(
      """
      SELECT id, started_at, ended_at, title, mic_wav_path, system_wav_path, notes \
      FROM sessions WHERE id = ?1
      """,
      [.integer(id.rawValue)],
      row: Self.rowToSession
    )
    return rows.first
  }

  /// List all sessions, newest first.
  public func list() throws -> [Session] {
    try connection.query(
      """
      SELECT id, started_at, ended_at, title, mic_wav_path, system_wav_path, notes \
      FROM sessions ORDER BY started_at DESC, id DESC
      """,
      row: Self.rowToSession
    )
  }

  /// Mark a session as ended at `endedAt`. No-op if `id` doesn't exist.
  public func markEnded(_ id: SessionID, at endedAt: Date) throws {
    try connection.run(
      "UPDATE sessions SET ended_at = ?1 WHERE id = ?2",
      [.text(SQLiteClock.string(from: endedAt)), .integer(id.rawValue)]
    )
  }

  /// Replace the title of a session. No-op if `id` doesn't exist.
  public func updateTitle(_ id: SessionID, title: String) throws {
    try connection.run(
      "UPDATE sessions SET title = ?1 WHERE id = ?2",
      [.text(title), .integer(id.rawValue)]
    )
  }

  /// Replace the freeform notes of a session.
  public func updateNotes(_ id: SessionID, notes: String) throws {
    try connection.run(
      "UPDATE sessions SET notes = ?1 WHERE id = ?2",
      [.text(notes), .integer(id.rawValue)]
    )
  }

  /// Delete a session. Cascades to its segments via the foreign key.
  public func delete(_ id: SessionID) throws {
    try connection.run("DELETE FROM sessions WHERE id = ?1", [.integer(id.rawValue)])
  }

  private static func rowToSession(_ row: Row) throws -> Session {
    try Session(
      id: SessionID(row.integer(at: 0)),
      startedAt: row.date(at: 1),
      endedAt: row.optionalDate(at: 2),
      title: row.text(at: 3),
      micWavPath: row.text(at: 4),
      systemWavPath: row.text(at: 5),
      notes: row.text(at: 6)
    )
  }
}

extension Row {
  func date(at index: Int32) throws -> Date {
    let text = text(at: index)
    guard let date = SQLiteClock.date(from: text) else {
      throw StorageError.invalidTimestamp(text)
    }
    return date
  }

  func optionalDate(at index: Int32) throws -> Date? {
    guard let text = optionalText(at: index) else { return nil }
    guard let date = SQLiteClock.date(from: text) else {
      throw StorageError.invalidTimestamp(text)
    }
    return date
  }
}
