import Foundation

/// Which audio source produced a segment.
///
/// Wisp captures the microphone and the system audio as two independent
/// streams, transcribes each separately, and uses the source as a free
/// speaker label (mic = self, system = others) — no ML diarization required.
public enum SourceLabel: String, CaseIterable, Hashable, Sendable {
  /// The user's microphone.
  case mic
  /// System audio (other meeting participants, media playback).
  case system

  /// Stable string form used in the `SQLite` schema and exports.
  public var stableString: String {
    rawValue
  }

  /// Parse the stable string form. Returns `nil` for unknown values.
  public init?(parsing text: String) {
    self.init(rawValue: text)
  }
}
