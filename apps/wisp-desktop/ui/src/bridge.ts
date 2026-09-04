/**
 * Bridge to the Rust host.
 *
 * Rust → JS: the host evaluates `window.__wisp.onEvent("<json>")` where the
 * argument is a JSON-encoded string. JS → Rust: `window.ipc.postMessage(json)`
 * (wry IPC), drained by the host's UI pump.
 */

import type { UiEvent, UiSnapshot } from "./types";

type Listener = (event: UiEvent) => void;

declare global {
  interface Window {
    ipc?: { postMessage(message: string): void };
    __wisp?: { onEvent(raw: string): void };
  }
}

const listeners = new Set<Listener>();

let snapshot: UiSnapshot = {
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
};

let bootedAt = Date.now();
let snapshotAt = Date.now();
let stateAt = Date.now();

export function getSnapshot(): UiSnapshot {
  return snapshot;
}

export function subscribe(listener: Listener): () => void {
  listeners.add(listener);
  return () => {
    listeners.delete(listener);
  };
}

function dispatch(event: UiEvent): void {
  switch (event.type) {
    case "state":
      snapshot = { ...snapshot, state: event };
      break;
    case "transcript":
      snapshot = { ...snapshot, transcript: event.segments };
      break;
    case "library":
      snapshot = { ...snapshot, sessions: event.sessions };
      break;
    case "notice":
      break;
  }
  snapshotAt = Date.now();
  if (event.type === "state") {
    stateAt = snapshotAt;
  }
  for (const listener of listeners) {
    listener(event);
  }
}

window.__wisp = {
  onEvent(raw: string): void {
    let event: UiEvent;
    try {
      event = JSON.parse(raw) as UiEvent;
    } catch {
      // Malformed payload — ignore.
      return;
    }
    dispatch(event);
  },
};

/** Forward uncaught JS errors to the host (logged on stderr while debugging). */
function reportJsError(message: string): void {
  send({ cmd: "__debugJsError", message });
}

window.addEventListener("error", (event) => {
  reportJsError(`${String(event.message)} @ ${String(event.filename)}:${String(event.lineno)}`);
});
window.addEventListener("unhandledrejection", (event) => {
  reportJsError(`unhandledrejection: ${String(event.reason)}`);
});

/** Announce readiness; the host replies with a full snapshot. */
export function boot(): void {
  bootedAt = Date.now();
  send({ cmd: "ready" });
}

/** Milliseconds since the host pushed the current snapshot. */
export function sinceSnapshot(): number {
  return Date.now() - Math.max(snapshotAt, bootedAt);
}

/** Milliseconds since the host pushed the last `state` event. */
export function sinceStatePush(): number {
  return Date.now() - Math.max(stateAt, bootedAt);
}

export function send(command: Record<string, unknown>): void {
  window.ipc?.postMessage(JSON.stringify(command));
}

export const commands = {
  toggleRecord: () => send({ cmd: "toggleRecord" }),
  toggleMute: () => send({ cmd: "toggleMute" }),
  newSession: () => send({ cmd: "newSession" }),
  openHistory: (sessionId: number) => send({ cmd: "openHistory", sessionId }),
  backToLibrary: () => send({ cmd: "backToLibrary" }),
  setLiveTitle: (title: string) => send({ cmd: "setLiveTitle", title }),
  renameSession: (sessionId: number, title: string) =>
    send({ cmd: "renameSession", sessionId, title }),
  requestPermission: (permission: "microphone" | "speech") =>
    send({ cmd: "requestPermission", permission }),
  openSettings: (permission: "microphone" | "speech") =>
    send({ cmd: "openSettings", permission }),
  copyTranscript: () => send({ cmd: "copyTranscript" }),
  exportTranscript: () => send({ cmd: "exportTranscript" }),
};
