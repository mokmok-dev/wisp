import Foundation
import WispCore
import WispLifecycle
import XCTest

@testable import WispAudioKit
@testable import WispDesktop

/// The dictation reducer and its helpers, exercised row by row so the
/// transition table cannot drift.
@MainActor
final class DictationTransitionTests: XCTestCase {
  private func assertTransition(
    from state: DictationState,
    on event: DictationEvent,
    to expectedState: DictationState,
    expecting effect: DictationEffect,
    file: StaticString = #filePath,
    line: UInt = #line
  ) {
    let transition = dictationTransition(from: state, on: event)
    guard let transition else {
      XCTFail("expected a transition from \(state) on \(event)", file: file, line: line)
      return
    }
    XCTAssertEqual(transition.state, expectedState, file: file, line: line)
    XCTAssertEqual(transition.effect, effect, file: file, line: line)
  }

  private func assertIgnored(
    from state: DictationState,
    on event: DictationEvent,
    file: StaticString = #filePath,
    line: UInt = #line
  ) {
    XCTAssertNil(dictationTransition(from: state, on: event), file: file, line: line)
  }

  // MARK: - Table rows

  func testIdlePlusHotKeyPressedBeginsCapture() {
    assertTransition(
      from: .idle,
      on: .hotKeyPressed,
      to: .starting,
      expecting: .beginCapture
    )
  }

  func testStartingPlusCaptureStartedListens() {
    assertTransition(
      from: .starting,
      on: .captureStarted,
      to: .listening,
      expecting: .none
    )
  }

  func testStartingPlusHotKeyPressedDiscards() {
    assertTransition(
      from: .starting,
      on: .hotKeyPressed,
      to: .finishing(.discard),
      expecting: .stopCapture(.discard)
    )
  }

  func testStartingPlusCaptureFailedFails() {
    assertTransition(
      from: .starting,
      on: .captureFailed(.microphoneDenied),
      to: .idle,
      expecting: .fail(.microphoneDenied)
    )
  }

  func testListeningPlusHotKeyPressedDelivers() {
    assertTransition(
      from: .listening,
      on: .hotKeyPressed,
      to: .finishing(.deliver),
      expecting: .stopCapture(.deliver)
    )
  }

  func testListeningPlusStopRequestedFollowsTheOutcome() {
    assertTransition(
      from: .listening,
      on: .stopRequested(.deliver),
      to: .finishing(.deliver),
      expecting: .stopCapture(.deliver)
    )
    assertTransition(
      from: .listening,
      on: .stopRequested(.discard),
      to: .finishing(.discard),
      expecting: .stopCapture(.discard)
    )
  }

  func testListeningPlusCaptureFailedFails() {
    assertTransition(
      from: .listening,
      on: .captureFailed(.analyzer("boom")),
      to: .idle,
      expecting: .fail(.analyzer("boom"))
    )
  }

  func testFinishingPlusCaptureFinishedSettles() {
    assertTransition(
      from: .finishing(.deliver),
      on: .captureFinished,
      to: .idle,
      expecting: .settle(.deliver)
    )
    assertTransition(
      from: .finishing(.discard),
      on: .captureFinished,
      to: .idle,
      expecting: .settle(.discard)
    )
  }

  func testFinishingKeepsItsOutcomeOnLateEvents() {
    assertTransition(
      from: .finishing(.deliver),
      on: .captureFailed(.engine("late")),
      to: .finishing(.deliver),
      expecting: .none
    )
    assertTransition(
      from: .finishing(.deliver),
      on: .hotKeyPressed,
      to: .finishing(.deliver),
      expecting: .none
    )
    assertTransition(
      from: .finishing(.discard),
      on: .stopRequested(.deliver),
      to: .finishing(.discard),
      expecting: .none
    )
  }

  func testTerminateDropsTheRunFromAnyState() {
    for state in [DictationState.idle, .starting, .listening, .finishing(.deliver)] {
      assertTransition(from: state, on: .terminate, to: .idle, expecting: .stopCapture(.discard))
    }
  }

  // MARK: - Ignored events, every row

  func testEveryEventIsEitherARowOrIgnored() {
    let states: [DictationState] = [
      .idle,
      .starting,
      .listening,
      .finishing(.deliver),
      .finishing(.discard),
    ]
    let events: [DictationEvent] = [
      .hotKeyPressed,
      .captureStarted,
      .captureFailed(.microphoneDenied),
      .captureFinished,
      .stopRequested(.deliver),
      .stopRequested(.discard),
      .terminate,
    ]
    // The rows asserted above, keyed so this sweep can assert the rest drop.
    let covering: (DictationState, DictationEvent) -> Bool = { state, event in
      switch (state, event) {
      case (.idle, .hotKeyPressed),
        (.starting, .captureStarted),
        (.starting, .hotKeyPressed),
        (.starting, .captureFailed),
        (.listening, .hotKeyPressed),
        (.listening, .stopRequested),
        (.listening, .captureFailed),
        (.finishing(.deliver), .captureFinished),
        (.finishing(.deliver), .captureFailed),
        (.finishing(.deliver), .hotKeyPressed),
        (.finishing(.deliver), .stopRequested),
        (.finishing(.discard), .captureFinished),
        (.finishing(.discard), .captureFailed),
        (.finishing(.discard), .hotKeyPressed),
        (.finishing(.discard), .stopRequested):
        return true
      case (_, .terminate):
        return true
      default:
        return false
      }
    }
    for state in states {
      for event in events {
        if covering(state, event) {
          XCTAssertNotNil(dictationTransition(from: state, on: event), "\(state) + \(event)")
        } else {
          XCTAssertNil(dictationTransition(from: state, on: event), "\(state) + \(event)")
        }
      }
    }
  }

