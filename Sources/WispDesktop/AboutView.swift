import SwiftUI

/// Small "About Wisp" sheet opened from the application menu.
struct AboutView: View {
  @Environment(\.dismiss) private var dismiss

  var body: some View {
    VStack(alignment: .leading, spacing: 10) {
      Text("Wisp").font(.title.weight(.semibold))
      Text("Version \(AppVersion.current)")
        .font(.callout)
        .foregroundStyle(.secondary)
      Text("WispAudioKit \(AppVersion.audioKit)")
        .font(.callout)
        .foregroundStyle(.secondary)
      Text(
        "On-device transcription on macOS 26. Recordings and transcripts stay on this Mac."
      )
      .font(.callout)
      .foregroundStyle(.secondary)
      .fixedSize(horizontal: false, vertical: true)
      Spacer(minLength: 0)
      HStack {
        Spacer()
        Button("OK") { dismiss() }
          .buttonStyle(.borderedProminent)
          .keyboardShortcut(.defaultAction)
      }
    }
    .padding(20)
    .frame(width: 360, height: 240, alignment: .topLeading)
  }
}
