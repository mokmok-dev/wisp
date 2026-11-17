import Foundation
import WispCore

/// Human-readable formatting shared by the views, so dates, durations, and the
/// recording clock read the same on every screen.
enum Format {
  /// Elapsed milliseconds as `mm:ss`, or `h:mm:ss` past one hour.
  static func clock(totalMilliseconds: UInt64) -> String {
    let totalSeconds = totalMilliseconds / 1000
    let hours = totalSeconds / 3600
    let minutes = (totalSeconds % 3600) / 60
    let seconds = totalSeconds % 60
    if hours > 0 {
      return String(format: "%d:%02d:%02d", hours, minutes, seconds)
    }
    return String(format: "%02d:%02d", minutes, seconds)
  }

  /// A persisted session duration, or an em dash when unknown.
  static func duration(seconds: Int?) -> String {
    guard let seconds, seconds >= 0 else { return "—" }
    return clock(totalMilliseconds: UInt64(seconds) * 1000)
  }

  /// A session start timestamp in the user's local timezone.
  static func startedAt(_ date: Date) -> String {
    let formatter = DateFormatter()
    formatter.locale = Locale(identifier: "en_US_POSIX")
    formatter.calendar = Calendar(identifier: .gregorian)
    formatter.timeZone = .current
    formatter.dateFormat = "yyyy-MM-dd HH:mm"
    return formatter.string(from: date)
  }
}

extension Session {
  /// The start time for a row's caption, `nil` when the title already is the
  /// default title for that start time.
  var startCaption: String? {
    let untitled = Library.defaultTitle(startedAt: startedAt)
    return title == untitled ? nil : Format.startedAt(startedAt)
  }
}
