import AppKit
import Foundation
import Observation
import WispAudioKit
import WispCore
import WispLifecycle
import WispStorage

/// Owns the pieces the screens and menus drive: the runner, persistence,
/// permissions, meeting detection, and the persisted preferences.
///
/// `AppModel` stays the single source of truth for what is on screen; this type
/// performs the side effects and writes results back through it.
@MainActor
@Observable
final class SessionController {
  /// Cadence of the pump that drains worker updates. 33 ms is one frame at
  /// 30 fps, well under the threshold where a button press feels late.
  static let pumpInterval = Duration.milliseconds(33)
  /// Re-read permission state on this interval so a change the user made in
  /// System Settings shows up without a relaunch.
  static let permissionRefreshInterval = Duration.milliseconds(1500)
  /// How long quit waits for the worker to reach a terminal update.
  static let quitGracePeriod = Duration.seconds(5)

  let model: AppModel
  let storage: Storage
  let recordingsRoot: URL
  private let settingsStore: SettingsStore
  private let runner: SessionRunner
  private let ingest: SessionIngest
  private let meeting: MeetingWatch
  private(set) var dictation: DictationController

  private(set) var settings: Settings
  private var pumpTask: Task<Void, Never>?
  private var permissionTask: Task<Void, Never>?

  /// Transient menu feedback, shown as an alert by the window.
  var toast: String?
  /// Set by the About menu item and consumed by the window's sheet.
  var aboutRequested = false

  init(
    dataDirectory: URL,
    storage: Storage,
    settingsStore: SettingsStore,
    settings: Settings
  ) {
    let model = AppModel()
    model.permissions = Permissions.current()
    self.model = model
    self.storage = storage
    self.settingsStore = settingsStore
    self.settings = settings
    let recordingsRoot = WispPaths.recordingsDirectory(in: dataDirectory)
    self.recordingsRoot = recordingsRoot
    runner = SessionRunner()
    ingest = SessionIngest(
      model: model,
      storage: storage,
      recordingsRoot: recordingsRoot
    )
    meeting = MeetingWatch(preferences: settings.meetings)
    dictation = DictationController(app: model, language: settings.transcriptionLanguage)

    // Recovery and the first library read run before any window exists, so
    // the menu and ⌘R cannot start a recording into a snapshot this call is
    // about to replace. `start()` handles only the async loops.
    _ = ingest.recoverPendingSessions()
    ingest.refreshLibrary()
  }

  /// Start the background loops. Recovery and the first library read already
  /// ran in `init`.
  func start() async {
    await meeting.start()
    startPump()
    startPermissionRefresh()
  }

  // MARK: - Pump

  private func startPump() {
    guard pumpTask == nil else { return }
    pumpTask = Task { [weak self] in
      while !Task.isCancelled {
        try? await Task.sleep(for: Self.pumpInterval)
        guard let self else { return }
        drain()
        model.tickClock()
        await ingestMeetingEvents()
      }
    }
  }

  private func drain() {
    let updates = runner.drainUpdates()
    guard !updates.isEmpty else { return }
    for update in updates {
      ingest.applyUpdate(update)
    }
  }

  private func ingestMeetingEvents() async {
    let events = await meeting.drainEvents()
    guard !events.isEmpty else { return }
    for event in events {
      apply(event)
    }
  }

  private func apply(_ event: MeetingEvent) {
    switch event {
    case .detected(let tool, let autoRecord):
      guard !model.state.isActive, model.setupComplete else { return }
      if autoRecord {
        startMeetingRecording(tool)
        return
      }
      model.meetingPrompt = MeetingPrompt(tool: tool)
    case .ended(let tool):
      if model.meetingPrompt?.tool == tool {
        model.meetingPrompt = nil
      }
    case .samplerUnavailable:
      wispLog("meeting detection: microphone activity is unavailable")
    }
  }

  // MARK: - Recording

  /// The menu/⌘R action: start, stop, or retry an unsaved transcript.
  func toggleRecording() {
    if model.hasPendingPersistence {
      if ingest.retryPendingPersistence() {
        _ = ingest.recoverPendingSessions()
        ingest.refreshLibrary()
      }
      return
    }
    switch model.state {
    case .idle, .failed:
      guard model.setupComplete else { return }
      startRecording(title: model.liveTitle)
    case .recording:
      model.setState(model.state.requestingStop)
      runner.stop()
    case .starting, .stopping:
      // A transition is in flight; taps are ignored until it lands.
      break
    }
  }

  /// Start a recording for a detected call. Unlike `toggleRecording` this
  /// never stops a session that is already running.
  private func startMeetingRecording(_ tool: MeetingTool) {
    guard !model.hasPendingPersistence,
      model.state == .idle || model.state == .failed,
      model.setupComplete
    else {
      return
    }
    startRecording(title: tool.label)
  }

