import Foundation
import WispAudioKit

// Wisp PoC CLI — thin wrapper around `WispAudioKit.WispSession`. Logs and
// transcription results are printed to stderr / stdout respectively so the
// behavior matches the pre-Session-API binary.

let outputDir: URL = if CommandLine.arguments.count > 1 {
    .init(fileURLWithPath: CommandLine.arguments[1])
} else {
    .init(fileURLWithPath: "./wisp-recordings")
}

wispLog("Wisp PoC starting; output dir: \(outputDir.path)")

let session: WispSession
do {
    session = try WispSession(
        outputDir: outputDir,
        onResult: { result in
            let label = result.source == .mic ? "MIC" : "SYS"
            let range = String(format: "%6.2f-%6.2fs", result.startSeconds, result.endSeconds)
            print("[\(label)] [seg \(result.segmentID)] [\(range)] \(result.text)")
        },
        onLog: { msg in
            wispLog(msg)
        }
    )
} catch {
    FileHandle.standardError.write(Data("[wispctl] FATAL: \(error)\n".utf8))
    exit(1)
}

do {
    try await session.start()
} catch {
    FileHandle.standardError.write(Data("[wispctl] FATAL: \(error)\n".utf8))
    exit(1)
}

wispLog("Recording. Speak in Japanese — try playing a YouTube clip too. Ctrl+C to stop.")

await waitForInterrupt()
await session.stop()

wispLog("Done.")
wispLog("  MIC WAV: \(session.micWavURL.path)")
wispLog("  SYS WAV: \(session.systemWavURL.path)")

// MARK: - CLI helpers

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
