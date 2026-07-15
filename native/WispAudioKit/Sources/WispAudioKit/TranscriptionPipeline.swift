@preconcurrency import AVFoundation
import CoreMedia
import Foundation
import os.lock
import Speech

/// One transcription pipeline = one audio source (mic OR system) feeding a
/// dedicated SpeechAnalyzer, plus a WAV writer at the source's native format.
///
/// The pipeline is intentionally per-source so we get speaker attribution
/// for free (mic = "self", system = "other") without ML diarization.
public final class TranscriptionPipeline: @unchecked Sendable {
    /// One transcription update emitted to the consumer.
    ///
    /// `segmentID` is monotonically increasing per pipeline. The same ID
    /// repeats while a partial is being revised; when the analyzer's
    /// finalized boundary advances (i.e. `range.start` moves past the
    /// previous result), `segmentID` ticks up to start a new segment.
    public struct Result: Sendable {
        public let label: String
        public let segmentID: UInt64
        public let text: String
        public let startSeconds: Double
        public let endSeconds: Double
    }

    public typealias OnResult = @Sendable (Result) -> Void

    public let label: String
    public let wavURL: URL

    public var sourceFormat: AVAudioFormat {
        converterLock.withLock { $0.sourceFormat }
    }

    private let analyzer: SpeechAnalyzer
    private let transcriber: SpeechTranscriber
    private let analyzerFormat: AVAudioFormat

    private struct ConverterState {
        var sourceFormat: AVAudioFormat
        var converter: AVAudioConverter
    }

    private let converterLock: OSAllocatedUnfairLock<ConverterState>
    private let wavFile: AVAudioFile
    private let inputContinuation: AsyncStream<AnalyzerInput>.Continuation
    private let onResult: OnResult
    private var resultsTask: Task<Void, Never>?

    public init(
        label: String,
        sourceFormat: AVAudioFormat,
        wavURL: URL,
        locale: Locale = Locale(identifier: "ja-JP"),
        onResult: @escaping OnResult
    ) async throws {
        self.label = label
        self.wavURL = wavURL
        self.onResult = onResult

        // SpeechTranscriber with progressive (streaming) preset
        let transcriber = SpeechTranscriber(locale: locale, preset: .progressiveTranscription)
        self.transcriber = transcriber

        // Best format the analyzer accepts
        guard let analyzerFormat = await SpeechAnalyzer
            .bestAvailableAudioFormat(compatibleWith: [transcriber])
        else {
            throw PoCError.noCompatibleFormat
        }
        self.analyzerFormat = analyzerFormat

        // Resampler/format-converter source → analyzer format
        guard let converter = AVAudioConverter(from: sourceFormat, to: analyzerFormat) else {
            throw PoCError.converterCreationFailed
        }
        converterLock = OSAllocatedUnfairLock(
            initialState: ConverterState(sourceFormat: sourceFormat, converter: converter)
        )

        // WAV files require interleaved PCM. AVAudioFile.write() auto-converts
        // from the buffer's format to the file's format, so non-interleaved
        // captures (like SCKit) get interleaved on write.
        guard let wavFormat = AVAudioFormat(
            commonFormat: sourceFormat.commonFormat,
            sampleRate: sourceFormat.sampleRate,
            channels: sourceFormat.channelCount,
            interleaved: true
        ) else {
            throw PoCError.converterCreationFailed
        }
        wavFile = try AVAudioFile(
            forWriting: wavURL,
            settings: wavFormat.settings,
            commonFormat: wavFormat.commonFormat,
            interleaved: true
        )

        // AsyncStream feeding the analyzer
        let (inputStream, inputContinuation) = AsyncStream<AnalyzerInput>.makeStream()
        self.inputContinuation = inputContinuation

        // Analyzer with volatile range observer. We don't act on the volatile
        // range directly — segment finalization is tracked from the results
        // stream below (when `range.start` advances, the current segment is
        // done and we tick the segment ID).
        analyzer = SpeechAnalyzer(
            inputSequence: inputStream,
            modules: [transcriber],
            options: nil,
            analysisContext: .init(),
            volatileRangeChangedHandler: nil
        )

        // Spawn the results consumer eagerly. Idempotent because we guard on
        // `resultsTask`.
        startResultsConsumer()

        wispLog(
            "[\(label)] pipeline ready — analyzer format sr=\(analyzerFormat.sampleRate) ch=\(analyzerFormat.channelCount) fmt=\(analyzerFormat.commonFormat.rawValue)"
        )
        wispLog("[\(label)] WAV: \(wavURL.path)")
    }

