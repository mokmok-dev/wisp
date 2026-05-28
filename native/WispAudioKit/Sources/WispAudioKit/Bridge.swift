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
    UnsafePointer<CChar>?, // text_utf8
    Int, // text_len
    Double, // start_seconds
    Double, // end_seconds
    UnsafeMutableRawPointer? // user_data
) -> Void

/// Callback for log messages. Same lifetime rules as `WispResultCallback`.
public typealias WispLogCallback = @convention(c) (
    UnsafePointer<CChar>?, // message_utf8
    Int, // message_len
    UnsafeMutableRawPointer? // user_data
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

final class SessionHandle: @unchecked Sendable {
    let session: WispSession
    private let lastError: OSAllocatedUnfairLock<ErrorBufferSlot>

    init(session: WispSession) {
        self.session = session
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
/// handle to hold it. Errors are limited to "couldn't create the output
/// directory" and "input pointer was NULL".
@_cdecl("wisp_session_new")
public func wisp_session_new(
    output_dir: UnsafePointer<CChar>?,
    locale: UnsafePointer<CChar>?,
    on_result: WispResultCallback?,
    on_log: WispLogCallback?,
    user_data: UnsafeMutableRawPointer?
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
    let onResultClosure: @Sendable (WispSession.Result) -> Void = { result in
        let text = result.text
        text.utf8CString.withUnsafeBufferPointer { buf in
            // utf8CString includes trailing NUL; drop it for explicit length.
            let len = buf.count > 0 ? buf.count - 1 : 0
            on_result(
                result.source.rawValue,
                result.segmentID,
                buf.baseAddress,
                len,
                result.startSeconds,
                result.endSeconds,
                ud.value
            )
        }
    }
    let onLogClosure: @Sendable (String) -> Void = { msg in
        guard let on_log else { return }
        msg.utf8CString.withUnsafeBufferPointer { buf in
            let len = buf.count > 0 ? buf.count - 1 : 0
            on_log(buf.baseAddress, len, ud.value)
        }
    }
    do {
        let session = try WispSession(
            outputDir: URL(fileURLWithPath: outputDirStr),
            locale: Locale(identifier: localeStr),
            onResult: onResultClosure,
            onLog: onLogClosure
        )
        return box(SessionHandle(session: session))
    } catch {
        return nil
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

/// Stop capture and wait for results to drain. Blocks until done.
@_cdecl("wisp_session_stop")
public func wisp_session_stop(session: OpaquePointer?) {
    guard let handle = unbox(session) else { return }
    let sem = DispatchSemaphore(value: 0)
    Task.detached {
        await handle.session.stop()
        sem.signal()
    }
    sem.wait()
}

/// Free the session. The caller must have already called
/// `wisp_session_stop`; otherwise resources may leak.
@_cdecl("wisp_session_free")
public func wisp_session_free(session: OpaquePointer?) {
    guard let p = session else { return }
    Unmanaged<SessionHandle>.fromOpaque(UnsafeRawPointer(p)).release()
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
