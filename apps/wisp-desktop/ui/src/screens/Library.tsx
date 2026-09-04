import { MicrophoneIcon, PlusIcon, WaveformIcon } from "@phosphor-icons/react";
import { Badge, Button, Empty, Text } from "@cloudflare/kumo";
import { Table } from "@cloudflare/kumo/components/table";
import { commands } from "../bridge";
import { formatDuration, formatStartedAt } from "../App";
import type { UiSnapshot } from "../types";

export function Library({ snapshot }: { snapshot: UiSnapshot }): React.JSX.Element {
  const { sessions, state } = snapshot;

  return (
    <div className="flex h-full flex-col overflow-hidden">
      <header className="flex items-center justify-between border-b border-kumo-hairline px-6 py-4">
        <div className="flex flex-col gap-0.5">
          <Text variant="heading" size="lg" as="h1">
            Sessions
          </Text>
          <Text variant="secondary" size="sm" as="p">
            {sessions.length === 0
              ? "Recordings and transcripts stay on this Mac."
              : `${sessions.length} session${sessions.length === 1 ? "" : "s"} saved locally.`}
          </Text>
        </div>
        <Button variant="primary" icon={PlusIcon} onClick={commands.newSession}>
          New Session
        </Button>
      </header>

      <div className="min-h-0 flex-1 overflow-y-auto px-6 py-4">
        {sessions.length === 0 ? (
          <div className="flex h-full items-center justify-center">
            <Empty
              icon={<WaveformIcon size={40} />}
              title="No sessions yet"
              description="Start a new session to capture a meeting, a call, or a voice memo — transcribed entirely on-device."
              contents={
                <Button variant="primary" icon={PlusIcon} onClick={commands.newSession}>
                  New Session
                </Button>
              }
            />
          </div>
        ) : (
          <Table>
            <Table.Header variant="compact">
              <Table.Row>
                <Table.Head>Title</Table.Head>
                <Table.Head className="w-44">Started</Table.Head>
                <Table.Head className="w-24">Duration</Table.Head>
                <Table.Head className="w-16" />
              </Table.Row>
            </Table.Header>
            <Table.Body>
              {sessions.map((session) => (
                <Table.Row
                  key={session.id}
                  className="cursor-pointer"
                  onClick={() => commands.openHistory(session.id)}
                >
                  <Table.Cell>
                    <span className="font-medium">{session.title}</span>
                  </Table.Cell>
                  <Table.Cell>
                    <Text variant="secondary" size="sm" as="span">
                      {formatStartedAt(session.startedAt)}
                    </Text>
                  </Table.Cell>
                  <Table.Cell>
                    <Text variant="secondary" size="sm" as="span">
                      {formatDuration(session.durationSeconds)}
                    </Text>
                  </Table.Cell>
                  <Table.Cell>
                    {session.endedAt === null ? (
                      <Badge variant="error">
                        <MicrophoneIcon size={12} /> live
                      </Badge>
                    ) : null}
                  </Table.Cell>
                </Table.Row>
              ))}
            </Table.Body>
          </Table>
        )}
      </div>

      {state.pendingPersistence ? (
        <footer className="border-t border-kumo-hairline px-6 py-3">
          <Text variant="error" size="sm" as="p">
            A session still needs to be saved. Press ⌘R or use the menu to retry
            before starting a new recording.
          </Text>
        </footer>
      ) : null}
    </div>
  );
}
