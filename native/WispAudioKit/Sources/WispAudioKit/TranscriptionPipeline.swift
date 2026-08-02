@preconcurrency import AVFoundation
import CoreMedia
import Foundation
import Speech
import os.lock

/// One transcription pipeline = one audio source (mic OR system) feeding a
/// dedicated SpeechAnalyzer, plus a background Ogg/Opus recorder.
///
/// The pipeline is intentionally per-source so we get speaker attribution
/// for free (mic = "self", system = "other") without ML diarization.
public final class TranscriptionPipeline: @unchecked Sendable {
  /// One transcription update emitted to the consumer.
  ///
  /// `segmentID` is monotonically increasing per pipeline. The same ID
  /// repeats while a volatile result is being revised and on the matching
  /// final result. The next result then starts a new segment.
  public struct Result: Sendable {
    public let label: String
    public let segmentID: UInt64
    public let isFinal: Bool
    public let text: String
    public let startSeconds: Double
    public let endSeconds: Double
    public let confidenceMean: Double?
    public let confidenceMin: Double?
  }

  public typealias OnResult = @Sendable (Result) -> Void
  public typealias OnError = @Sendable (_ terminal: Bool, _ message: String) -> Void
  public typealias OnRecordingError = @Sendable (_ message: String) -> Void
  public typealias OnRecordingDrop = @Sendable (_ droppedFrames: UInt64) -> Void

  public let label: String
  public let oggURL: URL

  public var sourceFormat: AVAudioFormat {
    sourceFormatLock.withLock { $0 }
  }

  private let sourceFormatLock: OSAllocatedUnfairLock<AVAudioFormat>
  private let recorder: OpusOggRecorder
  private let analysis: AnalyzerCoordinator

  /// Construct recording immediately, without doing any analyzer work.
  /// This is used by lazy system capture so Ogg staging starts before
  /// SpeechAnalyzer format negotiation can suspend.
  public init(
    recordingOnlyLabel label: String,
    sourceFormat: AVAudioFormat,
    oggURL: URL,
    onResult: @escaping OnResult,
    onError: @escaping OnError = { _, _ in },
    onRecordingError: @escaping OnRecordingError = { wispLog($0) },
    onRecordingDrop: @escaping OnRecordingDrop = { _ in }
  ) throws {
    self.label = label
    self.oggURL = oggURL
    sourceFormatLock = OSAllocatedUnfairLock(initialState: sourceFormat)
    analysis = AnalyzerCoordinator(
      label: label,
      sourceFormat: sourceFormat,
      onResult: onResult,
      onError: onError
    )
    recorder = try OpusOggRecorder(
      url: oggURL,
      sourceFormat: sourceFormat,
      onFatal: onRecordingError,
      onDroppedFrames: onRecordingDrop
    )
    wispLog("[\(label)] recording pipeline ready")
    wispLog("[\(label)] Ogg/Opus: \(oggURL.path)")
  }

  public convenience init(
    label: String,
    sourceFormat: AVAudioFormat,
    oggURL: URL,
    locale: Locale = Locale(identifier: "ja-JP"),
    transcriptionEnabled: Bool = true,
    allowRecordOnly: Bool = false,
    onResult: @escaping OnResult,
    onError: @escaping OnError = { _, _ in },
    onRecordingError: @escaping OnRecordingError = { wispLog($0) },
    onRecordingDrop: @escaping OnRecordingDrop = { _ in }
  ) async throws {
    try self.init(
      recordingOnlyLabel: label,
      sourceFormat: sourceFormat,
      oggURL: oggURL,
      onResult: onResult,
      onError: onError,
      onRecordingError: onRecordingError,
      onRecordingDrop: onRecordingDrop
    )
    if transcriptionEnabled {
      _ = try await enableAnalysis(locale: locale, allowRecordOnly: allowRecordOnly)
    } else {
      wispLog("[\(label)] recording-only pipeline ready")
    }
  }

