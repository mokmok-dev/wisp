import Foundation
import XCTest

@testable import WispStorage

final class MigrationTests: XCTestCase {
  private func userVersion(_ connection: Connection) throws -> UInt32 {
    let rows = try connection.query("PRAGMA user_version;") { $0.integer(at: 0) }
    return UInt32(clamping: rows.first ?? 0)
  }

  func testAppliesAllMigrationsOnFreshDatabase() throws {
    let connection = try Connection(path: ":memory:")
    try runMigrations(on: connection)
    XCTAssertEqual(try userVersion(connection), currentSchemaVersion)
    XCTAssertEqual(currentSchemaVersion, 2)
  }

  func testIdempotentWhenAlreadyCurrent() throws {
    let connection = try Connection(path: ":memory:")
    try runMigrations(on: connection)
    try runMigrations(on: connection)
    XCTAssertEqual(try userVersion(connection), currentSchemaVersion)
  }

  func testUpgradesAV1DatabaseAndRebuildsTheSearchIndex() throws {
    let connection = try Connection(path: ":memory:")
    try connection.execute(migrationSQL(at: 0))
    try connection.execute("PRAGMA user_version = 1;")
    try connection.run(
      """
      INSERT INTO sessions (started_at, title, mic_wav_path, system_wav_path) \
      VALUES ('2026-05-28T10:00:00Z', 'old', 's/mic.ogg', 's/system.ogg')
      """
    )
    try connection.run(
      """
      INSERT INTO segments \
      (session_id, source, segment_index, start_seconds, end_seconds, text, created_at) \
      VALUES (1, 'mic', 0, 0.0, 1.0, 'the weather is nice', '2026-05-28T10:00:01Z')
      """
    )

    try runMigrations(on: connection)

    XCTAssertEqual(try userVersion(connection), 2)
    let hits = try Segments(connection: connection).searchText("weather", limit: 10)
    XCTAssertEqual(hits.count, 1)
    XCTAssertEqual(hits.first?.text, "the weather is nice")

    try connection.run(
      """
      INSERT INTO segments \
      (session_id, source, segment_index, start_seconds, end_seconds, text, created_at) \
      VALUES (1, 'mic', 1, 2.0, 3.0, 'rain tomorrow', '2026-05-28T10:00:02Z')
      """
    )
    let rebuilt = try Segments(connection: connection).searchText("rain", limit: 10)
    XCTAssertEqual(rebuilt.count, 1)
    XCTAssertEqual(rebuilt.first?.text, "rain tomorrow")
  }

  func testCreatesExpectedTables() throws {
    let connection = try Connection(path: ":memory:")
    try runMigrations(on: connection)
    for table in ["sessions", "segments", "segments_fts"] {
      let count = try connection.query(
        "SELECT count(*) FROM sqlite_master WHERE name = ?1",
        [.text(table)]
      ) { $0.integer(at: 0) }
      XCTAssertEqual(count.first, 1, "table \(table) should exist")
    }
  }

  func testAFailingMigrationLeavesThePreviousVersionAndNoPartialSchema() throws {
    let connection = try Connection(path: ":memory:")
    try connection.execute(migrationSQL(at: 0))
    try connection.execute("PRAGMA user_version = 1;")
    // v2 rebuilds the search index from `segments`; renaming the table
    // makes that final statement fail after the earlier ones succeeded.
    try connection.execute("ALTER TABLE segments RENAME TO segments_moved;")

    XCTAssertThrowsError(try runMigrations(on: connection)) { error in
      guard case StorageError.migration(let target, _) = error else {
        XCTFail("unexpected error: \(error)")
        return
      }
      XCTAssertEqual(target, 2)
    }
    XCTAssertEqual(try userVersion(connection), 1)
    let tables = try connection.query(
      "SELECT count(*) FROM sqlite_master WHERE name = 'segments_fts'"
    ) { $0.integer(at: 0) }
    XCTAssertEqual(tables.first, 1, "the rolled-back DROP must restore the search table")
  }

  func testASchemaAheadOfThisBuildIsRejectedInsteadOfTrapping() throws {
    let connection = try Connection(path: ":memory:")
    try runMigrations(on: connection)
    // A database written by a newer build. The range `current..<count` would
    // trap here, taking the process down before the startup-failure screen.
    try connection.execute("PRAGMA user_version = 99;")

    XCTAssertThrowsError(try runMigrations(on: connection)) { error in
      guard case StorageError.migration(let target, let message) = error else {
        XCTFail("unexpected error: \(error)")
        return
      }
      XCTAssertEqual(target, 99)
      XCTAssertTrue(
        message.contains("newer than this build"),
        "the message must say why the database was refused, got: \(message)"
      )
    }
    XCTAssertEqual(try userVersion(connection), 99, "a refused open must not rewrite the version")
  }

  func testANegativeUserVersionIsRejectedRatherThanClamped() throws {
    let connection = try Connection(path: ":memory:")
    // `user_version` is signed; clamping a negative value to 0 would re-run
    // every migration over existing tables.
    try connection.execute("PRAGMA user_version = -1;")

    XCTAssertThrowsError(try runMigrations(on: connection)) { error in
      guard case StorageError.migration(_, let message) = error else {
        XCTFail("unexpected error: \(error)")
        return
      }
      XCTAssertTrue(
        message.contains("-1"),
        "the message must name the unreadable version, got: \(message)"
      )
    }
    XCTAssertEqual(try userVersion(connection), 0)
    let sessions = try connection.query(
      "SELECT count(*) FROM sqlite_master WHERE name = 'sessions'"
    ) { $0.integer(at: 0) }
    XCTAssertEqual(sessions.first, 0, "no migration may run after the version is refused")
  }

  func testStorageOpenReportsAFutureSchemaAsAThrownError() throws {
    let root = FileManager.default.temporaryDirectory
      .appendingPathComponent("wisp-future-schema-\(UUID().uuidString)", isDirectory: true)
    addTeardownBlock { try? FileManager.default.removeItem(at: root) }

    let current = try Storage.open(root: root)
    try current.connection.execute("PRAGMA user_version = 99;")

    // The call the app makes at launch. It must throw so `AppEnvironment`
    // can show the startup-failure screen instead of the process dying.
    XCTAssertThrowsError(try Storage.open(root: root)) { error in
      guard case StorageError.migration(let target, _) = error else {
        XCTFail("unexpected error: \(error)")
        return
      }
      XCTAssertEqual(target, 99)
    }
  }
}
