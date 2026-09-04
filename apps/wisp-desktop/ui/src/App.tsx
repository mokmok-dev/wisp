import { useEffect, useRef, useState } from "react";
import { MicrophoneIcon, WaveformIcon } from "@phosphor-icons/react";
import { Badge, Text, ToastProvider, useKumoToastManager } from "@cloudflare/kumo";
import { Sidebar } from "@cloudflare/kumo/components/sidebar";
import { commands, getSnapshot, isReady, subscribe } from "./bridge";
import { emptySnapshot, type UiEvent, type UiSnapshot } from "./types";
import { Library } from "./screens/Library";
import { LiveSession } from "./screens/LiveSession";
import { History } from "./screens/History";
import { Onboarding } from "./screens/Onboarding";

/** Subscribe the component tree to bridge events. */
export function useSnapshot(): UiSnapshot {
  const [snapshot, setSnapshot] = useState<UiSnapshot>(isReady() ? getSnapshot() : emptySnapshot());
  useEffect(() => {
    const update = () => setSnapshot(getSnapshot());
    update();
    return subscribe(update);
  }, []);
  return snapshot;
}

/** Surface host notices and errors as toasts. */
function useNotices(): void {
  const toasts = useKumoToastManager();
  const lastError = useRef<string | null>(null);

  useEffect(() => {
    return subscribe((event: UiEvent) => {
      if (event.type === "notice") {
        toasts.add({
          variant: event.kind === "success" ? "success" : "error",
          content: event.message,
        });
        return;
      }
      if (event.type === "state") {
        if (event.error && event.error !== lastError.current) {
          lastError.current = event.error;
          toasts.add({ variant: "error", content: event.error });
        }
        if (!event.error) {
          lastError.current = null;
        }
      }
    });
  }, [toasts]);
}

function WispSidebar({ snapshot }: { snapshot: UiSnapshot }): React.JSX.Element {
  const { state, sessions } = snapshot;
  const recording =
    state.phase === "recording" || state.phase === "starting" || state.phase === "stopping";
  const activeSessionId = state.view === "history" ? currentHistoryId(state) : null;

  return (
    <Sidebar>
      <Sidebar.Header>
        <Text variant="heading" as="span">
          Wisp
        </Text>
        <Badge variant={recording ? "error" : "secondary"}>{recording ? "REC" : "idle"}</Badge>
      </Sidebar.Header>
      <Sidebar.Content>
        <Sidebar.Group>
          <Sidebar.GroupLabel>Actions</Sidebar.GroupLabel>
          <Sidebar.Menu>
            <Sidebar.MenuItem>
              <Sidebar.MenuButton
                icon={MicrophoneIcon}
                active={state.view === "live"}
                onClick={commands.newSession}
              >
                New Session
              </Sidebar.MenuButton>
            </Sidebar.MenuItem>
          </Sidebar.Menu>
        </Sidebar.Group>
        <Sidebar.Group>
          <Sidebar.GroupLabel>Sessions</Sidebar.GroupLabel>
          {sessions.length === 0 ? (
            <div className="px-2">
              <Text variant="secondary" size="sm" as="p">
                No saved sessions yet.
              </Text>
            </div>
          ) : (
            <Sidebar.Menu>
              {sessions.map((session) => (
                <Sidebar.MenuItem key={session.id}>
                  <Sidebar.MenuButton
                    icon={WaveformIcon}
                    active={activeSessionId === session.id}
                    onClick={() => commands.openHistory(session.id)}
                  >
                    <span className="truncate">{session.title}</span>
                  </Sidebar.MenuButton>
                </Sidebar.MenuItem>
              ))}
            </Sidebar.Menu>
          )}
        </Sidebar.Group>
      </Sidebar.Content>
      <Sidebar.Footer>
        <Text variant="secondary" size="xs" as="span">
          On-device · private by default
        </Text>
      </Sidebar.Footer>
    </Sidebar>
  );
}

function currentHistoryId(state: UiSnapshot["state"]): number | null {
  return state.historySessionId ?? null;
}

export function App(): React.JSX.Element {
  const snapshot = useSnapshot();
  useNotices();
  const { state } = snapshot;
  const onboarding = !state.canRecord;

  return (
    <ToastProvider>
      {onboarding ? (
        <Onboarding permissions={state.permissions} />
      ) : (
        <Sidebar.Provider contained defaultOpen>
          <div className="flex h-full w-full overflow-hidden">
            <WispSidebar snapshot={snapshot} />
            <main className="flex min-w-0 flex-1 flex-col overflow-hidden">
              {state.view === "live" ? (
                <LiveSession snapshot={snapshot} />
              ) : state.view === "history" ? (
                <History snapshot={snapshot} />
              ) : (
                <Library snapshot={snapshot} />
              )}
            </main>
          </div>
        </Sidebar.Provider>
      )}
    </ToastProvider>
  );
}

/** Format elapsed milliseconds as `mm:ss` (or `h:mm:ss` past one hour). */
export function formatClock(totalMs: number): string {
  const totalSeconds = Math.max(0, Math.floor(totalMs / 1000));
  const hours = Math.floor(totalSeconds / 3600);
  const minutes = Math.floor((totalSeconds % 3600) / 60);
  const seconds = totalSeconds % 60;
  const mm = String(minutes).padStart(2, "0");
  const ss = String(seconds).padStart(2, "0");
  return hours > 0 ? `${hours}:${mm}:${ss}` : `${mm}:${ss}`;
}

export function formatDuration(seconds: number | null): string {
  if (seconds === null || seconds < 0) {
    return "—";
  }
  return formatClock(seconds * 1000);
}

export function formatStartedAt(iso: string): string {
  const date = new Date(iso);
  if (Number.isNaN(date.getTime())) {
    return iso;
  }
  return date.toLocaleString(undefined, {
    year: "numeric",
    month: "2-digit",
    day: "2-digit",
    hour: "2-digit",
    minute: "2-digit",
  });
}

/** Re-render on an interval while `active` (for the elapsed clock). */
export function useTicker(active: boolean, periodMs = 250): void {
  const [, setTick] = useState(0);
  useEffect(() => {
    if (!active) {
      return;
    }
    const id = window.setInterval(() => setTick((t) => t + 1), periodMs);
    return () => window.clearInterval(id);
  }, [active, periodMs]);
}
