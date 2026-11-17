@preconcurrency import AVFoundation
import Foundation
import Speech
import os.lock

/// Errors dictation capture reports to its owner.
public enum DictationCaptureError: Error, Equatable, Sendable, CustomStringConvertible {
  case microphoneDenied
  case speechDenied
  case unsupportedLocale(String)
  case unavailable(String)
  case engine(String)
  case analyzer(String)

  public var description: String {
    switch self {
    case .microphoneDenied:
      "Microphone permission is not granted"
    case .speechDenied:
      "Speech recognition permission is not granted"
    case .unsupportedLocale(let locale):
      "SpeechTranscriber does not support locale: \(locale)"
    case .unavailable(let message):
      "Dictation capture is unavailable: \(message)"
    case .engine(let message):
      "Dictation audio engine failure: \(message)"
    case .analyzer(let message):
      "Dictation SpeechAnalyzer failure: \(message)"
    }
  }
}

/// Microphone to one `SpeechAnalyzer`, with no recorder and no output directory.
///
/// Sendability follows `WispSession`: engine mutations serialise behind
/// `engineLock`, run ownership lives in a locked run state, and audio leaves
/// the tap through one preallocated `RealtimeAudioHandoff`. `start` is the
/// only path that spans awaits, so `finish` and `cancel` in its window only
/// mark a stop request it honours after every await.
public final class DictationCapture: @unchecked Sendable {
  public struct Update: Sendable, Equatable {
    public let segmentID: UInt64
    public let isFinal: Bool
    public let text: String
    public let startSeconds: Double
    public let endSeconds: Double
  }

  public typealias OnUpdate = @Sendable (Update) -> Void
  /// Terminal. Called at most once per run, on a non-main thread.
  public typealias OnFailure = @Sendable (DictationCaptureError) -> Void

  private let onUpdate: OnUpdate
  private let onFailure: OnFailure

  /// NotificationCenter removal is thread-safe, but the token has no
  /// Sendable conformance of its own to live inside the run state.
  private struct ConfigurationChangeToken: @unchecked Sendable {
    let observer: NSObjectProtocol
  }

  private enum Phase: Sendable {
    case idle
    case starting
    case running
    case finishing
  }

  private struct RunState: Sendable {
    var phase = Phase.idle
    var stopRequested = false
    var failureDelivered = false
    var engine: AVAudioEngine?
    var analysis: AnalyzerCoordinator?
    var handoff: RealtimeAudioHandoff?
    var streamContinuation: AsyncStream<AVAudioPCMBuffer>.Continuation?
    var consumer: Task<Void, Never>?
    var configObserver: ConfigurationChangeToken?
  }

  /// One claimed run's resources, harvested under the lock before teardown.
  private struct Teardown {
    let engine: AVAudioEngine
    let analysis: AnalyzerCoordinator
    let handoff: RealtimeAudioHandoff
    let streamContinuation: AsyncStream<AVAudioPCMBuffer>.Continuation?
    let consumer: Task<Void, Never>?
    let configObserver: ConfigurationChangeToken?
  }

  private let state = OSAllocatedUnfairLock(initialState: RunState())
  private let engineLock = OSAllocatedUnfairLock<Void>(initialState: ())

  public init(onUpdate: @escaping OnUpdate, onFailure: @escaping OnFailure) {
    self.onUpdate = onUpdate
    self.onFailure = onFailure
  }

  public func start(locale: Locale) async throws {
    // A chord press must never surface a TCC dialog over another app, so
    // undetermined counts as denied here.
    guard checkPermission(.microphone).isGranted else {
      throw DictationCaptureError.microphoneDenied
    }
    guard checkPermission(.speechRecognition).isGranted else {
      throw DictationCaptureError.speechDenied
    }
    let claimed = state.withLock { run in
      guard run.phase == .idle else { return false }
      run.phase = .starting
      run.stopRequested = false
      run.failureDelivered = false
      run.engine = nil
      run.analysis = nil
      run.handoff = nil
      run.streamContinuation = nil
      run.consumer = nil
      run.configObserver = nil
      return true
    }
    guard claimed else {
      throw DictationCaptureError.unavailable("a dictation run is already active")
    }
    try await performStart(locale: locale)
  }

  public func finish() async {
    await stop(cancelAnalyzer: false)
  }

  public func cancel() async {
    await stop(cancelAnalyzer: true)
  }

