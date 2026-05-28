@preconcurrency import AVFoundation
import CoreMedia
import Foundation
import os.lock
import Speech
import WispAudioKit

// MARK: - PoC entry

//
// Wisp PoC v3: Microphone + Core Audio Process Tap (system audio) → two
// parallel SpeechAnalyzer pipelines → stdout (tagged [MIC]/[SYS]) + 2 WAV files.
//
// Per-source pipelines give us speaker attribution for free (mic = self,
// system = others), no ML diarization required.
//
// vs. ScreenCaptureKit: Process Tap requests only "System Audio Recording"
// permission, supports per-process tapping, and has no video overhead.

do {
    try await run()
} catch {
    FileHandle.standardError.write(Data("[wispctl] FATAL: \(error)\n".utf8))
    exit(1)
}

func run() async throws {
    let outputDir = CommandLine.arguments.count > 1
        ? CommandLine.arguments[1]
        : "./wisp-recordings"
    try FileManager.default.createDirectory(
        atPath: outputDir,
        withIntermediateDirectories: true
    )

    let ts = ISO8601DateFormatter().string(from: Date())
        .replacingOccurrences(of: ":", with: "-")
    let micWavURL = URL(fileURLWithPath: "\(outputDir)/mic-\(ts).wav")
    let sysWavURL = URL(fileURLWithPath: "\(outputDir)/system-\(ts).wav")

    wispLog("Wisp PoC v2 (mic + system audio) starting")

    // 1. Permissions: mic + speech recognition.
    guard await AVAudioApplication.requestRecordPermission() else {
        throw PoCError.permissionDenied("Microphone")
    }
    let speechAuth = await requestSpeechAuthorization()
    guard speechAuth == .authorized else {
        throw PoCError.permissionDenied("Speech recognition (\(speechAuth.rawValue))")
    }

    // 2. Ensure Japanese model is installed (shared by both pipelines).
    let probeTranscriber = SpeechTranscriber(
        locale: Locale(identifier: "ja-JP"),
        preset: .progressiveTranscription
    )
    if let installReq = try await AssetInventory
        .assetInstallationRequest(supporting: [probeTranscriber])
    {
        wispLog("Downloading ja-JP speech model...")
        try await installReq.downloadAndInstall()
        wispLog("Model ready")
    }

    // 3. AVAudioEngine for microphone
    let engine = AVAudioEngine()
    let micFormat = engine.inputNode.outputFormat(forBus: 0)
    wispLog("[MIC] native format sr=\(micFormat.sampleRate) ch=\(micFormat.channelCount)")

    // 4. Mic pipeline
    let micPipeline = try await TranscriptionPipeline(
        label: "MIC",
        sourceFormat: micFormat,
        wavURL: micWavURL
    )
    micPipeline.startResultsConsumer()

    engine.inputNode.installTap(
        onBus: 0,
        bufferSize: 4096,
        format: micFormat
    ) { buffer, _ in
        micPipeline.push(buffer)
    }
    try engine.start()
    wispLog("[MIC] engine started")

    // 5. System audio capture via Core Audio Process Tap.
    //    The tap exposes its stream format synchronously, so we can build
    //    the SYS pipeline before any audio arrives (no lazy/race like SCKit).

    enum SysState {
        case idle
        case building
        case ready(TranscriptionPipeline)
        case failed
    }
    let sysState = OSAllocatedUnfairLock<SysState>(initialState: .idle)

    let systemCapture = ProcessTapCapture { buffer in
        // Fast path: pipeline already exists
        let action: (TranscriptionPipeline?, Bool) = sysState.withLock { state -> (
            TranscriptionPipeline?,
            Bool
        ) in
            switch state {
            case .ready(let p): return (p, false)
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

        let format = buffer.format
        wispLog(
            "[SYS] first audio buffer — building pipeline (sr=\(format.sampleRate) ch=\(format.channelCount))"
        )
        Task {
            do {
                let pipeline = try await TranscriptionPipeline(
                    label: "SYS",
                    sourceFormat: format,
                    wavURL: sysWavURL
                )
                pipeline.startResultsConsumer()
                sysState.withLock { $0 = .ready(pipeline) }
                pipeline.push(buffer)
            } catch {
                wispLog("[SYS] failed to build pipeline: \(error)")
                sysState.withLock { $0 = .failed }
            }
        }
    }
    try systemCapture.start()

    wispLog("Recording. Speak in Japanese — try playing a YouTube clip too. Ctrl+C to stop.")

    // 6. Wait for SIGINT
    await waitForInterrupt()

    // 7. Stop everything
    wispLog("Stopping...")
    engine.inputNode.removeTap(onBus: 0)
    engine.stop()
    systemCapture.stop()

    await micPipeline.finish()
    let sysPipeline: TranscriptionPipeline? = sysState.withLock { state in
        if case .ready(let p) = state { return p }
        return nil
    }
    if let sysPipeline {
        await sysPipeline.finish()
    } else {
        wispLog("[SYS] no audio was ever received (system was silent)")
    }

    wispLog("Done.")
    wispLog("  MIC WAV: \(micWavURL.path)")
    wispLog("  SYS WAV: \(sysWavURL.path)")
}

// MARK: - CLI helpers

func requestSpeechAuthorization() async -> SFSpeechRecognizerAuthorizationStatus {
    await withCheckedContinuation { cont in
        SFSpeechRecognizer.requestAuthorization { status in
            cont.resume(returning: status)
        }
    }
}

func waitForInterrupt() async {
    await withCheckedContinuation { (cont: CheckedContinuation<Void, Never>) in
        let source = DispatchSource.makeSignalSource(signal: SIGINT, queue: .global())
        source.setEventHandler {
            source.cancel()
            cont.resume()
        }
        source.resume()
        signal(SIGINT, SIG_IGN)
    }
}
