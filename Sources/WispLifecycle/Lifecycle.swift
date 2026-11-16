import Foundation
import WispCore

/// Executable session-lifecycle policy.
///
/// Keep state-independent data (timestamps, paths, transcript contents) in the
/// application. This module owns the decisions that protect a live transcript
/// from navigation and delayed worker updates.
public enum Phase: Hashable, Sendable, CaseIterable {
  case idle
  case starting
  case recording
  case stopping
  case failed

  /// Whether a worker or persistence owner may still hold this session.
  public var isActive: Bool {
    switch self {
    case .starting, .recording, .stopping: true
    case .idle, .failed: false
    }
  }

  /// Apply a `started` update without undoing an already-requested stop.
  public var workerStarted: Phase {
    if self == .stopping {
      return .stopping
    }
    return isActive ? .recording : self
  }

  /// Request a stop only from a phase in which a worker can be starting or
  /// running. Repeated stop requests are idempotent.
  public var requestingStop: Phase {
    isActive ? .stopping : self
  }
}

/// The screen that currently owns the transcript.
public enum ViewOwner: Hashable, Sendable, CaseIterable {
  case library
  case live
  case history
}

/// An asynchronous update from the session worker.
public enum WorkerUpdate: Hashable, Sendable, CaseIterable {
  case started
  case event
  case stopped
  case startFailed
  case error
}

/// The lifecycle fields needed to decide whether an asynchronous worker
/// update belongs to the currently visible live transcript.
public struct UpdateContext: Hashable, Sendable {
  public var phase: Phase
  public var view: ViewOwner
  public var currentSessionID: SessionID?

  public init(phase: Phase, view: ViewOwner, currentSessionID: SessionID?) {
    self.phase = phase
    self.view = view
    self.currentSessionID = currentSessionID
  }

  /// Whether an update tagged with `updateSessionID` may mutate the model.
  /// Only the live view that owns an active session accepts updates.
  public func accepts(_ updateSessionID: SessionID) -> Bool {
    phase.isActive && view == .live && currentSessionID == updateSessionID
  }

  /// The phase the context moves to for `update`, or `nil` when the update
  /// is rejected.
  public func nextPhase(
    _ updateSessionID: SessionID,
    update: WorkerUpdate
  ) -> Phase? {
    guard accepts(updateSessionID) else { return nil }
    switch update {
    case .started: return phase.workerStarted
    case .event: return phase
    case .stopped, .startFailed, .error: return .idle
    }
  }

  /// An active phase always has a live owner and a session to own.
  public var invariantHolds: Bool {
    !phase.isActive || (view == .live && currentSessionID != nil)
  }
}

/// Stateful driver around the production `UpdateContext` transition.
public struct Reducer: Hashable, Sendable {
  public private(set) var context: UpdateContext
  public private(set) var appliedEvents: UInt32 = 0
  public private(set) var terminalUpdates: UInt32 = 0

  public init(sessionID: SessionID) {
    context = UpdateContext(phase: .starting, view: .live, currentSessionID: sessionID)
  }

  public mutating func requestStop() {
    context.phase = context.phase.requestingStop
  }

  /// Returns whether the update was accepted for this session.
  @discardableResult
  public mutating func apply(_ sessionID: SessionID, update: WorkerUpdate) -> Bool {
    guard let nextPhase = context.nextPhase(sessionID, update: update) else {
      return false
    }
    switch update {
    case .event:
      appliedEvents = appliedEvents == .max ? .max : appliedEvents + 1
    case .stopped, .startFailed, .error:
      context.currentSessionID = nil
      terminalUpdates = terminalUpdates == .max ? .max : terminalUpdates + 1
    case .started:
      break
    }
    context.phase = nextPhase
    return true
  }
}

/// Navigation and restart must not replace a transcript while any worker or
/// persistence ownership remains.
public func canReplaceTranscript(
  phase: Phase,
  pendingPersistence: Bool,
  retainedOutput: Bool
) -> Bool {
  !phase.isActive && !pendingPersistence && !retainedOutput
}
