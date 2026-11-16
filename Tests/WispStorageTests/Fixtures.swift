import Foundation
import WispCore
import WispStorage
import XCTest

/// Shared fixtures for the storage suites.
enum Fixtures {
  static let referenceDate = Date(timeIntervalSince1970: 1_780_000_000)

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

  static func newSession(title: String = "test") -> NewSession {
    NewSession(
      startedAt: date(2026, 5, 28, 10, 0, 0),
      title: title,
      micWavPath: "s/mic.ogg",
      systemWavPath: "s/system.ogg"
    )
  }

  static func segment(
    session: SessionID,
    source: SourceLabel = .mic,
    index: UInt32,
    start: Double,
    end: Double,
    text: String
  ) -> NewSegment {
    NewSegment(
      sessionID: session,
      source: source,
      segmentIndex: index,
      startSeconds: start,
      endSeconds: end,
      text: text,
      speakerLabel: nil
    )
  }

  static func freshSession(_ storage: Storage) throws -> SessionID {
    try storage.sessions.create(newSession())
  }
}
