@preconcurrency import AVFoundation
import Foundation
import os.lock
import Speech

// MARK: - C-ABI bridge

//
// Functions in this file are intentionally `@_cdecl` exports — they are the
// entry points called from Rust via the `wisp-audiokit-sys` crate. Keep the
// surface area small and the types strictly C-compatible (primitives,
// `UnsafePointer`/`OpaquePointer`, `@convention(c)` function pointers).
//
// Conventions:
//   * All exported symbols are prefixed `wisp_`.
//   * Strings cross the boundary as NUL-terminated UTF-8 `const char*` for
//     short returns (version, error message), or length-tagged `(ptr, len)`
//     for emitted payloads that may contain interior NULs.
//   * Sessions cross as opaque pointers produced by
//     `Unmanaged.passRetained` / consumed by `takeRetainedValue`.

// MARK: - Version

/// Static, leaked C string holding the WispAudioKit version. Lives forever.
/// `nonisolated(unsafe)` because the pointer is immutably initialised once
/// and only read thereafter.
private nonisolated(unsafe) let wispAudioKitVersionCString: UnsafePointer<CChar> = {
    let utf8 = Array("0.1.0".utf8CString)
    let buf = UnsafeMutablePointer<CChar>.allocate(capacity: utf8.count)
    for (i, c) in utf8.enumerated() {
        buf[i] = c
    }
    return UnsafePointer(buf)
}()

/// Returns a static, NUL-terminated UTF-8 version string for the WispAudioKit
/// library. The pointer lives for the lifetime of the process; the caller
/// must not free it.
@_cdecl("wisp_audiokit_version")
public func wisp_audiokit_version() -> UnsafePointer<CChar> {
    wispAudioKitVersionCString
}

// MARK: - Session lifecycle

/// Callback for transcription results. `text_utf8` is NOT NUL-terminated;
/// use `text_len`. The pointer is only valid for the duration of the call.
public typealias WispResultCallback = @convention(c) (
    Int32, // source: 0=mic, 1=system
    UInt64, // segment_id
    Int32, // is_final: 0=volatile, 1=final
    UnsafePointer<CChar>?, // text_utf8
    Int, // text_len
    Double, // start_seconds
    Double, // end_seconds
    Double, // confidence_mean; NaN when unavailable
    Double, // confidence_min; NaN when unavailable
    UnsafeMutableRawPointer? // user_data
) -> Void

/// Callback for log messages. Same lifetime rules as `WispResultCallback`.
public typealias WispLogCallback = @convention(c) (
    UnsafePointer<CChar>?, // message_utf8
    Int, // message_len
    UnsafeMutableRawPointer? // user_data
) -> Void

/// Callback for one interleaved Float32 PCM chunk. The sample pointer is valid
/// only for the duration of the call; consumers must copy before returning.
public typealias WispAudioCallback = @convention(c) (
    Int32, // source: 0=mic, 1=system
    UInt64, // per-source sequence
    Double, // monotonic timestamp in seconds
    UInt32, // sample rate
    UInt32, // channels
    UnsafePointer<Float>?, // interleaved samples
    Int, // sample count
    UnsafeMutableRawPointer? // user_data
) -> Void

/// Typed recognizer failure callback. `terminal != 0` means no more results
/// can be produced by the platform transcriber for this session.
public typealias WispTranscriberErrorCallback = @convention(c) (
    Int32,
    UnsafePointer<CChar>?,
    Int,
    UnsafeMutableRawPointer?
) -> Void

public typealias WispAudioOverflowCallback = @convention(c) (
    Int32,
    UInt64,
    UnsafeMutableRawPointer?
) -> Void

/// Reserved terminal capture/recording failure callback.
public typealias WispTerminalErrorCallback = @convention(c) (
    Int32,
    UnsafePointer<CChar>?,
    Int,
    UnsafeMutableRawPointer?
) -> Void

/// Boxed session handle handed to C as an opaque pointer. Holds the Swift
/// `WispSession` plus a per-session last-error slot for the
/// `wisp_session_last_error_message` getter.
/// `OSAllocatedUnfairLock` requires `Sendable`. Wrap the raw pointer so we
/// can store it; the unsafe-sendable claim is honest: we never read or
/// write the pointed-to bytes from concurrent threads (the lock
/// serializes both).
private struct ErrorBufferSlot: @unchecked Sendable {
    var pointer: UnsafeMutablePointer<CChar>?
}

