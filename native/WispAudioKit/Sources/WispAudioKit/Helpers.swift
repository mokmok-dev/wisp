@preconcurrency import AVFoundation
import CoreMedia
import Foundation

public enum PoCError: Error, CustomStringConvertible {
    case permissionDenied(String)
    case noCompatibleFormat
    case converterCreationFailed
    case noDisplay
    case scStreamSetupFailed(String)
    case outputFilesAlreadyExist(String)

    public var description: String {
        switch self {
        case .permissionDenied(let name): "Permission denied: \(name)"
        case .noCompatibleFormat: "No compatible audio format for transcriber"
        case .converterCreationFailed: "Failed to create AVAudioConverter"
        case .noDisplay: "No display available for system audio capture"
        case .scStreamSetupFailed(let msg): "SCStream setup failed: \(msg)"
        case .outputFilesAlreadyExist(let path):
            "Recording output files already exist in: \(path)"
        }
    }
}

/// Logs a `[wispctl]`-prefixed message to stderr. Used by both the library
/// internals and the wispctl CLI for now; will move behind a callback once
/// the Rust FFI lands and the library can no longer assume a stderr.
///
/// Named `wispLog` (not `log`) to avoid colliding with Foundation's `log`
/// math overloads when the CLI imports both modules.
public func wispLog(_ msg: String) {
    FileHandle.standardError.write(Data("[wispctl] \(msg)\n".utf8))
}

/// Reference-typed mutable flag — useful for the AVAudioConverter input block,
/// which Swift 6 treats as @Sendable and disallows capturing local var-mut.
final class MutableFlag: @unchecked Sendable {
    var value: Bool = false
}

extension CMSampleBuffer {
    /// Copy this audio sample buffer into a freshly allocated AVAudioPCMBuffer.
    /// Returns nil for non-audio or malformed buffers.
    func toPCMBuffer() -> AVAudioPCMBuffer? {
        guard let formatDescription,
              var asbd = formatDescription.audioStreamBasicDescription
        else { return nil }

        guard let format = AVAudioFormat(streamDescription: &asbd) else { return nil }
        let frameCount = AVAudioFrameCount(numSamples)
        guard frameCount > 0,
              let outBuffer = AVAudioPCMBuffer(pcmFormat: format, frameCapacity: frameCount)
        else { return nil }
        outBuffer.frameLength = frameCount

        do {
            try withAudioBufferList { audioBufferList, _ in
                for (idx, srcBuf) in audioBufferList.enumerated() {
                    guard idx < Int(format.channelCount), let src = srcBuf.mData else { continue }
                    if let dst = outBuffer.floatChannelData?[idx] {
                        memcpy(dst, src, Int(srcBuf.mDataByteSize))
                    } else if let dst = outBuffer.int16ChannelData?[idx] {
                        memcpy(dst, src, Int(srcBuf.mDataByteSize))
                    }
                }
            }
        } catch {
            return nil
        }

        return outBuffer
    }
}