  private func performStart(locale: Locale) async throws {
    let engine = AVAudioEngine()
    let sourceFormat = engine.inputNode.outputFormat(forBus: 0)
    guard sourceFormat.sampleRate > 0, sourceFormat.channelCount > 0 else {
      await abandonPreInstall(engine: engine, analysis: nil)
      throw DictationCaptureError.unavailable("the microphone has no audio format yet")
    }
    let analysis = AnalyzerCoordinator(
      label: "DICT",
      sourceFormat: sourceFormat,
      onResult: { [onUpdate] result in
        onUpdate(
          Update(
            segmentID: result.segmentID,
            isFinal: result.isFinal,
            text: result.text,
            startSeconds: result.startSeconds,
            endSeconds: result.endSeconds
          )
        )
      },
      onError: { [weak self] terminal, message in
        guard terminal else {
          wispLog(message)
          return
        }
        Task { [weak self] in
          await self?.endRunWithFailure(.analyzer(message))
        }
      }
    )
    do {
      let supported = try await prepareSpeechAssets(locale: locale)
      if stopRequestedDuringStart() {
        await abandonPreInstall(engine: engine, analysis: analysis)
        return
      }
      _ = try await analysis.enable(locale: supported)
    } catch {
      await abandonPreInstall(engine: engine, analysis: analysis)
      throw Self.map(error)
    }
    if stopRequestedDuringStart() {
      await abandonPreInstall(engine: engine, analysis: analysis)
      return
    }

    let (stream, continuation) = AsyncStream<AVAudioPCMBuffer>.makeStream(
      bufferingPolicy: .bufferingNewest(32)
    )
    let consumer = Task { [analysis] in
      for await buffer in stream {
        await analysis.push(buffer)
      }
    }
    let handoff = RealtimeAudioHandoff(
      source: .mic,
      onBuffer: { captured in
        guard let buffer = captured.makePCMBuffer() else { return }
        continuation.yield(buffer)
      },
      onOverflow: { _, frames in
        wispLog("[DICT] dropped \(frames) frame(s) at the capture handoff")
      }
    )
    let token = installConfigurationObserver(on: engine)
    let installed = state.withLock { run -> Bool in
      guard run.phase == .starting, !run.stopRequested else { return false }
      run.engine = engine
      run.analysis = analysis
      run.handoff = handoff
      run.streamContinuation = continuation
      run.consumer = consumer
      run.configObserver = token
      run.phase = .running
      return true
    }
    guard installed else {
      await abandonPreInstall(
        engine: engine,
        analysis: analysis,
        streamContinuation: continuation,
        consumer: consumer,
        configObserver: token
      )
      return
    }

    // The engine closure runs on the engine's own serial queue, so the
    // reference box is the only race-free channel for its start result.
    let startOutcome = OSAllocatedUnfairLock<Error?>(initialState: nil)
    engineLock.withLock {
      do {
        installMicTap(on: engine, using: handoff)
        try engine.start()
      } catch {
        startOutcome.withLock { $0 = error }
      }
    }
    if let startError = startOutcome.withLock({ $0 }) {
      await stop(cancelAnalyzer: true)
      throw DictationCaptureError.engine(
        "the audio engine failed to start: \(startError)"
      )
    }
  }

  /// The tap body only hands PCM to the preallocated handoff; it must never
  /// allocate or touch the analyzer.
  private func installMicTap(on engine: AVAudioEngine, using handoff: RealtimeAudioHandoff) {
    let format = engine.inputNode.outputFormat(forBus: 0)
    engine.inputNode.installTap(
      onBus: 0,
      bufferSize: 4096,
      format: format
    ) { buffer, _ in
      handoff.enqueue(buffer, muted: false)
    }
  }

  private func installConfigurationObserver(on engine: AVAudioEngine) -> ConfigurationChangeToken {
    let observer = NotificationCenter.default.addObserver(
      forName: .AVAudioEngineConfigurationChange,
      object: engine,
      queue: nil
    ) { [weak self] _ in
      Task { [weak self] in
        await self?.performConfigurationChange()
      }
    }
    return ConfigurationChangeToken(observer: observer)
  }

  private func performConfigurationChange() async {
    let context = state.withLock {
      run -> (AVAudioEngine, AnalyzerCoordinator, RealtimeAudioHandoff)? in
      guard run.phase == .running,
        let engine = run.engine,
        let analysis = run.analysis,
        let handoff = run.handoff
      else { return nil }
      return (engine, analysis, handoff)
    }
    guard let (engine, analysis, handoff) = context else { return }
    let newFormat = engine.inputNode.outputFormat(forBus: 0)
    guard newFormat.sampleRate > 0, newFormat.channelCount > 0 else {
      wispLog("[DICT] input format not ready yet — waiting for next change")
      return
    }
    _ = await analysis.reconfigure(sourceFormat: newFormat)
    // Same lock channel as the start path above.
    let restartOutcome = OSAllocatedUnfairLock<Error?>(initialState: nil)
    engineLock.withLock {
      guard state.withLock({ $0.phase == .running && $0.engine === engine }) else { return }
      engine.inputNode.removeTap(onBus: 0)
      installMicTap(on: engine, using: handoff)
      if !engine.isRunning {
        do {
          try engine.start()
        } catch {
          restartOutcome.withLock { $0 = error }
        }
      }
    }
    if let restartError = restartOutcome.withLock({ $0 }) {
      await endRunWithFailure(
        .engine("failed to restart the audio engine after a device change: \(restartError)")
      )
    }
  }

