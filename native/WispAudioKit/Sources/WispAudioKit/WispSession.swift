@preconcurrency import AVFoundation
import CoreMedia
import Darwin
import Foundation
import os.lock
import Speech
import Synchronization

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
        public let isFinal: Bool
        public let text: String
        public let startSeconds: Double
        public let endSeconds: Double
        public let confidenceMean: Double?
        public let confidenceMin: Double?
    }

    public typealias OnResult = @Sendable (Result) -> Void
    public typealias OnLog = @Sendable (String) -> Void
    public typealias OnTranscriberError = @Sendable (Bool, String) -> Void
    public typealias OnTerminalError = @Sendable (Source?, String) -> Void
    public typealias OnAudioOverflow = @Sendable (Source, UInt64) -> Void
    public typealias OnAudio = @Sendable (
        Source,
        UInt64,
        Double,
        UInt32,
        UInt32,
        [Float]
    ) -> Void

    public let micOggURL: URL
    public let systemOggURL: URL

    private let locale: Locale
    private let transcriptionEnabled: Bool
    private let allowRecordOnly: Bool
    /// Legacy Swift/C sessions own the complete capture → analyzer path.
    /// The v2 C ABI disables this so Rust's SessionOrchestrator is the sole
    /// owner of analyzer input and captured PCM can never be submitted twice.
    let feedsCapturedAudioDirectlyToAnalyzer: Bool
    private let onResult: OnResult
    private let onAudio: OnAudio?
    private let onAudioOverflow: OnAudioOverflow
    private let onTranscriberError: OnTranscriberError
    private let onTerminalError: OnTerminalError
    private let onLog: OnLog
    private let transcriptionRuntimeEnabled: OSAllocatedUnfairLock<Bool>
    private var speechLocale: Locale?

    // Constructed lazily in start()
    private var engine: AVAudioEngine?
    private var micPipeline: TranscriptionPipeline?
    private var systemCapture: ProcessTapCapture?
    private let sysState = OSAllocatedUnfairLock<SysState>(initialState: .idle)
    private let lifecycleState = OSAllocatedUnfairLock<LifecycleState>(initialState: .initialized)
    private let transcriptionState = OSAllocatedUnfairLock<TranscriptionState>(
        initialState: .initialized
    )
    private let externalPushAdmissions = ExternalPushAdmissionBarrier()
    private let analyzerFinishProgress = OSAllocatedUnfairLock(
        initialState: AnalyzerFinishProgress()
    )

    private var configChangeObserver: NSObjectProtocol?
    private let micEngineLock = OSAllocatedUnfairLock<Void>(initialState: ())
    private let microphoneMuted = OSAllocatedUnfairLock<Bool>(initialState: false)
    private let captureMilestone = CaptureMilestone()
    private let audioClock = OSAllocatedUnfairLock<SessionAudioClock>(
        initialState: SessionAudioClock()
    )
    private lazy var microphoneHandoff = makeAudioHandoff(source: .mic)
    private lazy var systemHandoff = makeAudioHandoff(source: .system)
    /// Converts captured PCM to the 48 kHz input required by the Rust Opus
    /// encoder before it crosses the FFI boundary via `onAudio`.
    private let microphoneResampler = RealtimeResampler()
    private let systemResampler = RealtimeResampler()

    private enum SysState {
        case idle
        case ready(TranscriptionPipeline)
        case failed
        case stopped
    }

    private enum LifecycleState {
        case initialized
        case starting(Task<Void, Error>)
        case started
        case stopping(Task<Void, Never>)
        case stopped
    }

    private enum StartCompletion {
        case started
        case stopped(Task<Void, Never>?)
    }

    private enum TranscriptionState {
        case initialized
        case starting(Task<Void, Error>)
        case started
        case finishing(Task<Void, Error>)
        case finishPending
        case stopped
    }

    private enum SysStopAction {
        case finish(TranscriptionPipeline)
        case silent
        case done
    }

    /// Whether microphone capture reached the running state. Used by the FFI
    /// bridge to decide whether a failed start may still contain recoverable
    /// audio/transcription that must be finalised rather than discarded.
    public var hasStartedCapture: Bool {
        captureMilestone.reached
    }

    /// Replace microphone samples with silence while keeping the microphone
    /// and system-audio timelines aligned.
    public func setMicrophoneMuted(_ muted: Bool) {
        let changed = microphoneMuted.withLock { current in
            guard current != muted else { return false }
            current = muted
            return true
        }
        if changed {
            onLog(muted ? "[MIC] muted" : "[MIC] unmuted")
        }
    }

    public var isMicrophoneMuted: Bool {
        microphoneMuted.withLock { $0 }
    }

    public init(
        outputDir: URL,
        locale: Locale = Locale(identifier: "ja-JP"),
        transcriptionEnabled: Bool = true,
        allowRecordOnly: Bool = false,
        feedsCapturedAudioDirectlyToAnalyzer: Bool = true,
        onResult: @escaping OnResult,
        onAudio: OnAudio? = nil,
        onAudioOverflow: @escaping OnAudioOverflow = { _, _ in },
        onTranscriberError: @escaping OnTranscriberError = { _, _ in },
        onTerminalError: @escaping OnTerminalError = { _, _ in },
        onLog: @escaping OnLog = { _ in }
    ) throws {
        let fileManager = FileManager.default
        try fileManager.createDirectory(
            at: outputDir,
            withIntermediateDirectories: true
        )
        // Keep these names stable so callers can persist the exact paths.
        // Refuse to reuse a completed/partial recording directory instead of
        // silently overwriting its audio.
        let micOggURL = outputDir.appendingPathComponent("mic.ogg")
        let systemOggURL = outputDir.appendingPathComponent("system.ogg")
        if FileManager.default.fileExists(atPath: micOggURL.path)
            || FileManager.default.fileExists(atPath: systemOggURL.path)
        {
            throw PoCError.outputFilesAlreadyExist(outputDir.path)
        }
        let reservationURL = outputDir.appendingPathComponent(".wisp-recording-reserved")
        let reserved = reservationURL.withUnsafeFileSystemRepresentation { path in
            guard let path else { return false }
            let descriptor = Darwin.open(
                path,
                O_CREAT | O_EXCL | O_WRONLY,
                mode_t(S_IRUSR | S_IWUSR)
            )
            guard descriptor >= 0 else { return false }
            Darwin.close(descriptor)
            return true
        }
        guard reserved else {
            throw PoCError.outputFilesAlreadyExist(outputDir.path)
        }
        self.micOggURL = micOggURL
        self.systemOggURL = systemOggURL
        self.locale = locale
        self.transcriptionEnabled = transcriptionEnabled
        self.allowRecordOnly = allowRecordOnly
        self.feedsCapturedAudioDirectlyToAnalyzer = feedsCapturedAudioDirectlyToAnalyzer
        self.onResult = onResult
        self.onAudio = onAudio
        self.onAudioOverflow = onAudioOverflow
        self.onTranscriberError = onTranscriberError
        self.onTerminalError = onTerminalError
        self.onLog = onLog
        transcriptionRuntimeEnabled = OSAllocatedUnfairLock(initialState: false)
        speechLocale = nil
    }

    /// Compatibility start: capture first, then the configured transcriber.
    public func start() async throws {
        try await startCapture()
        guard transcriptionEnabled else {
            onLog("Transcription disabled by policy; recording only")
            return
        }
        do {
            try await startTranscription()
        } catch {
            let captureStillRunning = lifecycleState.withLock {
                if case .started = $0 {
                    return true
                }
                return false
            }
            if allowRecordOnly, captureStillRunning {
                try? await disableTranscription()
                onTranscriberError(true, "SpeechAnalyzer startup failed: \(error)")
                onLog("[ASR] startup failed; continuing record-only: \(error)")
                return
            }
            await abort()
            throw error
        }
    }

    /// Start capture and Ogg recording without touching speech permission,
    /// model inventory, or SpeechAnalyzer.
    public func startCapture() async throws {
        // Claim the one permitted start before the first suspension point.
        // Keeping the actual work in a shared task also gives a concurrent
        // stop a concrete barrier to await before it tears resources down.
        let startTask = try lifecycleState.withLock { state -> Task<Void, Error> in
            switch state {
            case .initialized:
                let task = Task { [self] in
                    try await performCaptureStart()
                }
                state = .starting(task)
                return task
            case .starting:
                throw PoCError.invalidLifecycle("start is already in progress")
            case .started:
                throw PoCError.invalidLifecycle("session is already started")
            case .stopping:
                throw PoCError.invalidLifecycle("session is stopping")
            case .stopped:
                throw PoCError.invalidLifecycle("session has already stopped")
            }
        }

        do {
            try await startTask.value
        } catch {
            await finishFailedStart()
            throw error
        }

        let completion = lifecycleState.withLock { state -> StartCompletion in
            switch state {
            case .starting:
                state = .started
                return .started
            case .stopping(let task):
                return .stopped(task)
            case .stopped:
                return .stopped(nil)
            case .initialized, .started:
                // Neither state is reachable for the sole accepted start.
                // Treat it as a cancelled start instead of publishing a
                // potentially unowned resource set.
                return .stopped(nil)
            }
        }
        switch completion {
        case .started:
            return
        case .stopped(let task):
            if let task {
                await task.value
            }
            throw PoCError.invalidLifecycle("session was stopped while start was in progress")
        }
    }

    private func performCaptureStart() async throws {
        guard await AVAudioApplication.requestRecordPermission() else {
            throw PoCError.permissionDenied("Microphone")
        }

        let engine = AVAudioEngine()
        let micFormat = engine.inputNode.outputFormat(forBus: 0)
        onLog("[MIC] native format sr=\(micFormat.sampleRate) ch=\(micFormat.channelCount)")

        let onResultLocal = onResult
        let transcriberRuntimeState = transcriptionRuntimeEnabled
        let sessionAudioClock = audioClock
        let onTranscriberErrorLocal = onTranscriberError
        let pipelineError: TranscriptionPipeline.OnError = { terminal, message in
            if terminal {
                transcriberRuntimeState.withLock { $0 = false }
            }
            onTranscriberErrorLocal(terminal, message)
        }
        let micPipeline = try TranscriptionPipeline(
            recordingOnlyLabel: "MIC",
            sourceFormat: micFormat,
            oggURL: micOggURL,
            onResult: { pipelineResult in
                guard transcriberRuntimeState.withLock({ $0 }) else { return }
                let offset = sessionAudioClock.withLock {
                    $0.trackStartSeconds(source: .mic)
                }
                onResultLocal(Result(
                    source: .mic,
                    segmentID: pipelineResult.segmentID,
                    isFinal: pipelineResult.isFinal,
                    text: pipelineResult.text,
                    startSeconds: offset + pipelineResult.startSeconds,
                    endSeconds: offset + pipelineResult.endSeconds,
                    confidenceMean: pipelineResult.confidenceMean,
                    confidenceMin: pipelineResult.confidenceMin
                ))
            },
            onError: pipelineError
        )
        self.micPipeline = micPipeline

        // Materialize both fixed pools before either Core Audio callback can
        // run; lazy initialization on a real-time thread would allocate.
        _ = microphoneHandoff
        _ = systemHandoff
        installMicTap(on: engine)
        // Anchor the shared session timeline immediately before capture can
        // begin. Using the first dequeued buffer as the origin would make
        // track offsets depend on worker scheduling: a later system frame
        // drained before an earlier microphone frame would incorrectly erase
        // their capture-time separation.
        audioClock.withLock {
            $0.anchor(at: DispatchTime.now().uptimeNanoseconds)
        }
        // Publish the engine before the throwing start call so failed starts
        // remove the installed tap during transactional cleanup.
        self.engine = engine
        try engine.start()
        captureMilestone.markReached()
        onLog("[MIC] engine started")

        configChangeObserver = NotificationCenter.default.addObserver(
            forName: .AVAudioEngineConfigurationChange,
            object: engine,
            queue: nil
        ) { [weak self] _ in
            self?.handleConfigurationChange()
        }

        // 4. System audio capture (Process Tap). The real-time callback only
        // copies into the preallocated handoff. Its worker constructs the Ogg
        // recorder from the first observed format before exposing that PCM to
        // Rust. SpeechAnalyzer input is then owned either by the legacy
        // compatibility session or exclusively by the v2 orchestrator.
        let systemHandoff = systemHandoff
        let systemCapture = ProcessTapCapture(onRawBuffer: { bufferList, format in
            systemHandoff.enqueue(bufferList: bufferList, format: format)
        })
        try systemCapture.start()
        self.systemCapture = systemCapture
    }

    /// Request speech permission, resolve/install the platform model, and
    /// configure analyzers. Capture remains independently owned and running.
    public func startTranscription() async throws {
        guard lifecycleState.withLock({
            if case .started = $0 {
                return true
            }
            return false
        }) else {
            throw PoCError.invalidLifecycle("capture must be running before transcription")
        }
        let task = try transcriptionState.withLock { state -> Task<Void, Error> in
            switch state {
            case .initialized:
                let task = Task { [self] in try await performTranscriptionStart() }
                state = .starting(task)
                return task
            case .starting:
                throw PoCError.invalidLifecycle("transcription start is already in progress")
            case .started:
                throw PoCError.invalidLifecycle("transcription is already started")
            case .finishing:
                throw PoCError.invalidLifecycle("transcription is finishing")
            case .finishPending:
                throw PoCError.invalidLifecycle("transcription finalization must be retried")
            case .stopped:
                throw PoCError.invalidLifecycle("transcription has already stopped")
            }
        }
        do {
            try await task.value
            let completion = transcriptionState.withLock {
                state -> (published: Bool, shutdown: Task<Void, Error>?) in
                switch state {
                case .starting:
                    externalPushAdmissions.open()
                    state = .started
                    return (true, nil)
                case .finishing(let shutdown):
                    return (false, shutdown)
                case .initialized, .started, .finishPending, .stopped:
                    return (false, nil)
                }
            }
            if let shutdown = completion.shutdown {
                try await shutdown.value
            }
            guard completion.published else {
                throw PoCError.invalidLifecycle(
                    "transcription was stopped while start was in progress"
                )
            }
        } catch {
            transcriptionRuntimeEnabled.withLock { $0 = false }
            let shutdown = transcriptionState.withLock {
                state -> Task<Void, Error>? in
                switch state {
                case .starting:
                    let task = Task<Void, Error> { [self] in
                        await disableAnalyzerResources()
                    }
                    state = .finishing(task)
                    return task
                case .finishing(let task):
                    return task
                case .initialized, .started, .finishPending, .stopped:
                    return nil
                }
            }
            if let shutdown {
                _ = try? await shutdown.value
            }
            transcriptionState.withLock { $0 = .stopped }
            throw error
        }
    }

    private func performTranscriptionStart() async throws {
        try Task.checkCancellation()
        let speechAuth = await requestSpeechAuthorization()
        try Task.checkCancellation()
        guard speechAuth == .authorized else {
            throw PoCError.permissionDenied("Speech recognition (\(speechAuth.rawValue))")
        }
        guard SpeechTranscriber.isAvailable else {
            throw PoCError.speechTranscriberUnavailable
        }
        guard let supported = await SpeechTranscriber.supportedLocale(
            equivalentTo: locale
        ) else {
            throw PoCError.unsupportedSpeechLocale(locale.identifier)
        }
        if supported.identifier != locale.identifier {
            onLog("Using supported speech locale \(supported.identifier) for \(locale.identifier)")
        }
        let probe = makeLiveSpeechTranscriber(locale: supported)
        if let request = try await AssetInventory.assetInstallationRequest(supporting: [probe]) {
            onLog("Downloading speech model for \(supported.identifier)...")
            try await request.downloadAndInstall()
            try Task.checkCancellation()
            onLog("Model ready")
        }
        guard let micPipeline else {
            throw PoCError.invalidLifecycle("microphone recording pipeline is unavailable")
        }
        speechLocale = supported
        _ = try await micPipeline.enableAnalysis(locale: supported, allowRecordOnly: false)
        try Task.checkCancellation()
        if let systemPipeline = sysState.withLock({ state -> TranscriptionPipeline? in
            guard case .ready(let pipeline) = state else { return nil }
            return pipeline
        }) {
            _ = try await systemPipeline.enableAnalysis(locale: supported, allowRecordOnly: false)
        }
        try Task.checkCancellation()
        transcriptionRuntimeEnabled.withLock { $0 = true }
    }

    private func installMicTap(on engine: AVAudioEngine) {
        let format = engine.inputNode.outputFormat(forBus: 0)
        let mutedState = microphoneMuted
        let handoff = microphoneHandoff
        engine.inputNode.installTap(
            onBus: 0,
            bufferSize: 4096,
            format: format
        ) { buffer, _ in
            guard let muted = mutedState.withLockIfAvailable({ $0 }) else {
                handoff.reportOverflow(frames: UInt64(buffer.frameLength))
                return
            }
            handoff.enqueue(buffer, muted: muted)
        }
    }

    private func makeAudioHandoff(source: Source) -> RealtimeAudioHandoff {
        RealtimeAudioHandoff(
            source: source,
            onBuffer: { [weak self] buffer in self?.consumeCaptured(buffer) },
            onOverflow: { [weak self] source, frames in
                self?.onAudioOverflow(source, frames)
            }
        )
    }

    private func consumeCaptured(_ captured: CapturedAudioBuffer) {
        let channels = Int(captured.channels)
        let sourceFormat = AVAudioFormat(
            commonFormat: .pcmFormatFloat32,
            sampleRate: captured.sampleRate,
            channels: AVAudioChannelCount(channels),
            interleaved: false
        )

        switch captured.source {
        case .mic:
            // Recording is owned by the Rust capture backend; no Swift
            // recording write occurs here.
            break
        case .system:
            // Recording is owned by the Rust capture backend. The system
            // pipeline is still built lazily (below) as the analyzer host.
            _ = sysState.withLock { state -> TranscriptionPipeline? in
                switch state {
                case .ready(let pipeline):
                    return pipeline
                case .idle:
                    guard let sourceFormat else {
                        state = .failed
                        onTerminalError(
                            .system,
                            "[SYS] recorder format unavailable"
                        )
                        return nil
                    }
                    do {
                        let pipeline = try TranscriptionPipeline(
                            recordingOnlyLabel: "SYS",
                            sourceFormat: sourceFormat,
                            oggURL: systemOggURL,
                            onResult: makeSystemResultHandler(),
                            onError: makePipelineErrorHandler()
                        )
                        state = .ready(pipeline)
                        return pipeline
                    } catch {
                        state = .failed
                        onTerminalError(
                            .system,
                            "[SYS] recorder initialization failed: \(error)"
                        )
                        return nil
                    }
                case .failed, .stopped:
                    return nil
                }
            }
        }

        if feedsCapturedAudioDirectlyToAnalyzer {
            // This is intentionally synchronous on the non-real-time handoff
            // worker. It preserves capture order and makes drainSynchronously
            // a real barrier before legacy stop() finalizes SpeechAnalyzer.
            // The v2 ABI always disables this path and feeds the same method
            // only after PCM traverses Rust's SessionOrchestrator.
            let semaphore = DispatchSemaphore(value: 0)
            let errorSlot = OSAllocatedUnfairLock<String?>(initialState: nil)
            Task {
                do {
                    try await self.pushCapturedAudioDirectlyToAnalyzer(
                        source: captured.source,
                        sampleRate: UInt32(captured.sampleRate.rounded()),
                        channels: UInt32(captured.channels),
                        samples: captured.samples
                    )
                } catch {
                    errorSlot.withLock { $0 = "\(error)" }
                }
                semaphore.signal()
            }
            semaphore.wait()
            if let error = errorSlot.withLock({ $0 }) {
                onTranscriberError(
                    false,
                    "[\(captured.source == .mic ? "MIC" : "SYS")] analyzer input failed: \(error)"
                )
            }
        }

        if let onAudio {
            let (sequence, timestamp) = audioClock.withLock { state in
                state.next(
                    source: captured.source,
                    frameCount: captured.frameCount,
                    sampleRate: captured.sampleRate,
                    hostNanoseconds: captured.hostNanoseconds
                )
            }
            let resampler = captured.source == .mic ? microphoneResampler : systemResampler
            let samples48k = resampler.resample(
                captured.samples,
                sampleRate: captured.sampleRate,
                channels: channels
            )
            onAudio(
                captured.source,
                sequence,
                timestamp,
                UInt32(RealtimeResampler.outputSampleRate.rounded()),
                UInt32(captured.channels),
                samples48k
            )
        }
    }

    private func makePipelineErrorHandler() -> TranscriptionPipeline.OnError {
        let runtime = transcriptionRuntimeEnabled
        let callback = onTranscriberError
        return { terminal, message in
            if terminal {
                runtime.withLock { $0 = false }
            }
            callback(terminal, message)
        }
    }

    private func makeSystemResultHandler() -> TranscriptionPipeline.OnResult {
        let runtime = transcriptionRuntimeEnabled
        let callback = onResult
        let clock = audioClock
        return { result in
            guard runtime.withLock({ $0 }) else { return }
            let offset = clock.withLock { $0.trackStartSeconds(source: .system) }
            callback(Result(
                source: .system,
                segmentID: result.segmentID,
                isFinal: result.isFinal,
                text: result.text,
                startSeconds: offset + result.startSeconds,
                endSeconds: offset + result.endSeconds,
                confidenceMean: result.confidenceMean,
                confidenceMin: result.confidenceMin
            ))
        }
    }

    /// Feed PCM to SpeechAnalyzer. The v2 ABI invokes this only after Rust's
    /// capture queue and orchestrator; the legacy facade invokes it directly
    /// from the non-real-time capture handoff.
    public func pushTranscriberAudio(
        source: Source,
        sampleRate: UInt32,
        channels: UInt32,
        samples: [Float]
    ) async throws {
        guard transcriptionEnabled else {
            throw PoCError.invalidLifecycle("transcription is disabled by policy")
        }
        let lifecycleError = transcriptionState.withLock { state -> String? in
            switch state {
            case .initialized:
                "transcription has not started"
            case .starting:
                "transcription is starting"
            case .started:
                externalPushAdmissions.admit()
                    ? nil
                    : "transcription input admission is closed"
            case .finishing:
                "transcription is finishing"
            case .finishPending:
                "transcription finalization must be retried"
            case .stopped:
                "transcription has already stopped"
            }
        }
        if let lifecycleError {
            throw PoCError.invalidLifecycle(lifecycleError)
        }
        defer { externalPushAdmissions.release() }
        guard transcriptionRuntimeEnabled.withLock({ $0 }) else {
            throw PoCError.invalidLifecycle("SpeechAnalyzer is not active")
        }
        try await submitTranscriberAudio(
            source: source,
            sampleRate: sampleRate,
            channels: channels,
            samples: samples
        )
    }

    /// Compatibility-owned capture may legitimately overlap analyzer startup
    /// and shutdown. Inactive frames on that internal path are intentionally
    /// ignored, while the public v2/ABI entry point above rejects them.
    private func pushCapturedAudioDirectlyToAnalyzer(
        source: Source,
        sampleRate: UInt32,
        channels: UInt32,
        samples: [Float]
    ) async throws {
        guard transcriptionRuntimeEnabled.withLock({ $0 }) else { return }
        try await submitTranscriberAudio(
            source: source,
            sampleRate: sampleRate,
            channels: channels,
            samples: samples
        )
    }

    /// Submit one frame after its caller has established ownership and
    /// lifecycle admission. External callers are admitted while the
    /// transcriber is `.started`; a concurrent shutdown after that point may
    /// safely finish this already-admitted frame.
    private func submitTranscriberAudio(
        source: Source,
        sampleRate: UInt32,
        channels: UInt32,
        samples: [Float]
    ) async throws {
        guard let buffer = samples.withUnsafeBufferPointer({
            pcmBufferFromInterleaved(
                interleavedSamples: $0,
                sampleRate: Double(sampleRate),
                channels: Int(channels)
            )
        }) else {
            throw PoCError.converterCreationFailed
        }
        switch source {
        case .mic:
            guard let micPipeline else {
                throw PoCError.invalidLifecycle("microphone analyzer is unavailable")
            }
            await micPipeline.pushAnalyzer(buffer)
        case .system:
            guard let locale = speechLocale else {
                throw PoCError.invalidLifecycle("speech locale is unavailable")
            }
            let pipeline = sysState.withLock { state -> TranscriptionPipeline? in
                guard case .ready(let pipeline) = state else { return nil }
                return pipeline
            }
            guard let pipeline else {
                throw PoCError.invalidLifecycle("system recording pipeline is unavailable")
            }
            if try await pipeline.enableAnalysis(
                locale: locale,
                allowRecordOnly: allowRecordOnly
            ) {
                await pipeline.pushAnalyzer(buffer)
            }
        }
    }

    /// Cancel both platform analyzers while preserving recording.
    public func disableTranscription() async throws {
        let shutdown = transcriptionState.withLock {
            state -> Task<Void, Error>? in
            switch state {
            case .initialized:
                state = .stopped
                return nil
            case .starting(let start):
                start.cancel()
                externalPushAdmissions.close()
                let task = Task<Void, Error> { [self] in
                    _ = try? await start.value
                    await externalPushAdmissions.waitUntilDrained()
                    await disableAnalyzerResources()
                }
                state = .finishing(task)
                return task
            case .started, .finishPending:
                externalPushAdmissions.close()
                let task = Task<Void, Error> { [self] in
                    await externalPushAdmissions.waitUntilDrained()
                    await disableAnalyzerResources()
                }
                state = .finishing(task)
                return task
            case .finishing(let task):
                return task
            case .stopped:
                return nil
            }
        }
        if let shutdown {
            try await shutdown.value
        }
        transcriptionRuntimeEnabled.withLock { $0 = false }
        transcriptionState.withLock { $0 = .stopped }
    }

    private func disableAnalyzerResources() async {
        await micPipeline?.disableAnalysis()
        let systemPipeline = sysState.withLock { state -> TranscriptionPipeline? in
            guard case .ready(let pipeline) = state else { return nil }
            return pipeline
        }
        await systemPipeline?.disableAnalysis()
    }

    private func handleConfigurationChange() {
        Task { [weak self] in
            await self?.performConfigurationChange()
        }
    }

    private func performConfigurationChange() async {
        let pair = micEngineLock.withLock { () -> (AVAudioEngine, TranscriptionPipeline)? in
            guard let engine, let micPipeline else { return nil }
            return (engine, micPipeline)
        }
        guard let (engine, micPipeline) = pair else { return }
        let newFormat = engine.inputNode.outputFormat(forBus: 0)
        onLog(
            "[MIC] configuration changed — new input format sr=\(newFormat.sampleRate) ch=\(newFormat.channelCount)"
        )
        guard newFormat.sampleRate > 0, newFormat.channelCount > 0 else {
            onLog("[MIC] input format not ready yet — waiting for next change")
            return
        }
        guard await micPipeline.reconfigure(sourceFormat: newFormat) else {
            onLog("[MIC] skipped restart because converter rebuild failed")
            return
        }
        micEngineLock.withLock {
            guard self.engine === engine else { return }
            engine.inputNode.removeTap(onBus: 0)
            installMicTap(on: engine)
            do {
                if !engine.isRunning {
                    try engine.start()
                }
                onLog("[MIC] engine restarted after device switch")
            } catch {
                onTerminalError(
                    .mic,
                    "[MIC] failed to restart engine after device switch: \(error)"
                )
            }
        }
    }

    /// Compatibility stop: stop producers/recording, then finalize analyzers.
    public func stop() async {
        await stopCapture()
        try? await finishTranscription()
    }

    /// Stop capture and discard staged PCM/transcript work.
    public func abort() async {
        await shutdownCapture(graceful: false)
        try? await disableTranscription()
        micPipeline = nil
        sysState.withLock { state in
            state = .stopped
        }
        transcriptionRuntimeEnabled.withLock { $0 = false }
        transcriptionState.withLock { $0 = .stopped }
    }

    /// Stop capture producers, synchronously drain callback handoffs, and
    /// finish recording. Analyzer input remains open for Rust's capture drain.
    public func stopCapture() async {
        await shutdownCapture(graceful: true)
    }

    private func shutdownCapture(graceful: Bool) async {
        let stopTask = lifecycleState.withLock { state -> Task<Void, Never>? in
            switch state {
            case .initialized:
                // A pre-start stop permanently consumes the session. This is
                // deliberately distinct from a reusable reset operation.
                state = .stopped
                return nil
            case .starting(let startTask):
                let task = Task { [self] in
                    _ = try? await startTask.value
                    await performCaptureStop(graceful: graceful)
                }
                state = .stopping(task)
                return task
            case .started:
                let task = Task { [self] in
                    await performCaptureStop(graceful: graceful)
                }
                state = .stopping(task)
                return task
            case .stopping(let task):
                return task
            case .stopped:
                return nil
            }
        }
        guard let stopTask else { return }
        await stopTask.value
        lifecycleState.withLock { $0 = .stopped }
    }

    private func finishFailedStart() async {
        let cleanupTask = lifecycleState.withLock { state -> Task<Void, Never>? in
            switch state {
            case .starting, .started:
                let task = Task { [self] in
                    await performCaptureStop(graceful: false)
                }
                state = .stopping(task)
                return task
            case .stopping(let task):
                return task
            case .initialized:
                state = .stopped
                return nil
            case .stopped:
                return nil
            }
        }
        if let cleanupTask {
            await cleanupTask.value
        }
        lifecycleState.withLock { $0 = .stopped }
    }

    private func performCaptureStop(graceful: Bool) async {
        onLog(graceful ? "Stopping..." : "Aborting...")

        if let observer = configChangeObserver {
            NotificationCenter.default.removeObserver(observer)
            configChangeObserver = nil
        }
        micEngineLock.withLock {
            if let engine {
                engine.inputNode.removeTap(onBus: 0)
                engine.stop()
            }
            engine = nil
        }

        if let systemCapture {
            systemCapture.stop()
        }
        systemCapture = nil

        if graceful {
            microphoneHandoff.drainSynchronously()
            systemHandoff.drainSynchronously()
        } else {
            microphoneHandoff.discardSynchronously()
            systemHandoff.discardSynchronously()
        }

        if let micPipeline {
            if graceful {
                await micPipeline.finishRecording()
            } else {
                await micPipeline.abortRecording()
            }
        }

        let action = sysState.withLock { state -> SysStopAction in
            switch state {
            case .ready(let pipeline):
                return .finish(pipeline)
            case .idle:
                state = .stopped
                return .silent
            case .failed:
                state = .stopped
                return .done
            case .stopped:
                return .done
            }
        }
        switch action {
        case .finish(let pipeline):
            if graceful {
                await pipeline.finishRecording()
            } else {
                await pipeline.abortRecording()
            }
        case .silent:
            if graceful {
                onLog("[SYS] no audio was ever received (system was silent)")
            }
        case .done:
            break
        }
        onLog(graceful ? "Stopped." : "Aborted.")
    }

    /// Close analyzer input only after Rust has drained capture PCM through the
    /// orchestrator, then wait for final result callbacks.
    public func finishTranscription() async throws {
        let task = transcriptionState.withLock { state -> Task<Void, Error>? in
            switch state {
            case .initialized, .stopped:
                state = .stopped
                return nil
            case .starting(let start):
                start.cancel()
                externalPushAdmissions.close()
                let task = Task<Void, Error> { [self] in
                    _ = try? await start.value
                    await externalPushAdmissions.waitUntilDrained()
                    await disableAnalyzerResources()
                }
                state = .finishing(task)
                return task
            case .started, .finishPending:
                externalPushAdmissions.close()
                let task = Task<Void, Error> { [self] in
                    await externalPushAdmissions.waitUntilDrained()
                    try await finishAnalyzerResources()
                }
                state = .finishing(task)
                return task
            case .finishing(let task):
                return task
            }
        }
        do {
            if let task {
                try await task.value
            }
        } catch {
            transcriptionState.withLock { $0 = .finishPending }
            throw error
        }
        transcriptionRuntimeEnabled.withLock { $0 = false }
        transcriptionState.withLock { $0 = .stopped }
        micPipeline = nil
        sysState.withLock { $0 = .stopped }
    }

    private func finishAnalyzerResources() async throws {
        let systemPipeline = sysState.withLock { state -> TranscriptionPipeline? in
            guard case .ready(let pipeline) = state else { return nil }
            return pipeline
        }
        let microphoneFinish = micPipeline.map { pipeline in
            { @Sendable in try await pipeline.finishAnalysis() }
        }
        let systemFinish = systemPipeline.map { pipeline in
            { @Sendable in try await pipeline.finishAnalysis() }
        }
        try await finishAnalyzerOperations(
            progress: analyzerFinishProgress,
            microphone: microphoneFinish,
            system: systemFinish
        )
    }
}

