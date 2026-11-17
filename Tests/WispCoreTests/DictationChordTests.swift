import Foundation
import XCTest

@testable import WispCore

final class DictationChordTests: XCTestCase {
  func testCodableRoundTripMatchesTheStoredShape() throws {
    let chord = DictationChord(keyCode: 49, modifiers: [.control, .option])
    let data = try JSONEncoder().encode(chord)
    let object = try XCTUnwrap(
      try JSONSerialization.jsonObject(with: data) as? [String: Any]
    )
    XCTAssertEqual(object["key_code"] as? Int, 49)
    XCTAssertEqual(object["modifiers"] as? [String], ["control", "option"])

    let loaded = try JSONDecoder().decode(DictationChord.self, from: data)
    XCTAssertEqual(loaded, chord)
  }

  func testDecodingDropsUnknownModifierNames() throws {
    let raw = Data(
      #"{"key_code":25,"modifiers":["control","hyper","hyperspace"]}"#.utf8
    )
    let chord = try JSONDecoder().decode(DictationChord.self, from: raw)
    XCTAssertEqual(chord.keyCode, 25)
    XCTAssertEqual(chord.modifiers, [.control])
  }

  func testDecodingCanonicalizesTheModifierOrder() throws {
    let raw = Data(
      #"{"key_code":49,"modifiers":["command","shift","option","control"]}"#.utf8
    )
    let allFour = try JSONDecoder().decode(DictationChord.self, from: raw)
    XCTAssertEqual(allFour.modifiers, [.control, .option, .shift, .command])
    XCTAssertTrue(
      try JSONDecoder().decode(DictationChord.self, from: try JSONEncoder().encode(allFour))
        .displayString
        .hasPrefix("⌃⌥⇧⌘")
    )
  }

  func testAKeyCodePastTheCarbonTableIsRejected() {
    XCTAssertNil(DictationChord(keyCode: 127, modifiers: [.control]))
  }

  func testDecodingAKeyCodePastTheCarbonTableThrows() {
    let raw = Data(#"{"key_code":127,"modifiers":["control"]}"#.utf8)
    XCTAssertThrowsError(try JSONDecoder().decode(DictationChord.self, from: raw))
  }

  func testAShiftOnlyChordIsRejected() {
    XCTAssertNil(DictationChord(keyCode: 49, modifiers: [.shift]))
    XCTAssertNil(DictationChord(keyCode: 49, modifiers: []))
  }

  func testDecodingAShiftOnlyChordThrows() {
    let raw = Data(#"{"key_code":49,"modifiers":["shift"]}"#.utf8)
    XCTAssertThrowsError(try JSONDecoder().decode(DictationChord.self, from: raw))
  }

  func testTheDefaultRendersControlOptionSpace() {
    XCTAssertEqual(DictationChord.default.displayString, "⌃⌥Space")
    XCTAssertEqual(
      DictationChord.default,
      DictationChord(keyCode: 49, modifiers: [.control, .option])
    )
  }

  func testASingleModifierRendersOneGlyphWithoutASeparator() throws {
    let chord = try XCTUnwrap(DictationChord(keyCode: 0x03, modifiers: .option))
    XCTAssertEqual(chord.displayString, "⌥F")
    XCTAssertEqual(chord.displayString.filter(\.isLetter), "F")
    XCTAssertFalse(chord.displayString.contains("-"))
    XCTAssertFalse(chord.displayString.contains("+"))
  }

  func testKeyLabelsCoverLettersSpaceArrowsAndUnmappedCodes() {
    XCTAssertEqual(DictationChord(keyCode: 0x03, modifiers: .control)?.keyLabel, "F")
    XCTAssertEqual(DictationChord(keyCode: 49, modifiers: .control)?.keyLabel, "Space")
    XCTAssertEqual(DictationChord(keyCode: 0x7B, modifiers: .control)?.keyLabel, "←")
    XCTAssertEqual(DictationChord(keyCode: 82, modifiers: .control)?.keyLabel, "Key 82")
  }
}
