import Foundation
import WispAudioKit
import WispCore
import os

/// Stable identity allocated before audio starts. Every worker update carries
/// it, so a delayed update can never mutate a newer session.
struct SessionStart: Equatable, Sendable {
  let sessionID: SessionID
  /// Timestamp and directory chosen once by the UI before the database row
  /// and the worker command were created. Every retry reuses them.
  let startedAt: Date
  let dirName: String
}

/// What one recording run is configured with.
struct SessionConfiguration: Sendable {
  var locale: String
  /// False when Speech Recognition is not granted, which records audio only.
  var transcriptionEnabled: Bool
  /// Wisp keeps the audio when Speech Recognition is denied, which is what
  /// lets onboarding accept a denied speech permission.
  var allowRecordOnly = true
}

/// Updates the worker sends back to the UI.
enum Update: Sendable {
  /// Audio is flowing and the session started successfully.
  case started(SessionStart)
  /// One transcription result from the session.
  case event(SessionID, WispSession.Result)
  /// The session stopped and has been torn down.
  case stopped(SessionID)
  /// Audio startup failed after constructing a session. Any partial capture has
  /// been stopped and its flushed events precede this update.
  case startFailed(SessionID, SessionError)
  /// Capture or transcription failed after start; cleanup has completed and the
  /// partial transcript must be finalised.
  case runtimeFailed(SessionID, SessionError)
  /// Session construction failed before capture could start.
  case error(SessionID, SessionError)
}

/// Owns the background task that drives one `WispSession` at a time.
///
/// Start and stop do async platform work, so each run lives in a detached task
/// and surfaces everything as `Update` values the main actor pumps.
/// `WispSession` delivers transcription through callbacks, so they feed an
/// ordered channel the run awaits instead of polling.
final class SessionRunner: @unchecked Sendable {
  private struct State: Sendable {
    var run: Run?
  }

  /// State for one accepted start.
  private final class Run: @unchecked Sendable {
    let start: SessionStart
    let channel = AudioEventChannel()
    let failures = OSAllocatedUnfairLock<SessionError?>(initialState: nil)
    let session = OSAllocatedUnfairLock<WispSession?>(initialState: nil)

    init(start: SessionStart) {
      self.start = start
    }
  }

  private let updates = OSAllocatedUnfairLock<[Update]>(initialState: [])

  private func appendUpdate(_ update: Update) {
    updates.withLock { $0.append(update) }
  }

  /// Take the first terminal runtime failure a run recorded, clearing it.
  private func takeFailure(_ run: Run) -> SessionError? {
    run.failures.withLock { current in
      defer { current = nil }
      return current
    }
  }
  private let state = OSAllocatedUnfairLock(initialState: State())

  /// Start a session. Returns false when one is already starting or running; a
  /// start while running is dropped, matching the serial worker loop.
  @discardableResult
  func start(
    outputDir: URL,
    configuration: SessionConfiguration,
    start: SessionStart
  ) -> Bool {
    state.withLock { state in
      guard state.run == nil else { return false }
      let run = Run(start: start)
      state.run = run
      // Detached so the run never inherits a caller's actor: quit blocks the
      // main actor while it waits for this task to finish.
      Task.detached { [self] in
        await execute(run: run, outputDir: outputDir, configuration: configuration)
      }
      return true
    }
  }

  /// Ask the running session to stop. Events the session flushes while stopping
  /// are published before `.stopped`.
  func stop() {
    state.withLock { $0.run }?.channel.send(.stop)
  }

  /// Apply the mute flag to the live session, if any.
  func setMicrophoneMuted(_ muted: Bool) {
    state.withLock { $0.run }?.session.withLock { $0?.setMicrophoneMuted(muted) }
  }

  /// Drain everything the worker has produced since the last call.
  func drainUpdates() -> [Update] {
    updates.withLock { queued in
      defer { queued.removeAll() }
      return queued
    }
  }

  /// Block until a terminal update for `sessionID` arrives, or until `timeout`
  /// expires. Returns every update collected on the way so the caller can apply
  /// them in order.
  ///
  /// Blocking is deliberate: the caller is `applicationShouldTerminate`, which
  /// must answer before the process exits. The run itself is detached, so it
  /// keeps making progress while the main actor waits.
  func waitForIdleSynchronously(sessionID: SessionID, timeout: Duration) -> [Update] {
    let deadline = ContinuousClock.now + timeout
    var collected: [Update] = []
    while true {
      collected.append(contentsOf: drainUpdates())
      if collected.contains(where: { $0.isTerminal(for: sessionID) }) {
        return collected
      }
      guard ContinuousClock.now < deadline else { return collected }
      Thread.sleep(forTimeInterval: 0.01)
    }
  }

  /// Ask the run to tear down without waiting. Used when the app exits.
  func shutdown() {
    state.withLock { $0.run }?.channel.send(.shutdown)
  }