  private func startRecording(title: String) {
    // A run that holds the microphone is a dictation run; refusing here
    // reports back through the HUD rather than failing the recording.
    guard
      canStartCapture(.recording, recording: model.state, dictation: dictation.model.state)
    else {
      dictation.showNotice(.microphoneBusy)
      return
    }
    let startedAt = Date()
    let dirName = Library.sessionDirectoryName(startedAt: startedAt)
    let outputDir = recordingsRoot.appendingPathComponent(dirName, isDirectory: true)

    let sessionID: SessionID
    do {
      sessionID = try Library.createSession(
        storage,
        startedAt: startedAt,
        dirName: dirName,
        title: title
      )
    } catch {
      // Normalize to a fresh live view so the failure has somewhere to show.
      _ = model.beginSessionStart()
      model.fail(.persistence("could not create the session record: \(error)"))
      return
    }

    guard model.beginSessionStart() else {
      try? storage.sessions.delete(sessionID)
      return
    }
    model.currentSessionID = sessionID
    model.linkedSessionID = sessionID
    model.currentSessionStartedAt = startedAt
    model.currentSessionDirName = dirName
    model.currentOutputDir = outputDir
    model.liveTitle = title

    let configuration = SessionConfiguration(
      locale: settings.transcriptionLanguage.code,
      transcriptionEnabled: model.permissions.speech.isGranted
    )
    let accepted = runner.start(
      outputDir: outputDir,
      configuration: configuration,
      start: SessionStart(sessionID: sessionID, startedAt: startedAt, dirName: dirName)
    )
    if !accepted {
      abandonUnstartedRecording(sessionID)
    }
  }

  /// The runner refused the start, so remove the row it never recorded into.
  private func abandonUnstartedRecording(_ sessionID: SessionID) {
    guard model.currentSessionID == sessionID else { return }
    model.pendingSessionWrite = .delete
    if ingest.retryPendingPersistence() {
      model.fail(.audio(.start("session runner is no longer available")))
    }
  }

  func toggleMute() {
    let muted = !model.microphoneMuted
    runner.setMicrophoneMuted(muted)
    if model.state == .recording {
      model.microphoneMuted = muted
    }
  }

  // MARK: - Library

  func openHistory(_ sessionID: SessionID) {
    ingest.openHistory(sessionID)
  }

  func setLibraryQuery(_ query: String) {
    ingest.searchLibrary(query)
  }

  func clearLibrarySearch() {
    ingest.searchLibrary("")
  }

  func confirmDelete(_ sessionID: SessionID) {
    ingest.deleteSession(sessionID)
  }

  func setLiveTitle(_ title: String) {
    ingest.updateLiveTitle(title)
  }

  func renameViewedSession(_ title: String) {
    guard case .history(let sessionID) = model.view, !title.isEmpty else { return }
    ingest.renameSession(sessionID, title: title)
  }

  // MARK: - Transcript actions

  /// Copy the transcript shown in the live or history screen.
  func copyTranscript() {
    guard transcriptIsVisible else { return }
    let copied = TranscriptExport.copyToClipboard(segments: model.segments)
    toast =
      copied
      ? "Transcript copied to the clipboard."
      : "There is no transcript to copy yet."
  }

  /// Export the transcript shown in the live or history screen as Markdown.
  func exportTranscript() {
    guard transcriptIsVisible else { return }
    let session = model.viewedSession
    let text = TranscriptExport.formatMarkdown(session: session, segments: model.segments)
    guard !text.isEmpty else {
      toast = "There is no transcript to export yet."
      return
    }
    let name = TranscriptExport.suggestedExportName(
      title: session?.title,
      fallback: "transcript"
    )
    let started = TranscriptExport.export(
      text: text,
      suggestedName: name,
      onError: { [weak self] message in
        self?.toast = "The transcript could not be exported: \(message)"
      }
    )
    toast =
      started
      ? "Choose where to save the Markdown file."
      : "The transcript could not be exported."
  }

  /// Copy and export only act on a transcript the user can see.
  private var transcriptIsVisible: Bool {
    switch model.view {
    case .live, .history, .settings: true
    case .library: false
    }
  }

  // MARK: - Preferences

  /// Re-read both permission statuses, keeping any request still in flight.
  func refreshPermissions() {
    let current = Permissions.current()
    guard
      current.microphone != model.permissions.microphone
        || current.speech != model.permissions.speech
    else {
      return
    }
    model.permissions.microphone = current.microphone
    model.permissions.speech = current.speech
  }

  /// Ask the OS for a permission. The dialog blocks until the user answers, so
  /// the request runs off the main actor and the result comes back here.
  func requestPermission(_ permission: Permission) {
    guard model.permissions.pending == nil else { return }
    model.permissions.pending = permission
    Task { [weak self] in
      let status = await Task.detached { await WispAudioKit.requestPermission(permission) }
        .value
      guard let self else { return }
      model.permissions.setStatus(permission, status)
      model.permissions.pending = nil
    }
  }

  /// Open the System Settings pane for a permission the OS will not re-prompt.
  func openPermissionSettings(_ permission: Permission) {
    let url =
      switch permission {
      case .microphone:
        "x-apple.systempreferences:com.apple.preference.security?Privacy_Microphone"
      case .speechRecognition:
        "x-apple.systempreferences:com.apple.preference.security?Privacy_SpeechRecognition"
      }
    guard let target = URL(string: url) else { return }
    NSWorkspace.shared.open(target)
  }

