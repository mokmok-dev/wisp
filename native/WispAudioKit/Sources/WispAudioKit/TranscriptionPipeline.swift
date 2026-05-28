@preconcurrency import AVFoundation
import CoreMedia
import Foundation
import Speech

/// One transcription pipeline = one audio source (mic OR system) feeding a
/// dedicated SpeechAnalyzer, plus a WAV writer at the source's native format.
///
/// The pipeline is intentionally per-source so we get speaker attribution
/// for free (mic = "self", system = "other") without ML diarization.
public final class TranscriptionPipeline: @unchecked Sendable {
    public let label: String
    public let wavURL: URL
    public let sourceFormat: AVAudioFormat

    private let analyzer: SpeechAnalyzer
    private let transcriber: SpeechTranscriber
    private let analyzerFormat: AVAudioFormat
    private let converter: AVAudioConverter
    private let wavFile: AVAudioFile
    private let inputContinuation: AsyncStream<AnalyzerInput>.Continuation
    private var resultsTask: Task<Void, Never>?

    public init(label: String, sourceFormat: AVAudioFormat, wavURL: URL) async throws {
        self.label = label
        self.wavURL = wavURL
        self.sourceFormat = sourceFormat

        // SpeechTranscriber for Japanese with progressive (streaming) preset
        let transcriber = SpeechTranscriber(
            locale: Locale(identifier: "ja-JP"),
            preset: .progressiveTranscription
        )
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
        self.converter = converter

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

        // Analyzer with volatile range observer
        let labelForHandler = label
        analyzer = SpeechAnalyzer(
            inputSequence: inputStream,
            modules: [transcriber],
            options: nil,
            analysisContext: .init(),
            volatileRangeChangedHandler: { range, changedStart, _ in
                if changedStart {
                    let s = CMTimeGetSeconds(range.start)
                    wispLog(
                        "[\(labelForHandler)] finalized boundary advanced to \(String(format: "%.2fs", s))"
                    )
                }
            }
        )

        wispLog(
            "[\(label)] pipeline ready — analyzer format sr=\(analyzerFormat.sampleRate) ch=\(analyzerFormat.channelCount) fmt=\(analyzerFormat.commonFormat.rawValue)"
        )
        wispLog("[\(label)] WAV: \(wavURL.path)")
    }

    /// Start consuming transcription results in the background.
    /// Idempotent: only spawns the task once.
    public func startResultsConsumer() {
        guard resultsTask == nil else { return }
        let label = label
        let transcriber = transcriber
        resultsTask = Task {
            do {
                for try await result in transcriber.results {
                    let text = String(result.text.characters)
                    let s = CMTimeGetSeconds(result.range.start)
                    let e = CMTimeGetSeconds(result.range.end)
                    let range = String(format: "%6.2f-%6.2fs", s, e)
                    print("[\(label)] [\(range)] \(text)")
                }
                wispLog("[\(label)] results stream finished")
            } catch {
                wispLog("[\(label)] results error: \(error)")
            }
        }
    }

    /// Push one audio buffer from the source. Writes to WAV and feeds the
    /// analyzer (resampling/format-converting on the fly).
    /// Safe to call from audio callback threads.
    public func push(_ buffer: AVAudioPCMBuffer) {
        // 1. WAV (native format)
        do {
            try wavFile.write(from: buffer)
        } catch {
            wispLog("[\(label)] WAV write error: \(error)")
        }

        // 2. Resample to analyzer format
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

    /// Stop feeding the analyzer and wait for final results to drain.
    public func finish() async {
        inputContinuation.finish()
        try? await analyzer.finalizeAndFinishThroughEndOfInput()
        _ = await resultsTask?.result
    }
}
