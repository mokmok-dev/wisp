import Foundation
import WispCore
import WispStorage
import XCTest

final class FinaliseSessionTests: XCTestCase {
  private var storage: Storage!
  private var sessionID: SessionID!
  private var endedAt: Date {
    Fixtures.date(2026, 7, 15, 11, 0, 0)
  }

  override func setUpWithError() throws {
    storage = try Storage.openInMemory()
    sessionID = try Fixtures.freshSession(storage)
  }

  private func segments(_ texts: [String], indices: [UInt32]? = nil) -> [NewSegment] {
    texts.enumerated().map { offset, text in
      Fixtures.segment(
        session: sessionID,
        index: indices?[offset] ?? UInt32(offset),
        start: Double(offset),
        end: Double(offset) + 1,
        text: text
      )
    }
  }

  func testPersistsSegmentsAndEndTimeAtomically() throws {
    try storage.finaliseSession(
      sessionID,
      segments: segments(["first", "second"]),
      endedAt: endedAt
    )

    let stored = try storage.segments.listBySession(sessionID)
    XCTAssertEqual(stored.map(\.text), ["first", "second"])
    XCTAssertEqual(try storage.sessions.get(sessionID)?.endedAt, endedAt)
  }

  func testRollsBackEveryWriteWhenASegmentFails() throws {
    try storage.segments.append(
      Fixtures.segment(
        session: sessionID,
        index: 1,
        start: 1,
        end: 2,
        text: "pre-existing"
      )
    )

    XCTAssertThrowsError(
      try storage.finaliseSession(
        sessionID,
        segments: segments(["must roll back", "duplicate index"], indices: [0, 0]),
        endedAt: endedAt
      )
    ) { error in
      guard case StorageError.sqlite = error else {
        XCTFail("duplicate segment should abort finalisation, got \(error)")
        return
      }
    }

    let stored = try storage.segments.listBySession(sessionID)
    XCTAssertEqual(stored.map(\.text), ["pre-existing"])
    XCTAssertEqual(
      try storage.segments.search("must", limit: 10),
      [],
      "the FTS trigger write should roll back with the segment"
    )
    XCTAssertNil(try storage.sessions.get(sessionID)?.endedAt)
  }

  func testRejectsSegmentsFromAnotherSession() throws {
    let otherID = try Fixtures.freshSession(storage)
    let foreign = Fixtures.segment(
      session: otherID,
      index: 0,
      start: 0,
      end: 1,
      text: "wrong"
    )

    XCTAssertThrowsError(
      try storage.finaliseSession(sessionID, segments: [foreign], endedAt: endedAt)
    ) { error in
      XCTAssertEqual(
        error as? StorageError,
        .sessionMismatch(expected: sessionID, actual: otherID)
      )
    }
    XCTAssertEqual(try storage.segments.listBySession(otherID), [])
    XCTAssertNil(try storage.sessions.get(sessionID)?.endedAt)
  }

  func testCanRetryWithACompleteSnapshot() throws {
    try storage.finaliseSession(
      sessionID,
      segments: segments(["old snapshot"]),
      endedAt: endedAt
    )
    let retryEnd = Fixtures.date(2026, 7, 15, 11, 1, 0)

    try storage.finaliseSession(
      sessionID,
      segments: segments(["replacement", "new tail"]),
      endedAt: retryEnd
    )

    let stored = try storage.segments.listBySession(sessionID)
    XCTAssertEqual(stored.map(\.text), ["replacement", "new tail"])
    XCTAssertEqual(try storage.segments.search("old", limit: 10), [])
    XCTAssertEqual(try storage.sessions.get(sessionID)?.endedAt, retryEnd)
  }

  func testRejectsAMissingSessionEvenWhenEmpty() throws {
    let missing = SessionID(404)
    XCTAssertThrowsError(
      try storage.finaliseSession(missing, segments: [], endedAt: endedAt)
    ) { error in
      XCTAssertEqual(error as? StorageError, .sessionNotFound(missing))
    }
  }

  func testAnEmptySnapshotClearsEverySegmentAndEndsTheSession() throws {
    try storage.segments.append(
      Fixtures.segment(session: sessionID, index: 0, start: 0, end: 1, text: "gone")
    )
    try storage.finaliseSession(sessionID, segments: [], endedAt: endedAt)

    XCTAssertEqual(try storage.segments.listBySession(sessionID), [])
    XCTAssertEqual(try storage.sessions.get(sessionID)?.endedAt, endedAt)
  }

  func testTheFirstFinaliseWinsWhenTwoSnapshotsRace() throws {
    try storage.finaliseSession(
      sessionID,
      segments: segments(["first"]),
      endedAt: endedAt
    )
    try storage.finaliseSession(
      sessionID,
      segments: segments(["second", "second tail"]),
      endedAt: Fixtures.date(2026, 7, 15, 11, 2, 0)
    )

    XCTAssertEqual(
      try storage.segments.listBySession(sessionID).map(\.text),
      ["second", "second tail"]
    )
  }

  func testArbitraryTextSurvivesTheTransactionalReplace() throws {
    let texts = ["plain", "with \"quotes\"", "50% off_under", "日本語のテキスト"]
    try storage.finaliseSession(
      sessionID,
      segments: segments(texts),
      endedAt: endedAt
    )
    XCTAssertEqual(try storage.segments.listBySession(sessionID).map(\.text), texts)
    XCTAssertEqual(try storage.sessions.get(sessionID)?.endedAt, endedAt)
  }
}
