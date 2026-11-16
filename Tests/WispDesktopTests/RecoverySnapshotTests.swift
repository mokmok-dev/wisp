import Foundation
import WispAudioKit
import WispCore
import WispStorage
import XCTest

@testable import WispDesktop

final class RecoverySnapshotTests: XCTestCase {
  private var storage: Storage!
  private var root: URL!
  private var recordings: URL!
  private var outputDir: URL!

  private let startedAt = DesktopFixtures.date(2026, 7, 15, 5, 0, 0)
  private let endedAt = DesktopFixtures.date(2026, 7, 15, 5, 1, 0)

  override func setUpWithError() throws {
    storage = try Storage.openInMemory()
    root = FileManager.default.temporaryDirectory
      .appendingPathComponent("wisp-recovery-test-\(UUID().uuidString)", isDirectory: true)
    recordings = root.appendingPathComponent("recordings", isDirectory: true)
    outputDir = recordings.appendingPathComponent("2026-07-15T050000Z-abc", isDirectory: true)
    try FileManager.default.createDirectory(at: outputDir, withIntermediateDirectories: true)
    addTeardownBlock { [root] in
      if let root {
        try? FileManager.default.removeItem(at: root)
      }
    }
  }

  override func tearDown() {
    storage = nil
    root = nil
    recordings = nil
    outputDir = nil
  }

  // MARK: - Codec

  @MainActor
  private func snapshot(
    pendingWrite: RecoveryPendingWrite = .finalise(
      endedAt:
        DesktopFixtures
        .date(2026, 7, 15, 5, 1, 0)),
    directoryName: String? = "2026-07-15T050000Z-abc"
  ) -> RecoverySnapshot {
    RecoverySnapshot(
      version: 1,
      sessionID: SessionID(7),
      startedAt: DesktopFixtures.date(2026, 7, 15, 5, 0, 0),
      directoryName: directoryName,
      pendingWrite: pendingWrite,
      segments: [
        RecoverySegment(
          source: .mic,
          id: 3,
          text: "live transcript",
          startSeconds: 0.25,
          endSeconds: 1.5,
          isFinal: true
        ),
        RecoverySegment(
          source: .system,
          id: 4,
          text: "応答",
          startSeconds: 2,
          endSeconds: 3.5,
          isFinal: false
        ),
      ]
    )
  }

  @MainActor
  func testSnapshotRoundTripsThroughJSON() throws {
    let original = snapshot()
    let decoded = try RecoverySnapshot.decode(original.encoded())

    XCTAssertEqual(decoded.version, 1)
    XCTAssertEqual(decoded.sessionID, SessionID(7))
    XCTAssertEqual(decoded.startedAt, startedAt)
    XCTAssertEqual(decoded.directoryName, "2026-07-15T050000Z-abc")
    XCTAssertEqual(decoded.pendingWrite, .finalise(endedAt: endedAt))
    XCTAssertEqual(
      decoded.segments.map(\.source),
      [.mic, .system],
      "the source label must survive as a stable string"
    )
    XCTAssertEqual(decoded.segments.map(\.id), [3, 4])
    XCTAssertEqual(decoded.segments.map(\.text), ["live transcript", "応答"])
    XCTAssertEqual(decoded.segments.map(\.startSeconds), [0.25, 2])
    XCTAssertEqual(decoded.segments.map(\.endSeconds), [1.5, 3.5])
    XCTAssertEqual(decoded.segments.map(\.isFinal), [true, false])
  }

  @MainActor
  func testTheEncodedKeysMatchTheOnDiskContract() throws {
    let json = try XCTUnwrap(
      String(data: snapshot().encoded(), encoding: .utf8)
    )
    for key in [
      "\"version\"",
      "\"session_id\"",
      "\"started_at\"",
      "\"directory_name\"",
      "\"pending_write\"",
      "\"segments\"",
      "\"source\"",
      "\"id\"",
      "\"text\"",
      "\"start_seconds\"",
      "\"end_seconds\"",
      "\"is_final\"",
      "\"kind\"",
      "\"ended_at\"",
    ] {
      XCTAssertTrue(json.contains(key), "missing \(key) in \(json)")
    }
    XCTAssertTrue(json.contains("\"finalise\""))
  }

