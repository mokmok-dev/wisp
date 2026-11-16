import Foundation
import WispCore
import XCTest

@testable import WispDesktop

/// Shared fixtures for the desktop suites.
enum DesktopFixtures {
  static func date(
    _ year: Int,
    _ month: Int,
    _ day: Int,
    _ hour: Int = 0,
    _ minute: Int = 0,
    _ second: Int = 0
  ) -> Date {
    var calendar = Calendar(identifier: .gregorian)
    calendar.timeZone = TimeZone(secondsFromGMT: 0) ?? .gmt
    let components = DateComponents(
      year: year,
      month: month,
      day: day,
      hour: hour,
      minute: minute,
      second: second
    )
    guard let date = calendar.date(from: components) else {
      fatalError("invalid fixture date")
    }
    return date
  }

  static func segment(
    _ source: SourceLabel,
    id: UInt64,
    text: String,
    start: Double = 0,
    end: Double = 1,
    isFinal: Bool = false
  ) -> LiveSegment {
    LiveSegment(
      source: source,
      id: id,
      text: text,
      startSeconds: start,
      endSeconds: end,
      isFinal: isFinal
    )
  }

  static func storedSession(
    id: Int64,
    title: String = "Weekly sync",
    startedAt: Date = date(2026, 7, 22, 9, 0, 0),
    endedAt: Date? = date(2026, 7, 22, 9, 47, 12)
  ) -> WispCore.Session {
    WispCore.Session(
      id: SessionID(id),
      startedAt: startedAt,
      endedAt: endedAt,
      title: title,
      micWavPath: "recordings/session/mic.ogg",
      systemWavPath: "recordings/session/system.ogg",
      notes: ""
    )
  }
}
