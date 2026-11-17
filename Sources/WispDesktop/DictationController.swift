import AppKit
import WispAudioKit
import WispCore

/// Performs the effects the dictation reducer returns: capture ownership,
/// clipboard, HUD visibility, notices, and the persisted chord's handler.
@MainActor
final class DictationController {
  let model: DictationModel
  /// Read only for the recording phase the arbiter consults.
  private let app: AppModel
  /// Set by `SessionController` from settings, so the language has one owner.
  var language: LanguageCode
  private var hud: DictationHUD?
  private var shortcut: GlobalShortcut?
  private var capture: DictationCapture?
  private var noticeTimer: Timer?
  private var settleWait: Task<Void, Never>?
  /// Every capture callback goes through this one stream, so updates and
  /// failures reach the reducer in delivery order even though the engine
  /// calls back off the main thread.
  private var events: AsyncStream<Event>.Continuation?
  private var pump: Task<Void, Never>?

  /// The one ordered channel between `DictationCapture` and the reducer.
  fileprivate enum Event: Sendable {
    case update(DictationCapture.Update)
    case started
    case failure(DictationCaptureError)
  }

  init(app: AppModel, language: LanguageCode) {
    self.app = app
    self.language = language
    model = DictationModel()
  }

  /// Register the persisted chord once, from `applicationDidFinishLaunching`.
  func install(shortcut: DictationChord?) {
    let (stream, continuation) = AsyncStream<Event>.makeStream()
    events = continuation
    pump = Task { [weak self] in
      for await event in stream {
        guard let self else { return }
        self.pumpEvent(event)
      }
    }
    hud = DictationHUD(model: model)
    self.shortcut = GlobalShortcut { [weak self] in
      self?.toggle()
    }
    if let shortcut, let failure = self.shortcut?.register(shortcut) {
      model.reportShortcutFailure(failure.message)
    }
  }

  /// Settings path: try registration, then report. Returns the failure
  /// message.
  func apply(shortcut: DictationChord?) -> String? {
    guard let shortcut else {
      self.shortcut?.unregister()
      model.reportShortcutFailure(nil)
      cancel()
      return nil
    }
    guard let failure = self.shortcut?.register(shortcut) else {
      model.reportShortcutFailure(nil)
      return nil
    }
    // `register` already restored the previous chord, so only the message is
    // left to report.
    model.reportShortcutFailure(failure.message)
    return failure.message
  }

  /// The chord handler and the menu item.
  func toggle() {
    guard canStartCapture(.dictation, recording: app.state, dictation: model.state) else {
      showNotice(.microphoneBusy)
      return
    }
    // Applied synchronously, before any await, so a recording start cannot
    // interleave with the claim.
    performEffect(model.apply(.hotKeyPressed))
  }

  /// Drop the current run without delivering anything.
  func cancel() {
    guard model.state.holdsMicrophone else { return }
    performEffect(model.apply(.stopRequested(.discard)))
  }

  func showNotice(_ notice: DictationNotice) {
    model.showNotice(notice)
    hud?.show()
    scheduleNoticeTimeout()
  }

  func shutdown() {
    noticeTimer?.invalidate()
    noticeTimer = nil
    settleWait?.cancel()
    settleWait = nil
    if model.state.holdsMicrophone, let capture {
      Task { await capture.cancel() }
    }
    capture = nil
    events?.finish()
    events = nil
    pump?.cancel()
    pump = nil
    shortcut?.unregister()
    hud?.dismiss()
    model.settle()
  }

  // MARK: - Effects

  private func performEffect(_ effect: DictationEffect) {
    switch effect {
    case .beginCapture:
      beginCapture()
    case .stopCapture(let outcome):
      stopCapture(outcome)
    case .fail(let error):
      fail(error)
    case .settle(let outcome):
      settle(outcome)
    case .none:
      break
    }
  }

