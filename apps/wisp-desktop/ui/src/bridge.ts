/**
 * Bridge to the Rust host over the loopback server.
 *
 * Rust → JS: JSON payloads on an SSE stream (`GET /events`).
 * JS → Rust: JSON commands via `POST /cmd` (failures surface as HTTP 400s
 * logged by the host).
 *
 * In dev mode (`npm run dev`), the app is opened as
 * `<dev-url>?wisp=<encoded loopback root incl. token>`; the `wisp` query
 * parameter selects the bridge base URL and carries the token. In the
 * packaged app the page is served from the loopback origin itself.
 */

import type { UiEvent, UiSnapshot } from "./types";

type Listener = (event: UiEvent) => void;

const params = new URLSearchParams(window.location.search);
const base = params.get("wisp") ?? "";
const token =
  new URL(base || window.location.href, window.location.href).searchParams.get("token") ?? "";

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

/** Forward uncaught JS errors to the host (logged on stderr while debugging). */
function reportJsError(message: string): void {
  void fetch(`${base}/cmd?token=${encodeURIComponent(token)}`, {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify({ cmd: "__debugJsError", message }),
  });
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
  const source = new EventSource(`${base}/events?token=${encodeURIComponent(token)}`);
  source.onopen = () => {
    send({ cmd: "ready" });
  };
  source.onmessage = (message) => {
    try {
      dispatch(JSON.parse(message.data) as UiEvent);
    } catch {
      // Malformed payload — ignore.
    }
  };
  source.onerror = () => {
    // EventSource reconnects automatically (the host process is going away
    // only when the app quits).
  };
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
  void fetch(`${base}/cmd?token=${encodeURIComponent(token)}`, {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify(command),
  }).then((response) => {
    if (!response.ok) {
      reportJsError(`command ${String((command as { cmd?: string }).cmd)} failed: ${String(response.status)}`);
    }
  });
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
