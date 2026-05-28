import Foundation

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
//     short returns (version), or length-tagged `(ptr, len)` for emitted
//     payloads that may contain interior NULs (none expected today, but cheap
//     to be correct).
//   * Sessions and other Swift-owned objects cross as opaque pointers
//     produced by `Unmanaged.passRetained` / consumed by `takeRetainedValue`.

/// Static, leaked C string holding the WispAudioKit version. Lives forever.
/// `nonisolated(unsafe)` because the pointer is immutably initialised once
/// and only read thereafter.
private nonisolated(unsafe) let wispAudioKitVersionCString: UnsafePointer<CChar> = {
    let utf8 = Array("0.1.0".utf8CString)
    let buf = UnsafeMutablePointer<CChar>.allocate(capacity: utf8.count)
    for (i, c) in utf8.enumerated() { buf[i] = c }
    return UnsafePointer(buf)
}()

/// Returns a static, NUL-terminated UTF-8 version string for the WispAudioKit
/// library. The pointer lives for the lifetime of the process; the caller
/// must not free it.
@_cdecl("wisp_audiokit_version")
public func wisp_audiokit_version() -> UnsafePointer<CChar> {
    wispAudioKitVersionCString
}