/// Serializes the callback-lifetime side of the C ABI. The thread marker is
/// intentionally per coordinator: it lets lifecycle exports recognize a
/// synchronous call made from this session's own result/log callback.
final class CallbackCoordinator: @unchecked Sendable {
    private let condition = NSCondition()
    private let threadMarkerKey = "dev.mokmok.wisp.callback.\(UUID().uuidString)"
    private var acceptsCallbacks = true
    private var inFlightCallbacks = 0

    func invoke(_ body: () -> Void) {
        condition.lock()
        guard acceptsCallbacks else {
            condition.unlock()
            return
        }
        inFlightCallbacks += 1
        condition.unlock()

        let dictionary = Thread.current.threadDictionary
        let oldDepth = dictionary[threadMarkerKey] as? Int ?? 0
        dictionary[threadMarkerKey] = oldDepth + 1
        defer {
            if oldDepth == 0 {
                dictionary.removeObject(forKey: threadMarkerKey)
            } else {
                dictionary[threadMarkerKey] = oldDepth
            }

            condition.lock()
            inFlightCallbacks -= 1
            if inFlightCallbacks == 0 {
                condition.broadcast()
            }
            condition.unlock()
        }
        body()
    }

    var isExecutingOnCurrentThread: Bool {
        (Thread.current.threadDictionary[threadMarkerKey] as? Int ?? 0) > 0
    }

    func suppressFutureCallbacks() {
        condition.lock()
        acceptsCallbacks = false
        condition.unlock()
    }

    func waitForCallbacksToDrain() {
        condition.lock()
        while inFlightCallbacks > 0 {
            condition.wait()
        }
        condition.unlock()
    }
}

final class SessionHandle: @unchecked Sendable {
    let session: WispSession
    let callbacks: CallbackCoordinator
    private let lastError: OSAllocatedUnfairLock<ErrorBufferSlot>

    init(session: WispSession, callbacks: CallbackCoordinator) {
        self.session = session
        self.callbacks = callbacks
        lastError = OSAllocatedUnfairLock(initialState: ErrorBufferSlot(pointer: nil))
    }

    /// Replace the stored error string. Frees the previous one. Pass nil to
    /// clear.
    func setError(_ message: String?) {
        lastError.withLock { slot in
            if let old = slot.pointer {
                old.deallocate()
            }
            guard let msg = message else {
                slot.pointer = nil
                return
            }
            let utf8 = Array(msg.utf8CString)
            let buf = UnsafeMutablePointer<CChar>.allocate(capacity: utf8.count)
            for (i, c) in utf8.enumerated() {
                buf[i] = c
            }
            slot.pointer = buf
        }
    }

    /// Returns the currently stored error string pointer (or nil). Caller
    /// must not free it; the pointer is invalidated by the next mutation.
    func errorPointer() -> UnsafePointer<CChar>? {
        // OSAllocatedUnfairLock requires the withLock body to return
        // Sendable; UnsafePointer isn't. Hoist the raw bit pattern out and
        // rebuild the typed pointer outside the lock.
        let raw = lastError.withLock { slot -> UInt? in
            slot.pointer.map { UInt(bitPattern: $0) }
        }
        guard let raw, let ptr = UnsafeMutablePointer<CChar>(bitPattern: raw) else { return nil }
        return UnsafePointer(ptr)
    }

    deinit {
        lastError.withLock { slot in
            if let p = slot.pointer { p.deallocate() }
            slot.pointer = nil
        }
    }
}

@inline(__always)
private func box(_ h: SessionHandle) -> OpaquePointer {
    OpaquePointer(Unmanaged.passRetained(h).toOpaque())
}

@inline(__always)
private func unbox(_ p: OpaquePointer?) -> SessionHandle? {
    guard let p else { return nil }
    return Unmanaged<SessionHandle>.fromOpaque(UnsafeRawPointer(p)).takeUnretainedValue()
}