  /// Configure SpeechAnalyzer once. Recorder construction is deliberately
  /// outside this transaction, so every error here is analyzer-only.
  @discardableResult
  public func enableAnalysis(
    locale: Locale,
    allowRecordOnly: Bool
  ) async throws -> Bool {
    do {
      return try await analysis.enable(locale: locale)
    } catch {
      switch analyzerSetupDisposition(
        allowRecordOnly: allowRecordOnly,
        error: error
      ) {
      case .recordOnly(let message):
        await analysis.reportSetupFallback(message)
        wispLog("[\(label)] analyzer unavailable; recording-only: \(error)")
        return false
      case .terminal:
        throw error
      }
    }
  }

  /// Queue capture PCM for Ogg/Opus recording. Analyzer input deliberately
  /// has a separate entry point so capture can never bypass the Rust
  /// `SessionOrchestrator`.
  public func pushRecording(_ buffer: AVAudioPCMBuffer) {
    recorder.push(buffer)
  }

  /// Submit one frame to SpeechAnalyzer. The v2 ABI calls this only after
  /// `CaptureBackend` and `SessionOrchestrator`; the legacy Swift/C facade
  /// calls it from its compatibility-owned capture path.
  public func pushAnalyzer(_ input: AVAudioPCMBuffer) async {
    await analysis.push(input)
  }

  @discardableResult
  public func reconfigure(sourceFormat newFormat: AVAudioFormat) async -> Bool {
    sourceFormatLock.withLock { $0 = newFormat }
    return await analysis.reconfigure(sourceFormat: newFormat)
  }

  /// Stop feeding the analyzer and wait for final results to drain.
  public func finish() async throws {
    await finishRecording()
    try await finishAnalysis()
  }

  /// Discard queued recorder/analyzer work without draining final results.
  public func abort() async {
    await abortRecording()
    await analysis.cancel()
  }

  /// Cancel only SpeechAnalyzer while recording remains active.
  public func disableAnalysis() async {
    await analysis.cancel()
  }

  public func finishRecording() async {
    await recorder.finish()
  }

  public func abortRecording() async {
    await recorder.abort()
  }

  public func finishAnalysis() async throws {
    try await analysis.finish()
  }
}

/// Owns every mutable SpeechAnalyzer object in one Swift synchronization
/// domain. Recorder/capture state never enters this actor.
typealias AnalyzerFinalizeOperation = @Sendable (SpeechAnalyzer?) async throws -> Void

