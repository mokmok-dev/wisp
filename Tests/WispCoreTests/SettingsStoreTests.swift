import Foundation
import XCTest

@testable import WispCore

final class SettingsStoreTests: XCTestCase {
  private var directory: URL!

  override func setUpWithError() throws {
    directory = FileManager.default.temporaryDirectory
      .appendingPathComponent("WispSettingsTests-\(UUID().uuidString)", isDirectory: true)
    try FileManager.default.createDirectory(at: directory, withIntermediateDirectories: true)
  }

  override func tearDownWithError() throws {
    try? FileManager.default.removeItem(at: directory)
  }

  func testMissingSettingsFileUsesDefaults() {
    XCTAssertEqual(SettingsStore(directory: directory).load(), Settings())
  }

  func testCorruptSettingsFileUsesDefaults() throws {
    try Data("not json".utf8).write(to: directory.appendingPathComponent("settings.json"))
    XCTAssertEqual(SettingsStore(directory: directory).load(), Settings())
  }

  func testSaveThenLoadRoundTrips() throws {
    let store = SettingsStore(directory: directory)
    var settings = Settings(
      theme: .dark,
      transcriptionLanguage: LanguageCode("en-US"),
      meetings: MeetingDetectionSettings(enabled: true)
    )
    settings.meetings.setPolicy(.autoRecord, for: .zoom)
    try store.save(settings)

    let raw = try String(contentsOf: store.fileURL, encoding: .utf8)
    XCTAssertTrue(raw.contains("\"dark\""), raw)
    XCTAssertTrue(raw.contains("\"en-US\""), raw)
    XCTAssertEqual(store.load(), settings)
  }

  func testAThemeChangeKeepsTheLanguageAndPolicies() throws {
    let store = SettingsStore(directory: directory)
    var settings = Settings(transcriptionLanguage: LanguageCode("fr-FR"))
    settings.meetings.setPolicy(.autoRecord, for: .zoom)
    try store.save(settings)

    var loaded = store.load()
    loaded.theme = .dark
    try store.save(loaded)

    let reloaded = store.load()
    XCTAssertEqual(reloaded.theme, .dark)
    XCTAssertEqual(reloaded.transcriptionLanguage.code, "fr-FR")
    XCTAssertEqual(reloaded.meetings.policy(for: .zoom), .autoRecord)
  }

  func testTheWriteLeavesNoTemporaryFileBehind() throws {
    let store = SettingsStore(directory: directory)
    try store.save(Settings(theme: .light))
    let entries = try FileManager.default.contentsOfDirectory(
      atPath: directory.path
    )
    XCTAssertEqual(entries, ["settings.json"])
  }
}

final class WispPathsTests: XCTestCase {
  func testOverrideWinsWhenSet() throws {
    let root = try WispPaths.dataDirectory(environment: [
      "WISP_DATA_DIR": "/tmp/wisp-override",
      "HOME": "/Users/example",
    ])
    XCTAssertEqual(root.path, "/tmp/wisp-override")
  }

  func testUnsetOverrideFallsBackToApplicationSupport() throws {
    let root = try WispPaths.dataDirectory(environment: ["HOME": "/Users/example"])
    XCTAssertEqual(
      root.path,
      "/Users/example/Library/Application Support/dev.mokmok.wisp"
    )
  }

  func testMissingHomeIsAnError() {
    XCTAssertThrowsError(try WispPaths.dataDirectory(environment: [:])) { error in
      XCTAssertEqual(error as? WispPathsError, .homeUnavailable)
    }
  }

  func testRecordingsAndDatabaseLiveUnderTheDataRoot() {
    let root = URL(fileURLWithPath: "/tmp/wisp")
    XCTAssertEqual(
      WispPaths.recordingsDirectory(in: root).path,
      "/tmp/wisp/recordings"
    )
    XCTAssertEqual(WispPaths.databaseFile(in: root).path, "/tmp/wisp/sessions.db")
  }
}
