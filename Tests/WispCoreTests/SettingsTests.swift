import Foundation
import XCTest

@testable import WispCore

final class SettingsTests: XCTestCase {
  func testDefaultsFollowTheSystemAppearanceAndRecordJapanese() {
    let settings = Settings()
    XCTAssertEqual(settings.theme, .system)
    XCTAssertEqual(settings.transcriptionLanguage.code, "ja-JP")
    XCTAssertFalse(settings.meetings.enabled)
  }

  func testSettingsRoundTripAsLowercaseJSON() throws {
    let settings = Settings(
      theme: .dark,
      transcriptionLanguage: LanguageCode("en-US"),
      meetings: MeetingDetectionSettings(enabled: true)
    )
    let data = try JSONEncoder().encode(settings)
    let raw = try XCTUnwrap(String(data: data, encoding: .utf8))
    XCTAssertTrue(raw.contains("\"dark\""), raw)
    XCTAssertTrue(raw.contains("\"en-US\""), raw)
    XCTAssertTrue(raw.contains("\"transcription_language\""), raw)

    let loaded = try JSONDecoder().decode(Settings.self, from: data)
    XCTAssertEqual(loaded.theme, .dark)
    XCTAssertEqual(loaded.transcriptionLanguage.code, "en-US")
    XCTAssertTrue(loaded.meetings.enabled)
  }

  func testAnUnknownThemeKeepsTheOtherPreferences() throws {
    let raw = Data(
      #"{"theme":"sepia","transcription_language":"en-US","meetings":{"enabled":true}}"#.utf8
    )
    let settings = try JSONDecoder().decode(Settings.self, from: raw)
    XCTAssertEqual(settings.theme, .system)
    XCTAssertEqual(settings.transcriptionLanguage.code, "en-US")
    XCTAssertTrue(settings.meetings.enabled)
  }

  func testAnUnknownLanguageIsPreservedVerbatim() throws {
    let raw = Data(#"{"theme":"light","transcription_language":"xx-YY"}"#.utf8)
    let settings = try JSONDecoder().decode(Settings.self, from: raw)
    XCTAssertEqual(settings.transcriptionLanguage.code, "xx-YY")
    XCTAssertFalse(settings.transcriptionLanguage.isKnown)
    XCTAssertEqual(settings.transcriptionLanguage.label, "xx-YY")
    XCTAssertEqual(settings.theme, .light)
  }

  func testAnUnknownToolKeepsTheOtherPreferences() throws {
    let raw = Data(
      """
      {
          "theme": "dark",
          "transcription_language": "en-US",
          "meetings": {
              "enabled": true,
              "policies": {"unknown-tool": "off", "zoom": "auto-record"}
          }
      }
      """.utf8
    )
    let settings = try JSONDecoder().decode(Settings.self, from: raw)
    XCTAssertEqual(settings.theme, .dark)
    XCTAssertEqual(settings.transcriptionLanguage.code, "en-US")
    XCTAssertTrue(settings.meetings.enabled)
    XCTAssertEqual(settings.meetings.policy(for: .zoom), .autoRecord)
    XCTAssertEqual(settings.meetings.policy(for: .discord), .ask)
  }

  func testOneMalformedFieldKeepsTheOthers() throws {
    let raw = Data(
      #"{"theme":"light","transcription_language":"en-US","meetings":{"policies":[]}}"#.utf8
    )
    let loaded = try JSONDecoder().decode(Settings.self, from: raw)
    XCTAssertEqual(loaded.theme, .light)
    XCTAssertEqual(loaded.transcriptionLanguage.code, "en-US")
    XCTAssertEqual(loaded.meetings, MeetingDetectionSettings())
  }

  func testKnownLanguagesHaveUniqueCodesAndLabels() {
    let codes = Set(LanguageCode.known.map(\.code))
    XCTAssertEqual(codes.count, LanguageCode.known.count)
    let labels = Set(LanguageCode.known.map(\.label))
    XCTAssertEqual(labels.count, LanguageCode.known.count)
    XCTAssertTrue(codes.contains(LanguageCode.fallback.code))
  }

  func testAStoredChordRoundTripsThroughJSON() throws {
    let settings = Settings(
      transcriptionLanguage: LanguageCode("en-US"),
      dictationShortcut: DictationChord(keyCode: 49, modifiers: [.command])
    )
    let data = try JSONEncoder().encode(settings)
    let loaded = try JSONDecoder().decode(Settings.self, from: data)
    XCTAssertEqual(
      loaded.dictationShortcut,
      DictationChord(keyCode: 49, modifiers: [.command])
    )
  }

  func testAnAbsentChordKeyDecodesAsTheDefault() throws {
    let raw = Data(#"{"theme":"dark","transcription_language":"ja-JP"}"#.utf8)
    let loaded = try JSONDecoder().decode(Settings.self, from: raw)
    XCTAssertEqual(loaded.dictationShortcut, .default)
    XCTAssertEqual(loaded.theme, .dark)
  }

  func testAnExplicitNullChordDecodesAsNilAndKeepsTheOtherFields() throws {
    let raw = Data(
      #"{"theme":"dark","transcription_language":"en-US","dictation_shortcut":null}"#.utf8
    )
    let loaded = try JSONDecoder().decode(Settings.self, from: raw)
    XCTAssertNil(loaded.dictationShortcut)
    XCTAssertEqual(loaded.theme, .dark)
    XCTAssertEqual(loaded.transcriptionLanguage.code, "en-US")
  }

  func testAMalformedChordDecodesAsNilAndKeepsTheOtherFields() throws {
    let raw = Data(
      #"{"theme":"light","transcription_language":"en-US","dictation_shortcut":"⌘Space"}"#.utf8
    )
    let loaded = try JSONDecoder().decode(Settings.self, from: raw)
    XCTAssertNil(loaded.dictationShortcut)
    XCTAssertEqual(loaded.theme, .light)
    XCTAssertEqual(loaded.transcriptionLanguage.code, "en-US")
    XCTAssertEqual(loaded.meetings, MeetingDetectionSettings())
  }
}
