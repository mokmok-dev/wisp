import SwiftUI
import WispAudioKit

/// Shown until the minimum permissions this build records with are granted.
///
/// One row per permission with the action that moves it forward: request the
/// OS prompt while undetermined, otherwise deep-link to System Settings.
struct OnboardingView: View {
  @Bindable var model: AppModel
  let controller: SessionController

  var body: some View {
    ScrollView {
      VStack(alignment: .leading, spacing: 24) {
        VStack(alignment: .leading, spacing: 6) {
          Text("Welcome to Wisp")
            .font(.title2.weight(.semibold))
          Text(
            "Wisp records your microphone and system audio at the same time and "
              + "transcribes both on-device. Nothing ever leaves your Mac."
          )
          .font(.callout)
          .foregroundStyle(.secondary)
          .fixedSize(horizontal: false, vertical: true)
        }
        VStack(spacing: 12) {
          PermissionRow(
            permission: .microphone,
            systemImage: "mic",
            model: model,
            controller: controller
          )
          PermissionRow(
            permission: .speechRecognition,
            systemImage: "waveform",
            model: model,
            controller: controller
          )
        }
        Text(
          "You can change these later in System Settings → Privacy & Security. "
            + "This screen updates automatically once permissions are granted."
        )
        .font(.callout)
        .foregroundStyle(.secondary)
        .fixedSize(horizontal: false, vertical: true)
      }
      .frame(maxWidth: 620, alignment: .leading)
      .padding(32)
      .frame(maxWidth: .infinity)
    }
  }
}

/// One permission, with its OS state and the action that changes it.
struct PermissionRow: View {
  let permission: Permission
  let systemImage: String
  @Bindable var model: AppModel
  let controller: SessionController

  var body: some View {
    HStack(spacing: 16) {
      RoundedRectangle(cornerRadius: 8)
        .fill(Color(nsColor: .controlBackgroundColor))
        .frame(width: 40, height: 40)
        .overlay(Image(systemName: systemImage).font(.system(size: 18)))
      VStack(alignment: .leading, spacing: 4) {
        HStack(spacing: 8) {
          Text(permission.title).font(.subheadline.weight(.semibold))
          statusTag
        }
        Text(permission.rationale)
          .font(.callout)
          .foregroundStyle(.secondary)
          .fixedSize(horizontal: false, vertical: true)
      }
      Spacer(minLength: 8)
      action
    }
    .padding(14)
    .background(Color(nsColor: .controlBackgroundColor), in: RoundedRectangle(cornerRadius: 10))
    .overlay(
      RoundedRectangle(cornerRadius: 10)
        .strokeBorder(Color(nsColor: .separatorColor), lineWidth: 1)
    )
  }

  private var status: PermissionStatus {
    permission == .microphone ? model.permissions.microphone : model.permissions.speech
  }

  private var isPending: Bool {
    model.permissions.pending == permission
  }

  private var statusTag: some View {
    let (label, color): (String, Color) =
      if isPending {
        ("Waiting…", .accentColor)
      } else {
        switch status {
        case .granted: ("Granted", .green)
        case .denied: ("Denied", .red)
        case .restricted: ("Restricted", .orange)
        case .undetermined: ("Not requested", .secondary)
        }
      }
    return TagView(label: label, color: color)
  }

  @ViewBuilder
  private var action: some View {
    switch status {
    case .granted:
      EmptyView()
    case .undetermined:
      Button {
        controller.requestPermission(permission)
      } label: {
        if isPending {
          ProgressView().controlSize(.small)
        } else {
          Text("Grant")
        }
      }
      .buttonStyle(.borderedProminent)
      .disabled(isPending)
    case .denied, .restricted:
      Button("Open Settings") {
        controller.openPermissionSettings(permission)
      }
    }
  }
}