  private func execute(
    run: Run,
    outputDir: URL,
    configuration: SessionConfiguration
  ) async {
    let session: WispSession
    do {
      session = try WispSession(
        outputDir: outputDir,
        locale: Locale(identifier: configuration.locale),
        transcriptionEnabled: configuration.transcriptionEnabled,
        allowRecordOnly: configuration.allowRecordOnly,
        onResult: { [channel = run.channel] result in
          channel.send(.event(result))
        },
        onTranscriberError: { terminal, message in
          // Printed here rather than carried to the main actor: routing it
          // through the event queue made the message depend on which view owned
          // the transcript, so diagnostics vanished outside the live screen.
          wispLog(message)
          if terminal {
            wispLog("Transcription stopped; recording continues.")
          }
        },
        onTerminalError: { [channel = run.channel, failures = run.failures] _, message in
          failures.withLock { if $0 == nil { $0 = .start(message) } }
          channel.send(.wake)
        },
        onLog: { message in
          wispLog(message)
        }
      )
    } catch {
      appendUpdate(.error(run.start.sessionID, .construction("\(error)")))
      finish(run)
      return
    }
    run.session.withLock { $0 = session }

    do {
      try await session.start()
    } catch {
      await publishStartFailure(run: run, session: session, error: error)
      finish(run)
      return
    }
    appendUpdate(.started(run.start))
    await pump(run: run, session: session)
    finish(run)
  }

  /// A failed start still owns whatever the platform captured before it failed.
  /// Emit `.startFailed` only when that partial output is worth keeping;
  /// otherwise `.error` deletes the pre-allocated row.
  private func publishStartFailure(
    run: Run,
    session: WispSession,
    error: Error
  ) async {
    var preservePartial = session.hasStartedCapture
    await session.stop()
    for event in run.channel.drainEvents() {
      // Any drained event is a transcript result, so its existence alone means
      // the analyzer produced output worth keeping.
      preservePartial = true
      appendUpdate(.event(run.start.sessionID, event))
    }
    let sessionError = error as? SessionError ?? .start("\(error)")
    appendUpdate(
      preservePartial
        ? .startFailed(run.start.sessionID, sessionError)
        : .error(run.start.sessionID, sessionError)
    )
  }

  private func pump(run: Run, session: WispSession) async {
    let sessionID = run.start.sessionID
    while let item = await run.channel.next() {
      switch item {
      case .stop, .shutdown:
        await stopAndPublish(run: run, session: session)
        return
      case .event(let event):
        appendUpdate(.event(sessionID, event))
      case .wake:
        break
      }
      guard let failure = takeFailure(run) else { continue }
      // A runtime failure is terminal, but native cleanup still runs first: the
      // analyzer may emit its last final while stopping, and the UI must
      // persist that row before it settles.
      await session.stop()
      let merged = mergeRuntimeFailures(failure, takeFailure(run))
      publishDrainedEvents(run: run)
      appendUpdate(.runtimeFailed(sessionID, merged))
      return
    }
  }

  private func stopAndPublish(run: Run, session: WispSession) async {
    await session.stop()
    publishDrainedEvents(run: run)
    if let failure = takeFailure(run) {
      appendUpdate(.runtimeFailed(run.start.sessionID, failure))
      return
    }
    appendUpdate(.stopped(run.start.sessionID))
  }

  private func publishDrainedEvents(run: Run) {
    for event in run.channel.drainEvents() {
      appendUpdate(.event(run.start.sessionID, event))
    }
  }

  private func finish(_ run: Run) {
    run.channel.finish()
    state.withLock { state in
      guard state.run === run else { return }
      state.run = nil
    }
  }
}

/// Fold a cleanup failure into the primary message rather than dropping it.
func mergeRuntimeFailures(_ primary: SessionError, _ cleanup: SessionError?) -> SessionError {
  guard let cleanup else { return primary }
  return .start("\(primary); cleanup/finalization also failed: \(cleanup)")
}

extension Update {
  /// Whether this update ends a recording run for `sessionID`.
  func isTerminal(for expected: SessionID) -> Bool {
    switch self {
    case .stopped(let sessionID),
      .startFailed(let sessionID, _),
      .runtimeFailed(let sessionID, _),
      .error(let sessionID, _):
      sessionID == expected
    case .started, .event:
      false
    }
  }
}

/// Ordered hand-off between the audio-kit callbacks and the run's task.
///
/// A stop sentinel travels through the same queue, so every event produced
/// before the stop request is observed before the run tears the session down.
final class AudioEventChannel: @unchecked Sendable {
  enum Item: Sendable {
    case event(WispSession.Result)
    case wake
    case stop
    case shutdown
  }

  private enum Next {
    case item(Item)
    case wait
    case end
  }

  private struct State: Sendable {
    var items: [Item] = []
    var waiter: CheckedContinuation<Item?, Never>?
    var finished = false
  }

  private let state = OSAllocatedUnfairLock(initialState: State())

  func send(_ item: Item) {
    let waiter: CheckedContinuation<Item?, Never>? = state.withLock { state in
      guard let waiter = state.waiter else {
        state.items.append(item)
        return nil
      }
      state.waiter = nil
      return waiter
    }
    waiter?.resume(returning: item)
  }

  func next() async -> Item? {
    await withCheckedContinuation { continuation in
      let action: Next = state.withLock { state in
        if !state.items.isEmpty {
          return .item(state.items.removeFirst())
        }
        if state.finished {
          return .end
        }
        state.waiter = continuation
        return .wait
      }
      switch action {
      case .item(let item): continuation.resume(returning: item)
      case .end: continuation.resume(returning: nil)
      case .wait: break
      }
    }
  }

  /// Everything already queued, ignoring the control sentinels.
  func drainEvents() -> [WispSession.Result] {
    state.withLock { state in
      let events = state.items.compactMap { item -> WispSession.Result? in
        guard case .event(let event) = item else { return nil }
        return event
      }
      state.items.removeAll()
      return events
    }
  }

  func finish() {
    let waiter: CheckedContinuation<Item?, Never>? = state.withLock { state in
      state.finished = true
      defer { state.waiter = nil }
      return state.waiter
    }
    waiter?.resume(returning: nil)
  }
}
