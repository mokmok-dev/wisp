import Foundation
import WispCore
import XCTest

@testable import WispLifecycle

final class LifecycleTests: XCTestCase {
  private let session = SessionID(7)
  private let allPhases = Phase.allCases
  private let allViews = ViewOwner.allCases

  func testPhaseActivityMarksWorkerOwnership() {
    let expected: [(Phase, Bool)] = [
      (.idle, false),
      (.starting, true),
      (.recording, true),
      (.stopping, true),
      (.failed, false),
    ]
    for (phase, active) in expected {
      XCTAssertEqual(phase.isActive, active, "phase \(phase)")
    }
  }

  func testStoppedRequestsArePreservedByLateStartedUpdates() {
    let expected: [(Phase, Phase)] = [
      (.idle, .idle),
      (.starting, .recording),
      (.recording, .recording),
      (.stopping, .stopping),
      (.failed, .failed),
    ]
    for (phase, started) in expected {
      XCTAssertEqual(phase.workerStarted, started, "phase \(phase)")
    }
  }

  func testStopRequestsOnlyAffectActivePhases() {
    let expected: [(Phase, Phase)] = [
      (.idle, .idle),
      (.starting, .stopping),
      (.recording, .stopping),
      (.stopping, .stopping),
      (.failed, .failed),
    ]
    for (phase, stopped) in expected {
      let stoppedPhase = phase.requestingStop
      XCTAssertEqual(stoppedPhase, stopped, "phase \(phase)")
      XCTAssertEqual(stoppedPhase.requestingStop, stoppedPhase, "idempotent")
    }
  }

  func testUpdateContextRequiresLiveOwnershipToAccept() {
    let live = UpdateContext(phase: .recording, view: .live, currentSessionID: session)
    XCTAssertTrue(live.accepts(session))
    XCTAssertFalse(live.accepts(SessionID(session.rawValue + 1)))

    for phase in allPhases {
      for view in allViews {
        for current in [nil, session, SessionID(session.rawValue + 1)] {
          let context = UpdateContext(
            phase: phase,
            view: view,
            currentSessionID: current
          )
          let expected = phase.isActive && view == .live && current == session
          XCTAssertEqual(context.accepts(session), expected, "context \(context)")
        }
      }
    }
  }

  func testNextPhaseMapsEveryAcceptedWorkerUpdate() {
    let recording = UpdateContext(
      phase: .recording,
      view: .live,
      currentSessionID: session
    )
    XCTAssertEqual(recording.nextPhase(session, update: .started), .recording)
    XCTAssertEqual(recording.nextPhase(session, update: .event), .recording)
    for terminal in [WorkerUpdate.stopped, .startFailed, .error] {
      XCTAssertEqual(recording.nextPhase(session, update: terminal), .idle)
    }
    XCTAssertNil(recording.nextPhase(SessionID(session.rawValue + 1), update: .event))

    var starting = recording
    starting.phase = .starting
    XCTAssertEqual(starting.nextPhase(session, update: .started), .recording)
  }

  func testContextInvariantRequiresLiveOwnershipWhileActive() {
    let contexts: [(UpdateContext, Bool)] = [
      (UpdateContext(phase: .idle, view: .library, currentSessionID: nil), true),
      (UpdateContext(phase: .recording, view: .live, currentSessionID: session), true),
      (UpdateContext(phase: .recording, view: .library, currentSessionID: session), false),
      (UpdateContext(phase: .recording, view: .live, currentSessionID: nil), false),
    ]
    for (context, expected) in contexts {
      XCTAssertEqual(context.invariantHolds, expected, "context \(context)")
    }
  }

  func testReducerAppliesLiveUpdatesAndCountsThem() {
    var reducer = Reducer(sessionID: session)
    XCTAssertEqual(
      reducer.context,
      UpdateContext(phase: .starting, view: .live, currentSessionID: session)
    )
    XCTAssertEqual(reducer.appliedEvents, 0)
    XCTAssertEqual(reducer.terminalUpdates, 0)

    XCTAssertFalse(reducer.apply(SessionID(session.rawValue + 1), update: .event))
    XCTAssertEqual(reducer.appliedEvents, 0)

    XCTAssertTrue(reducer.apply(session, update: .started))
    XCTAssertEqual(reducer.context.phase, .recording)
    XCTAssertTrue(reducer.apply(session, update: .event))
    XCTAssertTrue(reducer.apply(session, update: .event))
    XCTAssertEqual(reducer.appliedEvents, 2)

    reducer.requestStop()
    XCTAssertEqual(reducer.context.phase, .stopping)
    XCTAssertTrue(reducer.apply(session, update: .started))
    XCTAssertEqual(reducer.context.phase, .stopping)

    XCTAssertTrue(reducer.apply(session, update: .stopped))
    XCTAssertEqual(
      reducer.context,
      UpdateContext(phase: .idle, view: .live, currentSessionID: nil)
    )
    XCTAssertEqual(reducer.terminalUpdates, 1)

    XCTAssertFalse(reducer.apply(session, update: .error))
    XCTAssertEqual(reducer.terminalUpdates, 1)
  }

  func testTerminalUpdatesReleaseTheLiveSession() {
    for terminal in [WorkerUpdate.stopped, .startFailed, .error] {
      var reducer = Reducer(sessionID: session)
      XCTAssertTrue(reducer.apply(session, update: terminal), "update \(terminal)")
      XCTAssertEqual(reducer.context.phase, .idle)
      XCTAssertNil(reducer.context.currentSessionID)
      XCTAssertEqual(reducer.terminalUpdates, 1)
      XCTAssertFalse(reducer.apply(session, update: terminal), "update \(terminal)")
    }
  }

  func testDelayedUpdatesCannotMutateAnotherSession() {
    var reducer = Reducer(sessionID: session)
    let before = reducer
    let delayed = SessionID(session.rawValue + 1)
    for update in WorkerUpdate.allCases {
      XCTAssertFalse(reducer.apply(delayed, update: update), "update \(update)")
      XCTAssertEqual(reducer, before)
    }
  }

  func testCanReplaceTranscriptRequiresEveryOwnerToSettle() {
    for phase in allPhases {
      for pendingPersistence in [false, true] {
        for retainedOutput in [false, true] {
          let expected = !phase.isActive && !pendingPersistence && !retainedOutput
          XCTAssertEqual(
            canReplaceTranscript(
              phase: phase,
              pendingPersistence: pendingPersistence,
              retainedOutput: retainedOutput
            ),
            expected,
            "phase \(phase), pending \(pendingPersistence), output \(retainedOutput)"
          )
        }
      }
    }
  }

  /// The Rust suite pinned the same relationships with proptest; the state
  /// space is small enough to enumerate it exhaustively instead.
  func testAcceptsAgreesWithTheTransitionGuardForEveryContext() {
    let ids: [SessionID?] = [nil, session, SessionID(session.rawValue + 1)]
    let updateIDs: [SessionID] = [session, SessionID(session.rawValue + 1)]
    for phase in allPhases {
      for view in allViews {
        for current in ids {
          for update in updateIDs {
            let context = UpdateContext(
              phase: phase,
              view: view,
              currentSessionID: current
            )
            let expected = phase.isActive && view == .live && current == update
            XCTAssertEqual(context.accepts(update), expected)
            XCTAssertEqual(
              context.nextPhase(update, update: .event) != nil,
              expected
            )
          }
        }
      }
    }
  }
}
