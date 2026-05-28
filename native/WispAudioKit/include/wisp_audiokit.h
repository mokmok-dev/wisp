/* WispAudioKit C ABI — consumed by the `wisp-audiokit-sys` Rust crate.
 *
 * This header is hand-written (not generated). Keep it in sync with the
 * `@_cdecl` exports in Sources/WispAudioKit/Bridge.swift.
 */

#ifndef WISP_AUDIOKIT_H
#define WISP_AUDIOKIT_H

#include <stddef.h>
#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

/* Returns a static, NUL-terminated UTF-8 version string for the
 * WispAudioKit library. The returned pointer lives for the lifetime of
 * the process; the caller must not free it. */
const char* wisp_audiokit_version(void);

#ifdef __cplusplus
}
#endif

#endif /* WISP_AUDIOKIT_H */