/// Construct a new session. Does no I/O — call `wisp_session_start` next.
///
/// On failure returns `nil`; the error is not stored because there is no
/// handle to hold it. Errors are limited to output-directory setup (including
/// refusing to overwrite an existing Ogg file) and "input pointer was NULL".
@_cdecl("wisp_session_new")
public func wisp_session_new(
    output_dir: UnsafePointer<CChar>?,
    locale: UnsafePointer<CChar>?,
    on_result: WispResultCallback?,
    on_log: WispLogCallback?,
    user_data: UnsafeMutableRawPointer?
) -> OpaquePointer? {
    makeSessionHandle(
        output_dir: output_dir,
        locale: locale,
        transcription_enabled: 1,
        allow_record_only: 0,
        on_result: on_result,
        on_audio: nil,
        on_audio_overflow: nil,
        on_transcriber_error: nil,
        on_terminal_error: nil,
        on_log: on_log,
        user_data: user_data,
        feedsCapturedAudioDirectlyToAnalyzer: true
    )
}

/// Versioned constructor carrying backend-neutral policy and PCM/error
/// callbacks. Keep the original five-argument symbol ABI-stable.
@_cdecl("wisp_session_new_v2")
public func wisp_session_new_v2(
    output_dir: UnsafePointer<CChar>?,
    locale: UnsafePointer<CChar>?,
    transcription_enabled: Int32,
    allow_record_only: Int32,
    on_result: WispResultCallback?,
    on_audio: WispAudioCallback?,
    on_audio_overflow: WispAudioOverflowCallback?,
    on_transcriber_error: WispTranscriberErrorCallback?,
    on_terminal_error: WispTerminalErrorCallback?,
    on_log: WispLogCallback?,
    user_data: UnsafeMutableRawPointer?
) -> OpaquePointer? {
    makeSessionHandle(
        output_dir: output_dir,
        locale: locale,
        transcription_enabled: transcription_enabled,
        allow_record_only: allow_record_only,
        on_result: on_result,
        on_audio: on_audio,
        on_audio_overflow: on_audio_overflow,
        on_transcriber_error: on_transcriber_error,
        on_terminal_error: on_terminal_error,
        on_log: on_log,
        user_data: user_data,
        feedsCapturedAudioDirectlyToAnalyzer: false
    )
}

