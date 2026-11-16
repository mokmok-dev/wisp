import Foundation
import XCTest

@testable import WispCore

final class MeetingCatalogTests: XCTestCase {
  func testEveryToolKeyRoundTrips() {
    for tool in MeetingTool.allCases {
      XCTAssertEqual(MeetingTool(rawValue: tool.key), tool)
    }
    XCTAssertNil(MeetingTool(rawValue: "google-meet"))
    XCTAssertNil(MeetingTool(rawValue: ""))
  }

  func testCatalogMatchesTheShippedNamesAndBundleIDs() {
    let expected: [(MeetingTool, String, String, [String])] = [
      (.zoom, "zoom", "Zoom", ["us.zoom.xos"]),
      (
        .microsoftTeams,
        "microsoft-teams",
        "Microsoft Teams",
        ["com.microsoft.teams2", "com.microsoft.teams"]
      ),
      (.slack, "slack", "Slack huddles", ["com.tinyspeck.slackmacgap"]),
      (.webex, "webex", "Webex", ["com.cisco.webexmeetingsapp"]),
      (.faceTime, "face-time", "FaceTime", ["com.apple.FaceTime"]),
      (.discord, "discord", "Discord", ["com.hnc.Discord"]),
      (
        .browserMeeting,
        "browser-meeting",
        "Browser meetings",
        [
          "com.google.Chrome",
          "com.apple.Safari",
          "com.apple.SafariTechnologyPreview",
          "com.microsoft.edgemac",
          "org.mozilla.firefox",
          "com.brave.Browser",
          "company.thebrowser.Browser",
          "com.vivaldi.Vivaldi",
        ]
      ),
    ]
    XCTAssertEqual(MeetingTool.allCases.count, expected.count)
    for (tool, key, label, bundleIDs) in expected {
      XCTAssertEqual(tool.key, key)
      XCTAssertEqual(tool.label, label)
      XCTAssertEqual(tool.bundleIDs, bundleIDs)
    }
  }

  func testEveryBundleIDNamesItsTool() {
    for tool in MeetingTool.allCases {
      for bundleID in tool.bundleIDs {
        XCTAssertEqual(MeetingTool.fromBundleID(bundleID), tool)
      }
    }
    XCTAssertNil(MeetingTool.fromBundleID("com.example.Nope"))
    XCTAssertEqual(MeetingTool.fromBundleID("us.zoom.xos.helper"), .zoom)
    XCTAssertEqual(
      MeetingTool.fromBundleID("com.google.Chrome.helper.GPU"),
      .browserMeeting
    )
    XCTAssertNil(MeetingTool.fromBundleID("us.zoom.xosish"))
  }

  func testToolsHaveDistinctKeysLabelsAndBundleIDs() {
    let keys = Set(MeetingTool.allCases.map(\.key))
    XCTAssertEqual(keys.count, MeetingTool.allCases.count)

    let labels = Set(MeetingTool.allCases.map(\.label))
    XCTAssertEqual(labels.count, MeetingTool.allCases.count)

    let bundleIDs = MeetingTool.allCases.flatMap(\.bundleIDs)
    XCTAssertEqual(Set(bundleIDs).count, bundleIDs.count)
  }

  func testOnlyBrowserMeetingsWaitOutTheVoiceNoteWindow() {
    for tool in MeetingTool.allCases {
      let expected: TimeInterval = tool == .browserMeeting ? 20 : 2
      XCTAssertEqual(tool.detectionDelay, expected)
    }
  }
}

final class MeetingPolicyTests: XCTestCase {
  func testPolicyKeysRoundTripAndUnknownPoliciesAsk() throws {
    let expected: [(MeetingPolicy, String, String)] = [
      (.off, "off", "Off"),
      (.ask, "ask", "Ask"),
      (.autoRecord, "auto-record", "Auto-record"),
    ]
    for (policy, key, label) in expected {
      XCTAssertEqual(policy.key, key)
      XCTAssertEqual(policy.label, label)
      XCTAssertEqual(MeetingPolicy(rawValue: key), policy)
      let encoded = Data("\"\(policy.key)\"".utf8)
      XCTAssertEqual(try JSONDecoder().decode(MeetingPolicy.self, from: encoded), policy)
    }
    XCTAssertNil(MeetingPolicy(rawValue: "record"))
    XCTAssertEqual(
      try JSONDecoder().decode(MeetingPolicy.self, from: Data("\"record\"".utf8)),
      .ask
    )
  }

  func testPoliciesHaveDistinctLabels() {
    let labels = Set(MeetingPolicy.allCases.map(\.label))
    XCTAssertEqual(labels.count, MeetingPolicy.allCases.count)
  }

  func testPoliciesDropUnknownToolsAndDefaults() throws {
    let raw = Data(#"{"zoom":"auto-record","unknown-tool":"off","slack":"ask"}"#.utf8)
    let policies = try JSONDecoder().decode(MeetingPolicies.self, from: raw)
    XCTAssertEqual(policies.count, 1)
    XCTAssertEqual(policies.policy(for: .zoom), .autoRecord)
    XCTAssertEqual(policies.policy(for: .slack), .ask)
    XCTAssertEqual(policies.policy(for: .discord), .ask)

    let empty = try JSONDecoder().decode(MeetingPolicies.self, from: Data("null".utf8))
    XCTAssertTrue(empty.isEmpty)
  }

  func testSettingTheDefaultPolicyRemovesTheEntry() {
    var policies = MeetingPolicies()
    XCTAssertTrue(policies.isEmpty)
    policies.set(.zoom, to: .off)
    XCTAssertEqual(policies.count, 1)
    XCTAssertFalse(policies.isEmpty)
    policies.set(.discord, to: .autoRecord)
    XCTAssertEqual(policies.count, 2)
    policies.set(.zoom, to: .ask)
    XCTAssertEqual(policies.count, 1)
    policies.set(.discord, to: .ask)
    XCTAssertTrue(policies.isEmpty)
  }

  func testSettingsRoundTripAsJSONWithToolKeys() throws {
    var settings = MeetingDetectionSettings(enabled: true)
    XCTAssertTrue(settings.setPolicy(.autoRecord, for: .microsoftTeams))
    XCTAssertFalse(settings.setPolicy(.autoRecord, for: .microsoftTeams))

    let data = try JSONEncoder().encode(settings)
    let json = try XCTUnwrap(String(data: data, encoding: .utf8))
    XCTAssertTrue(json.contains("\"microsoft-teams\":\"auto-record\""), json)

    let parsed = try JSONDecoder().decode(MeetingDetectionSettings.self, from: data)
    XCTAssertEqual(parsed, settings)
  }

  func testDefaultSettingsDetectNothingAndAskForEveryTool() {
    let settings = MeetingDetectionSettings()
    XCTAssertFalse(settings.enabled)
    for tool in MeetingTool.allCases {
      XCTAssertEqual(settings.policy(for: tool), .ask)
    }
  }
}
