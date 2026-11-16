import Foundation
import WispCore
import XCTest

@testable import WispStorage

final class SegmentsTests: XCTestCase {
  private var storage: Storage!
  private var sessionID: SessionID!

  override func setUpWithError() throws {
    storage = try Storage.openInMemory()
    sessionID = try Fixtures.freshSession(storage)
  }

  private func append(
    _ text: String,
    source: SourceLabel = .mic,
    index: UInt32 = 0,
    start: Double = 0,
    end: Double = 1
  ) throws {
    try storage.segments.append(
      Fixtures.segment(
        session: sessionID,
        source: source,
        index: index,
        start: start,
        end: end,
        text: text
      )
    )
  }

  func testAppendThenListInterleavesByTime() throws {
    try append("hello", index: 0, start: 0, end: 2)
    try append("hi there", source: .system, index: 0, start: 1, end: 3)
    try append("let's start", index: 1, start: 5, end: 7)

    let all = try storage.segments.listBySession(sessionID)
    XCTAssertEqual(all.map(\.text), ["hello", "hi there", "let's start"])
  }

  func testAppendPersistsEveryField() throws {
    let before = Date()
    let id = try storage.segments.append(
      NewSegment(
        sessionID: sessionID,
        source: .system,
        segmentIndex: 3,
        startSeconds: 1.25,
        endSeconds: 2.5,
        text: "labelled",
        speakerLabel: "Alice"
      )
    )
    let stored = try storage.segments.listBySession(sessionID)
    XCTAssertEqual(stored.count, 1)
    let segment = try XCTUnwrap(stored.first)
    XCTAssertEqual(segment.id, id)
    XCTAssertEqual(segment.sessionID, sessionID)
    XCTAssertEqual(segment.source, .system)
    XCTAssertEqual(segment.segmentIndex, 3)
    XCTAssertEqual(segment.startSeconds, 1.25)
    XCTAssertEqual(segment.endSeconds, 2.5)
    XCTAssertEqual(segment.text, "labelled")
    XCTAssertEqual(segment.speakerLabel, "Alice")
    XCTAssertGreaterThanOrEqual(segment.createdAt, before.addingTimeInterval(-1))
  }

  func testDuplicateSegmentIndexWithinSourceFails() throws {
    try append("a", index: 0, start: 0, end: 1)
    XCTAssertThrowsError(try append("dup", index: 0, start: 0, end: 1)) { error in
      guard case StorageError.sqlite = error else {
        XCTFail("expected a sqlite constraint failure, got \(error)")
        return
      }
    }
  }

  func testTheSameIndexIsAllowedForTheOtherSource() throws {
    try append("a", source: .mic, index: 0, start: 0, end: 1)
    try append("b", source: .system, index: 0, start: 0, end: 1)
    XCTAssertEqual(try storage.segments.listBySession(sessionID).count, 2)
  }

  func testFullTextSearchReturnsTheRelevantSegment() throws {
    try append("today's weather is nice", index: 0, start: 0, end: 1)
    try append("let us discuss the roadmap", index: 1, start: 2, end: 3)
    try append("good morning everyone", source: .system, index: 0, start: 0.5, end: 1.5)

    let hits = try storage.segments.search("weather", limit: 10)
    XCTAssertEqual(hits.count, 1)
    XCTAssertTrue(hits[0].text.contains("weather"))
  }

  func testSearchTextMatchesATwoCharacterJapaneseWord() throws {
    try append("今日は天気がいいですね", index: 0, start: 0, end: 1)
    try append("会議の議題を確認します", source: .system, index: 0, start: 0, end: 1)

    let hits = try storage.segments.searchText("天気", limit: 10)
    XCTAssertEqual(hits.map(\.text), ["今日は天気がいいですね"])
  }

  func testSearchTextMatchesAThreeCharacterJapaneseWord() throws {
    try append("今日は天気がいいですね", index: 0, start: 0, end: 1)
    try append("会議の議題を確認します", source: .system, index: 0, start: 0, end: 1)

    let hits = try storage.segments.searchText("天気が", limit: 10)
    XCTAssertEqual(hits.map(\.text), ["今日は天気がいいですね"])
  }

  func testSearchTextRequiresEveryASCIITerm() throws {
    for (index, text) in ["alpha beta gamma", "alpha only", "beta only"].enumerated() {
      try append(
        text,
        index: UInt32(index),
        start: Double(index),
        end: Double(index) + 1
      )
    }

    let hits = try storage.segments.searchText("alpha beta", limit: 10)
    XCTAssertEqual(hits.map(\.text), ["alpha beta gamma"])
  }

  func testSearchTextTreatsADoubleQuoteLiterally() throws {
    try append("he said \"weather\" twice", index: 0, start: 0, end: 1)
    try append("weather is nice", source: .system, index: 0, start: 0, end: 1)

    let hits = try storage.segments.searchText("weather\"", limit: 10)
    XCTAssertEqual(hits.map(\.text), ["he said \"weather\" twice"])
  }

  func testSearchTextOfBlankInputReturnsNothing() throws {
    try append("anything", index: 0, start: 0, end: 1)
    XCTAssertEqual(try storage.segments.searchText("", limit: 10), [])
    XCTAssertEqual(try storage.segments.searchText("  \t ", limit: 10), [])
  }

  func testSearchTextTreatsLikeWildcardsLiterally() throws {
    try append("50% off", index: 0, start: 0, end: 1)
    try append("50 off", source: .system, index: 0, start: 0, end: 1)

    let hits = try storage.segments.searchText("%", limit: 10)
    XCTAssertEqual(hits.map(\.text), ["50% off"])
  }

  func testSearchTextTreatsUnderscoreLiterally() throws {
    try append("snake_case", index: 0, start: 0, end: 1)
    try append("snakeXcase", source: .system, index: 0, start: 0, end: 1)

    let hits = try storage.segments.searchText("_", limit: 10)
    XCTAssertEqual(hits.map(\.text), ["snake_case"])
  }

  func testSearchTextTreatsBackslashLiterally() throws {
    try append("a\\b", index: 0, start: 0, end: 1)
    try append("aXb", source: .system, index: 0, start: 0, end: 1)

    let hits = try storage.segments.searchText("\\", limit: 10)
    XCTAssertEqual(hits.map(\.text), ["a\\b"])
  }

  func testSearchTextHonoursTheLimit() throws {
    for index in 0..<5 {
      try append("shared word \(index)", index: UInt32(index), start: Double(index))
    }
    XCTAssertEqual(try storage.segments.searchText("shared", limit: 2).count, 2)
  }

  func testSubstringFallbackOrdersByStartThenID() throws {
    try append("aa", index: 1, start: 5, end: 6)
    try append("aa", index: 0, start: 1, end: 2)
    let hits = try storage.segments.searchText("a", limit: 10)
    XCTAssertEqual(hits.map(\.startSeconds), [1, 5])
  }

  func testDeletingASessionCascadesToItsSegments() throws {
    try append("x", index: 0, start: 0, end: 1)
    XCTAssertEqual(try storage.segments.listBySession(sessionID).count, 1)
    try storage.sessions.delete(sessionID)
    XCTAssertEqual(try storage.segments.listBySession(sessionID), [])
  }

  func testUnknownSourceLabelIsReported() throws {
    try append("x", index: 0, start: 0, end: 1)
    try storage.connection.run("UPDATE segments SET source = 'speaker'")
    XCTAssertThrowsError(try storage.segments.listBySession(sessionID)) { error in
      XCTAssertEqual(error as? StorageError, .invalidSourceLabel("speaker"))
    }
  }
}