private func makeSessionHandle(
    output_dir: UnsafePointer<CChar>?,
    locale: UnsafePointer<CChar>?,
    transcription_enabled: Int32,
    allow_record_only: Int32,
    on_result: WispResultCallback?,
    on_audio: WispAudioCallback?,
    on_audio_overflow: WispAudioOverflowCallback?,
    on_transcriber_error: WispTranscriberErrorCallback?,
    on_terminal_error: WispTerminalErrorCallback?,
    on_log: WispLogCallback?,
    user_data: UnsafeMutableRawPointer?,
    feedsCapturedAudioDirectlyToAnalyzer: Bool
) -> OpaquePointer? {
    guard let output_dir,
          let locale,
          let on_result
    else {
        return nil
    }
    let outputDirStr = String(cString: output_dir)
    let localeStr = String(cString: locale)
    // `user_data` is a `void*` we hand straight back to the C callbacks.
    // Crossing it through `@Sendable` Swift closures requires unchecked.
    let ud = UncheckedUserData(value: user_data)
    let callbacks = CallbackCoordinator()
    let onResultClosure: @Sendable (WispSession.Result) -> Void = { result in
        callbacks.invoke {
            let text = result.text
            text.utf8CString.withUnsafeBufferPointer { buf in
                // utf8CString includes trailing NUL; drop it for explicit length.
                let len = buf.count > 0 ? buf.count - 1 : 0
                on_result(
                    result.source.rawValue,
                    result.segmentID,
                    result.isFinal ? 1 : 0,
                    buf.baseAddress,
                    len,
                    result.startSeconds,
                    result.endSeconds,
                    result.confidenceMean ?? .nan,
                    result.confidenceMin ?? .nan,
                    ud.value
                )
            }
        }
    }
    let onLogClosure: @Sendable (String) -> Void = { msg in
        guard let on_log else { return }
        callbacks.invoke {
            msg.utf8CString.withUnsafeBufferPointer { buf in
                let len = buf.count > 0 ? buf.count - 1 : 0
                on_log(buf.baseAddress, len, ud.value)
            }
        }
    }
    let onTranscriberErrorClosure: @Sendable (Bool, String) -> Void = { terminal, msg in
        guard let on_transcriber_error else { return }
        callbacks.invoke {
            msg.utf8CString.withUnsafeBufferPointer { buf in
                let len = buf.count > 0 ? buf.count - 1 : 0
                on_transcriber_error(
                    terminal ? 1 : 0,
                    buf.baseAddress,
                    len,
                    ud.value
                )
            }
        }
    }
    let onAudioClosure: WispSession.OnAudio? = if let callback = on_audio {
        {
            (
                source: WispSession.Source,
                sequence: UInt64,
                timestamp: Double,
                sampleRate: UInt32,
                channels: UInt32,
                samples: [Float]
            ) in
            callbacks.invoke {
                samples.withUnsafeBufferPointer { buffer in
                    callback(
                        source.rawValue,
                        sequence,
                        timestamp,
                        sampleRate,
                        channels,
                        buffer.baseAddress,
                        buffer.count,
                        ud.value
                    )
                }
            }
        }
    } else {
        nil
    }
    let onAudioOverflowClosure: WispSession.OnAudioOverflow = { source, droppedFrames in
        guard let on_audio_overflow else { return }
        callbacks.invoke {
            on_audio_overflow(source.rawValue, droppedFrames, ud.value)
        }
    }
    let onTerminalErrorClosure: WispSession.OnTerminalError = { source, msg in
        guard let on_terminal_error else { return }
        callbacks.invoke {
            msg.utf8CString.withUnsafeBufferPointer { buf in
                let len = buf.count > 0 ? buf.count - 1 : 0
                on_terminal_error(source?.rawValue ?? -1, buf.baseAddress, len, ud.value)
            }
        }
    }
    do {
        let session = try WispSession(
            outputDir: URL(fileURLWithPath: outputDirStr),
            locale: Locale(identifier: localeStr),
            transcriptionEnabled: transcription_enabled != 0,
            allowRecordOnly: allow_record_only != 0,
            feedsCapturedAudioDirectlyToAnalyzer: feedsCapturedAudioDirectlyToAnalyzer,
            onResult: onResultClosure,
            onAudio: onAudioClosure,
            onAudioOverflow: onAudioOverflowClosure,
            onTranscriberError: onTranscriberErrorClosure,
            onTerminalError: onTerminalErrorClosure,
            onLog: onLogClosure
        )
        return box(SessionHandle(session: session, callbacks: callbacks))
    } catch {
        return nil
    }
}

private func runThrowingSynchronously(
    _ handle: SessionHandle,
    operation: @escaping @Sendable () async throws -> Void
) -> Int32 {
    let sem = DispatchSemaphore(value: 0)
    let errorSlot = OSAllocatedUnfairLock<String?>(initialState: nil)
    Task.detached {
        do {
            try await operation()
        } catch {
            errorSlot.withLock { $0 = "\(error)" }
        }
        sem.signal()
    }
    sem.wait()
    if let error = errorSlot.withLock({ $0 }) {
        handle.setError(error)
        return 1
    }
    handle.setError(nil)
    return 0
}

private func rejectReentrantSynchronousCall(
    _ handle: SessionHandle,
    operation: String
) -> Int32? {
    guard handle.callbacks.isExecutingOnCurrentThread else { return nil }
    handle.setError(
        "\(operation) cannot be called synchronously from this session's callback"
    )
    return 2
}

/// Start capture/recording only. Speech permission and analyzer setup are
/// deliberately owned by `wisp_session_start_transcription`.
@_cdecl("wisp_session_start_capture")
public func wisp_session_start_capture(session: OpaquePointer?) -> Int32 {
    guard let handle = unbox(session) else { return -1 }
    return runThrowingSynchronously(handle) {
        try await handle.session.startCapture()
    }
}

/// Configure the native platform transcriber for a running capture.
@_cdecl("wisp_session_start_transcription")
public func wisp_session_start_transcription(session: OpaquePointer?) -> Int32 {
    guard let handle = unbox(session) else { return -1 }
    return runThrowingSynchronously(handle) {
        try await handle.session.startTranscription()
    }
}