actor AnalyzerCoordinator {
  private let label: String
  private let onResult: TranscriptionPipeline.OnResult
  private let onError: TranscriptionPipeline.OnError
  private let timeline = OSAllocatedUnfairLock(initialState: AnalyzerTimeline())

  private var sourceFormat: AVAudioFormat
  private var analyzer: SpeechAnalyzer?
  private var transcriber: SpeechTranscriber?
  private var analyzerFormat: AVAudioFormat?
  private var converter: AVAudioConverter?
  private var continuation: AsyncStream<AnalyzerInput>.Continuation?
  private var resultsTask: Task<Void, Never>?
  private var configured = false
  private var active = false
  private let finalizeOperation: AnalyzerFinalizeOperation

  init(
    label: String,
    sourceFormat: AVAudioFormat,
    onResult: @escaping TranscriptionPipeline.OnResult,
    onError: @escaping TranscriptionPipeline.OnError,
    finalizeOperation: @escaping AnalyzerFinalizeOperation = { analyzer in
      try await analyzer?.finalizeAndFinishThroughEndOfInput()
    }
  ) {
    self.label = label
    self.sourceFormat = sourceFormat
    self.onResult = onResult
    self.onError = onError
    self.finalizeOperation = finalizeOperation
  }

  func enable(locale: Locale) async throws -> Bool {
    if configured { return active }
    configured = true
    do {
      let transcriber = makeLiveSpeechTranscriber(locale: locale)
      guard
        let analyzerFormat =
          await SpeechAnalyzer
          .bestAvailableAudioFormat(compatibleWith: [transcriber])
      else {
        throw PoCError.noCompatibleFormat
      }
      guard let converter = AVAudioConverter(from: sourceFormat, to: analyzerFormat) else {
        throw PoCError.converterCreationFailed
      }
      let (stream, continuation) = AsyncStream<AnalyzerInput>.makeStream(
        bufferingPolicy: .bufferingOldest(32)
      )
      let analyzer = SpeechAnalyzer(modules: [transcriber])
      try await analyzer.prepareToAnalyze(in: analyzerFormat)
      try await analyzer.start(inputSequence: stream)

      self.transcriber = transcriber
      self.analyzerFormat = analyzerFormat
      self.converter = converter
      self.continuation = continuation
      self.analyzer = analyzer
      active = true
      startResultsConsumer(transcriber)
      wispLog(
        "[\(label)] analyzer ready — sr=\(analyzerFormat.sampleRate) ch=\(analyzerFormat.channelCount) fmt=\(analyzerFormat.commonFormat.rawValue)"
      )
      return true
    } catch {
      await cancel()
      throw error
    }
  }

  func reportSetupFallback(_ message: String) {
    onError(true, "[\(label)] \(message)")
  }

  func push(_ input: AVAudioPCMBuffer) {
    guard active else { return }
    let droppedFrames = UInt64(input.frameLength)
    let duration = Double(input.frameLength) / input.format.sampleRate
    guard input.format.sampleRate == sourceFormat.sampleRate,
      input.format.channelCount == sourceFormat.channelCount
    else {
      reportGap(frames: droppedFrames, duration: duration, reason: "stale input format")
      return
    }
    guard let analyzerFormat, let converter, let continuation else {
      reportGap(frames: droppedFrames, duration: duration, reason: "analyzer unavailable")
      return
    }
    let ratio = analyzerFormat.sampleRate / sourceFormat.sampleRate
    let capacityDouble = (Double(input.frameLength) * ratio).rounded(.up)
    guard capacityDouble > 0,
      capacityDouble <= Double(UInt32.max),
      let converted = AVAudioPCMBuffer(
        pcmFormat: analyzerFormat,
        frameCapacity: AVAudioFrameCount(capacityDouble)
      )
    else {
      reportGap(frames: droppedFrames, duration: duration, reason: "conversion allocation")
      return
    }

    var convertError: NSError?
    let consumed = MutableFlag()
    let status = converter.convert(
      to: converted,
      error: &convertError,
      withInputFrom: { _, outStatus in
        if consumed.value {
          outStatus.pointee = .noDataNow
          return nil
        }
        consumed.value = true
        outStatus.pointee = .haveData
        return input
      }
    )
    guard convertError == nil, status != .error, converted.frameLength > 0 else {
      let reason = convertError.map { "conversion failed: \($0)" } ?? "conversion produced no PCM"
      reportGap(frames: droppedFrames, duration: duration, reason: reason)
      return
    }

    let convertedDuration = Double(converted.frameLength) / analyzerFormat.sampleRate
    switch continuation.yield(AnalyzerInput(buffer: converted)) {
    case .enqueued:
      timeline.withLock { $0.accept(duration: convertedDuration) }
    case .dropped:
      reportGap(
        frames: droppedFrames,
        duration: duration,
        reason: "bounded analyzer input queue overflow"
      )
    case .terminated:
      active = false
      onError(
        true,
        "[\(label)] analyzer input stream terminated with \(droppedFrames) frame(s) unconsumed"
      )
    @unknown default:
      reportGap(
        frames: droppedFrames,
        duration: duration,
        reason: "unknown input queue result"
      )
    }
  }

  func reconfigure(sourceFormat newFormat: AVAudioFormat) -> Bool {
    if sourceFormat.sampleRate == newFormat.sampleRate,
      sourceFormat.channelCount == newFormat.channelCount
    {
      return true
    }
    let oldFormat = sourceFormat
    sourceFormat = newFormat
    guard let analyzerFormat else { return true }
    guard let converter = AVAudioConverter(from: newFormat, to: analyzerFormat) else {
      converter = nil
      active = false
      onError(
        true,
        "[\(label)] SpeechAnalyzer converter creation failed after device change"
      )
      return true
    }
    self.converter = converter
    wispLog(
      "[\(label)] reconfigured source format sr=\(oldFormat.sampleRate)→\(newFormat.sampleRate) ch=\(oldFormat.channelCount)→\(newFormat.channelCount)"
    )
    return true
  }

  func finish() async throws {
    active = false
    continuation?.finish()
    continuation = nil
    // Keep analyzer/results ownership intact on failure. A later graceful
    // shutdown retries this exact phase, while any finals emitted before
    // the error remain observable through the callback queue.
    try await finalizeOperation(analyzer)
    _ = await resultsTask?.result
    clear()
  }

  func cancel() async {
    active = false
    continuation?.finish()
    continuation = nil
    await analyzer?.cancelAndFinishNow()
    resultsTask?.cancel()
    _ = await resultsTask?.result
    clear()
  }

  private func clear() {
    analyzer = nil
    analyzerFormat = nil
    converter = nil
    resultsTask = nil
    transcriber = nil
  }

  private func reportGap(
    frames: UInt64,
    duration: Double,
    reason: String
  ) {
    let retained = timeline.withLock { $0.drop(duration: duration) }
    guard retained else {
      // Exact revision mapping requires every unfinalized gap boundary.
      // Once that bounded history is exhausted, stop accepting analyzer
      // input instead of publishing timestamps from an approximation.
      active = false
      continuation?.finish()
      continuation = nil
      onError(
        true,
        "[\(label)] analyzer disabled after reaching the unfinalized gap history limit"
      )
      return
    }
    onError(
      false,
      "[\(label)] analyzer gap: dropped \(frames) frame(s) (\(reason))"
    )
  }

  private func startResultsConsumer(_ transcriber: SpeechTranscriber) {
    guard resultsTask == nil else { return }
    let label = label
    let onResult = onResult
    let onError = onError
    let timeline = timeline
    resultsTask = Task {
      var segmentIDs = ResultSegmentIDs()
      do {
        for try await result in transcriber.results {
          let segmentID = segmentIDs.id(isFinal: result.isFinal)
          let confidence = transcriptionConfidence(for: result.text)
          let mapped = timeline.withLock {
            state -> (Double, Double) in
            let compressedStart = CMTimeGetSeconds(result.range.start)
            let compressedEnd = CMTimeGetSeconds(result.range.end)
            let mapped = (
              state.map(compressedStart),
              state.map(compressedEnd)
            )
            if result.isFinal {
              state.finalize(through: compressedEnd)
            }
            return mapped
          }
          onResult(
            TranscriptionPipeline.Result(
              label: label,
              segmentID: segmentID,
              isFinal: result.isFinal,
              text: String(result.text.characters),
              startSeconds: mapped.0,
              endSeconds: mapped.1,
              confidenceMean: confidence?.mean,
              confidenceMin: confidence?.min
            ))
        }
        wispLog("[\(label)] results stream finished")
      } catch {
        if Task.isCancelled {
          wispLog("[\(label)] results consumer cancelled")
          return
        }
        onError(true, "[\(label)] SpeechAnalyzer results failed: \(error)")
        wispLog("[\(label)] results error: \(error)")
      }
    }
  }
}