struct AnalyzerFinishProgress {
    var microphone = false
    var system = false
}

/// Finalize both independent analyzer tracks even when one fails. Completed
/// tracks are marked immediately so a later ABI/Rust retry touches only the
/// unfinished work.
func finishAnalyzerOperations(
    progress: OSAllocatedUnfairLock<AnalyzerFinishProgress>,
    microphone: (@Sendable () async throws -> Void)?,
    system: (@Sendable () async throws -> Void)?
) async throws {
    var failures: [String] = []
    if progress.withLock({ !$0.microphone }) {
        do {
            try await microphone?()
            progress.withLock { $0.microphone = true }
        } catch {
            failures.append("microphone: \(error)")
        }
    }
    if progress.withLock({ !$0.system }) {
        do {
            try await system?()
            progress.withLock { $0.system = true }
        } catch {
            failures.append("system: \(error)")
        }
    }
    if !failures.isEmpty {
        throw PoCError.analyzerFinalizationFailed(failures.joined(separator: "; "))
    }
}

/// Linearizable admission barrier for synchronous C ABI pushes which execute
/// their Swift work in detached tasks. Lifecycle code closes admissions while
/// holding `transcriptionState`, then waits here without holding that lock.
final class ExternalPushAdmissionBarrier: @unchecked Sendable {
    private struct State {
        var accepting = false
        var inFlight = 0
        var waiters: [CheckedContinuation<Void, Never>] = []
    }

