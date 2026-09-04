import { MicrophoneIcon, SpeakerHighIcon } from "@phosphor-icons/react";
import { Badge, Button, Text } from "@cloudflare/kumo";
import { commands } from "../bridge";
import type { PermissionName, PermissionsState, PermissionStatus } from "../types";

const STATUS_VARIANT: Record<PermissionStatus, "secondary" | "success" | "error" | "warning"> = {
  undetermined: "secondary",
  granted: "success",
  denied: "error",
  restricted: "warning",
};

const STATUS_LABEL: Record<PermissionStatus, string> = {
  undetermined: "Not requested",
  granted: "Granted",
  denied: "Denied",
  restricted: "Restricted",
};

function PermissionRow({
  name,
  icon,
  title,
  rationale,
  status,
  pending,
}: {
  name: PermissionName;
  icon: React.ReactNode;
  title: string;
  rationale: string;
  status: PermissionStatus;
  pending: boolean;
}): React.JSX.Element {
  return (
    <div className="flex items-center gap-4 rounded-lg border border-kumo-hairline bg-kumo-surface p-4">
      <div className="flex h-10 w-10 shrink-0 items-center justify-center rounded-lg bg-kumo-fill">
        {icon}
      </div>
      <div className="min-w-0 flex-1">
        <div className="flex items-center gap-2">
          <Text bold as="span">
            {title}
          </Text>
          <Badge variant={STATUS_VARIANT[status]}>
            {pending ? "Waiting…" : STATUS_LABEL[status]}
          </Badge>
        </div>
        <Text variant="secondary" size="sm" as="p">
          {rationale}
        </Text>
      </div>
      {status === "denied" || status === "restricted" ? (
        <Button variant="secondary" onClick={() => commands.openSettings(name)}>
          Open Settings
        </Button>
      ) : status === "granted" ? null : (
        <Button variant="primary" loading={pending} onClick={() => commands.requestPermission(name)}>
          Grant
        </Button>
      )}
    </div>
  );
}

export function Onboarding({ permissions }: { permissions: PermissionsState }): React.JSX.Element {
  return (
    <div className="flex h-dvh w-full items-center justify-center bg-kumo-canvas p-8 overflow-y-auto">
      <div className="flex w-full max-w-xl flex-col gap-6">
        <div className="flex flex-col gap-1">
          <Text variant="heading" size="lg" as="h1">
            Welcome to Wisp
          </Text>
          <Text variant="secondary" as="p">
            Wisp records your microphone and system audio at the same time and
            transcribes both on-device. Nothing ever leaves your Mac.
          </Text>
        </div>

        <div className="flex flex-col gap-3">
          <PermissionRow
            name="microphone"
            icon={<MicrophoneIcon size={22} />}
            title="Microphone"
            rationale="Capture your voice for on-device transcription."
            status={permissions.microphone}
            pending={permissions.pending === "microphone"}
          />
          <PermissionRow
            name="speech"
            icon={<SpeakerHighIcon size={22} />}
            title="Speech Recognition"
            rationale="Run Apple's on-device speech model on captured audio."
            status={permissions.speech}
            pending={permissions.pending === "speech"}
          />
        </div>

        <Text variant="secondary" size="sm" as="p">
          You can change these later in System Settings → Privacy &amp; Security.
          This screen updates automatically once permissions are granted.
        </Text>
      </div>
    </div>
  );
}