struct AnalyzerTimeline {
  static let maximumRetainedGapCount = 4096

  private var compressedEnd = 0.0
  private var finalizedCompressedEnd = 0.0
  private var finalizedGapDuration = 0.0
  private var gaps: [(at: Double, duration: Double)] = []

  var retainedGapCount: Int {
    gaps.count
  }

  mutating func accept(duration: Double) {
    if duration.isFinite, duration > 0 {
      compressedEnd += duration
    }
  }

  /// Record one dropped interval while retaining exact revision mapping.
  /// Returns false when a new distinct boundary would exceed the explicit
  /// bound. The coordinator treats that as terminal because folding an
  /// unfinalized gap would corrupt revisions that still refer to it.
  @discardableResult
  mutating func drop(duration: Double) -> Bool {
    guard duration.isFinite, duration > 0 else { return true }
    if let last = gaps.last, last.at == compressedEnd {
      gaps[gaps.count - 1].duration += duration
      return true
    }
    guard gaps.count < Self.maximumRetainedGapCount else { return false }
    gaps.append((at: compressedEnd, duration: duration))
    return true
  }

  /// Fold gaps which belong to a finalized analyzer range into one
  /// cumulative offset. SpeechAnalyzer does not revise timestamps before a
  /// final result, so retaining every individual gap there would only make
  /// memory use and every future map operation grow without bound.
  mutating func finalize(through compressedSeconds: Double) {
    guard compressedSeconds.isFinite,
      compressedSeconds >= finalizedCompressedEnd
    else {
      return
    }
    var foldedCount = 0
    var foldedDuration = 0.0
    for gap in gaps {
      guard gap.at <= compressedSeconds else { break }
      foldedCount += 1
      foldedDuration += gap.duration
    }
    if foldedCount > 0 {
      finalizedGapDuration += foldedDuration
      gaps = Array(gaps.dropFirst(foldedCount))
    }
    finalizedCompressedEnd = compressedSeconds
  }

