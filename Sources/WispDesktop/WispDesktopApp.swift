import AppKit
import SwiftUI
import WispCore
import WispStorage

/// Entry point for the Wisp desktop app.
///
/// The data directory and the database are resolved before any window opens:
/// recording against an in-memory fallback would look successful and silently
/// discard the transcript at exit.
@main
struct WispDesktopApp: App {
  @NSApplicationDelegateAdaptor(AppDelegate.self) private var delegate

  init() {
    // Built before the first window so it opens on real data, not a
    // placeholder that swaps once the database loads.
    _ = AppEnvironment.shared
  }

  var body: some Scene {
    WindowGroup("Wisp") {
      if let controller = AppEnvironment.shared.controller {
        AppView(model: controller.model, controller: controller)
          .frame(minWidth: 900, minHeight: 640)
          .task { await AppEnvironment.shared.start() }
      } else {
        StartupFailureView(
          message: AppEnvironment.shared.startupFailure ?? "Wisp could not start."
        )
      }
    }
    .windowResizability(.contentMinSize)
    .commands {
      if let controller = AppEnvironment.shared.controller {
        AppCommands(model: controller.model, controller: controller)
      }
    }
  }
}

/// App-wide objects, built once so the first paint already has real data.
@MainActor
final class AppEnvironment {
  static let shared = AppEnvironment()

  private(set) var controller: SessionController?
  private(set) var startupFailure: String?
  private var hasStarted = false

  private init() {
    do {
      let dataDirectory = try WispPaths.dataDirectory()
      let storage = try Storage.open(root: dataDirectory)
      let settingsStore = SettingsStore(directory: dataDirectory)
      let controller = SessionController(
        dataDirectory: dataDirectory,
        storage: storage,
        settingsStore: settingsStore,
        settings: settingsStore.load()
      )
      self.controller = controller
    } catch {
      startupFailure =
        switch error {
        case WispPathsError.homeUnavailable:
          "\(error). Wisp needs a durable place to keep recordings."
        default:
          "Wisp cannot open its session database: \(error)"
        }
    }
  }

  /// Recover interrupted transcripts and start the background loops.
  func start() async {
    guard !hasStarted, let controller else { return }
    hasStarted = true
    await controller.start()
  }
}

/// Graceful quit: a live transcript is never dropped on the floor.
@MainActor
final class AppDelegate: NSObject, NSApplicationDelegate {
  func applicationDidFinishLaunching(_: Notification) {
    // NSApp exists only after launch, so the persisted appearance lands
    // here rather than while the environment is constructed.
    AppEnvironment.shared.controller?.applyTheme()
  }

  func applicationShouldTerminate(_: NSApplication) -> NSApplication.TerminateReply {
    guard let controller = AppEnvironment.shared.controller else { return .terminateNow }
    // Cancelling is the last resort: it happens only when neither the
    // database row nor a recovery snapshot holds the transcript and the
    // worker is still running.
    guard controller.prepareForTermination() else {
      // The app keeps running, so its background loops must too.
      return .terminateCancel
    }
    controller.shutdown()
    return .terminateNow
  }
}

/// Shown when the data directory or the database could not be opened.
private struct StartupFailureView: View {
  let message: String

  var body: some View {
    VStack(spacing: 12) {
      Image(systemName: "exclamationmark.triangle")
        .font(.system(size: 36, weight: .light))
      Text("Wisp cannot start").font(.headline)
      Text(message)
        .font(.callout)
        .foregroundStyle(.secondary)
        .multilineTextAlignment(.center)
        .fixedSize(horizontal: false, vertical: true)
      Button("Quit") { NSApp.terminate(nil) }
        .buttonStyle(.borderedProminent)
    }
    .padding(32)
    .frame(minWidth: 420, minHeight: 240)
  }
}
