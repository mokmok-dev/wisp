import Foundation

/// Light/dark appearance preference.
public enum ThemePreference: String, CaseIterable, Hashable, Sendable, Codable {
  /// Follow the macOS appearance (default).
  case system
  /// Always use the light theme.
  case light
  /// Always use the dark theme.
  case dark

  /// Decode an unknown or absent theme as the default, so one bad value
  /// does not cost the user every other preference.
  public init(from decoder: Decoder) throws {
    let container = try decoder.singleValueContainer()
    let raw = try container.decode(String.self)
    self = ThemePreference(rawValue: raw) ?? .system
  }

  public func encode(to encoder: Encoder) throws {
    var container = encoder.singleValueContainer()
    try container.encode(rawValue)
  }
}

/// BCP-47 identifier handed to the speech engine.
///
/// Stored verbatim so a code this build does not offer survives a save, and
/// so the Swift layer can map it with `supportedLocale(equivalentTo:)`.
public struct LanguageCode: Hashable, Sendable, Codable {
  public let code: String

  /// The locale Wisp recorded with before this setting existed. Changing
  /// the default would silently re-language every existing user.
  public static let fallback = LanguageCode("ja-JP")

  public init(_ code: String) {
    self.code = code
  }

  /// Whether the settings page offers this code.
  public var isKnown: Bool {
    Self.known.contains { $0.code == code }
  }

  /// The name shown in the picker, or the raw code when unknown.
  public var label: String {
    Self.known.first { $0.code == code }?.label ?? code
  }

  public init(from decoder: Decoder) throws {
    let container = try decoder.singleValueContainer()
    let raw = try container.decode(String.self)
    self = LanguageCode(raw)
  }

  public func encode(to encoder: Encoder) throws {
    var container = encoder.singleValueContainer()
    try container.encode(code)
  }
}

extension LanguageCode {
  /// Languages the settings page offers. `LanguageCode` accepts any string,
  /// so adding an entry here is all a new language needs.
  public static let known: [(code: String, label: String)] = [
    ("ja-JP", "Japanese"),
    ("en-US", "English (US)"),
    ("en-GB", "English (UK)"),
    ("zh-CN", "Chinese (Simplified)"),
    ("zh-TW", "Chinese (Traditional)"),
    ("ko-KR", "Korean"),
    ("es-ES", "Spanish (Spain)"),
    ("es-MX", "Spanish (Mexico)"),
    ("fr-FR", "French"),
    ("de-DE", "German"),
    ("it-IT", "Italian"),
    ("pt-BR", "Portuguese (Brazil)"),
    ("pt-PT", "Portuguese (Portugal)"),
    ("ru-RU", "Russian"),
    ("nl-NL", "Dutch"),
    ("sv-SE", "Swedish"),
    ("da-DK", "Danish"),
    ("nb-NO", "Norwegian"),
    ("fi-FI", "Finnish"),
    ("pl-PL", "Polish"),
    ("tr-TR", "Turkish"),
    ("ar-SA", "Arabic"),
    ("hi-IN", "Hindi"),
    ("th-TH", "Thai"),
    ("vi-VN", "Vietnamese"),
    ("id-ID", "Indonesian"),
    ("ms-MY", "Malay"),
    ("uk-UA", "Ukrainian"),
  ]
}

/// Persisted application settings.
public struct Settings: Hashable, Sendable, Codable {
  /// Light/dark appearance preference.
  public var theme: ThemePreference
  /// Speech language used for the next recording.
  public var transcriptionLanguage: LanguageCode
  /// Meeting detection and per-tool policies.
  public var meetings: MeetingDetectionSettings

  public init(
    theme: ThemePreference = .system,
    transcriptionLanguage: LanguageCode = .fallback,
    meetings: MeetingDetectionSettings = MeetingDetectionSettings()
  ) {
    self.theme = theme
    self.transcriptionLanguage = transcriptionLanguage
    self.meetings = meetings
  }

  private enum CodingKeys: String, CodingKey {
    case theme
    case transcriptionLanguage = "transcription_language"
    case meetings
  }

  /// Each field decodes on its own, so one malformed value does not cost the
  /// user every other preference.
  public init(from decoder: Decoder) throws {
    let container = try decoder.container(keyedBy: CodingKeys.self)
    theme = (try? container.decode(ThemePreference.self, forKey: .theme)) ?? .system
    transcriptionLanguage =
      (try? container.decode(LanguageCode.self, forKey: .transcriptionLanguage))
      ?? .fallback
    meetings =
      (try? container.decode(MeetingDetectionSettings.self, forKey: .meetings))
      ?? MeetingDetectionSettings()
  }
}
