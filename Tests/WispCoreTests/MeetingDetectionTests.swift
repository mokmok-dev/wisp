import Foundation
import XCTest

@testable import WispCore

final class MeetingDetectionTests: XCTestCase {
  private let base = ContinuousClock.now

  private func at(_ millis: UInt64) -> ContinuousClock.Instant {
    base + .milliseconds(Int64(millis))
  }

  private func ask(_: MeetingTool) -> MeetingPolicy {
    .ask
  }

  private func detected(_ observation: MeetingObservation?) -> MeetingDetection? {
    guard case .detected(let detection) = observation else { return nil }
    return detection
  }

  func testNativeToolIsDetectedAfterTwoSeconds() {
    var detector = MeetingDetector()
    XCTAssertNil(detector.observe(now: at(0), active: [.zoom], policy: ask))
    XCTAssertNil(detector.observe(now: at(1000), active: [.zoom], policy: ask))
    XCTAssertNil(detector.observe(now: at(1999), active: [.zoom], policy: ask))
    XCTAssertEqual(
      detected(detector.observe(now: at(2000), active: [.zoom], policy: ask)),
      MeetingDetection(tool: .zoom, autoRecord: false)
    )
  }

  func testBrowserMeetingWaitsOutTheVoiceNoteWindow() {
    var detector = MeetingDetector()
    for millis: UInt64 in [0, 10000, 19000, 19999] {
      XCTAssertNil(
        detector.observe(now: at(millis), active: [.browserMeeting], policy: ask)
      )
    }
    XCTAssertNotNil(
      detector.observe(now: at(20000), active: [.browserMeeting], policy: ask)
    )
  }

  func testAutoRecordComesFromTheToolPolicy() {
    var detector = MeetingDetector()
    let policy: (MeetingTool) -> MeetingPolicy = { $0 == .zoom ? .autoRecord : .ask }
    XCTAssertNil(detector.observe(now: at(0), active: [.zoom], policy: policy))
    XCTAssertEqual(
      detected(detector.observe(now: at(2000), active: [.zoom], policy: policy)),
      MeetingDetection(tool: .zoom, autoRecord: true)
    )
  }

  func testOffToolsAreNeverDetected() {
    var detector = MeetingDetector()
    let off: (MeetingTool) -> MeetingPolicy = { _ in .off }
    for millis: UInt64 in [0, 1000, 2000, 30000] {
      XCTAssertNil(detector.observe(now: at(millis), active: [.zoom], policy: off))
    }
    XCTAssertNil(detector.handledTool)
  }

  func testADifferentToolRestartsTheRun() {
    var detector = MeetingDetector()
    XCTAssertNil(detector.observe(now: at(0), active: [.zoom], policy: ask))
    XCTAssertNil(detector.observe(now: at(1500), active: [.microsoftTeams], policy: ask))
    XCTAssertNil(detector.observe(now: at(3499), active: [.microsoftTeams], policy: ask))
    XCTAssertEqual(
      detected(
        detector.observe(now: at(3500), active: [.microsoftTeams], policy: ask)
      )?.tool,
      .microsoftTeams
    )
  }

  func testAShortDropoutKeepsTimeTowardTheOffer() {
    var detector = MeetingDetector()
    XCTAssertNil(detector.observe(now: at(0), active: [.zoom], policy: ask))
    XCTAssertNil(detector.observe(now: at(1000), active: [], policy: ask))
    XCTAssertNotNil(detector.observe(now: at(2000), active: [.zoom], policy: ask))
  }

  func testALongDropoutClearsThePendingRun() {
    var detector = MeetingDetector()
    XCTAssertNil(detector.observe(now: at(0), active: [.zoom], policy: ask))
    XCTAssertNil(detector.observe(now: at(1000), active: [.zoom], policy: ask))
    let absent: UInt64 = 1000 + 2000 + 1
    XCTAssertNil(detector.observe(now: at(absent), active: [], policy: ask))
    let restarted = absent + 1
    XCTAssertNil(detector.observe(now: at(restarted), active: [.zoom], policy: ask))
    XCTAssertNil(
      detector.observe(now: at(restarted + 1999), active: [.zoom], policy: ask)
    )
    XCTAssertNotNil(
      detector.observe(now: at(restarted + 2000), active: [.zoom], policy: ask)
    )
  }

  func testAnotherToolIsDetectedAfterTheFirstCallLeaves() {
    var detector = MeetingDetector()
    XCTAssertNil(detector.observe(now: at(0), active: [.zoom], policy: ask))
    XCTAssertEqual(
      detected(detector.observe(now: at(2000), active: [.zoom], policy: ask))?.tool,
      .zoom
    )
    // Zoom still holds the microphone, so the Slack huddle waits.
    XCTAssertNil(detector.observe(now: at(3000), active: [.zoom, .slack], policy: ask))
    XCTAssertNil(detector.observe(now: at(4000), active: [.slack], policy: ask))
    XCTAssertNil(detector.observe(now: at(5000), active: [.slack], policy: ask))
    XCTAssertEqual(
      detected(detector.observe(now: at(6000), active: [.slack], policy: ask))?.tool,
      .slack
    )
  }