  @MainActor
  func testADeleteSnapshotRoundTrips() throws {
    let decoded = try RecoverySnapshot.decode(
      snapshot(pendingWrite: .delete).encoded()
    )
    XCTAssertEqual(decoded.pendingWrite, .delete)
  }

  @MainActor
  func testTheCodecCarriesTheVersionForTheLoaderToGate() throws {
    var encoded = try snapshot().encoded()
    let text = try XCTUnwrap(String(data: encoded, encoding: .utf8))
    encoded = Data(
      text.replacingOccurrences(of: "\"version\" : 1", with: "\"version\" : 2").utf8
    )
    XCTAssertEqual(try RecoverySnapshot.decode(encoded).version, 2)
  }

  // MARK: - Writing

  @MainActor
  private func makeIngest(model: AppModel) -> SessionIngest {
    SessionIngest(model: model, storage: storage, recordingsRoot: recordings)
  }

  /// A live transcript that is waiting to be finalised, as a crashed process
  /// would have left it.
  @MainActor
  private func pendingModel() -> AppModel {
    let model = AppModel()
    model.state = .failed
    model.view = .live
    model.currentSessionStartedAt = startedAt
    model.currentSessionDirName = outputDir.lastPathComponent
    model.currentOutputDir = outputDir
    model.pendingSessionWrite = .finalise(endedAt: endedAt)
    model.segments = [
      DesktopFixtures.segment(.mic, id: 1, text: "live transcript", isFinal: true),
      DesktopFixtures.segment(
        .system,
        id: 2,
        text: "応答",
        start: 0.5,
        end: 1.5,
        isFinal: true
      ),
    ]
    return model
  }

  @MainActor
  func testWritingASnapshotPlacesItBesideTheOggFiles() throws {
    let model = pendingModel()
    let path = try makeIngest(model: model).writeRecoverySnapshot()

    XCTAssertEqual(path.lastPathComponent, SessionIngest.recoveryFileName)
    XCTAssertEqual(path.deletingLastPathComponent(), outputDir)
    XCTAssertTrue(FileManager.default.fileExists(atPath: path.path))
    XCTAssertFalse(
      FileManager.default.fileExists(
        atPath: outputDir.appendingPathComponent(".\(SessionIngest.recoveryFileName).tmp")
          .path
      ),
      "the temporary file must be renamed away"
    )
  }

  @MainActor
  func testWritingASnapshotWithoutAPendingWriteFails() {
    let model = pendingModel()
    model.pendingSessionWrite = nil

    XCTAssertThrowsError(try makeIngest(model: model).writeRecoverySnapshot()) { error in
      XCTAssertEqual(error as? RecoveryError, .noPendingWrite)
    }
  }

  @MainActor
  func testWritingASnapshotWithoutAnOutputDirectoryFails() {
    let model = pendingModel()
    model.currentOutputDir = nil

    XCTAssertThrowsError(try makeIngest(model: model).writeRecoverySnapshot()) { error in
      XCTAssertEqual(error as? RecoveryError, .missingOutputDirectory)
    }
  }

  // MARK: - Recovery on launch

  @MainActor
  func testStartupRetriesAValidSnapshotAndRemovesIt() throws {
    let crashed = pendingModel()
    let path = try makeIngest(model: crashed).writeRecoverySnapshot()

    let model = AppModel()
    let recovered = makeIngest(model: model).recoverPendingSessions()

    XCTAssertEqual(recovered, 1)
    XCTAssertFalse(model.hasUnsettledSession)
    XCTAssertFalse(FileManager.default.fileExists(atPath: path.path))

    let sessions = try storage.sessions.list()
    XCTAssertEqual(sessions.count, 1)
    XCTAssertEqual(sessions[0].endedAt, endedAt)
    let stored = try storage.segments.listBySession(sessions[0].id)
    XCTAssertEqual(stored.map(\.text), ["live transcript", "応答"])
  }

  @MainActor
  func testStartupRecreatesAMissingRetainedRow() throws {
    let sessionID = try Library.createSession(
      storage,
      startedAt: startedAt,
      dirName: outputDir.lastPathComponent,
      title: ""
    )
    let crashed = pendingModel()
    crashed.currentSessionID = sessionID
    crashed.linkedSessionID = sessionID
    _ = try makeIngest(model: crashed).writeRecoverySnapshot()
    try storage.sessions.delete(sessionID)

    let model = AppModel()
    XCTAssertEqual(makeIngest(model: model).recoverPendingSessions(), 1)
    XCTAssertFalse(model.hasUnsettledSession)

    let sessions = try storage.sessions.list()
    XCTAssertEqual(sessions.count, 1)
    XCTAssertNotEqual(sessions[0].id, sessionID)
    XCTAssertEqual(
      try storage.segments.listBySession(sessions[0].id).map(\.text),
      ["live transcript", "応答"]
    )
  }

