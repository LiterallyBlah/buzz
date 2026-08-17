import { listen, type UnlistenFn } from "@tauri-apps/api/event";
import { Mic, MicOff } from "lucide-react";
import * as React from "react";

import { useFeatureEnabled } from "@/shared/features";
import { cn } from "@/shared/lib/cn";
import {
  AMBIENT_STATE_CHANGED_EVENT,
  ambientStatusLabel,
  getAmbientVoiceStatus,
  setAmbientVoiceMuted,
  type AmbientVoiceStatusReport,
} from "../lib/ambientVoiceApi";

/**
 * Always-visible listening indicator with an instant mute.
 *
 * A privacy requirement, not decoration. While the feature is on the operating
 * system's own microphone indicator is permanently lit, so the app has to be
 * able to say at a glance what it is doing with the audio — and let the user
 * shut the microphone in one click without hunting through settings.
 *
 * Renders nothing at all when the feature is off or not configured, so it
 * costs nothing for the users who never enable it.
 */
export function AmbientVoiceIndicator({ className }: { className?: string }) {
  const featureEnabled = useFeatureEnabled("ambientVoice");
  const [report, setReport] = React.useState<AmbientVoiceStatusReport | null>(
    null,
  );

  React.useEffect(() => {
    if (!featureEnabled) {
      setReport(null);
      return;
    }
    let disposed = false;
    let unlisten: UnlistenFn | null = null;
    void listen<AmbientVoiceStatusReport>(
      AMBIENT_STATE_CHANGED_EVENT,
      (event) => {
        if (!disposed) setReport(event.payload);
      },
    )
      .then((dispose) => {
        if (disposed) {
          dispose();
          return;
        }
        unlisten = dispose;
        void getAmbientVoiceStatus()
          .then((snapshot) => {
            if (!disposed) setReport((current) => current ?? snapshot);
          })
          .catch(() => {
            /* the indicator stays hidden until a state event arrives */
          });
      })
      .catch(() => {
        /* no listener, no indicator — never a broken control */
      });
    return () => {
      disposed = true;
      unlisten?.();
    };
  }, [featureEnabled]);

  if (!featureEnabled || !report?.enabled) return null;

  const label = ambientStatusLabel(report.status);
  return (
    <button
      aria-label={report.muted ? "Unmute ambient voice" : "Mute ambient voice"}
      className={cn(
        "flex items-center gap-1.5 rounded-full border border-border/50 px-2.5 py-1 text-2xs text-muted-foreground transition-colors hover:text-foreground",
        report.live && "text-foreground",
        className,
      )}
      data-testid="ambient-voice-indicator"
      onClick={() => {
        void setAmbientVoiceMuted(!report.muted)
          .then(setReport)
          .catch(() => {
            /* the next state event will correct the control */
          });
      }}
      title={label}
      type="button"
    >
      {report.muted ? (
        <MicOff className="h-3.5 w-3.5" />
      ) : (
        <Mic className={cn("h-3.5 w-3.5", report.live && "text-primary")} />
      )}
      <span className="truncate">{label}</span>
    </button>
  );
}