    private let state = OSAllocatedUnfairLock(initialState: State())

    func open() {
        state.withLock {
            precondition(!$0.accepting && $0.inFlight == 0)
            $0.accepting = true
        }
    }

    func admit() -> Bool {
        state.withLock {
            guard $0.accepting else { return false }
            $0.inFlight += 1
            return true
        }
    }

    func close() {
        state.withLock { $0.accepting = false }
    }

    func release() {
        let waiters = state.withLock { state -> [CheckedContinuation<Void, Never>] in
            precondition(state.inFlight > 0)
            state.inFlight -= 1
            guard state.inFlight == 0, !state.accepting else { return [] }
            defer { state.waiters.removeAll(keepingCapacity: true) }
            return state.waiters
        }
        for waiter in waiters {
            waiter.resume()
        }
    }

    func waitUntilDrained() async {
        await withCheckedContinuation { continuation in
            let resumeImmediately = state.withLock { state -> Bool in
                precondition(!state.accepting)
                guard state.inFlight > 0 else { return true }
                state.waiters.append(continuation)
                return false
            }
            if resumeImmediately {
                continuation.resume()
            }
        }
    }
}

final class CaptureMilestone: @unchecked Sendable {
    private let state = OSAllocatedUnfairLock<Bool>(initialState: false)

