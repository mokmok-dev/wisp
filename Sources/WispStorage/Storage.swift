import Foundation
import WispCore

/// Persistence layer for Wisp sessions.
///
/// Owns the `SQLite` schema for sessions and transcript segments.
///
/// Connection model: a single owned `sqlite3*` lives inside `Storage`.
/// `SQLite` serializes writers internally and the desktop app is the only
/// consumer, so there is no pool. The repositories borrow the connection.
public final class Storage {
  let connection: Connection
  /// The directory holding `sessions.db` and each session's recordings
  /// directory. `.memory` for an in-memory database.
  public let root: URL

  private init(connection: Connection, root: URL) {
    self.connection = connection
    self.root = root
  }

  /// Open (or create) the `SQLite` database at `<root>/sessions.db`.
  ///
  /// Creates the root directory if it doesn't exist, then runs any pending
  /// migrations.
  public static func open(root: URL) throws -> Storage {
    do {
      try FileManager.default.createDirectory(
        at: root,
        withIntermediateDirectories: true
      )
    } catch {
      throw StorageError.io(message: error.localizedDescription)
    }
    let databasePath = WispPaths.databaseFile(in: root).path
    let connection = try Connection(path: databasePath)
    try connection.execute("PRAGMA foreign_keys = ON;")
    try connection.execute("PRAGMA journal_mode = WAL;")
    try runMigrations(on: connection)
    return Storage(connection: connection, root: root)
  }

  /// Open an in-memory database. Used by tests and one-shot tools where
  /// persistence is undesired.
  public static func openInMemory() throws -> Storage {
    let connection = try Connection(path: ":memory:")
    try connection.execute("PRAGMA foreign_keys = ON;")
    try runMigrations(on: connection)
    return Storage(connection: connection, root: URL(fileURLWithPath: ":memory:"))
  }

  /// Sessions repository.
  public var sessions: Sessions {
    Sessions(connection: connection)
  }

  /// Segments repository.
  public var segments: Segments {
    Segments(connection: connection)
  }

  /// Persist all final transcript segments and mark their session ended in
  /// one transaction.
  ///
  /// If any segment fails to insert, or the session update fails, every
  /// write made by this call is rolled back.
  public func finaliseSession(
    _ sessionID: SessionID,
    segments newSegments: [NewSegment],
    endedAt: Date
  ) throws {
    if let segment = newSegments.first(where: { $0.sessionID != sessionID }) {
      throw StorageError.sessionMismatch(expected: sessionID, actual: segment.sessionID)
    }
    do {
      try connection.execute("BEGIN IMMEDIATE;")
      let exists = try connection.query(
        "SELECT EXISTS(SELECT 1 FROM sessions WHERE id = ?1)",
        [.integer(sessionID.rawValue)]
      ) { $0.integer(at: 0) != 0 }
      guard exists.first == true else {
        throw StorageError.sessionNotFound(sessionID)
      }
      // The in-memory transcript is the complete authoritative snapshot, so
      // replacing the session's rows keeps a retry idempotent and repairs a
      // partial write left by an interrupted finalise.
      try connection.run(
        "DELETE FROM segments WHERE session_id = ?1",
        [.integer(sessionID.rawValue)]
      )
      let segmentStore = Segments(connection: connection)
      for segment in newSegments {
        try segmentStore.append(segment)
      }
      try Sessions(connection: connection).markEnded(sessionID, at: endedAt)
      try connection.execute("COMMIT;")
    } catch {
      try? connection.execute("ROLLBACK;")
      throw error
    }
  }
}

extension Storage: CustomDebugStringConvertible {
  public var debugDescription: String {
    "Storage(root: \(root.path))"
  }
}