/// Start capture + transcription. Blocks the calling thread until the
/// session is fully ready (permissions granted, model installed, audio
/// flowing) or it fails. Returns 0 on success, non-zero on failure; call
/// `wisp_session_last_error_message` for details.
@_cdecl("wisp_session_start")
public func wisp_session_start(session: OpaquePointer?) -> Int32 {
    guard let handle = unbox(session) else { return -1 }
    let sem = DispatchSemaphore(value: 0)
    let errorSlot = OSAllocatedUnfairLock<String?>(initialState: nil)
    Task.detached {
        do {
            try await handle.session.start()
        } catch {
            errorSlot.withLock { $0 = "\(error)" }
        }
        sem.signal()
    }
    sem.wait()
    if let err = errorSlot.withLock({ $0 }) {
        handle.setError(err)
        return 1
    }
    handle.setError(nil)
    return 0
}

/// Return 1 when microphone capture reached the running state, otherwise 0.
/// This remains queryable after `wisp_session_start` fails and before stop.
@_cdecl("wisp_session_has_started_capture")
public func wisp_session_has_started_capture(session: OpaquePointer?) -> Int32 {
    guard let handle = unbox(session) else { return 0 }
    return handle.session.hasStartedCapture ? 1 : 0
}

/// Enable or disable microphone samples without affecting system capture.
@_cdecl("wisp_session_set_microphone_muted")
public func wisp_session_set_microphone_muted(
    session: OpaquePointer?,
    muted: Int32
) {
    guard let handle = unbox(session) else { return }
    handle.session.setMicrophoneMuted(muted != 0)
}

/// Submit orchestrated interleaved Float32 PCM to the platform transcriber.
/// Returns non-zero and stores a last-error message on invalid input/setup.
@_cdecl("wisp_session_push_transcriber_audio")
public func wisp_session_push_transcriber_audio(
    session: OpaquePointer?,
    source: Int32,
    sample_rate: UInt32,
    channels: UInt32,
    samples: UnsafePointer<Float>?,
    sample_count: Int
) -> Int32 {
    guard let handle = unbox(session) else { return -1 }
    if let result = rejectReentrantSynchronousCall(
        handle,
        operation: "wisp_session_push_transcriber_audio"
    ) {
        return result
    }
    guard let source = WispSession.Source(rawValue: source) else {
        handle.setError("wisp_session_push_transcriber_audio: invalid source")
        return -1
    }
    guard sample_rate > 0 else {
        handle.setError("wisp_session_push_transcriber_audio: sample_rate must be positive")
        return -1
    }
    guard channels > 0 else {
        handle.setError("wisp_session_push_transcriber_audio: channels must be positive")
        return -1
    }
    guard sample_count > 0 else {
        handle.setError("wisp_session_push_transcriber_audio: sample_count must be positive")
        return -1
    }
    guard sample_count.isMultiple(of: Int(channels)) else {
        handle.setError(
            "wisp_session_push_transcriber_audio: sample_count must contain complete frames"
        )
        return -1
    }
    guard let samples else {
        handle.setError("wisp_session_push_transcriber_audio: samples must not be NULL")
        return -1
    }
    let copied = Array(UnsafeBufferPointer(start: samples, count: sample_count))
    let sem = DispatchSemaphore(value: 0)
    let errorSlot = OSAllocatedUnfairLock<String?>(initialState: nil)
    Task.detached {
        do {
            try await handle.session.pushTranscriberAudio(
                source: source,
                sampleRate: sample_rate,
                channels: channels,
                samples: copied
            )
        } catch {
            errorSlot.withLock { $0 = "\(error)" }
        }
        sem.signal()
    }
    sem.wait()
    if let error = errorSlot.withLock({ $0 }) {
        handle.setError(error)
        return 1
    }
    handle.setError(nil)
    return 0
}

/// Cancel every SpeechAnalyzer in the session while capture/Ogg continue.
@_cdecl("wisp_session_disable_transcription")
public func wisp_session_disable_transcription(session: OpaquePointer?) -> Int32 {
    guard let handle = unbox(session) else { return -1 }
    if let result = rejectReentrantSynchronousCall(
        handle,
        operation: "wisp_session_disable_transcription"
    ) {
        return result
    }
    return runThrowingSynchronously(handle) {
        try await handle.session.disableTranscription()
    }
}

/// Stop capture producers and recording, but leave analyzer finalization to
/// the transcriber lifecycle.
@_cdecl("wisp_session_stop_capture")
public func wisp_session_stop_capture(session: OpaquePointer?) -> Int32 {
    guard let handle = unbox(session) else { return -1 }
    if let result = rejectReentrantSynchronousCall(
        handle,
        operation: "wisp_session_stop_capture"
    ) {
        return result
    }
    return runThrowingSynchronously(handle) {
        await handle.session.stopCapture()
    }
}