    var reached: Bool {
        state.withLock { $0 }
    }

    func markReached() {
        state.withLock { $0 = true }
    }
}

struct CapturedAudioBuffer {
    let source: WispSession.Source
    let sampleRate: Double
    let channels: Int
    let frameCount: UInt64
    let hostNanoseconds: UInt64
    let samples: [Float]

    func makePCMBuffer() -> AVAudioPCMBuffer? {
        samples.withUnsafeBufferPointer {
            pcmBufferFromInterleaved(
                interleavedSamples: $0,
                sampleRate: sampleRate,
                channels: channels
            )
        }
    }
}

private func pcmBufferFromInterleaved(
    interleavedSamples samples: UnsafeBufferPointer<Float>,
    sampleRate: Double,
    channels: Int
) -> AVAudioPCMBuffer? {
    guard sampleRate > 0,
          channels > 0,
          samples.count.isMultiple(of: channels),
          let format = AVAudioFormat(
              commonFormat: .pcmFormatFloat32,
              sampleRate: sampleRate,
              channels: AVAudioChannelCount(channels),
              interleaved: false
          )
    else {
        return nil
    }
    let frameCount = samples.count / channels
    guard let buffer = AVAudioPCMBuffer(
        pcmFormat: format,
        frameCapacity: AVAudioFrameCount(frameCount)
    ), let channelData = buffer.floatChannelData
    else {
        return nil
    }
    buffer.frameLength = AVAudioFrameCount(frameCount)
    for frame in 0 ..< frameCount {
        for channel in 0 ..< channels {
            channelData[channel][frame] = samples[frame * channels + channel]
        }
    }
    return buffer
}