  // MARK: - holdsMicrophone

  func testHoldsMicrophoneTracksTheMachine() {
    XCTAssertFalse(DictationState.idle.holdsMicrophone)
    XCTAssertTrue(DictationState.starting.holdsMicrophone)
    XCTAssertTrue(DictationState.listening.holdsMicrophone)
    XCTAssertTrue(DictationState.finishing(.deliver).holdsMicrophone)
    XCTAssertTrue(DictationState.finishing(.discard).holdsMicrophone)
  }

  // MARK: - settle is idempotent

  func testSettleClearsAndIsIdempotent() {
    let model = DictationModel()
    model.apply(.hotKeyPressed)
    model.apply(.captureStarted)
    model.ingest(update(text: "hello"))
    model.showNotice(.copied)
    model.settle()
    model.settle()

    XCTAssertEqual(model.state, .idle)
    XCTAssertTrue(model.segments.isEmpty)
    XCTAssertNil(model.notice)
  }

  // MARK: - ingest guards

  func testIngestDropsUpdatesOnceTheMicrophoneIsReleased() {
    let model = DictationModel()
    model.ingest(update(text: "before"))
    XCTAssertTrue(model.segments.isEmpty, "idle does not hold the microphone")

    model.apply(.hotKeyPressed)
    model.ingest(update(text: "while listening"))
    XCTAssertEqual(model.segments.map(\.text), ["while listening"])

    model.apply(.hotKeyPressed)
    model.ingest(update(id: 8, text: "late final"))
    XCTAssertEqual(
      model.segments.map(\.text),
      ["while listening", "late final"],
      "finishing still holds the microphone while the engine drains"
    )

    model.settle()
    model.ingest(update(text: "after settle"))
    XCTAssertTrue(model.segments.isEmpty, "idle no longer ingests")
  }

  func testIngestUpsertsThroughTheSharedMergeRule() {
    let model = DictationModel()
    model.apply(.hotKeyPressed)
    model.apply(.captureStarted)
    model.ingest(update(id: 1, text: "こん"))
    model.ingest(update(id: 1, text: "こんにちは"))
    model.ingest(update(id: 2, text: "次の文"))
    XCTAssertEqual(model.segments.count, 2)
    XCTAssertEqual(model.segments[0].displayText, "こんにちは")
    XCTAssertTrue(model.segments[0].isFinal, "a new segment id finalises the previous row")
  }

  private func update(
    id: UInt64 = 7,
    text: String,
    isFinal: Bool = false
  ) -> DictationCapture.Update {
    DictationCapture.Update(
      segmentID: id,
      isFinal: isFinal,
      text: text,
      startSeconds: 0,
      endSeconds: 1
    )
  }
}

/// The pure helpers: `dictationText`, the HUD frame arithmetic, and the
/// microphone arbiter.
@MainActor
final class DictationHelpersTests: XCTestCase {
  // MARK: - dictationText

  func testJoinsJapaneseSegmentsAcrossTheSentenceBreak() {
    let text = dictationText([
      DesktopFixtures.segment(.mic, id: 1, text: "一文目。二文目。"),
      DesktopFixtures.segment(.mic, id: 2, text: "三文目"),
    ])
    XCTAssertEqual(text, "一文目。二文目。三文目")
  }

  func testDropsEmptySegments() {
    let text = dictationText([
      DesktopFixtures.segment(.mic, id: 1, text: ""),
      DesktopFixtures.segment(.mic, id: 2, text: "   "),
      DesktopFixtures.segment(.mic, id: 3, text: "only this"),
    ])
    XCTAssertEqual(text, "only this")
  }

  func testNoSegmentMarksAndJoinsProse() {
    let empty = dictationText([DesktopFixtures.segment(.mic, id: 1, text: "")])
    XCTAssertEqual(empty, "")
    let text = dictationText([
      DesktopFixtures.segment(.mic, id: 1, text: "first"),
      DesktopFixtures.segment(.mic, id: 2, text: "second"),
    ])
    XCTAssertEqual(text, "first second")
  }

  /// A segment boundary is an analyzer pause, not a sentence a writer typed.
  func testJoinsLatinSegmentsWithOneSpace() {
    let text = dictationText([
      DesktopFixtures.segment(.mic, id: 1, text: "hello there."),
      DesktopFixtures.segment(.mic, id: 2, text: "How are you?"),
    ])
    XCTAssertEqual(text, "hello there. How are you?")
  }

