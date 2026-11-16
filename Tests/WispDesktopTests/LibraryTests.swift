import Foundation
import WispCore
import WispStorage
import XCTest

@testable import WispDesktop

final class LibraryTests: XCTestCase {
  private var storage: Storage!

  override func setUpWithError() throws {
    storage = try Storage.openInMemory()
  }

  override func tearDown() {
    storage = nil
  }

  func testTheDirectoryNameIsTimestampedAndUnique() {
    let startedAt = DesktopFixtures.date(2026, 7, 15, 4, 30, 0)
    let first = Library.sessionDirectoryName(startedAt: startedAt)
    let second = Library.sessionDirectoryName(startedAt: startedAt)

    XCTAssertTrue(
      first.hasPrefix("2026-07-15T043000Z-"),
      "existing recordings use the UTC minute form: \(first)"
    )
    XCTAssertEqual(first.count, "2026-07-15T043000Z-".count + 36, "a UUID suffix follows")
    XCTAssertNotEqual(first, second, "same-second starts must not share a directory")
  }

  func testTheDefaultTitleIsTheLocalStartMinute() {
    let startedAt = DesktopFixtures.date(2026, 7, 15, 4, 30, 0)
    XCTAssertEqual(Library.defaultTitle(startedAt: startedAt), Format.startedAt(startedAt))
  }

  func testCreatedSessionPathsMatchTheRecordingDirectory() throws {
    let startedAt = DesktopFixtures.date(2026, 7, 15, 4, 30, 0)
    let dirName = Library.sessionDirectoryName(startedAt: startedAt)

    let id = try Library.createSession(
      storage,
      startedAt: startedAt,
      dirName: dirName,
      title: ""
    )
    let session = try XCTUnwrap(try storage.sessions.get(id))

    XCTAssertEqual(session.micWavPath, "recordings/\(dirName)/mic.ogg")
    XCTAssertEqual(session.systemWavPath, "recordings/\(dirName)/system.ogg")
    XCTAssertEqual(session.startedAt, startedAt)
    XCTAssertEqual(session.title, Library.defaultTitle(startedAt: startedAt))
  }

  func testACustomTitleWinsAndABlankOneFallsBack() throws {
    let startedAt = DesktopFixtures.date(2026, 7, 15, 4, 30, 0)
    let named = try Library.createSession(
      storage,
      startedAt: startedAt,
      dirName: Library.sessionDirectoryName(startedAt: startedAt),
      title: "  Q3 review  "
    )
    XCTAssertEqual(try storage.sessions.get(named)?.title, "Q3 review")

    let blank = try Library.createSession(
      storage,
      startedAt: startedAt,
      dirName: Library.sessionDirectoryName(startedAt: startedAt),
      title: "  "
    )
    XCTAssertEqual(
      try storage.sessions.get(blank)?.title,
      Library.defaultTitle(startedAt: startedAt)
    )
  }

  func testFinaliseAssignsPerSourceIndicesAndSkipsEmptyText() throws {
    let startedAt = DesktopFixtures.date(2026, 7, 15, 4, 30, 0)
    let id = try Library.createSession(
      storage,
      startedAt: startedAt,
      dirName: Library.sessionDirectoryName(startedAt: startedAt),
      title: "x"
    )
    let endedAt = DesktopFixtures.date(2026, 7, 15, 4, 31, 0)
    let segments = [
      DesktopFixtures.segment(
        .mic,
        id: 1,
        text: "first mic",
        start: 0,
        end: 1,
        isFinal: true
      ),
      // SpeechAnalyzer emits zero-length results during silence.
      DesktopFixtures.segment(.mic, id: 2, text: "   ", start: 1, end: 2, isFinal: true),
      DesktopFixtures.segment(
        .system,
        id: 1,
        text: "first sys",
        start: 2,
        end: 3,
        isFinal: true
      ),
      DesktopFixtures.segment(
        .mic,
        id: 3,
        text: "second mic",
        start: 3,
        end: 4,
        isFinal: true
      ),
    ]

    try Library.finaliseSession(storage, sessionID: id, segments: segments, endedAt: endedAt)

    let stored = try storage.segments.listBySession(id)
    XCTAssertEqual(stored.map(\.text), ["first mic", "first sys", "second mic"])
    let indices = stored.map { ($0.source, $0.segmentIndex) }
    XCTAssertEqual(indices.map(\.0), [.mic, .system, .mic])
    // The dropped empty row still consumes its index, matching the walk order
    // the previous implementation wrote.
    XCTAssertEqual(indices.map(\.1), [0, 0, 2])
    XCTAssertEqual(try storage.sessions.get(id)?.endedAt, endedAt)
  }

