import Foundation
import WispCore
import XCTest

@testable import WispDesktop

final class TranscriptModelTests: XCTestCase {
  // MARK: - breakOnSentenceEnd

  func testBreaksBetweenSentencesButNotAtATrailingPeriod() {
    XCTAssertEqual(breakOnSentenceEnd("一文目。二文目。"), "一文目。\n二文目。")
  }

  func testPassesThroughTextWithoutAPeriod() {
    XCTAssertEqual(breakOnSentenceEnd("途中"), "途中")
  }

  func testPreservesTextWithOnlyATrailingPeriod() {
    XCTAssertEqual(breakOnSentenceEnd("こんにちは。"), "こんにちは。")
  }

  func testEmptyTextStaysEmpty() {
    XCTAssertEqual(breakOnSentenceEnd(""), "")
  }

  // MARK: - looksLikeContinuation

  func testIdenticalTextIsAContinuation() {
    XCTAssertTrue(looksLikeContinuation("おはようございます", "おはようございます"))
  }

  func testEmptyTextIsNeverAContinuation() {
    XCTAssertFalse(looksLikeContinuation("", "text"))
    XCTAssertFalse(looksLikeContinuation("text", ""))
  }

  func testSmallInTheMiddleRevisionIsAContinuation() {
    XCTAssertTrue(looksLikeContinuation("MaOSの標準機能を使って", "MacOSの標準機能を使って"))
  }

  func testDissimilarTextIsNotAContinuation() {
    XCTAssertFalse(looksLikeContinuation("おはようございます", "今日はいい天気ですね"))
  }

  func testShortStringsNeedSixMatchedCharacters() {
    // Five of five characters match but the floor is six, so these stay apart.
    XCTAssertFalse(looksLikeContinuation("abcde", "abcde!"))
    XCTAssertTrue(looksLikeContinuation("abcdef", "abcdef!"))
  }

  func testShortDissimilarStringsStayApart() {
    XCTAssertFalse(looksLikeContinuation("ab", "ba"))
  }

  // MARK: - Upsert rules

  func testPartialRevisionReplacesTextInPlace() {
    var segments: [LiveSegment] = []
    upsertSegment(DesktopFixtures.segment(.mic, id: 1, text: "こん"), into: &segments)
    upsertSegment(DesktopFixtures.segment(.mic, id: 1, text: "こんにちは"), into: &segments)

    XCTAssertEqual(segments.count, 1)
    XCTAssertEqual(segments[0].text, "こんにちは")
    XCTAssertFalse(segments[0].isFinal)
  }

  func testANewSegmentIDFinalisesThePreviousRow() {
    var segments: [LiveSegment] = []
    upsertSegment(DesktopFixtures.segment(.mic, id: 1, text: "前"), into: &segments)
    upsertSegment(DesktopFixtures.segment(.mic, id: 2, text: "次"), into: &segments)

    XCTAssertEqual(segments.count, 2)
    XCTAssertTrue(segments[0].isFinal)
    XCTAssertFalse(segments[1].isFinal)
  }

  func testMicAndSystemSegmentsAreIndependent() {
    var segments: [LiveSegment] = []
    upsertSegment(DesktopFixtures.segment(.mic, id: 1, text: "m1"), into: &segments)
    upsertSegment(DesktopFixtures.segment(.system, id: 1, text: "s1"), into: &segments)
    upsertSegment(DesktopFixtures.segment(.mic, id: 1, text: "m1-updated"), into: &segments)

    XCTAssertEqual(segments.map(\.text), ["m1-updated", "s1"])
    XCTAssertFalse(segments[0].isFinal)
    XCTAssertFalse(segments[1].isFinal)
  }

  func testAFinalResultLocksTheSegmentImmediately() {
    var segments: [LiveSegment] = []
    upsertSegment(
      DesktopFixtures.segment(.mic, id: 1, text: "final transcript", isFinal: true),
      into: &segments
    )

    XCTAssertEqual(segments.count, 1)
    XCTAssertTrue(segments[0].isFinal)
  }

  func testDuplicateTextUnderANewSegmentIDMerges() {
    var segments: [LiveSegment] = []
    upsertSegment(
      DesktopFixtures.segment(.mic, id: 1, text: "おはようございます。"),
      into: &segments
    )
    // The analyzer re-emits the same utterance under segment 2; it must not
    // push a duplicate row.
    upsertSegment(
      DesktopFixtures.segment(.mic, id: 2, text: "おはようございます。"),
      into: &segments
    )

    XCTAssertEqual(segments.count, 1)
    XCTAssertEqual(segments[0].id, 2)
    XCTAssertFalse(segments[0].isFinal)
  }

