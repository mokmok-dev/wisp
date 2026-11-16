import Foundation
import XCTest

@testable import WispCore

final class IdentifierTests: XCTestCase {
  /// Covers every integer shape the `SQLite` layer can hand back.
  private let rawIDs: [Int64] = [0, 1, 42, -3, .max, .min]

  func testSessionIDsRoundTripAndDescribeDecimal() {
    for raw in rawIDs {
      let id = SessionID(raw)
      XCTAssertEqual(id.rawValue, raw)
      XCTAssertEqual(id.description, String(raw))
    }
  }

  func testSegmentIDsRoundTripAndDescribeDecimal() {
    for raw in rawIDs {
      let id = SegmentID(raw)
      XCTAssertEqual(id.rawValue, raw)
      XCTAssertEqual(id.description, String(raw))
    }
  }

  func testIdentifiersCompareByRawValue() {
    XCTAssertLessThan(SessionID(-3), SessionID(0))
    XCTAssertLessThan(SegmentID(1), SegmentID(42))
    XCTAssertEqual(SessionID(7), SessionID(7))
    XCTAssertNotEqual(SessionID(7), SessionID(8))
  }
}

final class SourceLabelTests: XCTestCase {
  func testEveryLabelRoundTripsThroughItsStableString() {
    for source in SourceLabel.allCases {
      XCTAssertEqual(SourceLabel(parsing: source.stableString), source)
    }
  }

  func testUnknownLabelIsNil() {
    XCTAssertNil(SourceLabel(parsing: "speaker"))
    XCTAssertNil(SourceLabel(parsing: ""))
  }

  func testStableStringsAreMicAndSystem() {
    XCTAssertEqual(SourceLabel.mic.stableString, "mic")
    XCTAssertEqual(SourceLabel.system.stableString, "system")
  }
}