  /// Claim an active run for teardown, harvesting its resources. A stop
  /// while `start` is between awaits cannot claim anything; it marks the
  /// stop request `performStart` re-checks.
  private func claimTeardown() -> Teardown? {
    state.withLock { run -> Teardown? in
      guard run.phase == .running,
        let engine = run.engine,
        let analysis = run.analysis,
        let handoff = run.handoff
      else {
        if run.phase == .starting {
          run.stopRequested = true
        }
        return nil
      }
      run.phase = .finishing
      run.engine = nil
      run.analysis = nil
      run.handoff = nil
      let streamContinuation = run.streamContinuation
      let consumer = run.consumer
      let configObserver = run.configObserver
      run.streamContinuation = nil
      run.consumer = nil
      run.configObserver = nil
      return Teardown(
        engine: engine,
        analysis: analysis,
        handoff: handoff,
        streamContinuation: streamContinuation,
        consumer: consumer,
        configObserver: configObserver
      )
    }
  }

  private func stop(cancelAnalyzer: Bool) async {
    guard let context = claimTeardown() else { return }
    await performTeardown(context, cancelAnalyzer: cancelAnalyzer)
    state.withLock { run in
      if run.phase == .finishing {
        run.phase = .idle
      }
    }
  }

  /// End the run after a mid-flight engine or analyzer failure, tearing
  /// everything down before the owner hears about it once.
  private func endRunWithFailure(_ error: DictationCaptureError) async {
    guard let context = claimTeardown() else { return }
    await performTeardown(context, cancelAnalyzer: true)
    let deliver = state.withLock { run in
      if run.phase == .finishing {
        run.phase = .idle
      }
      guard !run.failureDelivered else { return false }
      run.failureDelivered = true
      return true
    }
    if deliver {
      onFailure(error)
    }
  }

  private func performTeardown(_ context: Teardown, cancelAnalyzer: Bool) async {
    engineLock.withLock {
      context.engine.inputNode.removeTap(onBus: 0)
      context.engine.stop()
    }
    if let configObserver = context.configObserver {
      NotificationCenter.default.removeObserver(configObserver.observer)
    }
    // Queued handoff slots are delivered by a dispatch source, so one
    // synchronous drain is what keeps the last captured beats.
    context.handoff.drainSynchronously()
    context.streamContinuation?.finish()
    if let consumer = context.consumer {
      _ = await consumer.result
    }
    do {
      if cancelAnalyzer {
        await context.analysis.cancel()
      } else {
        try await context.analysis.finish()
      }
    } catch {
      // Finals emitted before the failure were already delivered.
    }
  }

  /// Clean up pieces a not-yet-installed run built locally; the phase lock
  /// still shows `.starting`, so there is nothing to claim.
  private func abandonPreInstall(
    engine: AVAudioEngine,
    analysis: AnalyzerCoordinator?,
    streamContinuation: AsyncStream<AVAudioPCMBuffer>.Continuation? = nil,
    consumer: Task<Void, Never>? = nil,
    configObserver: ConfigurationChangeToken? = nil
  ) async {
    if let configObserver {
      NotificationCenter.default.removeObserver(configObserver.observer)
    }
    engine.inputNode.removeTap(onBus: 0)
    engine.stop()
    streamContinuation?.finish()
    if let consumer {
      _ = await consumer.result
    }
    await analysis?.cancel()
    releaseClaim()
  }

  private func releaseClaim() {
    state.withLock { run in
      guard run.phase == .starting else { return }
      run.phase = .idle
      run.engine = nil
      run.analysis = nil
      run.handoff = nil
      run.streamContinuation = nil
      run.consumer = nil
      run.configObserver = nil
    }
  }

  private func stopRequestedDuringStart() -> Bool {
    state.withLock { $0.stopRequested }
  }

  /// Fold asset/analyzer failures onto the public error surface instead of
  /// leaking `PoCError` past the module boundary.
  private static func map(_ error: Error) -> DictationCaptureError {
    guard let failure = error as? PoCError else {
      return .analyzer("\(error)")
    }
    let mapped: DictationCaptureError
    switch failure {
    case .unsupportedSpeechLocale(let locale):
      mapped = .unsupportedLocale(locale)
    case .analyzerFinalizationFailed(let message):
      mapped = .analyzer(message)
    case .noCompatibleFormat, .converterCreationFailed:
      mapped = .analyzer(failure.description)
    case .permissionDenied, .invalidLifecycle, .speechTranscriberUnavailable,
      .noDisplay, .scStreamSetupFailed, .outputFilesAlreadyExist:
      mapped = .unavailable(failure.description)
    }
    return mapped
  }
}
