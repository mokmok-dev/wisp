import { ArrowLeftIcon, CopyIcon, DownloadSimpleIcon } from "@phosphor-icons/react";
import { Badge, Button, Input, Text } from "@cloudflare/kumo";
import { commands } from "../bridge";
import { formatDuration, formatStartedAt } from "../App";
import type { UiSnapshot } from "../types";
import { TranscriptList } from "../components/TranscriptList";

export function History({ snapshot }: { snapshot: UiSnapshot }): React.JSX.Element {
  const { state, transcript } = snapshot;

  return (
    <div className="flex h-full flex-col overflow-hidden">
      <header className="flex items-center gap-3 border-b border-kumo-hairline px-6 py-4">
        <Button variant="ghost" shape="square" aria-label="Back to library" icon={ArrowLeftIcon} onClick={commands.backToLibrary} />
        <div className="flex min-w-0 flex-1 items-center gap-2">
          <Input
            aria-label="Session title"
            defaultValue={state.historyTitle}
            onBlur={(event) => {
              if (state.historySessionId !== null) {
                commands.renameSession(state.historySessionId, event.currentTarget.value);
              }
            }}
            className="max-w-md"
          />
        </div>
        <div className="flex items-center gap-2">
          <Text variant="secondary" size="sm" as="span">
            {formatStartedAt(state.historyStartedAt ?? "")}
          </Text>
          <Badge variant="secondary">{formatDuration(state.historyDurationSeconds)}</Badge>
          <Button variant="secondary" icon={CopyIcon} onClick={commands.copyTranscript}>
            Copy
          </Button>
          <Button variant="primary" icon={DownloadSimpleIcon} onClick={commands.exportTranscript}>
            Export
          </Button>
        </div>
      </header>

      <div className="min-h-0 flex-1 overflow-y-auto px-6 py-4">
        <TranscriptList segments={transcript} />
      </div>

      <footer className="flex items-center justify-between border-t border-kumo-hairline px-6 py-2.5">
        <Text variant="secondary" size="xs" as="span">
          stored locally as Ogg/Opus + SQLite
        </Text>
        <Text variant="secondary" size="xs" as="span">
          ⌘⇧C copy · ⌘⇧E export
        </Text>
      </footer>
    </div>
  );
}
