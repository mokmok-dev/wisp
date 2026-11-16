@preconcurrency import AVFoundation
import Foundation
import Speech

/// A TCC permission Wisp gates recording on.
public enum Permission: Sendable, Hashable {
  /// Microphone access; required by the mic capture path.
  case microphone
  /// On-device speech recognition; required by both transcription pipelines.
  case speechRecognition
}

/// Current OS-side state of one `Permission`.
public enum PermissionStatus: Sendable, Hashable {
  /// The user has not been asked yet, so a request shows the OS dialog.
  case undetermined
  /// Explicitly denied; only System Settings can re-enable it.
  case denied
  /// Usable.
  case granted
  /// Blocked by policy (parental controls, MDM).
  case restricted

  public var isGranted: Bool {
    self == .granted
  }
}

/// Read a permission's current state without prompting.
public func checkPermission(_ permission: Permission) -> PermissionStatus {
  switch permission {
  case .microphone:
    map(AVCaptureDevice.authorizationStatus(for: .audio))
  case .speechRecognition:
    map(SFSpeechRecognizer.authorizationStatus())
  }
}

/// Trigger the OS prompt for a permission that has never been asked.
///
/// Returns the current status immediately for a permission the OS will not
/// prompt for again. The dialog can stay open for a long time; the caller
/// decides where the wait runs.
public func requestPermission(_ permission: Permission) async -> PermissionStatus {
  switch permission {
  case .microphone:
    guard AVCaptureDevice.authorizationStatus(for: .audio) == .notDetermined else {
      return checkPermission(permission)
    }
    return await AVCaptureDevice.requestAccess(for: .audio) ? .granted : .denied
  case .speechRecognition:
    guard SFSpeechRecognizer.authorizationStatus() == .notDetermined else {
      return checkPermission(permission)
    }
    let status = await withCheckedContinuation { continuation in
      SFSpeechRecognizer.requestAuthorization { continuation.resume(returning: $0) }
    }
    return map(status)
  }
}

private func map(_ status: AVAuthorizationStatus) -> PermissionStatus {
  switch status {
  case .notDetermined: .undetermined
  case .denied: .denied
  case .authorized: .granted
  case .restricted: .restricted
  @unknown default: .undetermined
  }
}

private func map(_ status: SFSpeechRecognizerAuthorizationStatus) -> PermissionStatus {
  switch status {
  case .notDetermined: .undetermined
  case .denied: .denied
  case .authorized: .granted
  case .restricted: .restricted
  @unknown default: .undetermined
  }
}
