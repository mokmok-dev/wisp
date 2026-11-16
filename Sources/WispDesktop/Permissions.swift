import AppKit
import Foundation
import WispAudioKit
import WispCore

/// Permission state the onboarding and settings screens render.
///
/// The UI may enter the transcript once `satisfies(recordOnlyAllowed:)` holds.
/// While `pending` is set, a request is waiting on the OS dialog and the row
/// for that permission shows a spinner instead of a button.
struct Permissions: Equatable, Sendable {
  var microphone: PermissionStatus = .undetermined
  var speech: PermissionStatus = .undetermined
  var pending: Permission?

  /// Read both statuses without prompting.
  static func current() -> Permissions {
    Permissions(
      microphone: WispAudioKit.checkPermission(.microphone),
      speech: WispAudioKit.checkPermission(.speechRecognition)
    )
  }

  /// Whether the minimum permissions this build records with are available.
  ///
  /// macOS may record after Speech Recognition is explicitly denied or
  /// restricted only because the session policy permits record-only fallback.
  func satisfies(recordOnlyAllowed: Bool) -> Bool {
    guard microphone.isGranted else { return false }
    if speech.isGranted {
      return true
    }
    return recordOnlyAllowed && (speech == .denied || speech == .restricted)
  }

  mutating func setStatus(_ permission: Permission, _ status: PermissionStatus) {
    switch permission {
    case .microphone: microphone = status
    case .speechRecognition: speech = status
    }
  }
}

extension Permission {
  /// The row label and rationale shared by onboarding and settings.
  var title: String {
    switch self {
    case .microphone: "Microphone"
    case .speechRecognition: "Speech Recognition"
    }
  }
}
