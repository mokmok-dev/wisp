@preconcurrency import AVFoundation
import CoreMedia
import Foundation
import os.lock
import Speech

/// A live recording + transcription session.
///
/// Owns a microphone capture (`AVAudioEngine`) and a system-audio capture
/// (`ProcessTapCapture`), each feeding its own `TranscriptionPipeline`.
/// Transcription results from both pipelines are funnelled through a
/// single `onResult` callback, tagged with the originating source.
///
/// Lifecycle: `init` constructs the session but does no I/O; `start()`
/// requests permissions, downloads the ja-JP speech model if needed,
/// builds the mic pipeline, and starts both captures; `stop()` tears
/// everything down and drains pending results.
public final class WispSession: @unchecked Sendable {
    /// Which audio source a result came from.
    public enum Source: Int32, Sendable {
        case mic = 0
        case system = 1
    }

    /// One transcription update from either pipeline.
    public struct Result: Sendable {
        public let source: Source
        public let segmentID: UInt64
        public let text: String
        public let startSeconds: Double
        public let endSeconds: Double
    }

    public typealias OnResult = @Sendable (Result) -> Void
    public typealias OnLog = @Sendable (String) -> Void

    public let micWavURL: URL
    public let systemWavURL: URL

    private let locale: Locale
    private let onResult: OnResult
    private let onLog: OnLog

    // Constructed lazily in start()
    private var engine: AVAudioEngine?
    private var micPipeline: TranscriptionPipeline?
    private var systemCapture: ProcessTapCapture?
    private let sysState = OSAllocatedUnfairLock<SysState>(initialState: .idle)

    private enum SysState {
        case idle
        case building
        case ready(TranscriptionPipeline)
        case failed
    }

    public init(
        outputDir: URL,
        locale: Locale = Locale(identifier: "ja-JP"),
        onResult: @escaping OnResult,
        onLog: @escaping OnLog = { _ in }
    ) throws {
        try FileManager.default.createDirectory(
            at: outputDir,
            withIntermediateDirectories: true
        )
        let ts = ISO8601DateFormatter().string(from: Date())
            .replacingOccurrences(of: ":", with: "-")
        micWavURL = outputDir.appendingPathComponent("mic-\(ts).wav")
        systemWavURL = outputDir.appendingPathComponent("system-\(ts).wav")
        self.locale = locale
        self.onResult = onResult
        self.onLog = onLog
    }

    /// Request permissions, ensure the speech model is installed, build the
    /// mic pipeline, and start both captures. Returns once audio is flowing.
    public func start() async throws {
        // 1. Permissions
        guard await AVAudioApplication.requestRecordPermission() else {
            throw PoCError.permissionDenied("Microphone")
        }
        let speechAuth = await requestSpeechAuthorization()
        guard speechAuth == .authorized else {
            throw PoCError.permissionDenied("Speech recognition (\(speechAuth.rawValue))")
        }

        // 2. Ensure language model is installed (shared by both pipelines).
        let probe = SpeechTranscriber(locale: locale, preset: .progressiveTranscription)
        if let installReq = try await AssetInventory
            .assetInstallationRequest(supporting: [probe])
        {
            onLog("Downloading speech model for \(locale.identifier)...")
            try await installReq.downloadAndInstall()
            onLog("Model ready")
        }

        // 3. Microphone capture (AVAudioEngine)
        let engine = AVAudioEngine()
        let micFormat = engine.inputNode.outputFormat(forBus: 0)
        onLog("[MIC] native format sr=\(micFormat.sampleRate) ch=\(micFormat.channelCount)")

        let onResultLocal = onResult
        let micPipeline = try await TranscriptionPipeline(
            label: "MIC",
            sourceFormat: micFormat,
            wavURL: micWavURL,
            locale: locale,
            onResult: { pipelineResult in
                onResultLocal(Result(
                    source: .mic,
                    segmentID: pipelineResult.segmentID,
                    text: pipelineResult.text,
                    startSeconds: pipelineResult.startSeconds,
                    endSeconds: pipelineResult.endSeconds
                ))
            }
        )
        self.micPipeline = micPipeline

        engine.inputNode.installTap(
            onBus: 0,
            bufferSize: 4096,
            format: micFormat
        ) { buffer, _ in
            micPipeline.push(buffer)
        }
        try engine.start()
        self.engine = engine
        onLog("[MIC] engine started")

        // 4. System audio capture (Process Tap). Pipeline is built lazily
        //    when the first buffer arrives — we don't know the tap's format
        //    until then.
        let sysWavURL = systemWavURL
        let localeLocal = locale
        let onLogLocal = onLog
        let sysStateRef = sysState
        let systemCapture = ProcessTapCapture { buffer in
            // Fast path: pipeline already ready
            let action: (TranscriptionPipeline?, Bool) = sysStateRef.withLock { state -> (
                TranscriptionPipeline?,
                Bool
            ) in
                switch state {
                case .ready(let pipeline): return (pipeline, false)
                case .idle:
                    state = .building
                    return (nil, true)
                case .building, .failed:
                    return (nil, false)
                }
            }
            if let pipeline = action.0 {
                pipeline.push(buffer)
                return
            }
            guard action.1 else { return }

            // First buffer: build the pipeline now that we know the format.
            let format = buffer.format
            onLogLocal(
                "[SYS] first audio buffer — building pipeline (sr=\(format.sampleRate) ch=\(format.channelCount))"
            )
            Task {
                do {
                    let pipeline = try await TranscriptionPipeline(
                        label: "SYS",
                        sourceFormat: format,
                        wavURL: sysWavURL,
                        locale: localeLocal,
                        onResult: { pipelineResult in
                            onResultLocal(Result(
                                source: .system,
                                segmentID: pipelineResult.segmentID,
                                text: pipelineResult.text,
                                startSeconds: pipelineResult.startSeconds,
                                endSeconds: pipelineResult.endSeconds
                            ))
                        }
                    )
                    sysStateRef.withLock { $0 = .ready(pipeline) }
                    pipeline.push(buffer)
                } catch {
                    onLogLocal("[SYS] failed to build pipeline: \(error)")
                    sysStateRef.withLock { $0 = .failed }
                }
            }
        }
        try systemCapture.start()
        self.systemCapture = systemCapture
    }

    /// Stop both captures and wait for pending transcription results to
    /// drain. Idempotent — safe to call from a deinit or a signal handler.
    public func stop() async {
        onLog("Stopping...")
        if let engine {
            engine.inputNode.removeTap(onBus: 0)
            engine.stop()
        }
        engine = nil

        if let systemCapture {
            systemCapture.stop()
        }
        systemCapture = nil

        if let micPipeline {
            await micPipeline.finish()
        }
        micPipeline = nil

        let sysPipeline: TranscriptionPipeline? = sysState.withLock { state in
            if case .ready(let pipeline) = state { return pipeline }
            return nil
        }
        if let sysPipeline {
            await sysPipeline.finish()
        } else {
            onLog("[SYS] no audio was ever received (system was silent)")
        }
        onLog("Stopped.")
    }
}

private func requestSpeechAuthorization() async -> SFSpeechRecognizerAuthorizationStatus {
    await withCheckedContinuation { cont in
        SFSpeechRecognizer.requestAuthorization { status in
            cont.resume(returning: status)
        }
    }
}