/// Fixed-capacity, preallocated Core Audio callback handoff.
///
/// Producers only attempt unfair locks (`withLockIfAvailable`), copy into
/// existing arrays, and signal dispatch data sources. Allocation, Swift→C
/// callbacks, Rust Vec construction, recorder work, and clock locks all run
/// on the serial consumer queue. Contention/capacity exhaustion is explicit.
final class RealtimeAudioHandoff: @unchecked Sendable {
    static let slotCount = 8
    static let maximumSamplesPerBuffer = 262_144

    private struct SlotState {
        var ready = false
        var sampleRate = 0.0
        var channels = 0
        var frameCount: UInt64 = 0
        var hostNanoseconds: UInt64 = 0
        var samples = [Float](
            repeating: 0,
            count: RealtimeAudioHandoff.maximumSamplesPerBuffer
        )
    }

    private enum SlotAdmission {
        case accepted
        case occupied
        case overflowPending
    }

    private let source: WispSession.Source
    private let slots: [OSAllocatedUnfairLock<SlotState>]
    private let queue: DispatchQueue
    private let readySource: DispatchSourceUserDataAdd
    private let overflowSource: DispatchSourceUserDataAdd
    /// A nonzero value is both the producer gate and the exact number of
    /// dropped frames which still need an overflow marker. Keeping those two
    /// facts in one atomic prevents a generation/count acknowledgement race.
    private let pendingDroppedFrames = Atomic<UInt64>(0)
    private let onBuffer: @Sendable (CapturedAudioBuffer) -> Void
    private let onOverflow: @Sendable (WispSession.Source, UInt64) -> Void
    /// Deterministic test seam for forcing overflow between a producer's
    /// optimistic gate check and its admission commit. Nil in production.
    private let beforeAdmissionCommit: (@Sendable () -> Void)?

