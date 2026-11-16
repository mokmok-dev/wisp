import Foundation
import WispCore
import WispStorage
import XCTest

@testable import WispDesktop

/// Quitting must never drop a transcript, so the decision is exercised directly
/// rather than through the app delegate.
@MainActor
final class QuitTests: XCTestCase {
  private struct Fixture {
    let root: URL
    let storage: Storage
    let controller: SessionController
  }

  private let startedAt = DesktopFixtures.date(2026, 7, 15, 5, 0, 0)

  private func makeFixture(root: URL? = nil) throws -> Fixture {
    let root =
      root
      ?? FileManager.default.temporaryDirectory
      .appendingPathComponent("wisp-quit-test-\(UUID().uuidString)", isDirectory: true)
    try FileManager.default.createDirectory(at: root, withIntermediateDirectories: true)
    addTeardownBlock {
      try? FileManager.default.removeItem(at: root)
    }
    let storage = try Storage.openInMemory()
    return Fixture(
      root: root,
      storage: storage,
      controller: SessionController(
        dataDirectory: root,
        storage: storage,
        settingsStore: SettingsStore(directory: root),
        settings: Settings()
      )
    )
  }

  func testAnIdleAppQuitsImmediately() throws {
    XCTAssertTrue(try makeFixture().controller.prepareForTermination())
  }

  func testAnUnsavedTranscriptIsPersistedAndThenQuitIsAllowed() throws {
    let fixture = try makeFixture()
    let dirName = "2026-07-15T050000Z-quit"
    let sessionID = try Library.createSession(
      fixture.storage,
      startedAt: startedAt,
      dirName: dirName,
      title: "quit me"
    )
    let model = fixture.controller.model
    model.view = .live
    model.setState(.failed)
    model.currentSessionID = sessionID
    model.linkedSessionID = sessionID
    model.currentSessionStartedAt = startedAt
    model.currentSessionDirName = dirName
    model.pendingSessionWrite = .finalise(endedAt: startedAt)
    model.segments = [DesktopFixtures.segment(.mic, id: 1, text: "must survive", isFinal: true)]

    XCTAssertTrue(fixture.controller.prepareForTermination())
    XCTAssertFalse(model.hasUnsettledSession)
    XCTAssertEqual(
      try fixture.storage.segments.listBySession(sessionID).map(\.text),
      ["must survive"]
    )
  }

  func testAnUnretryableTranscriptWritesASidecarAndCancelsTheQuit() throws {
    let fixture = try makeFixture()
    let dirName = "2026-07-15T050000Z-quit-broken"
    let outputDir = fixture.root
      .appendingPathComponent("recordings/\(dirName)", isDirectory: true)
    try FileManager.default.createDirectory(at: outputDir, withIntermediateDirectories: true)
    let model = fixture.controller.model
    model.view = .live
    model.setState(.failed)
    model.currentSessionStartedAt = startedAt
    model.currentSessionDirName = dirName
    model.currentOutputDir = outputDir
    // A cleanup with no row to delete cannot commit, so the transcript can
    // only survive as a sidecar.
    model.pendingSessionWrite = .delete
    model.segments = [DesktopFixtures.segment(.mic, id: 1, text: "sidecar", isFinal: true)]

    XCTAssertFalse(
      fixture.controller.prepareForTermination(),
      "quitting without a durable transcript must be refused"
    )
    XCTAssertTrue(model.hasUnsettledSession)
    XCTAssertTrue(
      FileManager.default.fileExists(
        atPath: outputDir.appendingPathComponent(SessionIngest.recoveryFileName).path
      ),
      "a transcript the database refused must still reach disk"
    )
  }

  // MARK: - Startup ordering

  func testRecoveryCompletesBeforeTheControllerIsHandedOut() throws {
    // A snapshot from a crashed run. Recovery has to land during init: the menu
    // is live before SwiftUI's `.task` runs, so a ⌘R in that window would
    // otherwise start a recording whose transcript recovery then overwrites.
    let root = FileManager.default.temporaryDirectory
      .appendingPathComponent("wisp-startup-order-\(UUID().uuidString)", isDirectory: true)
    addTeardownBlock { try? FileManager.default.removeItem(at: root) }
    let recordings = WispPaths.recordingsDirectory(in: root)
    let outputDir = recordings.appendingPathComponent("2026-07-15T050000Z-crash", isDirectory: true)
    try FileManager.default.createDirectory(at: outputDir, withIntermediateDirectories: true)

    let crashed = AppModel()
    crashed.state = .failed
    crashed.view = .live
    // A cleanup snapshot with no row to delete cannot commit, so recovery
    // installs it as a live handle instead of silently discarding it.
    crashed.currentSessionID = nil
    crashed.currentSessionStartedAt = startedAt
    crashed.currentSessionDirName = outputDir.lastPathComponent
    crashed.currentOutputDir = outputDir
    crashed.pendingSessionWrite = .delete
    crashed.segments = [DesktopFixtures.segment(.mic, id: 1, text: "recovered", isFinal: true)]

    let seedStorage = try Storage.open(root: root)
    let seed = SessionIngest(model: crashed, storage: seedStorage, recordingsRoot: recordings)
    _ = try seed.writeRecoverySnapshot()

    // A fresh controller over the same root: no `start()` call, because that is
    // exactly the state the menu is live in.
    let storage = try Storage.open(root: root)
    let controller = SessionController(
      dataDirectory: root,
      storage: storage,
      settingsStore: SettingsStore(directory: root),
      settings: Settings()
    )

    XCTAssertTrue(
      controller.model.hasUnsettledSession,
      "recovery must already own the transcript before any input is possible"
    )
    XCTAssertEqual(controller.model.state, .failed)
    XCTAssertEqual(controller.model.segments.map(\.text), ["recovered"])
    XCTAssertTrue(
      FileManager.default.fileExists(
        atPath: outputDir.appendingPathComponent(SessionIngest.recoveryFileName).path
      ),
      "a snapshot that could not commit stays beside the Ogg files"
    )
  }

  func testTheLibraryIsPopulatedBeforeTheFirstRender() throws {
    let root = FileManager.default.temporaryDirectory
      .appendingPathComponent("wisp-startup-library-\(UUID().uuidString)", isDirectory: true)
    addTeardownBlock { try? FileManager.default.removeItem(at: root) }
    try FileManager.default.createDirectory(at: root, withIntermediateDirectories: true)

    let seedStorage = try Storage.open(root: root)
    for offset in 0..<3 {
      _ = try Library.createSession(
        seedStorage,
        startedAt: startedAt.addingTimeInterval(Double(offset)),
        dirName: "2026-07-15T05000\(offset)Z-seed",
        title: "seed \(offset)"
      )
    }

    let storage = try Storage.open(root: root)
    let controller = SessionController(
      dataDirectory: root,
      storage: storage,
      settingsStore: SettingsStore(directory: root),
      settings: Settings()
    )

    XCTAssertEqual(
      controller.model.library.count,
      3,
      "the first paint must not show an empty library"
    )
  }
}
