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
    int32_t     is_final,       /* 0=volatile, non-zero=final */
    const char* text_utf8,
    size_t      text_len,
    double      start_seconds,
    double      end_seconds,
    double      confidence_mean, /* NAN when unavailable */
    double      confidence_min,  /* NAN when unavailable */
    void*       user_data
);

/* Callback invoked for log lines. Same pointer-lifetime rules as
 * WispResultCallback. */
typedef void (*WispLogCallback)(
    const char* message_utf8,
    size_t      message_len,
    void*       user_data
);

/* Callback invoked for interleaved Float32 PCM. `samples` is valid only for
 * the duration of the callback. The callback must copy or enqueue without
 * blocking the real-time producer. */
typedef void (*WispAudioCallback)(
    int32_t      source,
    uint64_t     sequence,
    double       timestamp_seconds,
    uint32_t     sample_rate,
    uint32_t     channels,
    const float* samples,
    size_t       sample_count,
    void*        user_data
);

/* Typed SpeechAnalyzer failure callback. `terminal` is non-zero when the
 * recognizer cannot produce further results; zero identifies a recoverable
 * gap. String lifetime matches WispLogCallback. */
typedef void (*WispTranscriberErrorCallback)(
    int32_t     terminal,
    const char* message_utf8,
    size_t      message_len,
    void*       user_data
);

/* Invoked when bounded callback staging cannot accept PCM. */
typedef void (*WispAudioOverflowCallback)(
    int32_t  source,
    uint64_t dropped_frames,
    void*    user_data
);

/* Reserved terminal capture/recording failure callback. Unlike the log
 * callback, this lane is never used for ordinary diagnostics. */
typedef void (*WispTerminalErrorCallback)(
    int32_t     source,
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

/* Versioned constructor for backend-neutral capture/transcription options.
 * The original wisp_session_new symbol and five-argument ABI remain stable. */
WispSession* wisp_session_new_v2(
    const char*                  output_dir,
    const char*                  locale,
    int32_t                      transcription_enabled,
    int32_t                      allow_record_only,
    WispResultCallback           on_result,
    WispAudioCallback            on_audio,
    WispAudioOverflowCallback    on_audio_overflow,
    WispTranscriberErrorCallback on_transcriber_error,
    WispTerminalErrorCallback    on_terminal_error,
    WispLogCallback              on_log,
    void*                        user_data
);

/* Start capture and Ogg recording without requesting speech permission or
 * configuring SpeechAnalyzer. */
int32_t wisp_session_start_capture(WispSession* session);

/* Configure and start SpeechAnalyzer for an already-running capture. */
int32_t wisp_session_start_transcription(WispSession* session);

/* Start capture + transcription. Blocks until the session is ready
 * (permissions granted, model installed, audio flowing) or fails.
 * Returns 0 on success, non-zero on failure; query
 * wisp_session_last_error_message for details on failure. */
int32_t wisp_session_start(WispSession* session);

/* Returns 1 if microphone capture reached the running state, otherwise 0.
 * Query after a failed start and before stopping to decide whether partial
 * output must be preserved. */
int32_t wisp_session_has_started_capture(WispSession* session);

/* Replace microphone samples with silence while leaving system capture and
 * both recording timelines running. Pass non-zero to mute, zero to unmute. */
void wisp_session_set_microphone_muted(WispSession* session, int32_t muted);

/* Submit one orchestrated interleaved Float32 PCM frame to SpeechAnalyzer.
 * Returns non-zero when transcription is inactive or input is invalid; query
 * wisp_session_last_error_message for details. */
int32_t wisp_session_push_transcriber_audio(
    WispSession* session,
    int32_t source,
    uint32_t sample_rate,
    uint32_t channels,
    const float* samples,
    size_t sample_count
);

/* Cancel every SpeechAnalyzer while capture/Ogg recording continue. */
int32_t wisp_session_disable_transcription(WispSession* session);

/* Stop capture producers and finish Ogg recording while preserving analyzer
 * input/finalization state for wisp_session_finish_transcription. */
int32_t wisp_session_stop_capture(WispSession* session);

/* Finish analyzer input and wait for final transcript callbacks. */
int32_t wisp_session_finish_transcription(WispSession* session);

/* Stop capture and wait for results to drain. Blocks when called outside a
 * Wisp callback. A reentrant call from this session's result/log callback
 * requests stop and returns immediately so that callback can unwind; a
 * subsequent wisp_session_free remains a full stop-and-callback barrier. */
void wisp_session_stop(WispSession* session);

/* Stop capture without draining staged PCM or transcript finals. */
void wisp_session_abort(WispSession* session);

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