/// Finish analyzer input after every buffered capture frame has crossed Rust.
@_cdecl("wisp_session_finish_transcription")
public func wisp_session_finish_transcription(session: OpaquePointer?) -> Int32 {
    guard let handle = unbox(session) else { return -1 }
    if let result = rejectReentrantSynchronousCall(
        handle,
        operation: "wisp_session_finish_transcription"
    ) {
        return result
    }
    return runThrowingSynchronously(handle) {
        try await handle.session.finishTranscription()
    }
}

/// Stop capture and wait for results to drain. Blocks until done, except for
/// a reentrant call from this session's callback: that call suppresses future
/// callbacks, initiates stop, and returns so the current callback can unwind.
@_cdecl("wisp_session_stop")
public func wisp_session_stop(session: OpaquePointer?) {
    guard let handle = unbox(session) else { return }
    if handle.callbacks.isExecutingOnCurrentThread {
        // Blocking here would deadlock: pipeline.finish() waits for the
        // results callback that is synchronously waiting in this function.
        // Suppression makes it safe for the current callback to unwind while
        // the shared, idempotent session stop completes in the background.
        handle.callbacks.suppressFutureCallbacks()
        Task.detached {
            await handle.session.stop()
        }
        return
    }
    stopSynchronously(handle.session)
}

/// Abort capture without draining staged PCM or SpeechAnalyzer finals.
@_cdecl("wisp_session_abort")
public func wisp_session_abort(session: OpaquePointer?) {
    guard let handle = unbox(session) else { return }
    handle.callbacks.suppressFutureCallbacks()
    if handle.callbacks.isExecutingOnCurrentThread {
        Task.detached {
            await handle.session.abort()
        }
        return
    }
    let sem = DispatchSemaphore(value: 0)
    Task.detached {
        await handle.session.abort()
        sem.signal()
    }
    sem.wait()
}

private func stopSynchronously(_ session: WispSession) {
    let sem = DispatchSemaphore(value: 0)
    Task.detached {
        await session.stop()
        sem.signal()
    }
    sem.wait()
}

/// Stop if necessary and free the session. A free made from inside a callback
/// consumes the opaque handle immediately, then defers destruction until the
/// current callback unwinds and the asynchronous stop barrier completes.
@_cdecl("wisp_session_free")
public func wisp_session_free(session: OpaquePointer?) {
    guard let p = session else { return }
    let unmanaged = Unmanaged<SessionHandle>.fromOpaque(UnsafeRawPointer(p))
    let handle = unmanaged.takeUnretainedValue()

    if handle.callbacks.isExecutingOnCurrentThread {
        handle.callbacks.suppressFutureCallbacks()
        // Consume the caller-owned retain now, invalidating the opaque handle
        // immediately. The detached task keeps the object alive until both
        // lifecycle and callback barriers are satisfied.
        let retainedHandle = unmanaged.takeRetainedValue()
        Task.detached {
            await retainedHandle.session.stop()
            retainedHandle.callbacks.waitForCallbacksToDrain()
        }
        return
    }

    // Harden the API against a caller that forgot the documented stop call.
    // This also joins a stop that was initiated reentrantly and returned
    // early, preserving free as the final no-callback/no-resource barrier.
    stopSynchronously(handle.session)
    handle.callbacks.suppressFutureCallbacks()
    handle.callbacks.waitForCallbacksToDrain()
    unmanaged.release()
}

/// Returns the last error message recorded against this session, or NULL
/// if there is no recorded error. The returned pointer is owned by the
/// session and is invalidated by the next mutating call on it.
@_cdecl("wisp_session_last_error_message")
public func wisp_session_last_error_message(session: OpaquePointer?) -> UnsafePointer<CChar>? {
    guard let handle = unbox(session) else { return nil }
    return handle.errorPointer()
}

// MARK: - Permissions

