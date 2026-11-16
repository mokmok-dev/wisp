import Foundation
import WispCore
import XCTest

@testable import WispStorage

final class SessionsTests: XCTestCase {
  private var storage: Storage!

  override func setUpWithError() throws {
    storage = try Storage.openInMemory()
  }

  func testCreateThenGetRoundTrips() throws {
    let id = try storage.sessions.create(Fixtures.newSession(title: "standup"))
    let got = try XCTUnwrap(try storage.sessions.get(id))
    XCTAssertEqual(got.id, id)
    XCTAssertEqual(got.title, "standup")
    XCTAssertNil(got.endedAt)
    XCTAssertEqual(got.notes, "")
    XCTAssertEqual(got.startedAt, Fixtures.date(2026, 5, 28, 10, 0, 0))
    XCTAssertEqual(got.micWavPath, "s/mic.ogg")
    XCTAssertEqual(got.systemWavPath, "s/system.ogg")
  }

  func testListReturnsNewestFirst() throws {
    var early = Fixtures.newSession(title: "a")
    early.startedAt = Fixtures.date(2026, 1, 1)
    var late = Fixtures.newSession(title: "b")
    late.startedAt = Fixtures.date(2026, 6, 1)
    try storage.sessions.create(early)
    try storage.sessions.create(late)

    let all = try storage.sessions.list()
    XCTAssertEqual(all.count, 2)
    XCTAssertEqual(all.map(\.title), ["b", "a"])
  }

  func testMarkEndedAndEditsPersist() throws {
    let id = try storage.sessions.create(Fixtures.newSession(title: "draft"))
    let end = Fixtures.date(2026, 5, 28, 11, 0, 0)
    try storage.sessions.markEnded(id, at: end)
    try storage.sessions.updateTitle(id, title: "Q2 planning")
    try storage.sessions.updateNotes(id, notes: "good chat")

    let got = try XCTUnwrap(try storage.sessions.get(id))
    XCTAssertEqual(got.endedAt, end)
    XCTAssertEqual(got.title, "Q2 planning")
    XCTAssertEqual(got.notes, "good chat")
  }

  func testDeleteRemovesTheSession() throws {
    let id = try storage.sessions.create(Fixtures.newSession(title: "doomed"))
    try storage.sessions.delete(id)
    XCTAssertNil(try storage.sessions.get(id))
  }

  func testMissingIDIsNil() throws {
    XCTAssertNil(try storage.sessions.get(SessionID(9999)))
  }

  func testEditingAMissingSessionIsANoOp() throws {
    let missing = SessionID(404)
    try storage.sessions.markEnded(missing, at: Fixtures.referenceDate)
    try storage.sessions.updateTitle(missing, title: "x")
    try storage.sessions.updateNotes(missing, notes: "y")
    XCTAssertNil(try storage.sessions.get(missing))
  }

  func testTimestampsUseTheCompatibleTextFormat() throws {
    let startedAt = Fixtures.date(2026, 7, 15, 10, 0, 0)
    var session = Fixtures.newSession()
    session.startedAt = startedAt
    let id = try storage.sessions.create(session)

    let raw = try storage.connection.query(
      "SELECT started_at FROM sessions WHERE id = ?1",
      [.integer(id.rawValue)]
    ) { $0.text(at: 0) }
    XCTAssertEqual(raw.first, "2026-07-15 10:00:00+00:00")
  }

  func testSubSecondTimestampsSurviveARoundTrip() throws {
    let startedAt = Fixtures.date(2026, 7, 15, 10, 0, 0).addingTimeInterval(0.125)
    var session = Fixtures.newSession()
    session.startedAt = startedAt
    let id = try storage.sessions.create(session)

    let got = try XCTUnwrap(try storage.sessions.get(id))
    XCTAssertEqual(
      got.startedAt.timeIntervalSince1970,
      startedAt.timeIntervalSince1970,
      accuracy: 0.001
    )
  }

  func testTextRoundTripsArbitraryCharacters() throws {
    let id = try storage.sessions.create(Fixtures.newSession())
    let title = "議事録 – 50% off_under\\score \"quoted\""
    let notes = "line one\nline two\tand a tab"
    try storage.sessions.updateTitle(id, title: title)
    try storage.sessions.updateNotes(id, notes: notes)

    let got = try XCTUnwrap(try storage.sessions.get(id))
    XCTAssertEqual(got.title, title)
    XCTAssertEqual(got.notes, notes)
    XCTAssertEqual(try storage.sessions.list().count, 1)
  }

  func testOpenCreatesTheRootAndDatabaseFile() throws {
    let root = FileManager.default.temporaryDirectory
      .appendingPathComponent("WispStorageTests-\(UUID().uuidString)", isDirectory: true)
    defer { try? FileManager.default.removeItem(at: root) }

    let opened = try Storage.open(root: root)
    XCTAssertEqual(opened.root, root)
    XCTAssertTrue(
      FileManager.default.fileExists(atPath: root.appendingPathComponent("sessions.db").path)
    )
  }

  func testDebugRenderingNamesTheStorageRoot() {
    XCTAssertTrue("\(storage!)".contains(":memory:"))
  }
}