  func testATypoRevisionMerges() {
    var segments: [LiveSegment] = []
    upsertSegment(
      DesktopFixtures.segment(.mic, id: 1, text: "MaOSの標準機能を使って"),
      into: &segments
    )
    upsertSegment(
      DesktopFixtures.segment(.mic, id: 2, text: "MacOSの標準機能を使って"),
      into: &segments
    )

    XCTAssertEqual(segments.count, 1)
    XCTAssertEqual(segments[0].text, "MacOSの標準機能を使って")
  }

  func testDissimilarTextStartsANewSegment() {
    var segments: [LiveSegment] = []
    upsertSegment(
      DesktopFixtures.segment(.mic, id: 1, text: "おはようございます"),
      into: &segments
    )
    upsertSegment(
      DesktopFixtures.segment(.mic, id: 2, text: "今日はいい天気ですね"),
      into: &segments
    )

    XCTAssertEqual(segments.count, 2)
    XCTAssertTrue(segments[0].isFinal)
    XCTAssertFalse(segments[1].isFinal)
  }

  func testAFinalRowIsNotReplacedByAContinuation() {
    var segments: [LiveSegment] = []
    upsertSegment(
      DesktopFixtures.segment(.mic, id: 1, text: "おはようございます。", isFinal: true),
      into: &segments
    )
    upsertSegment(
      DesktopFixtures.segment(.mic, id: 2, text: "おはようございます。"),
      into: &segments
    )

    XCTAssertEqual(segments.count, 2)
    XCTAssertTrue(segments[0].isFinal)
  }

  func testDisplayTextTracksTheRevision() {
    var segments: [LiveSegment] = []
    upsertSegment(DesktopFixtures.segment(.mic, id: 1, text: "一文目。"), into: &segments)
    XCTAssertEqual(segments[0].displayText, "一文目。")

    upsertSegment(
      DesktopFixtures.segment(.mic, id: 1, text: "一文目。二文目。"),
      into: &segments
    )
    XCTAssertEqual(segments[0].displayText, "一文目。\n二文目。")
  }
}

@MainActor
final class AppModelTests: XCTestCase {
  private func started() -> AppModel {
    let model = AppModel()
    model.beginSessionStart()
    return model
  }

  func testRecordingRejectsTopLevelNavigation() {
    let model = started()
    model.setState(.recording)
    model.currentSessionID = SessionID(10)
    upsertSegment(DesktopFixtures.segment(.mic, id: 1, text: "live"), into: &model.segments)

    model.showLibrary()
    XCTAssertEqual(model.view, .live)

    model.showHistory(DesktopFixtures.storedSession(id: 20), segments: [])
    XCTAssertEqual(model.view, .live)

    model.showNewSession()
    XCTAssertEqual(model.state, .recording)
    XCTAssertEqual(model.currentSessionID, SessionID(10))
    XCTAssertEqual(model.segments.map(\.text), ["live"])
  }

  func testStoppingSessionKeepsTheFlushedTranscript() {
    let model = started()
    model.setState(.stopping)
    model.currentSessionID = SessionID(10)
    upsertSegment(
      DesktopFixtures.segment(.mic, id: 1, text: "flushed transcript"),
      into: &model.segments
    )

    model.showLibrary()

    XCTAssertEqual(model.view, .live)
    XCTAssertEqual(model.currentSessionID, SessionID(10))
    XCTAssertEqual(model.segments[0].text, "flushed transcript")
  }

  func testFailedPersistenceKeepsTheTranscriptProtectedForRetry() {
    let model = started()
    model.setState(.failed)
    model.currentSessionID = SessionID(10)
    model.pendingSessionWrite = .finalise(endedAt: Date())
    upsertSegment(
      DesktopFixtures.segment(.mic, id: 1, text: "not persisted yet", isFinal: true),
      into: &model.segments
    )

    model.showLibrary()
    model.showHistory(DesktopFixtures.storedSession(id: 20), segments: [])
    model.showNewSession()
    model.beginSessionStart()

    XCTAssertEqual(model.view, .live)
    XCTAssertEqual(model.state, .failed)
    XCTAssertEqual(model.currentSessionID, SessionID(10))
    XCTAssertEqual(model.segments[0].text, "not persisted yet")
  }