    private var overflowIsPending: Bool {
        pendingDroppedFrames.load(ordering: .acquiring) > 0
    }

    init(
        source: WispSession.Source,
        slotCount: Int = RealtimeAudioHandoff.slotCount,
        onBuffer: @escaping @Sendable (CapturedAudioBuffer) -> Void,
        onOverflow: @escaping @Sendable (WispSession.Source, UInt64) -> Void,
        beforeAdmissionCommit: (@Sendable () -> Void)? = nil
    ) {
        self.source = source
        self.onBuffer = onBuffer
        self.onOverflow = onOverflow
        self.beforeAdmissionCommit = beforeAdmissionCommit
        slots = (0 ..< max(1, slotCount)).map { _ in
            OSAllocatedUnfairLock(initialState: SlotState())
        }
        queue = DispatchQueue(
            label: "dev.mokmok.wisp.audio-handoff.\(source.rawValue)",
            qos: .userInitiated
        )
        readySource = DispatchSource.makeUserDataAddSource(queue: queue)
        overflowSource = DispatchSource.makeUserDataAddSource(queue: queue)
        readySource.setEventHandler { [weak self] in self?.drainReadySlots() }
        overflowSource.setEventHandler { [weak self] in
            guard let self else { return }
            _ = overflowSource.data
            publishPendingOverflow()
        }
        readySource.resume()
        overflowSource.resume()
    }

    func enqueue(_ buffer: AVAudioPCMBuffer, muted: Bool) {
        let frameCount = Int(buffer.frameLength)
        if overflowIsPending {
            reportOverflow(frames: UInt64(max(0, frameCount)))
            return
        }
        let channels = Int(buffer.format.channelCount)
        let (sampleCount, arithmeticOverflow) =
            frameCount.multipliedReportingOverflow(by: channels)
        guard !arithmeticOverflow,
              frameCount > 0,
              channels > 0,
              sampleCount <= Self.maximumSamplesPerBuffer,
              let channelData = buffer.floatChannelData
        else {
            reportOverflow(frames: UInt64(max(0, frameCount)))
            return
        }

        for slot in slots {
            let admission = slot.withLockIfAvailableUnchecked { state -> SlotAdmission in
                guard !state.ready else { return .occupied }
                if muted {
                    state.samples.withUnsafeMutableBufferPointer {
                        $0.baseAddress?.initialize(repeating: 0, count: sampleCount)
                    }
                } else if buffer.format.isInterleaved {
                    state.samples.withUnsafeMutableBufferPointer {
                        $0.baseAddress?.update(from: channelData[0], count: sampleCount)
                    }
                } else {
                    for frame in 0 ..< frameCount {
                        for channel in 0 ..< channels {
                            state.samples[frame * channels + channel] =
                                channelData[channel][frame]
                        }
                    }
                }
                state.sampleRate = buffer.format.sampleRate
                state.channels = channels
                state.frameCount = UInt64(frameCount)
                state.hostNanoseconds = DispatchTime.now().uptimeNanoseconds
                beforeAdmissionCommit?()
                // An overflow may have raced the optimistic check above while
                // this producer copied PCM. The slot lock makes the commit a
                // linearization point: overflow draining either observes this
                // as pre-gap PCM or this check rejects it into the gap count.
                guard !overflowIsPending else { return .overflowPending }
                state.ready = true
                return .accepted
            }
            switch admission {
            case .accepted:
                readySource.add(data: 1)
                return
            case .overflowPending:
                reportOverflow(frames: UInt64(frameCount))
                return
            case .occupied, nil:
                continue
            }
        }
        reportOverflow(frames: UInt64(frameCount))
    }

