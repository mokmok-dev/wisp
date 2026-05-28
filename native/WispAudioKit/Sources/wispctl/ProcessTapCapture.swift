@preconcurrency import AVFoundation
import CoreAudio
import Foundation
import os.lock

/// Captures system audio via Core Audio Process Tap (macOS 14.2+).
///
/// Unlike ScreenCaptureKit, this API:
///   - Requests "System Audio Recording" permission only (not Screen Recording).
///   - Can target/exclude individual processes by AudioObjectID (or bundle ID on
///     macOS 26+).
///
/// Pipeline:
///   1. `AudioHardwareCreateProcessTap` → process tap (AudioObjectID)
///   2. Create a private Aggregate Device that wraps the tap as a sub-tap
///   3. `AudioDeviceCreateIOProcIDWithBlock` to start receiving PCM via HAL
final class ProcessTapCapture: @unchecked Sendable {
    private let onBuffer: (AVAudioPCMBuffer) -> Void

    private var tapID: AudioObjectID = .init(kAudioObjectUnknown)
    private var aggregateID: AudioObjectID = .init(kAudioObjectUnknown)
    private var ioProcID: AudioDeviceIOProcID?
    private(set) var captureFormat: AVAudioFormat?

    // Diagnostics: count callbacks vs successful buffer conversions
    private let ioCallbackCount = OSAllocatedUnfairLock<Int>(initialState: 0)
    private let bufferYieldCount = OSAllocatedUnfairLock<Int>(initialState: 0)

    init(onBuffer: @escaping (AVAudioPCMBuffer) -> Void) {
        self.onBuffer = onBuffer
    }

    func start() throws {
        // 1. Find our own process AudioObjectID so we can exclude it from the tap.
        let ourPID = getpid()
        let ourProcessObjectID = try translatePIDToProcessObject(pid: ourPID)

        // 2. Create a stereo tap of everything EXCEPT ourselves (so we don't loop
        //    our own transcription output back into the tap).
        //    Swift refinement gives us [AudioObjectID] (not [NSNumber]).
        let desc = CATapDescription(
            stereoGlobalTapButExcludeProcesses: [ourProcessObjectID]
        )
        desc.name = "Wisp System Audio Tap"
        desc.isPrivate = true

        var localTapID = AudioObjectID(kAudioObjectUnknown)
        var status = AudioHardwareCreateProcessTap(desc, &localTapID)
        guard status == noErr, localTapID != AudioObjectID(kAudioObjectUnknown) else {
            throw PoCError.scStreamSetupFailed("AudioHardwareCreateProcessTap: \(status)")
        }
        tapID = localTapID
        log("[SYS] Process tap created (id=\(tapID))")

        // 3. Get the tap's UID — needed to reference it from the aggregate device.
        let tapUID = try getCFStringProperty(
            objectID: tapID,
            selector: AudioObjectPropertySelector(kAudioTapPropertyUID)
        )
        log("[SYS] Tap UID: \(tapUID)")

        // 4. Build a private aggregate device that includes our tap.
        let aggregateUID = UUID().uuidString
        let aggregateDict: [String: Any] = [
            kAudioAggregateDeviceNameKey: "Wisp Tap Aggregate",
            kAudioAggregateDeviceUIDKey: aggregateUID,
            kAudioAggregateDeviceIsPrivateKey: true,
            kAudioAggregateDeviceIsStackedKey: false,
            kAudioAggregateDeviceTapAutoStartKey: true,
            kAudioAggregateDeviceTapListKey: [
                [
                    kAudioSubTapUIDKey: tapUID,
                    kAudioSubTapDriftCompensationKey: true,
                ],
            ],
        ]
        var localAggID = AudioObjectID(kAudioObjectUnknown)
        status = AudioHardwareCreateAggregateDevice(
            aggregateDict as CFDictionary,
            &localAggID
        )
        guard status == noErr, localAggID != AudioObjectID(kAudioObjectUnknown) else {
            throw PoCError.scStreamSetupFailed("AudioHardwareCreateAggregateDevice: \(status)")
        }
        aggregateID = localAggID
        log("[SYS] Aggregate device created (id=\(aggregateID))")

        // 5. Discover the aggregate's input stream format.
        var asbd = try getStreamFormat(deviceID: aggregateID)
        guard let avFormat = AVAudioFormat(streamDescription: &asbd) else {
            throw PoCError.noCompatibleFormat
        }
        captureFormat = avFormat
        log(
            "[SYS] Tap stream format sr=\(avFormat.sampleRate) ch=\(avFormat.channelCount) fmt=\(avFormat.commonFormat.rawValue)"
        )

        // 6. Install IOProc to receive audio buffers.
        let onBufferLocal = onBuffer
        let formatLocal = avFormat
        let ioCounter = ioCallbackCount
        let yieldCounter = bufferYieldCount
        var localProcID: AudioDeviceIOProcID?
        status = AudioDeviceCreateIOProcIDWithBlock(
            &localProcID,
            aggregateID,
            DispatchQueue.global(qos: .userInitiated),
            { _, inputData, _, _, _ in
                ioCounter.withLock { $0 += 1 }
                guard let pcm = makePCMBuffer(
                    from: inputData,
                    format: formatLocal
                ) else { return }
                yieldCounter.withLock { $0 += 1 }
                onBufferLocal(pcm)
            }
        )
        guard status == noErr, let procID = localProcID else {
            throw PoCError.scStreamSetupFailed("AudioDeviceCreateIOProcIDWithBlock: \(status)")
        }
        ioProcID = procID

        // 7. Start.
        status = AudioDeviceStart(aggregateID, procID)
        guard status == noErr else {
            throw PoCError.scStreamSetupFailed("AudioDeviceStart: \(status)")
        }
        log("[SYS] Process tap streaming started")
    }