  @MainActor
  func testStartupDiscardsAStaleSnapshotForAnAlreadyPersistedSession() throws {
    let sessionID = try Library.createSession(
      storage,
      startedAt: startedAt,
      dirName: outputDir.lastPathComponent,
      title: ""
    )
    let completed = pendingModel()
    completed.currentSessionID = sessionID
    completed.linkedSessionID = sessionID
    let path = try makeIngest(model: completed).writeRecoverySnapshot()

    // The transaction committed and only the sidecar unlink was interrupted.
    try Library.finaliseSession(
      storage,
      sessionID: sessionID,
      segments: completed.segments,
      endedAt: endedAt
    )
    XCTAssertTrue(FileManager.default.fileExists(atPath: path.path))

    let model = AppModel()
    XCTAssertEqual(makeIngest(model: model).recoverPendingSessions(), 1)
    XCTAssertFalse(model.hasUnsettledSession)
    XCTAssertFalse(FileManager.default.fileExists(atPath: path.path))

    let sessions = try storage.sessions.list()
    XCTAssertEqual(sessions.count, 1, "a stale sidecar must not duplicate the row")
    XCTAssertEqual(
      try storage.segments.listBySession(sessionID).map(\.text),
      ["live transcript", "応答"]
    )
  }

  @MainActor
  func testStartupRejectsASnapshotWhoseDirectoryDoesNotMatchItsLocation() throws {
    let model = pendingModel()
    let displaced = RecoverySnapshot(
      version: 1,
      sessionID: nil,
      startedAt: startedAt,
      directoryName: "somewhere-else",
      pendingWrite: .finalise(endedAt: endedAt),
      segments: [
        RecoverySegment(
          source: .mic,
          id: 1,
          text: "orphan",
          startSeconds: 0,
          endSeconds: 1,
          isFinal: true
        )
      ]
    )
    try displaced.encoded().write(
      to: outputDir.appendingPathComponent(SessionIngest.recoveryFileName)
    )

    let fresh = AppModel()
    XCTAssertEqual(makeIngest(model: fresh).recoverPendingSessions(), 0)
    XCTAssertFalse(fresh.hasUnsettledSession)
    XCTAssertTrue(
      FileManager.default.fileExists(
        atPath: outputDir.appendingPathComponent(SessionIngest.recoveryFileName).path
      ),
      "an unusable snapshot is left in place for a human"
    )
    XCTAssertEqual(try storage.sessions.list().count, 0)
    _ = model
  }

  @MainActor
  func testStartupRejectsASnapshotWithAnUnparsableTimestamp() throws {
    let json = """
      {
        "version" : 1,
        "session_id" : null,
        "started_at" : "not a date",
        "directory_name" : "2026-07-15T050000Z-abc",
        "pending_write" : { "kind" : "finalise", "ended_at" : "2026-07-15T05:01:00+00:00" },
        "segments" : []
      }
      """
    try Data(json.utf8).write(
      to: outputDir.appendingPathComponent(SessionIngest.recoveryFileName)
    )

    let fresh = AppModel()
    XCTAssertEqual(makeIngest(model: fresh).recoverPendingSessions(), 0)
    XCTAssertFalse(fresh.hasUnsettledSession)
    XCTAssertEqual(try storage.sessions.list().count, 0)
  }

  @MainActor
  func testADeleteSnapshotForAMissingRowIsAlreadyDone() throws {
    // A cleanup snapshot whose row is gone needs no work, so recovery drops the
    // sidecar instead of reinstalling a live handle for a session that ended.
    let broken = RecoverySnapshot(
      version: 1,
      sessionID: SessionID(999),
      startedAt: startedAt,
      directoryName: outputDir.lastPathComponent,
      pendingWrite: .delete,
      segments: [
        RecoverySegment(
          source: .mic,
          id: 1,
          text: "already gone",
          startSeconds: 0,
          endSeconds: 1,
          isFinal: true
        )
      ]
    )
    let path = outputDir.appendingPathComponent(SessionIngest.recoveryFileName)
    try broken.encoded().write(to: path)

    let fresh = AppModel()
    XCTAssertEqual(makeIngest(model: fresh).recoverPendingSessions(), 1)
    XCTAssertFalse(fresh.hasUnsettledSession)
    XCTAssertFalse(FileManager.default.fileExists(atPath: path.path))
    XCTAssertEqual(try storage.sessions.list().count, 0)
  }

