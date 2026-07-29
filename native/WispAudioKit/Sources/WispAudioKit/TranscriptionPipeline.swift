@preconcurrency import AVFoundation
import CoreMedia
import Foundation
import os.lock
import Speech

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

    public let label: String
    public let oggURL: URL

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
    private let recorder: OpusOggRecorder
    private let inputContinuation: AsyncStream<AnalyzerInput>.Continuation
    private let onResult: OnResult
    private var resultsTask: Task<Void, Never>?

    public init(
        label: String,
        sourceFormat: AVAudioFormat,
        oggURL: URL,
        locale: Locale = Locale(identifier: "ja-JP"),
        onResult: @escaping OnResult
    ) async throws {
        self.label = label
        self.oggURL = oggURL
        self.onResult = onResult

        // Streaming results with audio time ranges and per-run confidence.
        let transcriber = makeLiveSpeechTranscriber(locale: locale)
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

        recorder = try OpusOggRecorder(url: oggURL, sourceFormat: sourceFormat)

        // AsyncStream feeding the analyzer
        let (inputStream, inputContinuation) = AsyncStream<AnalyzerInput>.makeStream()
        self.inputContinuation = inputContinuation

        analyzer = SpeechAnalyzer(modules: [transcriber])

        // Consume results before starting analysis so even an immediately
        // produced update has a waiting receiver. Capture is installed only
        // after this initializer returns, so preheating cannot lose speech.
        startResultsConsumer()
        do {
            try await analyzer.prepareToAnalyze(in: analyzerFormat)
            try await analyzer.start(inputSequence: inputStream)
        } catch {
            inputContinuation.finish()
            await analyzer.cancelAndFinishNow()
            resultsTask?.cancel()
            throw error
        }

        wispLog(
            "[\(label)] pipeline ready — analyzer format sr=\(analyzerFormat.sampleRate) ch=\(analyzerFormat.channelCount) fmt=\(analyzerFormat.commonFormat.rawValue)"
        )
        wispLog("[\(label)] Ogg/Opus: \(oggURL.path)")
    }

    /// Push one audio buffer from the source. Queues it for Ogg/Opus encoding
    /// and feeds the analyzer (resampling/format-converting on the fly).
    /// Safe to call from audio callback threads.
    public func push(_ buffer: AVAudioPCMBuffer, muted: Bool = false) {
        let input: AVAudioPCMBuffer
        if muted {
            guard let silence = AVAudioPCMBuffer(
                pcmFormat: buffer.format,
                frameCapacity: buffer.frameLength
            ) else { return }
            silence.frameLength = buffer.frameLength
            let buffers = UnsafeMutableAudioBufferListPointer(silence.mutableAudioBufferList)
            for audioBuffer in buffers where audioBuffer.mDataByteSize > 0 {
                if let data = audioBuffer.mData {
                    data.initializeMemory(
                        as: UInt8.self,
                        repeating: 0,
                        count: Int(audioBuffer.mDataByteSize)
                    )
                }
            }
            input = silence
        } else {
            input = buffer
        }

        // 1. Ogg/Opus. The recorder copies into a bounded queue; codec and
        // file I/O stay off the real-time callback thread.
        recorder.push(input)

        // 2. Resample to analyzer format
        let (sourceFormat, converter): (AVAudioFormat, AVAudioConverter) =
            converterLock.withLock { ($0.sourceFormat, $0.converter) }

        guard input.format.sampleRate == sourceFormat.sampleRate,
              input.format.channelCount == sourceFormat.channelCount
        else {
            wispLog(
                "[\(label)] dropping buffer with stale format sr=\(input.format.sampleRate) ch=\(input.format.channelCount) (expected sr=\(sourceFormat.sampleRate) ch=\(sourceFormat.channelCount))"
            )
            return
        }

        let ratio = analyzerFormat.sampleRate / sourceFormat.sampleRate
        let outCapacity = AVAudioFrameCount((Double(input.frameLength) * ratio).rounded(.up))
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
                return input
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
        await recorder.finish()
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
            var segmentIDs = ResultSegmentIDs()
            do {
                for try await result in transcriber.results {
                    let segmentID = segmentIDs.id(isFinal: result.isFinal)
                    let confidence = transcriptionConfidence(for: result.text)

                    onResult(Result(
                        label: label,
                        segmentID: segmentID,
                        isFinal: result.isFinal,
                        text: String(result.text.characters),
                        startSeconds: CMTimeGetSeconds(result.range.start),
                        endSeconds: CMTimeGetSeconds(result.range.end),
                        confidenceMean: confidence?.mean,
                        confidenceMin: confidence?.min
                    ))
                }
                wispLog("[\(label)] results stream finished")
            } catch {
                wispLog("[\(label)] results error: \(error)")
            }
        }
    }
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