    func enqueue(
        bufferList: UnsafePointer<AudioBufferList>,
        format: AVAudioFormat
    ) {
        let buffers = UnsafeMutableAudioBufferListPointer(
            UnsafeMutablePointer(mutating: bufferList)
        )
        if overflowIsPending {
            let bytesPerFrame = max(
                1,
                UInt64(format.streamDescription.pointee.mBytesPerFrame)
            )
            let estimatedFrames = buffers.first.map {
                UInt64($0.mDataByteSize) / bytesPerFrame
            } ?? 1
            reportOverflow(frames: max(1, estimatedFrames))
            return
        }
        guard let layout = packedNativeFloat32Layout(
            buffers: buffers,
            format: format,
            maximumSamples: Self.maximumSamplesPerBuffer
        ) else {
            reportOverflow(frames: 1)
            return
        }

        for slot in slots {
            let admission = slot.withLockIfAvailableUnchecked { state -> SlotAdmission in
                guard !state.ready else { return .occupied }
                if layout.interleaved {
                    // Every pointer, byte count, channel count, and ASBD field
                    // was validated before entering unsafe typed reads.
                    let data = buffers[0].mData!
                    let input = data.assumingMemoryBound(to: Float.self)
                    state.samples.withUnsafeMutableBufferPointer {
                        $0.baseAddress?.update(from: input, count: layout.sampleCount)
                    }
                } else {
                    for channel in 0 ..< layout.channels {
                        let data = buffers[channel].mData!
                        let input = data.assumingMemoryBound(to: Float.self)
                        for frame in 0 ..< layout.frameCount {
                            state.samples[frame * layout.channels + channel] = input[frame]
                        }
                    }
                }
                state.sampleRate = format.sampleRate
                state.channels = layout.channels
                state.frameCount = UInt64(layout.frameCount)
                state.hostNanoseconds = DispatchTime.now().uptimeNanoseconds
                beforeAdmissionCommit?()
                guard !overflowIsPending else { return .overflowPending }
                state.ready = true
                return .accepted
            }
            switch admission {
            case .accepted:
                readySource.add(data: 1)
                return
            case .overflowPending:
                reportOverflow(frames: UInt64(layout.frameCount))
                return
            case .occupied, nil:
                continue
            }
        }
        reportOverflow(frames: UInt64(layout.frameCount))
    }

    func reportOverflow(frames: UInt64) {
        guard frames > 0 else { return }
        addPendingDroppedFrames(frames)
        // The dispatch source is only a wake-up; the UInt64 atomic owns the
        // count so Dispatch's platform-sized coalescing cannot truncate it.
        overflowSource.add(data: 1)
    }

    func drainSynchronously() {
        // Both PCM and overflow notifications use this queue, so this is one
        // total-order barrier for the native producer handoff.
        queue.sync {
            drainReadySlots()
            publishPendingOverflow()
        }
    }

    func discardSynchronously() {
        queue.sync {
            for slot in slots {
                slot.withLock { $0.ready = false }
            }
            _ = pendingDroppedFrames.exchange(0, ordering: .acquiringAndReleasing)
        }
    }

    private func addPendingDroppedFrames(_ frames: UInt64) {
        var observed = pendingDroppedFrames.load(ordering: .acquiring)
        while true {
            let (sum, overflow) = observed.addingReportingOverflow(frames)
            let desired = overflow ? UInt64.max : sum
            let result = pendingDroppedFrames.compareExchange(
                expected: observed,
                desired: desired,
                ordering: .acquiringAndReleasing
            )
            if result.exchanged {
                return
            }
            observed = result.original
        }
    }

    private func publishPendingOverflow() {
        // Any slot committed before the first pending increment is pre-gap
        // PCM. Drain it before atomically reopening the gate.
        drainReadySlots()
        let dropped = pendingDroppedFrames.exchange(
            0,
            ordering: .acquiringAndReleasing
        )
        if dropped > 0 {
            onOverflow(source, dropped)
        }
    }

    private func drainReadySlots() {
        for slot in slots {
            slot.withLock { state in
                guard state.ready else { return }
                let sampleCount = Int(state.frameCount) * state.channels
                let samples = Array(state.samples.prefix(sampleCount))
                onBuffer(CapturedAudioBuffer(
                    source: source,
                    sampleRate: state.sampleRate,
                    channels: state.channels,
                    frameCount: state.frameCount,
                    hostNanoseconds: state.hostNanoseconds,
                    samples: samples
                ))
                state.ready = false
            }
        }
    }
}

/// Resamples interleaved `Float32` PCM down to the 48 kHz input required by
/// the Rust Opus encoder. The converter is cached per (sample rate, channel
/// count) and rebuilt when the device format changes. When the source is
/// already 48 kHz the input is passed through unchanged with no copy.
final class RealtimeResampler: @unchecked Sendable {
    static let outputSampleRate: Double = 48000

    private let state = OSAllocatedUnfairLock<State>(initialState: State())

    private struct State {
        var converter: AVAudioConverter?
        var sourceFormat: AVAudioFormat?
        var outputFormat: AVAudioFormat?
    }

    /// Converts one interleaved `Float32` chunk from `sampleRate` to 48 kHz,
    /// returning interleaved `Float32`.
    func resample(_ samples: [Float], sampleRate: Double, channels: Int) -> [Float] {
        guard sampleRate.isFinite, sampleRate > 0, channels > 0 else { return samples }
        if sampleRate == Self.outputSampleRate {
            return samples
        }
        guard let sourceFormat = AVAudioFormat(
            commonFormat: .pcmFormatFloat32,
            sampleRate: sampleRate,
            channels: AVAudioChannelCount(channels),
            interleaved: false
        ), let outputFormat = AVAudioFormat(
            commonFormat: .pcmFormatFloat32,
            sampleRate: Self.outputSampleRate,
            channels: AVAudioChannelCount(channels),
            interleaved: false
        ) else {
            return samples
        }

        let converter = state.withLock { state -> AVAudioConverter? in
            if state.sourceFormat?.sampleRate == sourceFormat.sampleRate,
               state.sourceFormat?.channelCount == sourceFormat.channelCount
            {
                return state.converter
            }
            guard let converter = AVAudioConverter(from: sourceFormat, to: outputFormat) else {
                return nil
            }
            state.converter = converter
            state.sourceFormat = sourceFormat
            state.outputFormat = outputFormat
            return converter
        }
        guard let converter else { return samples }

        guard let inputBuffer = samples.withUnsafeBufferPointer({ ptr in
            pcmBufferFromInterleaved(
                interleavedSamples: ptr,
                sampleRate: sampleRate,
                channels: channels
            )
        }) else { return samples }

        let inputFrames = samples.count / channels
        let ratio = Self.outputSampleRate / sampleRate
        let capacity = Int((Double(inputFrames) * ratio).rounded(.up))
        guard capacity > 0, capacity <= Int(Int32.max),
              let outputBuffer = AVAudioPCMBuffer(
                  pcmFormat: outputFormat,
                  frameCapacity: AVAudioFrameCount(capacity)
              )
        else { return samples }

        var convertedFrames = 0
        let consumed = ConverterInputFlag()
        let status = converter.convert(
            to: outputBuffer,
            error: nil,
            withInputFrom: { _, outStatus in
                if consumed.value {
                    outStatus.pointee = .noDataNow
                    return nil
                }
                consumed.value = true
                outStatus.pointee = .haveData
                return inputBuffer
            }
        )
        if status == .error {
            return samples
        }
        convertedFrames = Int(outputBuffer.frameLength)
        guard convertedFrames > 0 else { return samples }

        return interleavedSamples(
            from: outputBuffer,
            frameCount: convertedFrames,
            channels: channels
        )
    }