  func testLoadHistoryReturnsFinalisedSegmentsInTimeOrder() throws {
    let startedAt = DesktopFixtures.date(2026, 7, 15, 4, 30, 0)
    let id = try Library.createSession(
      storage,
      startedAt: startedAt,
      dirName: Library.sessionDirectoryName(startedAt: startedAt),
      title: "x"
    )
    try Library.finaliseSession(
      storage,
      sessionID: id,
      segments: [
        DesktopFixtures.segment(.system, id: 1, text: "一文目。二文目。", start: 0, end: 1),
        DesktopFixtures.segment(.mic, id: 2, text: "はい", start: 2, end: 3),
      ],
      endedAt: DesktopFixtures.date(2026, 7, 15, 4, 31, 0)
    )

    let loaded = try Library.loadHistory(storage, sessionID: id)

    XCTAssertEqual(loaded.map(\.text), ["一文目。二文目。", "はい"])
    XCTAssertTrue(loaded.allSatisfy(\.isFinal))
    XCTAssertEqual(loaded[0].displayText, "一文目。\n二文目。")
    XCTAssertEqual(loaded.map(\.id), [0, 0])
  }

  // MARK: - Recording removal

  /// A stored session whose Ogg paths sit inside `relativeDirectory`.
  private func sessionFixture(id: Int64, relativeDirectory: String) -> WispCore.Session {
    WispCore.Session(
      id: SessionID(id),
      startedAt: DesktopFixtures.date(2026, 7, 15, 4, 30, 0),
      endedAt: DesktopFixtures.date(2026, 7, 16, 4, 30, 0),
      title: "Test",
      micWavPath: "\(relativeDirectory)/mic.ogg",
      systemWavPath: "\(relativeDirectory)/system.ogg",
      notes: ""
    )
  }

  private func makeRecordingTree() throws -> (root: URL, recordings: URL, sessionDir: URL) {
    let root = FileManager.default.temporaryDirectory
      .appendingPathComponent("wisp-library-test-\(UUID().uuidString)", isDirectory: true)
    let recordings = root.appendingPathComponent("recordings", isDirectory: true)
    let sessionDir = recordings.appendingPathComponent(
      "2026-07-15T043000Z-abc",
      isDirectory: true
    )
    try FileManager.default.createDirectory(at: sessionDir, withIntermediateDirectories: true)
    try Data("audio".utf8).write(to: sessionDir.appendingPathComponent("mic.ogg"))
    addTeardownBlock {
      try? FileManager.default.removeItem(at: root)
    }
    return (root, recordings, sessionDir)
  }

  func testRemovalDeletesOnlyTheSessionDirectoryBelowTheRecordingsRoot() throws {
    let tree = try makeRecordingTree()
    let session = sessionFixture(
      id: 1,
      relativeDirectory: "recordings/2026-07-15T043000Z-abc"
    )

    XCTAssertTrue(
      try Library.removeSessionRecordings(
        storageRoot: tree.root,
        recordingsRoot: tree.recordings,
        session: session
      )
    )
    XCTAssertFalse(FileManager.default.fileExists(atPath: tree.sessionDir.path))
  }

  func testRemovalRefusesAPathOutsideTheRecordingsRoot() throws {
    let tree = try makeRecordingTree()
    let outside = tree.root.appendingPathComponent("elsewhere", isDirectory: true)
    try FileManager.default.createDirectory(at: outside, withIntermediateDirectories: true)
    try Data("audio".utf8).write(to: outside.appendingPathComponent("mic.ogg"))

    let session = WispCore.Session(
      id: SessionID(2),
      startedAt: DesktopFixtures.date(2026, 7, 15, 4, 30, 0),
      endedAt: nil,
      title: "Test",
      micWavPath: "recordings/../elsewhere/mic.ogg",
      systemWavPath: "recordings/../elsewhere/system.ogg",
      notes: ""
    )

    XCTAssertFalse(
      try Library.removeSessionRecordings(
        storageRoot: tree.root,
        recordingsRoot: tree.recordings,
        session: session
      )
    )
    XCTAssertTrue(
      FileManager.default.fileExists(atPath: outside.appendingPathComponent("mic.ogg").path)
    )
  }

  func testRemovalTreatsAMissingDirectoryAsNothingToDo() throws {
    let tree = try makeRecordingTree()
    let session = WispCore.Session(
      id: SessionID(3),
      startedAt: DesktopFixtures.date(2026, 7, 15, 4, 30, 0),
      endedAt: nil,
      title: "Test",
      micWavPath: "recordings/absent/mic.ogg",
      systemWavPath: "recordings/absent/system.ogg",
      notes: ""
    )

    XCTAssertFalse(
      try Library.removeSessionRecordings(
        storageRoot: tree.root,
        recordingsRoot: tree.recordings,
        session: session
      )
    )
  }

  func testRemovalRefusesTracksInDifferentDirectories() throws {
    let tree = try makeRecordingTree()
    let session = WispCore.Session(
      id: SessionID(4),
      startedAt: DesktopFixtures.date(2026, 7, 15, 4, 30, 0),
      endedAt: nil,
      title: "Test",
      micWavPath: "recordings/2026-07-15T043000Z-abc/mic.ogg",
      systemWavPath: "recordings/other/system.ogg",
      notes: ""
    )

    XCTAssertFalse(
      try Library.removeSessionRecordings(
        storageRoot: tree.root,
        recordingsRoot: tree.recordings,
        session: session
      )
    )
    XCTAssertTrue(FileManager.default.fileExists(atPath: tree.sessionDir.path))
  }
}
