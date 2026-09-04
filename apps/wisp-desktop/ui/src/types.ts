/**
 * DTOs mirrored from the Rust side (`apps/wisp-desktop/src/web_bridge.rs`).
 * Field names are camelCase via serde renaming.
 */

export type ViewKind = "library" | "live" | "history";

export type Phase = "idle" | "starting" | "recording" | "stopping" | "failed";

export type PermissionName = "microphone" | "speech";

export type PermissionStatus =
  | "undetermined"
  | "denied"
  | "granted"
  | "restricted";

export interface PermissionsState {
  microphone: PermissionStatus;
  speech: PermissionStatus;
  pending: PermissionName | null;
}

export interface StateEvent {
  type: "state";
  view: ViewKind;
  phase: Phase;
  elapsedMs: number;
  microphoneMuted: boolean;
  permissions: PermissionsState;
  liveTitle: string;
  historyTitle: string;
  historySessionId: number | null;
  historyStartedAt: string | null;
  historyDurationSeconds: number | null;
  pendingPersistence: boolean;
  error: string | null;
  canRecord: boolean;
}

export type SegmentSource = "mic" | "system";

export interface SegmentDto {
  source: SegmentSource;
  id: number;
  text: string;
  displayText: string;
  startSeconds: number;
  endSeconds: number;
  isFinal: boolean;
}

export interface TranscriptEvent {
  type: "transcript";
  segments: SegmentDto[];
}

export interface SessionDto {
  id: number;
  title: string;
  startedAt: string;
  endedAt: string | null;
  durationSeconds: number | null;
}

export interface LibraryEvent {
  type: "library";
  sessions: SessionDto[];
}

export interface NoticeEvent {
  type: "notice";
  kind: "success" | "error";
  message: string;
}

export type UiEvent =
  | StateEvent
  | TranscriptEvent
  | LibraryEvent
  | NoticeEvent;

export interface UiSnapshot {
  state: StateEvent;
  transcript: SegmentDto[];
  sessions: SessionDto[];
}

export const emptySnapshot = (): UiSnapshot => ({
  state: {
    type: "state",
    view: "library",
    phase: "idle",
    elapsedMs: 0,
    microphoneMuted: false,
    permissions: {
      microphone: "undetermined",
      speech: "undetermined",
      pending: null,
    },
    liveTitle: "",
    historyTitle: "",
    historySessionId: null,
    historyStartedAt: null,
    historyDurationSeconds: null,
    pendingPersistence: false,
    error: null,
    canRecord: false,
  },
  transcript: [],
  sessions: [],
});