  func testStartingASessionNormalizesHistoryToAFreshLiveView() {
    let model = AppModel()
    model.showHistory(
      DesktopFixtures.storedSession(id: 20),
      segments: [DesktopFixtures.segment(.mic, id: 0, text: "old", isFinal: true)]
    )

    XCTAssertTrue(model.beginSessionStart())

    XCTAssertEqual(model.view, .live)
    XCTAssertEqual(model.state, .starting)
    XCTAssertTrue(model.segments.isEmpty)
    XCTAssertNil(model.viewedSession)
    XCTAssertNil(model.currentSessionID)
  }

  func testSettingsKeepsAcceptingTheLiveSession() {
    let model = started()
    model.setState(.recording)
    model.currentSessionID = SessionID(10)

    model.showSettings()

    XCTAssertEqual(model.view, .settings)
    XCTAssertNotNil(model.workerUpdatePhase(SessionID(10), update: .event))
    XCTAssertEqual(model.state, .recording)
  }

  func testClosingSettingsReturnsToTheScreenItOpenedFrom() {
    let model = AppModel()
    model.showHistory(
      DesktopFixtures.storedSession(id: 20),
      segments: [DesktopFixtures.segment(.mic, id: 0, text: "old", isFinal: true)]
    )

    model.showSettings()
    model.closeSettings()

    XCTAssertEqual(model.view, .history(SessionID(20)))
    XCTAssertEqual(model.segments.map(\.text), ["old"])
  }

  func testOpeningSettingsTwiceKeepsTheFirstOrigin() {
    let model = AppModel()
    model.showLibrary()
    model.showSettings()
    model.showSettings()

    model.closeSettings()

    XCTAssertEqual(model.view, .library)
  }

  func testSettingsNeverDiscardsASettlingSession() {
    let model = started()
    model.setState(.failed)
    model.currentSessionID = SessionID(10)
    model.pendingSessionWrite = .finalise(endedAt: Date())
    upsertSegment(
      DesktopFixtures.segment(.mic, id: 1, text: "not persisted yet", isFinal: true),
      into: &model.segments
    )

    model.showSettings()
    model.closeSettings()

    XCTAssertTrue(model.hasUnsettledSession)
    XCTAssertEqual(model.segments.count, 1)
    XCTAssertEqual(model.currentSessionID, SessionID(10))
  }

  func testNavigationResetsTheLiveTitle() {
    let model = AppModel()
    model.liveTitle = "Q3 planning"
    model.showLibrary()
    XCTAssertEqual(model.liveTitle, "")

    model.liveTitle = "Q3 planning"
    model.showNewSession()
    XCTAssertEqual(model.liveTitle, "")
  }

  func testSetupRequiresTheMicrophoneAndSpeechUnlessRecordOnlyIsAllowed() {
    let model = AppModel()
    model.permissions = Permissions(microphone: .granted, speech: .denied)
    XCTAssertTrue(model.setupComplete)

    model.permissions = Permissions(microphone: .granted, speech: .undetermined)
    XCTAssertFalse(
      model.setupComplete,
      "onboarding should still offer the Speech prompt before denial"
    )

    model.permissions = Permissions(microphone: .undetermined, speech: .granted)
    XCTAssertFalse(model.setupComplete)
  }

  func testPermissionRuleAllowsRecordOnlyFallback() {
    let permissions = Permissions(microphone: .granted, speech: .denied)
    XCTAssertTrue(permissions.satisfies(recordOnlyAllowed: true))
    XCTAssertFalse(permissions.satisfies(recordOnlyAllowed: false))

    let restricted = Permissions(microphone: .granted, speech: .restricted)
    XCTAssertTrue(restricted.satisfies(recordOnlyAllowed: true))
  }

  func testHistorySegmentsArriveFinalised() {
    let model = AppModel()
    model.showHistory(
      DesktopFixtures.storedSession(id: 20),
      segments: [DesktopFixtures.segment(.mic, id: 0, text: "old")]
    )

    XCTAssertTrue(model.segments.allSatisfy(\.isFinal))
  }

  func testUnsettledSessionBlocksNavigationAndStart() {
    let model = started()
    model.setState(.recording)
    model.currentOutputDir = URL(fileURLWithPath: "/tmp/wisp")

    XCTAssertTrue(model.hasUnsettledSession)
    XCTAssertFalse(model.beginSessionStart())
  }
}