    private func interleavedSamples(
        from buffer: AVAudioPCMBuffer,
        frameCount: Int,
        channels: Int
    ) -> [Float] {
        guard let channelData = buffer.floatChannelData else { return [] }
        var out = [Float](repeating: 0, count: frameCount * channels)
        out.withUnsafeMutableBufferPointer { outBuf in
            for channel in 0 ..< channels {
                for frame in 0 ..< frameCount {
                    outBuf[frame * channels + channel] = channelData[channel][frame]
                }
            }
        }
        return out
    }
}

private final class ConverterInputFlag: @unchecked Sendable {
    var value = false
}

struct PackedFloat32Layout {
    let interleaved: Bool
    let channels: Int
    let frameCount: Int
    let sampleCount: Int
    let bytesPerBuffer: Int
}

/// Validate the complete HAL layout before any `mData` pointer is rebound to
/// `Float`. Only packed, native-endian, 32-bit Float LPCM is accepted.
func packedNativeFloat32Layout(
    buffers: UnsafeMutableAudioBufferListPointer,
    format: AVAudioFormat,
    maximumSamples: Int
) -> PackedFloat32Layout? {
    let asbd = format.streamDescription.pointee
    let channels = Int(asbd.mChannelsPerFrame)
    guard asbd.mFormatID == kAudioFormatLinearPCM,
          asbd.mSampleRate.isFinite,
          asbd.mSampleRate > 0,
          asbd.mSampleRate.rounded() <= Double(UInt32.max),
          channels > 0,
          channels == Int(format.channelCount),
          asbd.mBitsPerChannel == 32,
          asbd.mFramesPerPacket == 1
    else {
        return nil
    }

    let requiredFlags = AudioFormatFlags(
        kAudioFormatFlagIsFloat | kAudioFormatFlagIsPacked
    )
    let forbiddenFlags = AudioFormatFlags(
        kAudioFormatFlagIsBigEndian
            | kAudioFormatFlagIsSignedInteger
            | kAudioFormatFlagIsAlignedHigh
    )
    guard asbd.mFormatFlags & requiredFlags == requiredFlags,
          asbd.mFormatFlags & forbiddenFlags == 0
    else {
        return nil
    }

    let nonInterleaved =
        asbd.mFormatFlags & AudioFormatFlags(kAudioFormatFlagIsNonInterleaved) != 0
    let interleaved = !nonInterleaved
    guard interleaved == format.isInterleaved else { return nil }

    let (packedFrameBytes, frameByteOverflow) =
        (interleaved ? channels : 1).multipliedReportingOverflow(
            by: MemoryLayout<Float>.stride
        )
    guard !frameByteOverflow,
          packedFrameBytes > 0,
          let packedFrameBytes32 = UInt32(exactly: packedFrameBytes),
          asbd.mBytesPerFrame == packedFrameBytes32,
          asbd.mBytesPerPacket == packedFrameBytes32,
          buffers.count == (interleaved ? 1 : channels)
    else {
        return nil
    }

    guard let first = buffers.first,
          first.mData != nil,
          first.mDataByteSize > 0,
          Int(first.mDataByteSize).isMultiple(of: packedFrameBytes)
    else {
        return nil
    }
    let frameCount = Int(first.mDataByteSize) / packedFrameBytes
    let (requiredBytes, byteOverflow) =
        frameCount.multipliedReportingOverflow(by: packedFrameBytes)
    let (sampleCount, sampleOverflow) =
        frameCount.multipliedReportingOverflow(by: channels)
    guard !byteOverflow,
          !sampleOverflow,
          frameCount > 0,
          sampleCount <= maximumSamples,
          buffers.enumerated().allSatisfy({ index, buffer in
              buffer.mData.map {
                  Int(bitPattern: $0).isMultiple(of: MemoryLayout<Float>.alignment)
              } == true
                  && Int(buffer.mDataByteSize) == requiredBytes
                  && Int(buffer.mNumberChannels) == (interleaved ? channels : 1)
                  && (interleaved ? index == 0 : index < channels)
          })
    else {
        return nil
    }
    return PackedFloat32Layout(
        interleaved: interleaved,
        channels: channels,
        frameCount: frameCount,
        sampleCount: sampleCount,
        bytesPerBuffer: requiredBytes
    )
}

struct SessionAudioClock {
    private struct TrackState {
        var sequence: UInt64 = 0
        var endSeconds: Double = 0
        var startSeconds: Double?
    }

    private var originNanoseconds: UInt64?
    private var microphone = TrackState()
    private var system = TrackState()

    mutating func anchor(at hostNanoseconds: UInt64) {
        if originNanoseconds == nil {
            originNanoseconds = hostNanoseconds
        }
    }

    mutating func next(
        source: WispSession.Source,
        frameCount: UInt64,
        sampleRate: Double,
        hostNanoseconds: UInt64
    ) -> (sequence: UInt64, timestamp: Double) {
        precondition(sampleRate > 0)
        let origin = originNanoseconds ?? hostNanoseconds
        originNanoseconds = origin
        let elapsedNanoseconds = hostNanoseconds >= origin ? hostNanoseconds - origin : 0
        let observedEnd = Double(elapsedNanoseconds) / 1_000_000_000
        let duration = Double(frameCount) / sampleRate

        switch source {
        case .mic:
            let timestamp = max(microphone.endSeconds, max(0, observedEnd - duration))
            let sequence = microphone.sequence
            if microphone.startSeconds == nil {
                microphone.startSeconds = timestamp
            }
            microphone.sequence &+= 1
            microphone.endSeconds = timestamp + duration
            return (sequence, timestamp)
        case .system:
            let timestamp = max(system.endSeconds, max(0, observedEnd - duration))
            let sequence = system.sequence
            if system.startSeconds == nil {
                system.startSeconds = timestamp
            }
            system.sequence &+= 1
            system.endSeconds = timestamp + duration
            return (sequence, timestamp)
        }
    }

    func trackStartSeconds(source: WispSession.Source) -> Double {
        switch source {
        case .mic:
            microphone.startSeconds ?? 0
        case .system:
            system.startSeconds ?? 0
        }
    }
}

private func requestSpeechAuthorization() async -> SFSpeechRecognizerAuthorizationStatus {
    await withCheckedContinuation { cont in
        SFSpeechRecognizer.requestAuthorization { status in
            cont.resume(returning: status)
        }
    }
}