  @MainActor
  func testRecoveryScansSnapshotsInPathOrder() throws {
    let first = pendingModel()
    let firstPath = try makeIngest(model: first).writeRecoverySnapshot()

    let otherDir = recordings.appendingPathComponent(
      "2026-07-15T060000Z-def",
      isDirectory: true
    )
    try FileManager.default.createDirectory(at: otherDir, withIntermediateDirectories: true)
    let second = AppModel()
    second.state = .failed
    second.view = .live
    second.currentSessionStartedAt = startedAt
    second.currentSessionDirName = otherDir.lastPathComponent
    second.currentOutputDir = otherDir
    second.pendingSessionWrite = .finalise(endedAt: endedAt)
    second.segments = [DesktopFixtures.segment(.mic, id: 5, text: "second", isFinal: true)]
    _ = try makeIngest(model: second).writeRecoverySnapshot()

    let model = AppModel()
    XCTAssertEqual(makeIngest(model: model).recoverPendingSessions(), 2)
    XCTAssertEqual(try storage.sessions.list().count, 2)
    XCTAssertFalse(FileManager.default.fileExists(atPath: firstPath.path))
  }

  @MainActor
  func testAFailedFinalisationWritesARecoverySnapshotAndRetryRecreatesTheRow() throws {
    let model = pendingModel()
    model.segments = [
      DesktopFixtures.segment(
        .mic,
        id: 1,
        text: "live transcript",
        isFinal: true
      )
    ]
    // Without launch metadata the retry cannot resolve or create a row, which
    // is the case a durable snapshot has to cover.
    let launchMetadata = model.currentSessionStartedAt
    model.currentSessionStartedAt = nil

    let ingest = makeIngest(model: model)
    XCTAssertFalse(ingest.retryPendingPersistence())
    XCTAssertEqual(model.state, .failed)
    XCTAssertNotNil(model.lastError)
    let path = outputDir.appendingPathComponent(SessionIngest.recoveryFileName)
    XCTAssertTrue(FileManager.default.fileExists(atPath: path.path))

    model.currentSessionStartedAt = launchMetadata
    XCTAssertTrue(ingest.retryPendingPersistence())
    XCTAssertEqual(model.state, .idle)
    XCTAssertFalse(model.hasUnsettledSession)
    XCTAssertFalse(FileManager.default.fileExists(atPath: path.path))
    XCTAssertEqual(try storage.sessions.list().count, 1)
    XCTAssertEqual(
      try storage.segments.listBySession(XCTUnwrap(try storage.sessions.list().first).id)
        .map(\.text),
      ["live transcript"]
    )
  }

  @MainActor
  func testPersistenceRefusesAnIDOwnedByADifferentRecording() throws {
    let foreignID = try Library.createSession(
      storage,
      startedAt: startedAt,
      dirName: "2026-07-15T040000Z-other",
      title: "foreign"
    )
    let model = pendingModel()
    model.currentSessionID = foreignID

    XCTAssertFalse(makeIngest(model: model).retryPendingPersistence())
    XCTAssertEqual(try storage.sessions.get(foreignID)?.endedAt, nil)
    XCTAssertEqual(try storage.sessions.list().count, 1)
  }

  @MainActor
  func testDeletingAnUnstartedRowReleasesTheHandle() throws {
    let sessionID = try Library.createSession(
      storage,
      startedAt: startedAt,
      dirName: outputDir.lastPathComponent,
      title: ""
    )
    let model = pendingModel()
    model.currentSessionID = sessionID
    model.linkedSessionID = sessionID
    model.pendingSessionWrite = .delete
    model.state = .failed

    XCTAssertTrue(makeIngest(model: model).retryPendingPersistence())
    XCTAssertEqual(model.state, .idle)
    XCTAssertNil(model.currentSessionID)
    XCTAssertFalse(model.hasUnsettledSession)
    XCTAssertEqual(try storage.sessions.list().count, 0)
  }

