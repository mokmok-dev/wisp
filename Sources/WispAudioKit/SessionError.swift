import Foundation

/// Errors surfaced by `WispSession` operations.
public enum SessionError: Error, Equatable, Sendable, CustomStringConvertible {
  /// The session could not be constructed (output directory, locale).
  case construction(String)
  /// Capture or transcription failed to start, or failed while running.
  case start(String)

  public var description: String {
    switch self {
    case .construction(let message):
      "WispAudioKit session construction failed: \(message)"
    case .start(let message):
      "WispAudioKit session start failed: \(message)"
    }
  }
}
