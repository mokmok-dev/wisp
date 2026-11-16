import AppKit
import SwiftUI
import WispCore

/// Menu bar commands and their keyboard shortcuts.
///
/// The recording item's label flips with the session state so the menu always
/// names what ⌘R will do. Copy and export are silent no-ops on the library
/// screen, and report an empty transcript when one is showing but has no text.
struct AppCommands: Commands {
  @Bindable var model: AppModel
  let controller: SessionController

  var body: some Commands {
    CommandGroup(replacing: .appInfo) {
      Button("About Wisp") { controller.aboutRequested = true }
    }

    CommandGroup(replacing: .appSettings) {
      Button("Settings…") { model.showSettings() }
        .keyboardShortcut(",", modifiers: .command)
    }

    CommandGroup(after: .appSettings) {
      Divider()
      Button(recordingLabel) { controller.toggleRecording() }
        .keyboardShortcut("r", modifiers: .command)
      Divider()
      Button("Copy Transcript") { controller.copyTranscript() }
        .keyboardShortcut("c", modifiers: [.command, .shift])
      Button("Export Transcript…") { controller.exportTranscript() }
        .keyboardShortcut("e", modifiers: [.command, .shift])
    }

    CommandGroup(replacing: .appTermination) {
      Button("Quit Wisp") { NSApp.terminate(nil) }
        .keyboardShortcut("q", modifiers: .command)
    }

    CommandMenu("Appearance") {
      Button("Toggle Light/Dark Appearance") { controller.toggleTheme() }
        .keyboardShortcut("d", modifiers: [.command, .shift])
      Divider()
      appearanceRow("Follow System", .system)
      appearanceRow("Light", .light)
      appearanceRow("Dark", .dark)
    }
  }

  /// One appearance row. A `Toggle` is not used: clicking the checked row
  /// reports `false`, which would clear the checkmark with nothing to restore
  /// it. A button with a manual checkmark always performs the action, so the
  /// mark only ever reflects the stored preference.
  private func appearanceRow(_ title: String, _ preference: ThemePreference) -> some View {
    Button {
      controller.setTheme(preference)
    } label: {
      if controller.settings.theme == preference {
        Label(title, systemImage: "checkmark")
      } else {
        Text(title)
      }
    }
  }

  /// "Stop" while a session is live or transitioning, "Start" otherwise, and
  /// the retry label when a transcript still needs saving.
  private var recordingLabel: String {
    if model.hasPendingPersistence {
      return "Retry Saving Session"
    }
    switch model.state {
    case .starting, .recording, .stopping: return "Stop Recording"
    case .idle, .failed: return "Start Recording"
    }
  }
}