    /// Push one audio buffer from the source. Writes to WAV and feeds the
    /// analyzer (resampling/format-converting on the fly).
    /// Safe to call from audio callback threads.
    public func push(_ buffer: AVAudioPCMBuffer) {
        // 1. WAV (native format)
        if buffer.format.sampleRate == wavFile.processingFormat.sampleRate,
           buffer.format.channelCount == wavFile.processingFormat.channelCount
        {
            do {
                try wavFile.write(from: buffer)
            } catch {
                wispLog("[\(label)] WAV write error: \(error)")
            }
        }

        // 2. Resample to analyzer format
        let (sourceFormat, converter): (AVAudioFormat, AVAudioConverter) =
            converterLock.withLock { ($0.sourceFormat, $0.converter) }

        guard buffer.format.sampleRate == sourceFormat.sampleRate,
              buffer.format.channelCount == sourceFormat.channelCount
        else {
            wispLog(
                "[\(label)] dropping buffer with stale format sr=\(buffer.format.sampleRate) ch=\(buffer.format.channelCount) (expected sr=\(sourceFormat.sampleRate) ch=\(sourceFormat.channelCount))"
            )
            return
        }

        let ratio = analyzerFormat.sampleRate / sourceFormat.sampleRate
        let outCapacity = AVAudioFrameCount((Double(buffer.frameLength) * ratio).rounded(.up))
        guard outCapacity > 0,
              let converted = AVAudioPCMBuffer(
                  pcmFormat: analyzerFormat,
                  frameCapacity: outCapacity
              )
        else { return }

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
                return buffer
            }
        )
        if let convertError {
            wispLog("[\(label)] convert error: \(convertError)")
            return
        }
        guard status != .error, converted.frameLength > 0 else { return }

        inputContinuation.yield(AnalyzerInput(buffer: converted))
    }

    @discardableResult
    public func reconfigure(sourceFormat newFormat: AVAudioFormat) -> Bool {
        converterLock.withLock { state -> Bool in
            if state.sourceFormat.sampleRate == newFormat.sampleRate,
               state.sourceFormat.channelCount == newFormat.channelCount
            {
                return true
            }
            guard let converter = AVAudioConverter(from: newFormat, to: analyzerFormat) else {
                wispLog(
                    "[\(label)] reconfigure failed: no converter for sr=\(newFormat.sampleRate) ch=\(newFormat.channelCount)"
                )
                return false
            }
            wispLog(
                "[\(label)] reconfigured source format sr=\(state.sourceFormat.sampleRate)→\(newFormat.sampleRate) ch=\(state.sourceFormat.channelCount)→\(newFormat.channelCount)"
            )
            state.sourceFormat = newFormat
            state.converter = converter
            return true
        }
    }

    /// Stop feeding the analyzer and wait for final results to drain.
    public func finish() async {
        inputContinuation.finish()
        try? await analyzer.finalizeAndFinishThroughEndOfInput()
        _ = await resultsTask?.result
    }

    private func startResultsConsumer() {
        guard resultsTask == nil else { return }
        let label = label
        let transcriber = transcriber
        let onResult = onResult
        resultsTask = Task {
            var lastStart: CMTime?
            var segmentID: UInt64 = 0
            do {
                for try await result in transcriber.results {
                    // Tick the segment ID whenever the finalized boundary advances.
                    let start = result.range.start
                    if lastStart != start {
                        segmentID += 1
                        lastStart = start
                    }

                    onResult(Result(
                        label: label,
                        segmentID: segmentID,
                        text: String(result.text.characters),
                        startSeconds: CMTimeGetSeconds(result.range.start),
                        endSeconds: CMTimeGetSeconds(result.range.end)
                    ))
                }
                wispLog("[\(label)] results stream finished")
            } catch {
                wispLog("[\(label)] results error: \(error)")
            }
        }
    }
}
