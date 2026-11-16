import SwiftUI
import WispAudioKit
import WispCore

/// The window's content: title bar, status strip, meeting prompt, sidebar, and
/// whichever screen is active.
///
/// The view owns no recording state; every control calls `SessionController`,
/// which writes results back into `AppModel`.
struct AppView: View {
  @Bindable var model: AppModel
  let controller: SessionController

  var body: some View {
    NavigationSplitView(columnVisibility: $model.sidebarVisibility) {
      sidebar
        .navigationSplitViewColumnWidth(min: 190, ideal: 210, max: 240)
    } detail: {
      VStack(spacing: 0) {
        chrome
        Divider()
        VStack(spacing: 0) {
          statusStrip
          meetingPrompt
          content
        }
      }
      .frame(minWidth: 560, minHeight: 440)
    }
    .toolbar(removing: .sidebarToggle)
    .navigationTitle("Wisp")
    .sheet(
      isPresented: Binding(
        get: { controller.aboutRequested },
        set: { controller.aboutRequested = $0 }
      )
    ) {
      AboutView()
    }
    .alert(
      "Wisp",
      isPresented: Binding(
        get: { controller.toast != nil },
        set: {
          if !$0 {
            controller.toast = nil
          }
        }
      )
    ) {
      Button("OK", role: .cancel) { controller.toast = nil }
    } message: {
      Text(controller.toast ?? "")
    }
  }

  // MARK: - Chrome

  private var chrome: some View {
    HStack(spacing: 8) {
      Text("Wisp")
        .font(.headline)
      stateTag
      Spacer()
    }
    .padding(.horizontal, 16)
    .padding(.vertical, 10)
  }

  private var stateTag: some View {
    let (label, color): (String, Color) =
      if model.hasPendingPersistence {
        ("Unsaved", .orange)
      } else if model.state.isActive {
        ("REC", .red)
      } else {
        ("idle", .secondary)
      }
    return TagView(label: label, color: color)
  }

  // MARK: - Sidebar

  private var sidebar: some View {
    VStack(alignment: .leading, spacing: 0) {
      VStack(alignment: .leading, spacing: 2) {
        Text("Wisp").font(.subheadline.weight(.semibold))
        Text("on-device").font(.caption).foregroundStyle(.secondary)
      }
      .padding(.horizontal, 16)
      .padding(.top, 12)
      List {
        Section("Menu") {
          SidebarRow(
            title: "New Session",
            systemImage: "mic",
            active: model.view == .live
          ) { model.showNewSession() }
          SidebarRow(
            title: "Session History",
            systemImage: "clock.arrow.circlepath",
            active: isLibraryActive
          ) { model.showLibrary() }
          SidebarRow(
            title: "Settings",
            systemImage: "gearshape",
            active: model.view == .settings
          ) { model.showSettings() }
        }
      }
      .listStyle(.sidebar)
      Spacer(minLength: 0)
      Divider()
      HStack(spacing: 8) {
        Text("On-device · private by default")
          .font(.caption2)
          .foregroundStyle(.secondary)
        Spacer(minLength: 0)
      }
      .padding(.horizontal, 12)
      .padding(.vertical, 8)
    }
  }

  private var isLibraryActive: Bool {
    if case .history = model.view {
      return true
    }
    return model.view == .library
  }

  // MARK: - Content

  @ViewBuilder
  private var content: some View {
    if model.view == .settings {
      SettingsView(model: model, controller: controller)
    } else if !model.setupComplete {
      OnboardingView(model: model, controller: controller)
    } else {
      switch model.view {
      case .library:
        LibraryView(model: model, controller: controller)
      case .live:
        LiveView(model: model, controller: controller)
      case .history:
        HistoryView(model: model, controller: controller)
      case .settings:
        SettingsView(model: model, controller: controller)
      }
    }
  }

  // MARK: - Status strip

