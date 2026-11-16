import Foundation

/// Timestamps cross the `SQLite` boundary as text: `YYYY-MM-DD HH:MM:SS[.fraction]+00:00`.
///
/// `ORDER BY started_at` sorts that text, so the spelling is part of the
/// on-disk contract — changing it would reorder an existing library. The
/// fraction is grouped in threes (`.SSS`, `.SSSSSS`, `.SSSSSSSSS`) so a
/// fixed-width fraction keeps the text order equal to the numeric order.
/// Reads also accept RFC 3339 (`T` separator, `Z` suffix, any offset).
enum SQLiteClock {
  /// Render `date` for storage.
  static func string(from date: Date) -> String {
    let seconds = date.timeIntervalSince1970
    let whole = seconds.rounded(.down)
    let stamp = utcStamp(secondsSinceEpoch: Int64(whole))
    let nanoseconds = Int(((seconds - whole) * 1_000_000_000).rounded())
    return stamp + fraction(nanoseconds) + "+00:00"
  }

  /// Parse a stored timestamp, or `nil` when the text is not a date.
  static func date(from text: String) -> Date? {
    var cursor = Cursor(text)
    guard let year = cursor.integer(), cursor.skip("-"),
      let month = cursor.integer(), cursor.skip("-"),
      let day = cursor.integer(),
      cursor.skip(" ") || cursor.skip("T"),
      let hour = cursor.integer(), cursor.skip(":"),
      let minute = cursor.integer(), cursor.skip(":"),
      let second = cursor.integer()
    else {
      return nil
    }
    var fraction = 0.0
    if cursor.skip(".") {
      let digits = cursor.digits()
      guard !digits.isEmpty else { return nil }
      fraction = Double("0." + digits) ?? 0
    }
    var offset = 0.0
    if cursor.skip("Z") {
      offset = 0
    } else if cursor.skip("+") || cursor.skip("-") {
      let negative = cursor.lastMatched == "-"
      guard let offsetHour = cursor.integer(), cursor.skip(":"),
        let offsetMinute = cursor.integer()
      else {
        return nil
      }
      offset = Double(offsetHour * 3600 + offsetMinute * 60)
      if negative {
        offset = -offset
      }
    } else if !cursor.isAtEnd {
      return nil
    }

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
    guard let utc = calendar.date(from: components) else { return nil }
    return utc.addingTimeInterval(fraction - offset)
  }

  private static func fraction(_ nanoseconds: Int) -> String {
    if nanoseconds == 0 {
      return ""
    }
    if nanoseconds % 1_000_000 == 0 {
      return String(format: ".%03d", nanoseconds / 1_000_000)
    }
    if nanoseconds % 1000 == 0 {
      return String(format: ".%06d", nanoseconds / 1000)
    }
    return String(format: ".%09d", nanoseconds)
  }

  private static func utcStamp(secondsSinceEpoch: Int64) -> String {
    var time = time_t(secondsSinceEpoch)
    var broken = tm()
    gmtime_r(&time, &broken)
    return String(
      format: "%04d-%02d-%02d %02d:%02d:%02d",
      broken.tm_year + 1900,
      broken.tm_mon + 1,
      broken.tm_mday,
      broken.tm_hour,
      broken.tm_min,
      broken.tm_sec
    )
  }

  /// Minimal left-to-right reader.
  ///
  /// `Date.ISO8601FormatStyle` rejects two forms this reader accepts: a stamp
  /// with no offset at all, and a fraction longer than nine digits. A database
  /// written by an older or third-party tool can hold either, and a failed read
  /// surfaces as a missing row, so the accepted set is not narrowed here.
  private struct Cursor {
    private let scalars: [Character]
    private var index = 0
    private(set) var lastMatched: Character?

    init(_ text: String) {
      scalars = Array(text)
    }

    var isAtEnd: Bool {
      index >= scalars.count
    }

    mutating func skip(_ character: Character) -> Bool {
      guard !isAtEnd, scalars[index] == character else { return false }
      index += 1
      lastMatched = character
      return true
    }

    mutating func digits() -> String {
      let start = index
      while !isAtEnd, scalars[index].isASCII, scalars[index].isNumber {
        index += 1
      }
      return String(scalars[start..<index])
    }

    mutating func integer() -> Int? {
      let start = index
      let text = digits()
      guard !text.isEmpty, let value = Int(text) else {
        index = start
        return nil
      }
      return value
    }
  }
}
