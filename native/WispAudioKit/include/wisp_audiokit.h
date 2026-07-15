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

/* ----- Library metadata --------------------------------------------------- */

/* Returns a static, NUL-terminated UTF-8 version string for the
 * WispAudioKit library. The returned pointer lives for the lifetime of
 * the process; the caller must not free it. */
const char* wisp_audiokit_version(void);

/* ----- Session ------------------------------------------------------------ */

/* Opaque handle returned by wisp_session_new. */
typedef struct WispSession WispSession;

/* Source identifier passed to result callbacks. */
#define WISP_SOURCE_MIC    0
#define WISP_SOURCE_SYSTEM 1

/* Callback invoked for each transcription result. `text_utf8` is NOT
 * NUL-terminated; use `text_len`. Both pointers are valid only for the
 * duration of the call — copy out before returning. */
typedef void (*WispResultCallback)(
    int32_t     source,         /* WISP_SOURCE_MIC or WISP_SOURCE_SYSTEM */
    uint64_t    segment_id,
    const char* text_utf8,
    size_t      text_len,
    double      start_seconds,
    double      end_seconds,
    void*       user_data
);

/* Callback invoked for log lines. Same pointer-lifetime rules as
 * WispResultCallback. */
typedef void (*WispLogCallback)(
    const char* message_utf8,
    size_t      message_len,
    void*       user_data
);

/* Construct a new session. Does no I/O; call wisp_session_start next.
 * Returns NULL on failure (e.g. invalid arguments, output directory
 * couldn't be created). `output_dir` and `locale` are NUL-terminated
 * UTF-8 strings (locale e.g. "ja-JP"). */
WispSession* wisp_session_new(
    const char*        output_dir,
    const char*        locale,
    WispResultCallback on_result,
    WispLogCallback    on_log,
    void*              user_data
);

/* Start capture + transcription. Blocks until the session is ready
 * (permissions granted, model installed, audio flowing) or fails.
 * Returns 0 on success, non-zero on failure; query
 * wisp_session_last_error_message for details on failure. */
int32_t wisp_session_start(WispSession* session);

/* Returns 1 if microphone capture reached the running state, otherwise 0.
 * Query after a failed start and before stopping to decide whether partial
 * output must be preserved. */
int32_t wisp_session_has_started_capture(WispSession* session);

/* Stop capture and wait for results to drain. Blocks when called outside a
 * Wisp callback. A reentrant call from this session's result/log callback
 * requests stop and returns immediately so that callback can unwind; a
 * subsequent wisp_session_free remains a full stop-and-callback barrier. */
void wisp_session_stop(WispSession* session);

/* Stop if necessary and free the session handle. When called reentrantly from
 * a Wisp callback, ownership is consumed immediately and destruction is
 * deferred until that callback unwinds and stop completes. */
void wisp_session_free(WispSession* session);

/* Returns the last error message recorded against this session, or NULL
 * if there is no recorded error. The returned pointer is owned by the
 * session and is invalidated by the next mutating call on it. */
const char* wisp_session_last_error_message(WispSession* session);

/* ----- Permissions -------------------------------------------------------- */

/* Permission identifiers. */
#define WISP_PERMISSION_MICROPHONE         0
#define WISP_PERMISSION_SPEECH_RECOGNITION 1

/* Status returned by wisp_permission_status / wisp_permission_request.
 * Negative values are reserved for "invalid permission id" / future use. */
#define WISP_PERMISSION_STATUS_UNDETERMINED 0
#define WISP_PERMISSION_STATUS_DENIED       1
#define WISP_PERMISSION_STATUS_GRANTED      2
#define WISP_PERMISSION_STATUS_RESTRICTED   3 /* speech only */

/* Returns the current status of the given permission without prompting. */
int32_t wisp_permission_status(int32_t permission);

/* Triggers the OS permission prompt (if the status is undetermined) and
 * blocks the caller until the user responds. If the status is already
 * granted/denied/restricted, returns immediately with the current value.
 * Safe to call from a background thread. */
int32_t wisp_permission_request(int32_t permission);

#ifdef __cplusplus
}
#endif

#endif /* WISP_AUDIOKIT_H */