  func testOneCallIsOfferedOnceUntilItLeavesTheMicrophone() {
    var detector = MeetingDetector()
    XCTAssertNil(detector.observe(now: at(0), active: [.zoom], policy: ask))
    XCTAssertNotNil(detector.observe(now: at(2000), active: [.zoom], policy: ask))
    XCTAssertNil(detector.observe(now: at(3000), active: [.zoom], policy: ask))
    XCTAssertNil(detector.observe(now: at(4000), active: [.zoom], policy: ask))
  }

  func testTheSnoozeEndsAfterAMinuteAwayAndAnnouncesItOnce() {
    var detector = MeetingDetector()
    XCTAssertNil(detector.observe(now: at(0), active: [.zoom], policy: ask))
    XCTAssertNotNil(detector.observe(now: at(2000), active: [.zoom], policy: ask))
    let reset: UInt64 = 2000 + 60000
    XCTAssertNil(detector.observe(now: at(reset - 1), active: [], policy: ask))
    XCTAssertEqual(
      detector.observe(now: at(reset), active: [], policy: ask),
      .ended(tool: .zoom)
    )
    XCTAssertNil(detector.observe(now: at(reset + 1), active: [], policy: ask))
  }

  func testAnUnreadablePollDoesNotRestartAPendingRun() {
    var detector = MeetingDetector()
    XCTAssertNil(detector.observe(now: at(0), active: [.zoom], policy: ask))
    detector.observeUnavailable(now: at(1000))
    XCTAssertNotNil(detector.observe(now: at(2000), active: [.zoom], policy: ask))
  }

  func testALongUnreadableStretchRestartsTheRun() {
    var detector = MeetingDetector()
    XCTAssertNil(detector.observe(now: at(0), active: [.zoom], policy: ask))
    detector.observeUnavailable(now: at(20000))
    XCTAssertNil(detector.observe(now: at(21999), active: [.zoom], policy: ask))
    XCTAssertNotNil(detector.observe(now: at(22000), active: [.zoom], policy: ask))
  }

  func testAnUnreadablePollDoesNotAgeASnooze() {
    var detector = MeetingDetector()
    XCTAssertNil(detector.observe(now: at(0), active: [.zoom], policy: ask))
    XCTAssertNotNil(detector.observe(now: at(2000), active: [.zoom], policy: ask))
    let reset: UInt64 = 2000 + 60000
    detector.observeUnavailable(now: at(reset - 1))
    XCTAssertNil(detector.observe(now: at(reset), active: [], policy: ask))
    XCTAssertNil(detector.observe(now: at(reset - 1 + 59999), active: [], policy: ask))
    XCTAssertEqual(
      detector.observe(now: at(reset - 1 + 60000), active: [], policy: ask),
      .ended(tool: .zoom)
    )
  }

  func testMarkHandledSnoozesACallStartedByHand() {
    var detector = MeetingDetector()
    detector.markHandled(now: at(0), tool: .microsoftTeams)
    XCTAssertEqual(detector.handledTool, .microsoftTeams)
    XCTAssertNil(
      detector.observe(now: at(5000), active: [.microsoftTeams], policy: ask)
    )
  }

  func testResetForgetsRunsAndSnoozes() {
    var detector = MeetingDetector()
    detector.markHandled(now: at(0), tool: .zoom)
    detector.reset()
    XCTAssertNil(detector.handledTool)
    XCTAssertNil(detector.observe(now: at(1000), active: [.zoom], policy: ask))
    XCTAssertNotNil(detector.observe(now: at(3000), active: [.zoom], policy: ask))
  }

  func testALongObservationGapRestartsTheRun() {
    var detector = MeetingDetector()
    XCTAssertNil(detector.observe(now: at(0), active: [.zoom], policy: ask))
    XCTAssertNil(detector.observe(now: at(20000), active: [.zoom], policy: ask))
    XCTAssertNil(detector.observe(now: at(21999), active: [.zoom], policy: ask))
    XCTAssertNotNil(detector.observe(now: at(22000), active: [.zoom], policy: ask))
  }

  func testANativeToolWinsOverABrowserHoldingTheMicrophone() {
    var detector = MeetingDetector()
    let active: [MeetingTool] = [.browserMeeting, .zoom]
    XCTAssertNil(detector.observe(now: at(0), active: active, policy: ask))
    XCTAssertEqual(
      detected(detector.observe(now: at(2000), active: active, policy: ask))?.tool,
      .zoom
    )
  }
}
