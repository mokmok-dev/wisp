import Foundation
import WispAudioKit

// Wisp PoC CLI — thin wrapper around `WispAudioKit.WispSession`. Logs and
// transcription results are printed to stderr / stdout respectively so the
// behavior matches the pre-Session-API binary.

let outputRoot: URL = if CommandLine.arguments.count > 1 {
    .init(fileURLWithPath: CommandLine.arguments[1])
} else {
    .init(fileURLWithPath: "./wisp-recordings")
}

let outputDir: URL
do {
    outputDir = try createRecordingDirectory(in: outputRoot)
} catch {
    FileHandle.standardError.write(
        Data("[wispctl] FATAL: failed to create recording directory: \(error)\n".utf8)
    )
    exit(1)
}

wispLog("Wisp PoC starting")
wispLog("  Output root: \(outputRoot.standardizedFileURL.path)")
wispLog("  Recording directory: \(outputDir.path)")

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

wispLog("  MIC WAV: \(session.micWavURL.path)")
wispLog("  SYS WAV: \(session.systemWavURL.path)")

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

/// Create an isolated directory for one recording beneath the user-selected
/// root. `WispSession` uses stable WAV filenames inside this directory, so the
/// UUID suffix prevents rapid or concurrent CLI runs from overwriting files.
func createRecordingDirectory(
    in root: URL,
    now: Date = Date(),
    id: UUID = UUID(),
    fileManager: FileManager = .default
) throws -> URL {
    let formatter = ISO8601DateFormatter()
    formatter.formatOptions = [.withInternetDateTime]
    let timestamp = formatter
        .string(from: now)
        .replacingOccurrences(of: ":", with: "")
    let directoryName = "recording-\(timestamp)-\(id.uuidString.lowercased())"
    let directory = root.standardizedFileURL
        .appendingPathComponent(directoryName, isDirectory: true)
    try fileManager.createDirectory(
        at: directory,
        withIntermediateDirectories: true
    )
    return directory
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