  func testJoinsJapaneseSegmentsWithoutASpace() {
    let text = dictationText([
      DesktopFixtures.segment(.mic, id: 1, text: "セッションIDを"),
      DesktopFixtures.segment(.mic, id: 2, text: "変えたら"),
    ])
    XCTAssertEqual(text, "セッションIDを変えたら")
  }

  func testJoinsAMixedBoundaryWithoutDoublingTheSpace() {
    let text = dictationText([
      DesktopFixtures.segment(.mic, id: 1, text: "これは Wisp"),
      DesktopFixtures.segment(.mic, id: 2, text: "です"),
    ])
    XCTAssertEqual(text, "これは Wispです")
  }

  /// The clipboard carries what was spoken. `displayText` adds a break after a
  /// sentence-ending `。` for the transcript layout, which would paste as a
  /// hard line break.
  func testCopiesRawTextWithoutLayoutBreaks() {
    let segment = DesktopFixtures.segment(.mic, id: 1, text: "一文目。二文目。")
    XCTAssertEqual(segment.displayText, "一文目。\n二文目。")
    XCTAssertEqual(dictationText([segment]), "一文目。二文目。")
  }

  // MARK: - DictationHUDFrame

  func testOriginCentresAndRespectsTheInset() {
    let visibleFrame = CGRect(x: 100, y: 0, width: 1200, height: 800)
    let size = CGSize(width: 520, height: 90)
    let origin = DictationHUDFrame.origin(size: size, in: visibleFrame)
    XCTAssertEqual(origin.x, 100 + (1200 - 520) / 2, accuracy: 0.001)
    XCTAssertEqual(origin.y, DictationHUDFrame.bottomInset, accuracy: 0.001)
  }

  func testOriginMatchesWhenTheWidthEqualsTheVisibleWidth() {
    let visibleFrame = CGRect(x: 0, y: 0, width: 520, height: 600)
    let origin = DictationHUDFrame.origin(
      size: CGSize(width: DictationHUDFrame.width, height: 40),
      in: visibleFrame
    )
    XCTAssertEqual(origin.x, 0, accuracy: 0.001)
  }

  func testTargetPicksTheContainingFrame() {
    let mainScreen = CGRect(x: 0, y: 0, width: 1440, height: 900)
    let sideScreen = CGRect(x: -1440, y: 0, width: 1440, height: 900)
    let picked = DictationHUDFrame.target(
      mouse: CGPoint(x: -100, y: 300),
      visibleFrames: [mainScreen, sideScreen]
    )
    XCTAssertEqual(picked, sideScreen)
  }

  func testTargetFallsBackToNilOutsideEveryFrame() {
    let mainScreen = CGRect(x: 0, y: 0, width: 1440, height: 900)
    XCTAssertNil(
      DictationHUDFrame.target(mouse: CGPoint(x: 5000, y: 5000), visibleFrames: [mainScreen])
    )
    XCTAssertNotNil(
      DictationHUDFrame.target(mouse: CGPoint(x: 10, y: 10), visibleFrames: [mainScreen])
    )
  }

  func testHUDWidthIs520() {
    XCTAssertEqual(DictationHUDFrame.width, 520)
  }

  // MARK: - Arbiter matrix

  /// Each claimant answers only for the other side, so a run can always stop
  /// itself while a start over the other side's claim is refused.
  func testArbiterAnswersOnlyForTheOtherSide() {
    for phase in Phase.allCases {
      for state in [DictationState.idle, .starting, .listening, .finishing(.deliver)] {
        XCTAssertEqual(
          canStartCapture(.recording, recording: phase, dictation: state),
          !state.holdsMicrophone,
          "recording claim, \(phase) × \(state)"
        )
        XCTAssertEqual(
          canStartCapture(.dictation, recording: phase, dictation: state),
          !phase.isActive,
          "dictation claim, \(phase) × \(state)"
        )
      }
    }
  }

  func testArbiterDeniesAStartOverTheOtherSide() {
    XCTAssertFalse(canStartCapture(.dictation, recording: .recording, dictation: .idle))
    XCTAssertFalse(canStartCapture(.dictation, recording: .starting, dictation: .idle))
    XCTAssertFalse(canStartCapture(.recording, recording: .idle, dictation: .listening))
    XCTAssertFalse(
      canStartCapture(.recording, recording: .idle, dictation: .finishing(.deliver))
    )
    XCTAssertTrue(canStartCapture(.dictation, recording: .idle, dictation: .idle))
    XCTAssertTrue(canStartCapture(.recording, recording: .failed, dictation: .idle))
  }

  /// The chord must be able to finish the run it started.
  func testArbiterAdmitsTheChordThatStopsItsOwnRun() {
    XCTAssertTrue(canStartCapture(.dictation, recording: .idle, dictation: .listening))
    XCTAssertTrue(canStartCapture(.dictation, recording: .idle, dictation: .starting))
  }
}
