import AppKit
import SwiftUI

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