  @ViewBuilder
  private var statusStrip: some View {
    if let error = model.lastError {
      StatusStrip(
        title: "The last action failed.",
        detail: error.description,
        color: .red
      ) {
        Button("Dismiss") { model.lastError = nil }
          .controlSize(.small)
      }
    } else if model.hasPendingPersistence {
      StatusStrip(
        title: "A session still needs to be saved.",
        detail: "Press ⌘R to retry.",
        color: .orange
      ) {
        Button("Retry Saving") { controller.toggleRecording() }
          .controlSize(.small)
      }
    } else if model.state.isActive, !model.permissions.speech.isGranted {
      StatusStrip(
        title: "Recording without transcription.",
        detail: "Speech Recognition is not granted. Audio is still being saved.",
        color: .orange
      ) {
        Button("Open Settings") { controller.openPermissionSettings(.speechRecognition) }
          .controlSize(.small)
      }
    }
  }

  // MARK: - Meeting prompt

  @ViewBuilder
  private var meetingPrompt: some View {
    if !model.state.isActive, let prompt = model.meetingPrompt {
      StatusStrip(
        title: prompt.tool == .browserMeeting
          ? "A browser is using your microphone."
          : "\(prompt.tool.label) is using your microphone.",
        detail: "Record this meeting and transcribe it on this Mac.",
        color: .accentColor
      ) {
        Button("Not now") { model.meetingPrompt = nil }
          .controlSize(.small)
        Button("Record") {
          controller.toggleRecording()
        }
        .controlSize(.small)
        .buttonStyle(.borderedProminent)
      }
    }
  }
}

// MARK: - Shared chrome pieces

struct StatusStrip<Actions: View>: View {
  let title: String
  let detail: String
  let color: Color
  @ViewBuilder let actions: () -> Actions

  var body: some View {
    HStack(alignment: .center, spacing: 12) {
      RoundedRectangle(cornerRadius: 2)
        .fill(color)
        .frame(width: 3, height: 28)
      VStack(alignment: .leading, spacing: 2) {
        Text(title).font(.subheadline.weight(.semibold))
        Text(detail).font(.caption).foregroundStyle(.secondary)
      }
      Spacer(minLength: 0)
      actions()
    }
    .padding(.horizontal, 16)
    .padding(.vertical, 8)
    .background(Color(nsColor: .controlBackgroundColor))
    .overlay(alignment: .bottom) { Divider() }
  }
}

struct TagView: View {
  let label: String
  let color: Color

  var body: some View {
    Text(label)
      .font(.caption2)
      .padding(.horizontal, 7)
      .padding(.vertical, 2)
      .background(color.opacity(0.16), in: Capsule())
      .foregroundStyle(color)
  }
}

private struct SidebarRow: View {
  let title: String
  let systemImage: String
  let active: Bool
  let action: () -> Void

  var body: some View {
    Button(action: action) {
      Label(title, systemImage: systemImage)
        .frame(maxWidth: .infinity, alignment: .leading)
        .contentShape(Rectangle())
    }
    .buttonStyle(.plain)
    .listRowBackground(
      active ? Color.accentColor.opacity(0.16) : Color.clear
    )
    .foregroundStyle(active ? Color.accentColor : Color.primary)
  }
}

/// Base styling for the content area below a screen header.
struct ScreenBody<Content: View>: View {
  @ViewBuilder let content: () -> Content

  var body: some View {
    content()
      .frame(maxWidth: .infinity, maxHeight: .infinity)
      .padding(.horizontal, 16)
      .padding(.vertical, 12)
  }
}

/// Base styling for a screen footer.
struct ScreenFooter: View {
  let leading: String
  let trailing: String?

  var body: some View {
    HStack {
      Text(leading)
      Spacer()
      if let trailing {
        Text(trailing)
      }
    }
    .font(.caption2)
    .foregroundStyle(.secondary)
    .padding(.horizontal, 16)
    .padding(.vertical, 6)
    .overlay(alignment: .top) { Divider() }
  }
}
