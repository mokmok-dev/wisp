import Foundation

/// A user-chosen keyboard chord that starts or finishes dictation from
/// anywhere in macOS.
public struct DictationChord: Hashable, Sendable, Codable {
  public struct Modifiers: OptionSet, Hashable, Sendable {
    public let rawValue: UInt8
    public init(rawValue: UInt8) {
      self.rawValue = rawValue
    }
    public static let control = Modifiers(rawValue: 1 << 0)
    public static let option = Modifiers(rawValue: 1 << 1)
    public static let shift = Modifiers(rawValue: 1 << 2)
    public static let command = Modifiers(rawValue: 1 << 3)
    /// Every bit a chord may carry.
    public static let all: Modifiers = [.control, .option, .shift, .command]
    /// A chord needs at least one of these, or it would swallow ordinary typing.
    public static let required: Modifiers = [.control, .option, .command]
  }

  public let keyCode: UInt16
  public let modifiers: Modifiers

  /// Fails for a key code past the Carbon table (the largest `kVK_*` is 126)
  /// or a chord with none of `Modifiers.required`.
  public init?(keyCode: UInt16, modifiers: Modifiers) {
    guard keyCode <= 126 else { return nil }
    guard !modifiers.isDisjoint(with: Modifiers.required) else { return nil }
    self.keyCode = keyCode
    self.modifiers = modifiers
  }

  /// ⌃⌥Space. Control-Space switches input sources and ⌘Space opens Spotlight.
  public static let `default` = DictationChord(
    keyCode: 49,
    modifiers: [.control, .option]
  )!

  /// `⌃⌥Space`: modifier glyphs in control, option, shift, command order, then the key.
  public var displayString: String {
    var glyphs = ""
    if modifiers.contains(.control) { glyphs += "⌃" }
    if modifiers.contains(.option) { glyphs += "⌥" }
    if modifiers.contains(.shift) { glyphs += "⇧" }
    if modifiers.contains(.command) { glyphs += "⌘" }
    return glyphs + keyLabel
  }

  /// `Space`, `D`, `F5`, `←`, or `Key 99` for a code no table row covers.
  public var keyLabel: String {
    Self.keyCodeLabels[keyCode] ?? "Key \(keyCode)"
  }

  /// Carbon delivers the same key code for the left and right variants of a
  /// modifier key, so both rows share one glyph.
  private static let keyCodeLabels: [UInt16: String] = [
    0x00: "A",
    0x01: "S",
    0x02: "D",
    0x03: "F",
    0x04: "H",
    0x05: "G",
    0x06: "Z",
    0x07: "X",
    0x08: "C",
    0x09: "V",
    0x0A: "§",
    0x0B: "B",
    0x0C: "Q",
    0x0D: "W",
    0x0E: "E",
    0x0F: "R",
    0x10: "Y",
    0x11: "T",
    0x12: "1",
    0x13: "2",
    0x14: "3",
    0x15: "4",
    0x16: "6",
    0x17: "5",
    0x18: "=",
    0x19: "9",
    0x1A: "7",
    0x1B: "-",
    0x1C: "8",
    0x1D: "0",
    0x1E: "]",
    0x1F: "O",
    0x20: "U",
    0x21: "[",
    0x22: "I",
    0x23: "P",
    0x24: "Return",
    0x25: "L",
    0x26: "J",
    0x27: "'",
    0x28: "K",
    0x29: ";",
    0x2A: "\\",
    0x2B: ",",
    0x2C: "/",
    0x2D: "N",
    0x2E: "M",
    0x2F: ".",
    0x30: "Tab",
    0x31: "Space",
    0x32: "`",
    0x33: "Delete",
    0x35: "Escape",
    0x37: "⌘",
    0x38: "⇧",
    0x39: "⇪",
    0x3A: "⌥",
    0x3B: "⌃",
    0x3C: "⇧",
    0x3D: "⌥",
    0x3E: "⌃",
    0x3F: "Fn",
    0x7A: "F1",
    0x78: "F2",
    0x63: "F3",
    0x76: "F4",
    0x60: "F5",
    0x61: "F6",
    0x62: "F7",
    0x64: "F8",
    0x65: "F9",
    0x6D: "F10",
    0x67: "F11",
    0x6F: "F12",
    0x7B: "←",
    0x7C: "→",
    0x7D: "↓",
    0x7E: "↑",
    0x73: "Home",
    0x77: "End",
    0x74: "Page Up",
    0x79: "Page Down",
    0x72: "Help",
    0x75: "Forward Delete",
  ]

  private static let modifierNamesInCanonicalOrder: [(name: String, bit: Modifiers)] = [
    ("control", .control),
    ("option", .option),
    ("shift", .shift),
    ("command", .command),
  ]

  enum CodingKeys: String, CodingKey {
    case keyCode = "key_code"
    case modifiers
  }

  public init(from decoder: Decoder) throws {
    let container = try decoder.container(keyedBy: CodingKeys.self)
    let keyCode = try container.decode(UInt16.self, forKey: .keyCode)
    let named = try container.decode([String].self, forKey: .modifiers)
    var modifiers = Modifiers()
    for name in named {
      for candidate in Self.modifierNamesInCanonicalOrder
      where candidate.name == name {
        modifiers.insert(candidate.bit)
      }
    }
    guard
      let resolved = DictationChord(
        keyCode: keyCode,
        modifiers: modifiers
      )
    else {
      throw DecodingError.dataCorrupted(
        DecodingError.Context(
          codingPath: decoder.codingPath,
          debugDescription:
            "DictationChord carries no control, option, or command modifier, or its key code exceeds the Carbon table"
        )
      )
    }
    self = resolved
  }

  public func encode(to encoder: Encoder) throws {
    var container = encoder.container(keyedBy: CodingKeys.self)
    try container.encode(keyCode, forKey: .keyCode)
    let names =
      Self.modifierNamesInCanonicalOrder
      .filter { modifiers.contains($0.bit) }
      .map { $0.name }
    try container.encode(names, forKey: .modifiers)
  }
}