    func stop() {
        let ioCount = ioCallbackCount.withLock { $0 }
        let yieldCount = bufferYieldCount.withLock { $0 }
        log("[SYS] diagnostics: IOProc=\(ioCount) calls, yields=\(yieldCount) buffers")

        if let procID = ioProcID, aggregateID != AudioObjectID(kAudioObjectUnknown) {
            AudioDeviceStop(aggregateID, procID)
            AudioDeviceDestroyIOProcID(aggregateID, procID)
        }
        ioProcID = nil

        if aggregateID != AudioObjectID(kAudioObjectUnknown) {
            AudioHardwareDestroyAggregateDevice(aggregateID)
            aggregateID = AudioObjectID(kAudioObjectUnknown)
        }
        if tapID != AudioObjectID(kAudioObjectUnknown) {
            AudioHardwareDestroyProcessTap(tapID)
            tapID = AudioObjectID(kAudioObjectUnknown)
        }
    }
}

// MARK: - HAL helpers

private func translatePIDToProcessObject(pid: pid_t) throws -> AudioObjectID {
    var pidVar = pid
    var objectID = AudioObjectID(kAudioObjectUnknown)
    var size = UInt32(MemoryLayout<AudioObjectID>.size)
    var addr = AudioObjectPropertyAddress(
        mSelector: AudioObjectPropertySelector(kAudioHardwarePropertyTranslatePIDToProcessObject),
        mScope: AudioObjectPropertyScope(kAudioObjectPropertyScopeGlobal),
        mElement: AudioObjectPropertyElement(kAudioObjectPropertyElementMain)
    )
    let status = AudioObjectGetPropertyData(
        AudioObjectID(kAudioObjectSystemObject),
        &addr,
        UInt32(MemoryLayout<pid_t>.size),
        &pidVar,
        &size,
        &objectID
    )
    guard status == noErr else {
        throw PoCError.scStreamSetupFailed("TranslatePIDToProcessObject: \(status)")
    }
    return objectID
}

private func getCFStringProperty(
    objectID: AudioObjectID,
    selector: AudioObjectPropertySelector
) throws -> String {
    var addr = AudioObjectPropertyAddress(
        mSelector: selector,
        mScope: AudioObjectPropertyScope(kAudioObjectPropertyScopeGlobal),
        mElement: AudioObjectPropertyElement(kAudioObjectPropertyElementMain)
    )
    var size = UInt32(MemoryLayout<CFString?>.size)
    var cfStringRef: CFString?
    let status = withUnsafeMutablePointer(to: &cfStringRef) { ptr in
        AudioObjectGetPropertyData(objectID, &addr, 0, nil, &size, ptr)
    }
    guard status == noErr, let cfStringRef else {
        throw PoCError.scStreamSetupFailed("Get CFString property \(selector): \(status)")
    }
    return cfStringRef as String
}

private func getStreamFormat(deviceID: AudioObjectID) throws -> AudioStreamBasicDescription {
    var addr = AudioObjectPropertyAddress(
        mSelector: AudioObjectPropertySelector(kAudioDevicePropertyStreamFormat),
        mScope: AudioObjectPropertyScope(kAudioDevicePropertyScopeInput),
        mElement: AudioObjectPropertyElement(kAudioObjectPropertyElementMain)
    )
    var asbd = AudioStreamBasicDescription()
    var size = UInt32(MemoryLayout<AudioStreamBasicDescription>.size)
    let status = AudioObjectGetPropertyData(deviceID, &addr, 0, nil, &size, &asbd)
    guard status == noErr else {
        throw PoCError.scStreamSetupFailed("Get StreamFormat: \(status)")
    }
    return asbd
}

/// Build an AVAudioPCMBuffer from a HAL AudioBufferList. Copies the data so the
/// buffer outlives the IOProc callback.
private func makePCMBuffer(
    from listPtr: UnsafePointer<AudioBufferList>,
    format: AVAudioFormat
) -> AVAudioPCMBuffer? {
    let mutableListPtr = UnsafeMutableAudioBufferListPointer(
        UnsafeMutablePointer(mutating: listPtr)
    )
    guard let firstBuffer = mutableListPtr.first else { return nil }

    let bytesPerFrame = Int(format.streamDescription.pointee.mBytesPerFrame)
    guard bytesPerFrame > 0 else { return nil }

    // For interleaved Float32 stereo the whole frame is in mBuffers[0].
    // For non-interleaved planar, mBuffers has N entries (one per channel).
    let frameCount = AVAudioFrameCount(Int(firstBuffer.mDataByteSize) / bytesPerFrame)
    guard frameCount > 0,
          let pcm = AVAudioPCMBuffer(pcmFormat: format, frameCapacity: frameCount)
    else { return nil }
    pcm.frameLength = frameCount

    for (idx, srcBuf) in mutableListPtr.enumerated() {
        guard idx < Int(format.channelCount), let srcPtr = srcBuf.mData else { continue }
        let byteCount = Int(srcBuf.mDataByteSize)
        if let dst = pcm.floatChannelData?[idx] {
            memcpy(dst, srcPtr, byteCount)
        } else if let dst = pcm.int16ChannelData?[idx] {
            memcpy(dst, srcPtr, byteCount)
        }
    }
    return pcm
}