  func setTheme(_ preference: ThemePreference) {
    guard settings.theme != preference else { return }
    settings.theme = preference
    saveSettings()
    applyTheme()
  }

  /// Follow the OS appearance (nil) or pin one, without a restart.
  func applyTheme() {
    switch settings.theme {
    case .system: NSApp.appearance = nil
    case .light: NSApp.appearance = NSAppearance(named: .aqua)
    case .dark: NSApp.appearance = NSAppearance(named: .darkAqua)
    }
  }

  func setLanguage(_ code: String) {
    let language = LanguageCode(code)
    guard settings.transcriptionLanguage != language else { return }
    settings.transcriptionLanguage = language
    dictation.language = language
    saveSettings()
  }

  func setMeetingDetectionEnabled(_ enabled: Bool) {
    guard settings.meetings.enabled != enabled else { return }
    settings.meetings.enabled = enabled
    saveSettings()
    pushMeetingPreferences()
    if !enabled, model.meetingPrompt != nil {
      model.meetingPrompt = nil
    }
  }

  func setMeetingPolicy(_ tool: MeetingTool, _ policy: MeetingPolicy) {
    guard settings.meetings.setPolicy(policy, for: tool) else { return }
    saveSettings()
    pushMeetingPreferences()
    if policy == .off, model.meetingPrompt?.tool == tool {
      model.meetingPrompt = nil
    }
  }

  // MARK: - Dictation

  /// Register the persisted chord once, from `applicationDidFinishLaunching`.
  func installDictation() {
    dictation.install(shortcut: settings.dictationShortcut)
  }

  /// Register first, persist only on success: a chord the OS refused must
  /// never become the stored default.
  func setDictationShortcut(_ chord: DictationChord?) {
    if let message = dictation.apply(shortcut: chord) {
      dictation.model.reportShortcutFailure(message)
      return
    }
    settings.dictationShortcut = chord
    saveSettings()
  }

  func toggleDictation() {
    dictation.toggle()
  }

  private func pushMeetingPreferences() {
    let preferences = settings.meetings
    Task { [meeting] in
      await meeting.setPreferences(preferences)
    }
  }

  private func saveSettings() {
    do {
      try settingsStore.save(settings)
    } catch {
      model.lastError = .persistence("could not save settings: \(error)")
    }
  }

  // MARK: - Permissions loop

  private func startPermissionRefresh() {
    guard permissionTask == nil else { return }
    permissionTask = Task { [weak self] in
      while !Task.isCancelled {
        try? await Task.sleep(for: Self.permissionRefreshInterval)
        guard let self else { return }
        refreshPermissions()
      }
    }
  }

  // MARK: - Termination

  /// Stop the worker and make the transcript durable before the app exits.
  ///
  /// Returns false when quit must be cancelled because the transcript is
  /// neither persisted nor snapshot-durable while the worker is still active.
  /// This is the data-loss guard for the whole app.
  func prepareForTermination() -> Bool {
    if model.hasPendingPersistence {
      return ingest.retryPendingPersistence()
    }

    // A dictation run owns the microphone and nothing else; dropping it
    // first keeps the recording path below from waiting on a claim that
    // quit is about to dissolve anyway.
    if dictation.model.state.holdsMicrophone {
      dictation.cancel()
    }

    if model.state == .recording || model.state == .starting {
      runner.stop()
      model.setState(model.state.requestingStop)
    }

    let sessionID: SessionID? =
      switch model.state {
      case .recording, .starting, .stopping: model.currentSessionID
      case .idle, .failed: nil
      }
    if let sessionID {
      let updates = runner.waitForIdleSynchronously(
        sessionID: sessionID,
        timeout: Self.quitGracePeriod
      )
      for update in updates {
        ingest.applyUpdate(update)
      }
    }

    // A stopped transcript whose transaction rolled back gets one immediate
    // retry; the window still exposes Retry if the user stays.
    if model.hasPendingPersistence, ingest.retryPendingPersistence() {
      return true
    }
    if !model.hasUnsettledSession {
      return true
    }

    let workerIsStillActive = model.state.isActive
    if workerIsStillActive, model.pendingSessionWrite == nil {
      model.pendingSessionWrite = .finalise(endedAt: Date())
    }
    var recoveryIsDurable = true
    do {
      try ingest.writeRecoverySnapshot()
    } catch {
      recoveryIsDurable = false
      model.lastError = .persistence(
        "cannot quit safely because the recovery transcript could not be saved: \(error)"
      )
    }
    // A snapshot is only sufficient once the worker has stopped: if the
    // stop barrier timed out, later native callbacks would be lost.
    return !workerIsStillActive && recoveryIsDurable
  }

  /// Stop the background loops. The worker itself keeps running so a quit can
  /// still drain it.
  func shutdown() {
    pumpTask?.cancel()
    pumpTask = nil
    permissionTask?.cancel()
    permissionTask = nil
    dictation.shutdown()
    runner.shutdown()
    Task { [meeting] in await meeting.shutdown() }
  }
}
