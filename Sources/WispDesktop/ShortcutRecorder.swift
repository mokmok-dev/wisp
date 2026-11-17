import AppKit
import Carbon.HIToolbox
import SwiftUI
import WispCore

/// The dictation chord row in Settings. Clicking enters capture mode: the
/// next keystroke becomes the chord, Escape leaves the old one in place.
struct DictationShortcutField: View {
  let chord: DictationChord?
  let failure: String?
  let onChange: (DictationChord?) -> Void

  @State private var capturing = false
  /// Kept only while capturing, so the monitor outlives no key press.
  @State private var monitor: Any?

  var body: some View {
    VStack(alignment: .leading, spacing: 8) {
      HStack(spacing: 12) {
        Button {
          capturing ? stopCapturing() : startCapturing()
        } label: {
          Text(recorderLabel)
            .font(.system(size: 13).weight(.medium))
            .padding(.horizontal, 12)
            .padding(.vertical, 5)
            .background(
              Color(nsColor: .controlBackgroundColor),
              in: RoundedRectangle(cornerRadius: 6)
            )
            .overlay(
              RoundedRectangle(cornerRadius: 6)
                .strokeBorder(
                  capturing ? Color.accentColor : Color(nsColor: .separatorColor),
                  lineWidth: capturing ? 2 : 1
                )
            )
        }
        .buttonStyle(.plain)
        .accessibilityLabel("Dictation shortcut")
        .accessibilityValue(chord?.displayString ?? "Off")

        Button("Clear") {
          stopCapturing()
          onChange(nil)
        }
        .disabled(chord == nil)
      }
      if let failure {
        Text(failure)
          .font(.caption)
          .foregroundStyle(.red)
      }
    }
    .onAppear {
      if capturing {
        installMonitor()
      }
    }
    .onChange(of: capturing) { _, isCapturing in
      isCapturing ? installMonitor() : removeMonitor()
    }
    .onDisappear {
      removeMonitor()
    }
  }

  private var recorderLabel: String {
    capturing ? "Press keys…" : (chord?.displayString ?? "Off")
  }

  // MARK: - Capture mode

  private func startCapturing() {
    capturing = true
  }

  private func stopCapturing() {
    capturing = false
  }

  private func installMonitor() {
    guard monitor == nil else { return }
    monitor = NSEvent.addLocalMonitorForEvents(matching: .keyDown) { event in
      handleKey(event)
    }
  }

  private func removeMonitor() {
    if let monitor {
      NSEvent.removeMonitor(monitor)
      self.monitor = nil
    }
  }

  /// Parse the keystroke the recorder caught. The event is passed on when it
  /// is not the recorder's to consume.
  private func handleKey(_ event: NSEvent) -> NSEvent? {
    guard capturing, event.type == .keyDown else { return event }
    if event.keyCode == UInt16(kVK_Escape) {
      stopCapturing()
      return nil
    }
    // Device flags carry no chord meaning and would otherwise be part of a
    // normal press. Caps lock's state arrives in `.capsLock`.
    var flags = event.modifierFlags
    flags.subtract([.capsLock, .function, .numericPad])
    var modifiers = DictationChord.Modifiers()
    if flags.contains(.control) { modifiers.insert(.control) }
    if flags.contains(.option) { modifiers.insert(.option) }
    if flags.contains(.shift) { modifiers.insert(.shift) }
    if flags.contains(.command) { modifiers.insert(.command) }
    guard let chord = DictationChord(keyCode: event.keyCode, modifiers: modifiers) else {
      // Swallowing a bare letter would break ordinary typing, so the
      // keystroke is let through instead.
      return event
    }
    onChange(chord)
    stopCapturing()
    return nil
  }
}
