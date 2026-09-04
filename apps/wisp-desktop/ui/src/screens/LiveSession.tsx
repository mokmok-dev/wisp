import { ArrowClockwiseIcon, MicrophoneIcon, MicrophoneSlashIcon, RecordIcon, StopIcon } from "@phosphor-icons/react";
import { Badge, Button, Input, Text } from "@cloudflare/kumo";
import { commands, sinceStatePush } from "../bridge";
import { formatClock, useTicker } from "../App";
import type { UiSnapshot } from "../types";
import { TranscriptList } from "../components/TranscriptList";

const PHASE_LABEL: Record<string, string> = {
  idle: "Ready",
  starting: "Starting…",
  recording: "Recording",
  stopping: "Finishing…",
  failed: "Failed",
};

export function LiveSession({ snapshot }: { snapshot: UiSnapshot }): React.JSX.Element {
  const { state, transcript } = snapshot;
  const active = state.phase === "recording";
  useTicker(active);

  // The host stamps `elapsedMs` when it pushes state; interpolate locally
  // between pushes so the clock stays smooth without extra IPC traffic.
  const elapsed = active ? state.elapsedMs + sinceStatePush() : state.elapsedMs;

  const busy = state.phase === "starting" || state.phase === "stopping";
  const canStart = state.phase === "idle" && !state.pendingPersistence;

  return (
    <div className="flex h-full flex-col overflow-hidden">
      <header className="flex items-center gap-3 border-b border-kumo-hairline px-6 py-4">
        <div className="flex min-w-0 flex-1 items-center gap-2">
          <Input
            aria-label="Session title"
            placeholder="Untitled session"
            value={state.liveTitle}
            disabled={state.phase !== "idle" && state.phase !== "recording"}
            onChange={(event) => commands.setLiveTitle(event.currentTarget.value)}
            className="max-w-md"
          />
        </div>
        <div className="flex items-center gap-3">
          {active ? <span className="wisp-rec-dot" aria-hidden /> : null}
          <Badge variant={state.phase === "failed" ? "error" : active ? "error" : "secondary"}>
            {state.pendingPersistence ? "Unsaved" : PHASE_LABEL[state.phase] ?? state.phase}
          </Badge>
          <span className="font-mono text-lg text-kumo-subtle tabular-nums">
            {formatClock(elapsed)}
          </span>
          {canStart || state.phase === "failed" ? (
            <Button
              variant="primary"
              icon={state.phase === "failed" ? ArrowClockwiseIcon : RecordIcon}
              loading={busy}
              onClick={commands.toggleRecord}
            >
              {state.phase === "failed" ? "Retry" : "Record"}
            </Button>
          ) : (
            <Button
              variant={active ? "secondary" : "primary"}
              icon={active ? StopIcon : RecordIcon}
              loading={busy}
              onClick={commands.toggleRecord}
            >
              {active ? "Stop" : busy ? "…" : "Record"}
            </Button>
          )}
          {active ? (
            <Button
              variant={state.microphoneMuted ? "primary" : "secondary"}
              icon={state.microphoneMuted ? MicrophoneSlashIcon : MicrophoneIcon}
              onClick={commands.toggleMute}
              title={state.microphoneMuted ? "Unmute microphone" : "Mute microphone"}
            >
              {state.microphoneMuted ? "Unmute" : "Mute"}
            </Button>
          ) : null}
        </div>
      </header>

      <div className="min-h-0 flex-1 overflow-hidden px-6 py-4">
        <TranscriptList segments={transcript} follow />
      </div>

      <footer className="flex items-center justify-between border-t border-kumo-hairline px-6 py-2.5">
        <Text variant="secondary" size="xs" as="span">
          Transcribed on-device · mic and system audio are kept as separate tracks
        </Text>
        <Text variant="secondary" size="xs" as="span">
          ⌘R to start / stop
        </Text>
      </footer>
    </div>
  );
}
