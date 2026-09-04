import { useEffect, useRef } from "react";
import { Badge, Text } from "@cloudflare/kumo";
import type { SegmentDto } from "../types";

const SOURCE_LABEL: Record<SegmentDto["source"], string> = {
  mic: "MIC",
  system: "SYS",
};

const SOURCE_ACCENT: Record<SegmentDto["source"], string> = {
  mic: "border-l-blue-400",
  system: "border-l-orange-300",
};

const SOURCE_BADGE: Record<SegmentDto["source"], "primary" | "warning"> = {
  mic: "primary",
  system: "warning",
};

export function TranscriptList({
  segments,
  follow = false,
}: {
  segments: SegmentDto[];
  /** Keep the newest segment in view as it arrives (live view). */
  follow?: boolean;
}): React.JSX.Element {
  const containerRef = useRef<HTMLDivElement | null>(null);
  const stickToBottom = useRef(true);

  // Follow the newest segment while the user has not scrolled away from the
  // bottom; scrolling up pins the view so earlier lines can be read.
  useEffect(() => {
    const container = containerRef.current;
    if (!container) {
      return;
    }
    const onScroll = () => {
      stickToBottom.current =
        container.scrollHeight - container.scrollTop - container.clientHeight < 48;
    };
    container.addEventListener("scroll", onScroll);
    return () => container.removeEventListener("scroll", onScroll);
  }, []);

  const lastCount = useRef(0);
  useEffect(() => {
    const container = containerRef.current;
    if (!container) {
      return;
    }
    if (follow && stickToBottom.current && segments.length !== lastCount.current) {
      container.scrollTop = container.scrollHeight;
    }
    lastCount.current = segments.length;
  }, [follow, segments]);

  if (segments.length === 0) {
    return (
      <div className="flex h-full items-center justify-center">
        <Text variant="secondary" as="p">
          Transcript will appear here as it is recognized.
        </Text>
      </div>
    );
  }

  return (
    <div ref={containerRef} className="h-full overflow-y-auto">
      <div className="flex flex-col gap-2">
        {segments.map((segment, index) => {
          const isLatest = index === segments.length - 1;
          return (
            <div
              key={`${segment.source}-${segment.id}`}
              className={`rounded-md border border-kumo-hairline bg-kumo-surface border-l-2 px-4 py-3 ${SOURCE_ACCENT[segment.source]}`}
            >
              <div className="mb-1 flex items-center gap-2">
                <Badge variant={SOURCE_BADGE[segment.source]}>{SOURCE_LABEL[segment.source]}</Badge>
                {segment.isFinal ? null : (
                  <Text variant="secondary" size="xs" as="span">
                    transcribing…
                  </Text>
                )}
              </div>
              <p className="whitespace-pre-wrap text-[15px] leading-relaxed text-kumo-default">
                {segment.displayText}
                {!segment.isFinal && isLatest ? <span className="wisp-caret" aria-hidden /> : null}
              </p>
            </div>
          );
        })}
      </div>
    </div>
  );
}
