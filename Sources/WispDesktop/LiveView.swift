import SwiftUI
import WispLifecycle

/// The recording screen: title, recorder controls, and the streaming
/// mic/system transcript.
struct LiveView: View {
  @Bindable var model: AppModel
  let controller: SessionController

  private var isRecording: Bool {
    model.state == .recording
  }

  private var isBusy: Bool {
    model.state == .starting || model.state == .stopping
  }

  private var titleEditable: Bool {
    model.state == .idle || model.state == .recording
  }

  private var phaseLabel: String {
    if model.hasPendingPersistence {
      return "Unsaved"
    }
    return switch model.state {
    case .idle: "Ready"
    case .starting: "Starting…"
    case .recording: "Recording"
    case .stopping: "Finishing…"
    case .failed: "Failed"
    }
  }

  private var phaseColor: Color {
    if model.hasPendingPersistence || model.state == .recording || model.state == .failed {
      return .red
    }
    if model.state == .starting || model.state == .stopping {
      return .accentColor
    }
    return .secondary
  }

  var body: some View {
    VStack(spacing: 0) {
      header
      Divider()
      ScreenBody {
        TranscriptView(segments: model.segments, follows: true)
      }
      ScreenFooter(
        leading: "Transcribed on-device · mic and system audio are kept as separate tracks",
        trailing: "⌘R to start / stop"
      )
    }
  }

  private var header: some View {
    HStack(spacing: 12) {
      TextField(
        "Untitled session",
        text: Binding(
          get: { model.liveTitle },
          set: { controller.setLiveTitle($0) }
        )
      )
      .textFieldStyle(.roundedBorder)
      .frame(maxWidth: 420)
      .disabled(!titleEditable)

      HStack(spacing: 12) {
        if isRecording {
          Circle().fill(Color.red).frame(width: 8, height: 8)
        }
        TagView(label: phaseLabel, color: phaseColor)
        Text(Format.clock(totalMilliseconds: model.elapsedMilliseconds))
          .font(.system(.title3, design: .monospaced))
          .monospacedDigit()
        recordButton
        if isRecording {
          Button {
            controller.toggleMute()
          } label: {
            Label(
              model.microphoneMuted ? "Unmute" : "Mute",
              systemImage: "mic"
            )
          }
          .buttonStyle(.bordered)
          .help(model.microphoneMuted ? "Unmute microphone" : "Mute microphone")
        }
      }
      Spacer(minLength: 0)
    }
    .padding(.horizontal, 16)
    .padding(.vertical, 12)
  }

  @ViewBuilder
  private var recordButton: some View {
    let label =
      switch model.state {
      case .failed: "Retry"
      case .recording: "Stop"
      case .starting, .stopping: "…"
      case .idle: "Record"
      }
    let icon =
      switch model.state {
      case .failed: "arrow.clockwise"
      case .recording: "stop.fill"
      default: "mic"
      }
    let button = Button {
      controller.toggleRecording()
    } label: {
      HStack(spacing: 6) {
        if isBusy {
          ProgressView().controlSize(.small)
        } else {
          Image(systemName: icon)
        }
        Text(label)
      }
    }
    .disabled(isBusy)
    .help(
      model.state == .idle || model.state == .failed
        ? "Start recording (⌘R)"
        : "Recording controls (⌘R)")

    if isRecording {
      button.buttonStyle(.bordered)
    } else {
      button.buttonStyle(.borderedProminent)
    }
  }
}
