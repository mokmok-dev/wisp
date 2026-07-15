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
public final class ProcessTapCapture: @unchecked Sendable {
    private let onBuffer: (AVAudioPCMBuffer) -> Void
    private let lifecycleLock = NSLock()

    private var tapID: AudioObjectID = .init(kAudioObjectUnknown)
    private var aggregateID: AudioObjectID = .init(kAudioObjectUnknown)
    private var ioProcID: AudioDeviceIOProcID?
    public private(set) var captureFormat: AVAudioFormat?

    // Diagnostics: count callbacks vs successful buffer conversions
    private let ioCallbackCount = OSAllocatedUnfairLock<Int>(initialState: 0)
    private let bufferYieldCount = OSAllocatedUnfairLock<Int>(initialState: 0)

    public init(onBuffer: @escaping (AVAudioPCMBuffer) -> Void) {
        self.onBuffer = onBuffer
    }

    deinit {
        stop()
    }

    public func start() throws {
        // Keep start/stop mutually exclusive. In particular, a stop racing a
        // partially completed start must not return before that start either
        // commits all three HAL resources or rolls all of them back.
        lifecycleLock.lock()
        defer { lifecycleLock.unlock() }

        guard tapID == AudioObjectID(kAudioObjectUnknown),
              aggregateID == AudioObjectID(kAudioObjectUnknown),
              ioProcID == nil
        else {
            throw PoCError.invalidLifecycle("system audio capture is already started")
        }

        // Keep resources local until AudioDeviceStart succeeds. Every throw
        // below runs this rollback, including Core Audio APIs that report an
        // error while still returning a partially created object.
        var localTapID = AudioObjectID(kAudioObjectUnknown)
        var localAggID = AudioObjectID(kAudioObjectUnknown)
        var localProcID: AudioDeviceIOProcID?
        var committed = false
        defer {
            if !committed {
                destroyCaptureResources(
                    tapID: localTapID,
                    aggregateID: localAggID,
                    ioProcID: localProcID
                )
            }
        }

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

        var status = AudioHardwareCreateProcessTap(desc, &localTapID)
        guard status == noErr, localTapID != AudioObjectID(kAudioObjectUnknown) else {
            throw PoCError.scStreamSetupFailed("AudioHardwareCreateProcessTap: \(status)")
        }
        wispLog("[SYS] Process tap created (id=\(localTapID))")

        // 3. Get the tap's UID — needed to reference it from the aggregate device.
        let tapUID = try getCFStringProperty(
            objectID: localTapID,
            selector: AudioObjectPropertySelector(kAudioTapPropertyUID)
        )
        wispLog("[SYS] Tap UID: \(tapUID)")

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
        status = AudioHardwareCreateAggregateDevice(
            aggregateDict as CFDictionary,
            &localAggID
        )
        guard status == noErr, localAggID != AudioObjectID(kAudioObjectUnknown) else {
            throw PoCError.scStreamSetupFailed("AudioHardwareCreateAggregateDevice: \(status)")
        }
        wispLog("[SYS] Aggregate device created (id=\(localAggID))")

        // 5. Discover the aggregate's input stream format.
        var asbd = try getStreamFormat(deviceID: localAggID)
        guard let avFormat = AVAudioFormat(streamDescription: &asbd) else {
            throw PoCError.noCompatibleFormat
        }
        wispLog(
            "[SYS] Tap stream format sr=\(avFormat.sampleRate) ch=\(avFormat.channelCount) fmt=\(avFormat.commonFormat.rawValue)"
        )

        // 6. Install IOProc to receive audio buffers.
        let onBufferLocal = onBuffer
        let formatLocal = avFormat
        let ioCounter = ioCallbackCount
        let yieldCounter = bufferYieldCount
        status = AudioDeviceCreateIOProcIDWithBlock(
            &localProcID,
            localAggID,
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

        // 7. Start.
        status = AudioDeviceStart(localAggID, procID)
        guard status == noErr else {
            throw PoCError.scStreamSetupFailed("AudioDeviceStart: \(status)")
        }

        tapID = localTapID
        aggregateID = localAggID
        ioProcID = procID
        captureFormat = avFormat
        committed = true
        wispLog("[SYS] Process tap streaming started")
    }

    public func stop() {
        lifecycleLock.lock()
        defer { lifecycleLock.unlock() }

        let ioCount = ioCallbackCount.withLock { $0 }
        let yieldCount = bufferYieldCount.withLock { $0 }
        wispLog("[SYS] diagnostics: IOProc=\(ioCount) calls, yields=\(yieldCount) buffers")

        destroyCaptureResources(tapID: tapID, aggregateID: aggregateID, ioProcID: ioProcID)
        ioProcID = nil
        aggregateID = AudioObjectID(kAudioObjectUnknown)
        tapID = AudioObjectID(kAudioObjectUnknown)
        captureFormat = nil
    }
}

// MARK: - HAL helpers

private func destroyCaptureResources(
    tapID: AudioObjectID,
    aggregateID: AudioObjectID,
    ioProcID: AudioDeviceIOProcID?
) {
    if let ioProcID, aggregateID != AudioObjectID(kAudioObjectUnknown) {
        // AudioDeviceStop is harmless when AudioDeviceStart never committed,
        // and covers the defensive case where HAL started despite returning
        // a non-zero status.
        AudioDeviceStop(aggregateID, ioProcID)
        AudioDeviceDestroyIOProcID(aggregateID, ioProcID)
    }
    if aggregateID != AudioObjectID(kAudioObjectUnknown) {
        AudioHardwareDestroyAggregateDevice(aggregateID)
    }
    if tapID != AudioObjectID(kAudioObjectUnknown) {
        AudioHardwareDestroyProcessTap(tapID)
    }
}

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
