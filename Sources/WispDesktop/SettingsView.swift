import SwiftUI
import WispCore

/// Preferences: appearance, transcription language, permissions, and per-tool
/// meeting detection. Every change saves immediately.
struct SettingsView: View {
  @Bindable var model: AppModel
  let controller: SessionController

  var body: some View {
    VStack(spacing: 0) {
      header
      Divider()
      ScrollView {
        VStack(alignment: .leading, spacing: 24) {
          appearanceSection
          transcriptionSection
          permissionsSection
          meetingSection
          Text(
            "Detection reads app bundle identifiers from Core Audio. Audio is captured "
              + "only by a recording you start or auto-record."
          )
          .font(.caption)
          .foregroundStyle(.secondary)
          .fixedSize(horizontal: false, vertical: true)
        }
        .frame(maxWidth: 760, alignment: .leading)
        .padding(24)
        .frame(maxWidth: .infinity)
      }
      ScreenFooter(leading: "stored locally as settings.json", trailing: nil)
    }
  }

  private var header: some View {
    HStack(spacing: 12) {
      Button {
        model.closeSettings()
      } label: {
        Label("Back", systemImage: "chevron.left")
      }
      .controlSize(.small)
      .help("Back to the previous screen")
      Text("Settings").font(.title2.weight(.semibold))
      Spacer(minLength: 0)
    }
    .padding(.horizontal, 16)
    .padding(.vertical, 12)
  }

  // MARK: - Sections

  private var appearanceSection: some View {
    Section(title: "Appearance") {
      Card {
        Row(detail: "Follow the macOS setting, or pin light or dark.", last: true) {
          Picker(
            "",
            selection: Binding(
              get: { controller.settings.theme },
              set: { controller.setTheme($0) }
            )
          ) {
            Text("Follow System").tag(ThemePreference.system)
            Text("Light").tag(ThemePreference.light)
            Text("Dark").tag(ThemePreference.dark)
          }
          .pickerStyle(.segmented)
          .labelsHidden()
          .accessibilityLabel("Appearance")
          .frame(width: 280)
        }
      }
    }
  }

  private var transcriptionSection: some View {
    Section(title: "Transcription") {
      Card {
        Row(title: "Language", detail: languageDetail, last: true) {
          Picker(
            "",
            selection: Binding(
              get: { controller.settings.transcriptionLanguage.code },
              set: { controller.setLanguage($0) }
            )
          ) {
            ForEach(LanguageCode.known, id: \.code) { language in
              Text(language.label).tag(language.code)
            }
            // An unknown stored code keeps its own row so opening the picker
            // does not silently rewrite it to the first known language.
            if !controller.settings.transcriptionLanguage.isKnown {
              Text(controller.settings.transcriptionLanguage.code)
                .tag(controller.settings.transcriptionLanguage.code)
            }
          }
          .labelsHidden()
          .frame(width: 240)
        }
      }
    }
  }

  private var languageDetail: String {
    let language = controller.settings.transcriptionLanguage
    if language.isKnown {
      return "The on-device model checks support when a recording starts."
    }
    return
      "\(language.code) is not in this build's language list. Pick one before your next recording."
  }

  private var permissionsSection: some View {
    Section(title: "Permissions") {
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
    }
  }

  private var meetingSection: some View {
    Section(title: "Meeting detection") {
      Card {
        Row(
          title: "Detect meeting apps",
          detail:
            "Watch which apps hold the microphone and offer to record. Off by default.",
          last: false
        ) {
          Toggle(
            "Detect meeting apps",
            isOn: Binding(
              get: { controller.settings.meetings.enabled },
              set: { controller.setMeetingDetectionEnabled($0) }
            )
          )
          .labelsHidden()
          .toggleStyle(.switch)
        }
        ForEach(Array(MeetingTool.allCases.enumerated()), id: \.element) { index, tool in
          let policy = controller.settings.meetings.policy(for: tool)
          Row(
            title: tool.label,
            detail: policyDetail(policy),
            last: index == MeetingTool.allCases.count - 1
          ) {
            Picker(
              "",
              selection: Binding(
                get: { policy },
                set: { controller.setMeetingPolicy(tool, $0) }
              )
            ) {
              ForEach(MeetingPolicy.allCases, id: \.self) { candidate in
                Text(candidate.label).tag(candidate)
              }
            }
            .pickerStyle(.segmented)
            .labelsHidden()
            .frame(width: 280)
            .disabled(!controller.settings.meetings.enabled)
          }
        }
      }
    }
  }

  private func policyDetail(_ policy: MeetingPolicy) -> String? {
    policy == .autoRecord ? "Recording starts on its own." : nil
  }
}

// MARK: - Layout helpers

private struct Section<Content: View>: View {
  let title: String
  @ViewBuilder let content: () -> Content

  var body: some View {
    VStack(alignment: .leading, spacing: 8) {
      Text(title).font(.subheadline.weight(.semibold))
      content()
    }
  }
}

private struct Card<Content: View>: View {
  @ViewBuilder let content: () -> Content

  var body: some View {
    VStack(spacing: 0) {
      content()
    }
    .frame(maxWidth: .infinity, alignment: .leading)
    .background(Color(nsColor: .controlBackgroundColor), in: RoundedRectangle(cornerRadius: 10))
    .overlay(
      RoundedRectangle(cornerRadius: 10)
        .strokeBorder(Color(nsColor: .separatorColor), lineWidth: 1)
    )
  }
}

private struct Row<Control: View>: View {
  var title: String?
  var detail: String?
  let last: Bool
  @ViewBuilder let control: () -> Control

  var body: some View {
    HStack(spacing: 16) {
      VStack(alignment: .leading, spacing: 2) {
        if let title {
          Text(title).font(.callout.weight(.semibold))
        }
        if let detail {
          Text(detail)
            .font(.caption)
            .foregroundStyle(.secondary)
            .fixedSize(horizontal: false, vertical: true)
        }
      }
      Spacer(minLength: 8)
      control()
    }
    .padding(.horizontal, 14)
    .padding(.vertical, 10)
    .frame(minHeight: 56)
    .overlay(alignment: .bottom) {
      if !last {
        Divider()
      }
    }
  }
}
