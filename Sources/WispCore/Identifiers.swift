import Foundation

/// Identifier for one recording session (one meeting).
///
/// Newtype around the `SQLite` rowid so it can't be mixed up with other
/// integer IDs (segment IDs, indices, etc.).
public struct SessionID: Hashable, Comparable, Sendable, CustomStringConvertible {
  public let rawValue: Int64

  public init(_ rawValue: Int64) {
    self.rawValue = rawValue
  }

  public var description: String {
    String(rawValue)
  }

  public static func < (lhs: Self, rhs: Self) -> Bool {
    lhs.rawValue < rhs.rawValue
  }
}

/// Identifier for one transcript segment.
public struct SegmentID: Hashable, Comparable, Sendable, CustomStringConvertible {
  public let rawValue: Int64

  public init(_ rawValue: Int64) {
    self.rawValue = rawValue
  }

  public var description: String {
    String(rawValue)
  }

  public static func < (lhs: Self, rhs: Self) -> Bool {
    lhs.rawValue < rhs.rawValue
  }
}