  func map(_ compressedSeconds: Double) -> Double {
    var gapDuration = finalizedGapDuration
    for gap in gaps {
      guard gap.at <= compressedSeconds else { break }
      gapDuration += gap.duration
    }
    return compressedSeconds + gapDuration
  }
}

enum AnalyzerSetupDisposition: Equatable {
  case recordOnly(String)
  case terminal
}

/// Central policy boundary for all analyzer-only setup failures. Recorder
/// creation happens before this boundary and is intentionally never eligible.
func analyzerSetupDisposition(
  allowRecordOnly: Bool,
  error: Error
) -> AnalyzerSetupDisposition {
  guard allowRecordOnly else { return .terminal }
  return .recordOnly("SpeechAnalyzer initialization failed: \(error)")
}

/// Use one configuration for the installation probe and both live pipelines.
/// The preset supplies volatile and fast results plus time indexing; confidence
/// is added so consumers can identify transcript chunks worth reviewing.
func makeLiveSpeechTranscriber(locale: Locale) -> SpeechTranscriber {
  let preset = SpeechTranscriber.Preset.timeIndexedProgressiveTranscription
  return SpeechTranscriber(
    locale: locale,
    transcriptionOptions: preset.transcriptionOptions,
    reportingOptions: preset.reportingOptions,
    attributeOptions: preset.attributeOptions.union([.transcriptionConfidence])
  )
}

/// Assign one stable identifier to all revisions and the final result for an
/// utterance. A final-only result also receives its own identifier.
struct ResultSegmentIDs {
  private var nextID: UInt64 = 1
  private var activeID: UInt64?

  mutating func id(isFinal: Bool) -> UInt64 {
    let id: UInt64
    if let activeID {
      id = activeID
    } else {
      id = nextID
      nextID &+= 1
      activeID = id
    }
    if isFinal {
      activeID = nil
    }
    return id
  }
}

private func transcriptionConfidence(
  for text: AttributedString
) -> (mean: Double, min: Double)? {
  var weightedSum = 0.0
  var characterCount = 0
  var minimum = Double.greatestFiniteMagnitude

  for run in text.runs {
    guard let confidence = run.transcriptionConfidence else { continue }
    let count = text[run.range].characters.count
    weightedSum += confidence * Double(count)
    characterCount += count
    minimum = Swift.min(minimum, confidence)
  }

  guard characterCount > 0 else { return nil }
  return (weightedSum / Double(characterCount), minimum)
}
