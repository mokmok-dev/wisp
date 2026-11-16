import Foundation

/// An offered call stays snoozed until its app has been off the microphone
/// for about a minute, so one call is offered once.
public let absentForReset: Duration = .seconds(60)

/// A call that drops off one microphone poll and returns right after keeps
/// its time toward the offer.
public let dropoutGrace: Duration = .seconds(2)

/// Observations further apart than this do not make one run: a sleeping Mac
/// or a stalled loop restarts the clock instead of counting the gap.
public let maxObservationGap: Duration = .seconds(10)

/// A call that has held the microphone long enough to count.
public struct MeetingDetection: Hashable, Sendable {
  public let tool: MeetingTool
  /// The tool is set to start recording without an offer.
  public let autoRecord: Bool

  public init(tool: MeetingTool, autoRecord: Bool) {
    self.tool = tool
    self.autoRecord = autoRecord
  }
}

/// What one observation produced.
public enum MeetingObservation: Hashable, Sendable {
  /// A call became stable.
  case detected(MeetingDetection)
  /// A previously offered call left the microphone long enough to reset.
  case ended(tool: MeetingTool)
}

/// Decides when one app holding the microphone becomes a meeting.
///
/// The platform probe reports candidates once a second; this type owns the
/// timing rules that separate a call from a voice note, and the snooze that
/// keeps one call from being offered twice. Pure: the caller supplies `now`.
public struct MeetingDetector: Sendable {
  /// How long a condition has held, on the monotonic clock, so a slow loop
  /// iteration cannot stretch a threshold.
  private struct Run {
    var since: ContinuousClock.Instant
    var last: ContinuousClock.Instant

    init(now: ContinuousClock.Instant) {
      since = now
      last = now
    }

    mutating func extend(to now: ContinuousClock.Instant) -> Duration {
      if now - last > maxObservationGap {
        since = now
      }
      last = now
      return since.duration(to: now)
    }
  }

  private struct Pending {
    var tool: MeetingTool
    var run: Run
  }

  private var pending: Pending?
  /// Last time each offered call's app was on the microphone. A call stays
  /// snoozed until its entry is a minute old.
  private var handled: [MeetingTool: ContinuousClock.Instant] = [:]

  public init() {}

  /// One observation under the user's per-tool policies.
  ///
  /// `active` is the set of tools currently holding the microphone, in any
  /// order. A tool with `.off` is never detected and never offered.
  public mutating func observe(
    now: ContinuousClock.Instant,
    active: [MeetingTool],
    policy: (MeetingTool) -> MeetingPolicy
  ) -> MeetingObservation? {
    // One call at a time: while an offered call still holds the
    // microphone, another app joining it does not raise a second offer.
    var handledIsPresent = false
    for tool in Set(handled.keys).filter(active.contains) {
      handled[tool] = now
      handledIsPresent = true
    }
    if handledIsPresent {
      return nil
    }

    // `MeetingTool.allCases` is declaration order, so the expired snooze
    // is chosen deterministically.
    let expired = MeetingTool.allCases.first { tool in
      guard let lastPresent = handled[tool] else { return false }
      return lastPresent.duration(to: now) >= absentForReset
    }
    if let expired {
      handled[expired] = nil
      pending = nil
      return .ended(tool: expired)
    }

    let candidate = MeetingTool.allCases.first {
      active.contains($0) && policy($0) != .off
    }
    guard let tool = candidate else {
      if let current = pending, current.run.last.duration(to: now) > dropoutGrace {
        pending = nil
      }
      return nil
    }

    var held = Duration.zero
    if var current = pending, current.tool == tool {
      held = current.run.extend(to: now)
      pending = current
    } else {
      pending = Pending(tool: tool, run: Run(now: now))
    }
    if held < .seconds(tool.detectionDelay) {
      return nil
    }

    pending = nil
    handled[tool] = now
    return .detected(
      MeetingDetection(tool: tool, autoRecord: policy(tool) == .autoRecord)
    )
  }

  /// A poll that could not read microphone activity.
  ///
  /// Unknown is not absence: a pending run keeps its progress, and a
  /// snooze's absence clock restarts so a failed read cannot count as the
  /// app having left.
  public mutating func observeUnavailable(now: ContinuousClock.Instant) {
    if var current = pending {
      if now - current.run.last > maxObservationGap {
        current.run.since = now
      }
      current.run.last = now
      pending = current
    }
    for tool in Set(handled.keys) {
      handled[tool] = now
    }
  }

  /// Treat `tool` as already offered, for a call the user started recording
  /// by hand after it was detected.
  public mutating func markHandled(
    now: ContinuousClock.Instant,
    tool: MeetingTool
  ) {
    pending = nil
    handled[tool] = now
  }

  /// Forget every run and snooze, as when detection is turned off.
  public mutating func reset() {
    pending = nil
    handled.removeAll()
  }

  /// The call currently snoozed, if any.
  public var handledTool: MeetingTool? {
    MeetingTool.allCases.first { handled[$0] != nil }
  }
}