  // MARK: - Update application

  @MainActor
  func testStartedKeepsAnInFlightStopFromRegressing() throws {
    let sessionID = try Library.createSession(
      storage,
      startedAt: startedAt,
      dirName: outputDir.lastPathComponent,
      title: ""
    )
    let model = AppModel()
    model.beginSessionStart()
    model.currentSessionID = sessionID
    model.currentSessionStartedAt = startedAt
    model.currentSessionDirName = outputDir.lastPathComponent
    model.setState(.stopping)

    makeIngest(model: model).applyUpdate(
      .started(
        SessionStart(
          sessionID: sessionID,
          startedAt: startedAt,
          dirName: outputDir.lastPathComponent
        )
      )
    )

    XCTAssertEqual(model.state, .stopping)
    XCTAssertEqual(model.currentSessionID, sessionID)
  }

  @MainActor
  func testAStaleUpdateCannotMutateTheCurrentTranscript() throws {
    let staleDirectory = "2026-07-15T040000Z-stale"
    let currentID = try Library.createSession(
      storage,
      startedAt: startedAt,
      dirName: outputDir.lastPathComponent,
      title: ""
    )
    let staleID = try Library.createSession(
      storage,
      startedAt: startedAt,
      dirName: staleDirectory,
      title: ""
    )
    let model = AppModel()
    model.beginSessionStart()
    model.currentSessionID = currentID
    model.linkedSessionID = currentID
    model.currentSessionStartedAt = startedAt
    model.currentSessionDirName = outputDir.lastPathComponent

    let ingest = makeIngest(model: model)
    ingest.applyUpdate(
      .started(
        SessionStart(sessionID: staleID, startedAt: startedAt, dirName: staleDirectory)
      )
    )
    ingest.applyUpdate(
      .event(
        staleID,
        WispSession.Result(
          source: .mic,
          segmentID: 1,
          isFinal: false,
          text: "stale transcript",
          startSeconds: 0,
          endSeconds: 1,
          confidenceMean: nil,
          confidenceMin: nil
        )
      )
    )
    ingest.applyUpdate(.stopped(staleID))

    XCTAssertEqual(model.state, .starting)
    XCTAssertEqual(model.currentSessionID, currentID)
    XCTAssertEqual(model.linkedSessionID, currentID)
    XCTAssertTrue(model.segments.isEmpty)
  }

  @MainActor
  func testStoppedFinalisesSegmentsAndPersistsThem() throws {
    let sessionID = try Library.createSession(
      storage,
      startedAt: startedAt,
      dirName: outputDir.lastPathComponent,
      title: ""
    )
    let model = AppModel()
    model.beginSessionStart()
    model.setState(.recording)
    model.currentSessionID = sessionID
    model.linkedSessionID = sessionID
    model.currentSessionStartedAt = startedAt
    model.currentSessionDirName = outputDir.lastPathComponent
    model.currentOutputDir = outputDir
    model.segments = [DesktopFixtures.segment(.mic, id: 1, text: "hello")]

    makeIngest(model: model).applyUpdate(.stopped(sessionID))

    XCTAssertEqual(model.state, .idle)
    XCTAssertTrue(model.segments.allSatisfy(\.isFinal))
    XCTAssertNil(model.currentSessionID)
    XCTAssertNil(model.pendingSessionWrite)
    XCTAssertNil(model.currentOutputDir)
    let stored = try storage.segments.listBySession(sessionID)
    XCTAssertEqual(stored.map(\.text), ["hello"])
    XCTAssertNotNil(try storage.sessions.get(sessionID)?.endedAt)
  }

  @MainActor
  func testConstructionFailureDeletesThePreallocatedRow() throws {
    let sessionID = try Library.createSession(
      storage,
      startedAt: startedAt,
      dirName: outputDir.lastPathComponent,
      title: ""
    )
    let model = AppModel()
    model.beginSessionStart()
    model.currentSessionID = sessionID
    model.linkedSessionID = sessionID
    model.currentSessionStartedAt = startedAt
    model.currentSessionDirName = outputDir.lastPathComponent

    makeIngest(model: model).applyUpdate(
      .error(sessionID, .construction("could not construct"))
    )

    XCTAssertEqual(model.state, .failed)
    XCTAssertNil(model.currentSessionID)
    XCTAssertEqual(try storage.sessions.list().count, 0)
  }
}
