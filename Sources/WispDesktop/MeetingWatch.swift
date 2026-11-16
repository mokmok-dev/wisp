import Foundation
import WispAudioKit
import WispCore

/// What the watcher reports to the UI.
enum MeetingEvent: Sendable, Equatable {
  /// A call held the microphone long enough to act on.
  case detected(tool: MeetingTool, autoRecord: Bool)
  /// A previously offered call left the microphone long enough to reset.
  case ended(tool: MeetingTool)
  /// Microphone activity could not be read. The detector kept its state.
  case samplerUnavailable
}

/// Polls which apps hold the microphone and turns that into one-shot events.
///
/// Detection runs off the main actor so no view frame waits on Core Audio; the
/// pump drains the events and applies them to the model.
actor MeetingWatch {
  /// How often microphone activity is sampled.
  static let pollInterval = Duration.seconds(1)

  private var preferences: MeetingDetectionSettings
  private var detector = MeetingDetector()
  private var announcedFailure = false
  private var events: [MeetingEvent] = []
  private var pollTask: Task<Void, Never>?

  init(preferences: MeetingDetectionSettings) {
    self.preferences = preferences
  }

  /// Start the poll loop. Nothing is sampled until detection is enabled.
  func start() {
    guard pollTask == nil else { return }
    pollTask = Task { [weak self] in
      while !Task.isCancelled {
        guard let self else { return }
        await poll()
        try? await Task.sleep(for: Self.pollInterval)
      }
    }
  }

  func shutdown() {
    pollTask?.cancel()
    pollTask = nil
  }

  /// Replace the enabled flag and per-tool policies.
  ///
  /// Only an actual enable/disable starts a fresh run: a policy edit must not
  /// clear the snooze for a call that is still in progress.
  func setPreferences(_ next: MeetingDetectionSettings) {
    let enabledChanged = preferences.enabled != next.enabled
    preferences = next
    if enabledChanged {
      detector.reset()
    }
  }

  /// Take every event since the last drain.
  func drainEvents() -> [MeetingEvent] {
    defer { events.removeAll() }
    return events
  }

  private func poll() async {
    guard preferences.enabled else {
      detector.reset()
      return
    }
    // `activeMeetingTools()` walks Core Audio synchronously. Detached so the
    // HAL round-trip cannot occupy this actor's executor and stall the pump
    // that drains events on the same cooperative pool.
    let active: [MeetingTool]
    do {
      active = try await Task.detached { try activeMeetingTools() }.value
    } catch {
      detector.observeUnavailable(now: .now)
      // One notice per failure streak: a broken sampler would otherwise flood
      // the log once a second.
      if !announcedFailure {
        announcedFailure = true
        events.append(.samplerUnavailable)
      }
      return
    }
    announcedFailure = false
    let policies = preferences
    guard
      let observation = detector.observe(
        now: .now,
        active: active,
        policy: { policies.policy(for: $0) }
      )
    else {
      return
    }
    switch observation {
    case .detected(let detection):
      events.append(.detected(tool: detection.tool, autoRecord: detection.autoRecord))
    case .ended(let tool):
      events.append(.ended(tool: tool))
    }
  }
}