  /// Claim the machine for dictation. A refusal never prompts: the chord must
  /// not raise a TCC dialog over another app.
  private func beginCapture() {
    model.clearNotice()
    hud?.show()
    if !WispAudioKit.checkPermission(.microphone).isGranted {
      refuse(.microphoneDenied, notice: .permissionRequired("Microphone"))
      return
    }
    if !WispAudioKit.checkPermission(.speechRecognition).isGranted {
      refuse(.speechDenied, notice: .permissionRequired("Speech recognition"))
      return
    }
    guard let events else { return }
    let capture = DictationCapture(
      onUpdate: { update in
        events.yield(.update(update))
      },
      onFailure: { error in
        events.yield(.failure(error))
      }
    )
    self.capture = capture
    let locale = Locale(identifier: language.code)
    let eventStream = events
    Task {
      do {
        try await capture.start(locale: locale)
        eventStream.yield(.started)
      } catch let error as DictationCaptureError {
        eventStream.yield(.failure(error))
      } catch {
        eventStream.yield(.failure(.engine("\(error)")))
      }
    }
  }

  /// A start that cannot even be attempted: report once and return to idle.
  private func refuse(_ error: DictationCaptureError, notice: DictationNotice) {
    model.apply(.captureFailed(error))
    showNotice(notice)
  }

  private func pumpEvent(_ event: Event) {
    switch event {
    case .update(let update):
      model.ingest(update)
    case .started:
      performEffect(model.apply(.captureStarted))
    case .failure(let error):
      performEffect(model.apply(.captureFailed(error)))
    }
  }

  private func stopCapture(_ outcome: DictationOutcome) {
    noticeTimer?.invalidate()
    noticeTimer = nil
    guard let capture else {
      // Nothing to drain; skip straight to the settle so the state cannot
      // wedge in `.finishing`.
      applyCaptureFinished()
      return
    }
    settleWait = Task { [weak self] in
      // `finish` gives the analyzer a moment to deliver its last finals; two
      // seconds is the bound, after which cancel keeps the state moving.
      let finished = await withTaskGroup(of: Bool.self) { group in
        group.addTask {
          await capture.finish()
          return true
        }
        group.addTask {
          try? await Task.sleep(for: .seconds(2))
          return false
        }
        let finished = await group.next() ?? false
        group.cancelAll()
        return finished
      }
      if !finished {
        await capture.cancel()
      }
      self?.applyCaptureFinished()
    }
  }

  private func applyCaptureFinished() {
    settleWait = nil
    performEffect(model.apply(.captureFinished))
  }

  /// Copy the transcript and close the run down.
  private func settle(_ outcome: DictationOutcome) {
    noticeTimer?.invalidate()
    noticeTimer = nil
    let text = outcome == .deliver ? dictationText(model.segments) : ""
    // Settle clears the rows and the notice together, so the outcome message
    // goes up afterwards; the other order leaves the HUD on the empty
    // placeholder until the timer fires.
    model.settle()
    if outcome == .discard { return }
    if text.isEmpty {
      showNotice(.nothingRecognized)
    } else {
      let pasteboard = NSPasteboard.general
      pasteboard.clearContents()
      pasteboard.setString(text, forType: .string)
      showNotice(.copied)
    }
  }

  /// Salvage: the text gathered so far still reaches the clipboard on a
  /// mid-flight failure.
  private func fail(_ error: DictationCaptureError) {
    let text = dictationText(model.segments)
    model.settle()
    if text.isEmpty {
      showNotice(.failed(error.description))
    } else {
      let pasteboard = NSPasteboard.general
      pasteboard.clearContents()
      pasteboard.setString(text, forType: .string)
      showNotice(.copiedAfterFailure)
    }
  }

  private func scheduleNoticeTimeout() {
    noticeTimer?.invalidate()
    let timer = Timer(timeInterval: 2.0, repeats: false) { [weak self] _ in
      MainActor.assumeIsolated {
        self?.expireNotice()
      }
    }
    RunLoop.main.add(timer, forMode: .common)
    noticeTimer = timer
  }

  private func expireNotice() {
    noticeTimer = nil
    // A live run owns the HUD; only an idle machine dismisses on a timer.
    guard model.state == .idle else { return }
    model.clearNotice()
    hud?.dismiss()
  }
}
