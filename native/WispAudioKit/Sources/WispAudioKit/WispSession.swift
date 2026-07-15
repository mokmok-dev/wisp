@preconcurrency import AVFoundation
import CoreMedia
import Darwin
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
    private let lifecycleState = OSAllocatedUnfairLock<LifecycleState>(initialState: .initialized)

    private var configChangeObserver: NSObjectProtocol?
    private let micEngineLock = OSAllocatedUnfairLock<Void>(initialState: ())

    private enum SysState {
        case idle
        case building(Task<Void, Never>)
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

    private enum SysStopAction {
        case waitForBuild(Task<Void, Never>)
        case finish(TranscriptionPipeline)
        case silent
        case done
    }

    /// Whether microphone capture reached the running state. Used by the FFI
    /// bridge to decide whether a failed start may still contain recoverable
    /// audio/transcription that must be finalised rather than discarded.
    public var hasStartedCapture: Bool {
        micEngineLock.withLock { engine != nil }
    }

    public init(
        outputDir: URL,
        locale: Locale = Locale(identifier: "ja-JP"),
        onResult: @escaping OnResult,
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
        let micWavURL = outputDir.appendingPathComponent("mic.wav")
        let systemWavURL = outputDir.appendingPathComponent("system.wav")
        if FileManager.default.fileExists(atPath: micWavURL.path)
            || FileManager.default.fileExists(atPath: systemWavURL.path)
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
        self.micWavURL = micWavURL
        self.systemWavURL = systemWavURL
        self.locale = locale
        self.onResult = onResult
        self.onLog = onLog
    }

    /// Request permissions, ensure the speech model is installed, build the
    /// mic pipeline, and start both captures. Returns once audio is flowing.
    public func start() async throws {
        // Claim the one permitted start before the first suspension point.
        // Keeping the actual work in a shared task also gives a concurrent
        // stop a concrete barrier to await before it tears resources down.
        let startTask = try lifecycleState.withLock { state -> Task<Void, Error> in
            switch state {
            case .initialized:
                let task = Task { [self] in
                    try await performStart()
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
            if let task { await task.value }
            throw PoCError.invalidLifecycle("session was stopped while start was in progress")
        }
    }

    private func performStart() async throws {
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

        installMicTap(on: engine, pipeline: micPipeline)
        // Publish the engine before the throwing start call so failed starts
        // remove the installed tap during transactional cleanup.
        self.engine = engine
        try engine.start()
        onLog("[MIC] engine started")

        configChangeObserver = NotificationCenter.default.addObserver(
            forName: .AVAudioEngineConfigurationChange,
            object: engine,
            queue: nil
        ) { [weak self] _ in
            self?.handleConfigurationChange()
        }

        // 4. System audio capture (Process Tap). Pipeline is built lazily
        //    when the first buffer arrives — we don't know the tap's format
        //    until then.
        let sysWavURL = systemWavURL
        let localeLocal = locale
        let onLogLocal = onLog
        let sysStateRef = sysState
        let systemCapture = ProcessTapCapture { buffer in
            sysStateRef.withLock { state in
                switch state {
                case .ready(let pipeline):
                    // Keep push inside the state lock. stop() takes the same
                    // lock before finishing the pipeline, so a callback that
                    // is already in flight cannot race finish().
                    pipeline.push(buffer)
                case .idle:
                    // First buffer: build the pipeline now that we know the
                    // format. Retaining this task in SysState lets stop()
                    // await it before the FFI caller releases its context.
                    let format = buffer.format
                    let buildTask = Task {
                        onLogLocal(
                            "[SYS] first audio buffer — building pipeline (sr=\(format.sampleRate) ch=\(format.channelCount))"
                        )
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
                            let accepted = sysStateRef.withLock { state in
                                guard case .building = state else { return false }
                                // Push before publishing .ready. A stopping
                                // caller either waits for this whole task or
                                // observes a fully initialized pipeline.
                                pipeline.push(buffer)
                                state = .ready(pipeline)
                                return true
                            }
                            if !accepted {
                                await pipeline.finish()
                            }
                        } catch {
                            onLogLocal("[SYS] failed to build pipeline: \(error)")
                            sysStateRef.withLock { state in
                                if case .building = state {
                                    state = .failed
                                }
                            }
                        }
                    }
                    state = .building(buildTask)
                case .building, .failed, .stopped:
                    break
                }
            }
        }
        try systemCapture.start()
        self.systemCapture = systemCapture
    }

    private func installMicTap(on engine: AVAudioEngine, pipeline: TranscriptionPipeline) {
        let format = engine.inputNode.outputFormat(forBus: 0)
        engine.inputNode.installTap(
            onBus: 0,
            bufferSize: 4096,
            format: format
        ) { buffer, _ in
            pipeline.push(buffer)
        }
    }

    private func handleConfigurationChange() {
        micEngineLock.withLock {
            guard let engine, let micPipeline else { return }

            let newFormat = engine.inputNode.outputFormat(forBus: 0)
            onLog(
                "[MIC] configuration changed — new input format sr=\(newFormat.sampleRate) ch=\(newFormat.channelCount)"
            )

            guard newFormat.sampleRate > 0, newFormat.channelCount > 0 else {
                onLog("[MIC] input format not ready yet — waiting for next change")
                return
            }

            guard micPipeline.reconfigure(sourceFormat: newFormat) else {
                onLog("[MIC] skipped restart because converter rebuild failed")
                return
            }

            engine.inputNode.removeTap(onBus: 0)
            installMicTap(on: engine, pipeline: micPipeline)
            do {
                if !engine.isRunning {
                    try engine.start()
                }
                onLog("[MIC] engine restarted after device switch")
            } catch {
                onLog("[MIC] failed to restart engine after device switch: \(error)")
            }
        }
    }

    /// Stop both captures and wait for pending transcription results to
    /// drain. Idempotent — safe to call from a deinit or a signal handler.
    public func stop() async {
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
                    await performStop()
                }
                state = .stopping(task)
                return task
            case .started:
                let task = Task { [self] in
                    await performStop()
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
                    await performStop()
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
        if let cleanupTask { await cleanupTask.value }
        lifecycleState.withLock { $0 = .stopped }
    }

    private func performStop() async {
        onLog("Stopping...")

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

        if let micPipeline {
            await micPipeline.finish()
        }
        micPipeline = nil

        while true {
            let action = sysState.withLock { state -> SysStopAction in
                switch state {
                case .building(let task):
                    return .waitForBuild(task)
                case .ready(let pipeline):
                    state = .stopped
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
            case .waitForBuild(let task):
                await task.value
            case .finish(let pipeline):
                await pipeline.finish()
                onLog("Stopped.")
                return
            case .silent:
                onLog("[SYS] no audio was ever received (system was silent)")
                onLog("Stopped.")
                return
            case .done:
                onLog("Stopped.")
                return
            }
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