//
// Two TCC services gate Wisp: microphone (AVAudioApplication) and speech
// recognition (SFSpeechRecognizer). Both have a synchronous status getter
// and an async request API; we expose both shapes so the UI can decide
// between "open the OS prompt" and "deep-link to System Settings" based on
// the current state.
//
// Permission identifiers (kept in sync with wisp_audiokit.h):
//   0 = microphone
//   1 = speech recognition
//
// Status identifiers:
//   0 = undetermined (never asked)
//   1 = denied
//   2 = granted
//   3 = restricted (speech only — e.g. parental controls)
//   negative = invalid permission id

private let wispPermissionMicrophone: Int32 = 0
private let wispPermissionSpeech: Int32 = 1

private let wispPermissionStatusUndetermined: Int32 = 0
private let wispPermissionStatusDenied: Int32 = 1
private let wispPermissionStatusGranted: Int32 = 2
private let wispPermissionStatusRestricted: Int32 = 3

/// Returns the current status of the given permission without prompting.
///
/// Microphone uses `AVCaptureDevice` (the macOS-canonical media capture
/// permission API), not `AVAudioApplication` — the latter is primarily an
/// iOS API and its request method doesn't reliably trigger the TCC prompt
/// on macOS.
@_cdecl("wisp_permission_status")
public func wisp_permission_status(permission: Int32) -> Int32 {
    switch permission {
    case wispPermissionMicrophone:
        avAuthorizationStatusToWisp(AVCaptureDevice.authorizationStatus(for: .audio))
    case wispPermissionSpeech:
        switch SFSpeechRecognizer.authorizationStatus() {
        case .notDetermined: wispPermissionStatusUndetermined
        case .denied: wispPermissionStatusDenied
        case .authorized: wispPermissionStatusGranted
        case .restricted: wispPermissionStatusRestricted
        @unknown default: wispPermissionStatusUndetermined
        }
    default:
        -1
    }
}

private func avAuthorizationStatusToWisp(_ status: AVAuthorizationStatus) -> Int32 {
    switch status {
    case .notDetermined: wispPermissionStatusUndetermined
    case .denied: wispPermissionStatusDenied
    case .authorized: wispPermissionStatusGranted
    case .restricted: wispPermissionStatusRestricted
    @unknown default: wispPermissionStatusUndetermined
    }
}

/// Triggers the OS permission prompt (if undetermined) and blocks the
/// caller until the user has responded — or returns immediately with the
/// current status if the OS would not show a prompt (already granted /
/// denied / restricted).
///
/// Called from a background thread by the Rust side; the underlying
/// callbacks fire on arbitrary queues so we just gate on a semaphore.
@_cdecl("wisp_permission_request")
public func wisp_permission_request(permission: Int32) -> Int32 {
    switch permission {
    case wispPermissionMicrophone:
        if AVCaptureDevice.authorizationStatus(for: .audio) != .notDetermined {
            return wisp_permission_status(permission: permission)
        }
        let sem = DispatchSemaphore(value: 0)
        let resultSlot = OSAllocatedUnfairLock<Bool>(initialState: false)
        AVCaptureDevice.requestAccess(for: .audio) { granted in
            resultSlot.withLock { $0 = granted }
            sem.signal()
        }
        sem.wait()
        return resultSlot.withLock { $0 } ? wispPermissionStatusGranted
            : wispPermissionStatusDenied
    case wispPermissionSpeech:
        if SFSpeechRecognizer.authorizationStatus() != .notDetermined {
            return wisp_permission_status(permission: permission)
        }
        let sem = DispatchSemaphore(value: 0)
        let resultSlot = OSAllocatedUnfairLock<SFSpeechRecognizerAuthorizationStatus>(
            initialState: .notDetermined
        )
        SFSpeechRecognizer.requestAuthorization { status in
            resultSlot.withLock { $0 = status }
            sem.signal()
        }
        sem.wait()
        return switch resultSlot.withLock({ $0 }) {
        case .notDetermined: wispPermissionStatusUndetermined
        case .denied: wispPermissionStatusDenied
        case .authorized: wispPermissionStatusGranted
        case .restricted: wispPermissionStatusRestricted
        @unknown default: wispPermissionStatusUndetermined
        }
    default:
        return -1
    }
}

// MARK: - Internal helpers

/// Wraps a raw user-data pointer so it can be captured by `@Sendable`
/// closures. The pointer is opaque to us — we never deref it.
private struct UncheckedUserData: @unchecked Sendable {
    let value: UnsafeMutableRawPointer?
}
