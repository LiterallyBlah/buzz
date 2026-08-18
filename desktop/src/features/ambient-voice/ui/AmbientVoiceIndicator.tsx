import { listen, type UnlistenFn } from "@tauri-apps/api/event";
import { Mic, MicOff } from "lucide-react";
import * as React from "react";

import { useFeatureEnabled } from "@/shared/features";
import { cn } from "@/shared/lib/cn";
import {
  AMBIENT_STATE_CHANGED_EVENT,
  ambientReportLabel,
  getAmbientVoiceStatus,
  setAmbientIndicatorPosition,
  setAmbientVoiceMuted,
  type AmbientIndicatorPosition,
  type AmbientVoiceStatusReport,
} from "../lib/ambientVoiceApi";
import {
  clampIndicatorPosition,
  defaultIndicatorPosition,
  isIndicatorDrag,
  FALLBACK_INDICATOR_BOX,
  type IndicatorBox,
} from "../lib/indicatorPosition";

/** A press in flight, from `pointerdown` until the pointer is released. */
type IndicatorDrag = {
  pointerId: number;
  /** Pointer position at press, in client coordinates. */
  fromX: number;
  fromY: number;
  /** Pill position at press. */
  originX: number;
  originY: number;
  /** Set once the pointer has travelled far enough to be a drag. */
  moved: boolean;
};

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
 *
 * The pill is dragged by its own body, because it is too small to carry a
 * separate handle and every part of it is a plausible grip. That makes the
 * press ambiguous — mute or move — so the pointer has to travel
 * `INDICATOR_DRAG_THRESHOLD_PX` before the press is treated as a drag, and a
 * drag suppresses the click the browser fires afterwards.
 */
export function AmbientVoiceIndicator({ className }: { className?: string }) {
  const featureEnabled = useFeatureEnabled("ambientVoice");
  const [report, setReport] = React.useState<AmbientVoiceStatusReport | null>(
    null,
  );
  const [position, setPosition] =
    React.useState<AmbientIndicatorPosition | null>(null);
  const [dragging, setDragging] = React.useState(false);
  const buttonRef = React.useRef<HTMLButtonElement | null>(null);
  const dragRef = React.useRef<IndicatorDrag | null>(null);
  // Set on the pointerup that ended a drag, consumed by the click the browser
  // fires immediately after it.
  const suppressClickRef = React.useRef(false);

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

  const measure = React.useCallback(
    (): IndicatorBox => ({
      width: buttonRef.current?.offsetWidth || FALLBACK_INDICATOR_BOX.width,
      height: buttonRef.current?.offsetHeight || FALLBACK_INDICATOR_BOX.height,
    }),
    [],
  );

  const persistedPosition = report?.indicatorPosition ?? null;
  const enabled = featureEnabled && (report?.enabled ?? false);

  /**
   * Move the pill to `next`, or re-clamp where it already is when `next` is
   * null. The current position wins over the persisted one so a drag whose
   * save failed is not undone by the next resize.
   */
  const settle = React.useCallback(
    (next: AmbientIndicatorPosition | null) => {
      const box = measure();
      const view = viewport();
      setPosition((current) => {
        const base =
          next ??
          current ??
          persistedPosition ??
          defaultIndicatorPosition(view, box);
        const clamped = clampIndicatorPosition(base, view, box);
        return current && current.x === clamped.x && current.y === clamped.y
          ? current
          : clamped;
      });
    },
    [measure, persistedPosition],
  );

  // Restore before the first paint: measuring in a layout effect means the
  // clamped position is what the user actually sees, not a corrected flash.
  // `settle` already changes identity with the persisted position, so this
  // re-runs when a stored position arrives.
  React.useLayoutEffect(() => {
    if (!enabled) return;
    settle(null);
  }, [enabled, settle]);

  // A window that shrank below the pill's parked position must not strand it
  // off-screen — there would be no way left to reach the mute control.
  React.useEffect(() => {
    if (!enabled) return;
    const onResize = () => settle(null);
    window.addEventListener("resize", onResize);
    return () => window.removeEventListener("resize", onResize);
  }, [enabled, settle]);

  if (!featureEnabled || !report?.enabled) return null;

  const placement =
    position ??
    persistedPosition ??
    defaultIndicatorPosition(viewport(), FALLBACK_INDICATOR_BOX);
  // The whole report, not just the status: a worker that is alive reports
  // "listening" even when nothing is reaching it, and this pill exists to say
  // what is happening to the audio. The live affordance follows the same fact —
  // truthful copy beside a lit microphone would still read as "it is hearing
  // me".
  const label = ambientReportLabel(report);
  const live = report.live && !report.audioStale;

  const positionFrom = (
    drag: IndicatorDrag,
    event: React.PointerEvent<HTMLButtonElement>,
  ) =>
    clampIndicatorPosition(
      {
        x: drag.originX + (event.clientX - drag.fromX),
        y: drag.originY + (event.clientY - drag.fromY),
      },
      viewport(),
      measure(),
    );

  return (
    <button
      aria-label={report.muted ? "Unmute ambient voice" : "Mute ambient voice"}
      className={cn(
        "fixed flex touch-none select-none items-center gap-1.5 rounded-full border border-border/50 px-2.5 py-1 text-2xs text-muted-foreground transition-colors hover:text-foreground",
        live && "text-foreground",
        dragging ? "cursor-grabbing" : "cursor-grab",
        className,
      )}
      data-dragging={dragging ? "true" : undefined}
      data-testid="ambient-voice-indicator"
      onClick={() => {
        if (suppressClickRef.current) {
          suppressClickRef.current = false;
          return;
        }
        void setAmbientVoiceMuted(!report.muted)
          .then(setReport)
          .catch(() => {
            /* the next state event will correct the control */
          });
      }}
      onPointerCancel={() => {
        dragRef.current = null;
        setDragging(false);
      }}
      onPointerDown={(event) => {
        if (event.button !== 0) return;
        dragRef.current = {
          pointerId: event.pointerId,
          fromX: event.clientX,
          fromY: event.clientY,
          originX: placement.x,
          originY: placement.y,
          moved: false,
        };
        event.currentTarget.setPointerCapture?.(event.pointerId);
      }}
      onPointerMove={(event) => {
        const drag = dragRef.current;
        if (!drag || drag.pointerId !== event.pointerId) return;
        if (
          !drag.moved &&
          !isIndicatorDrag(
            event.clientX - drag.fromX,
            event.clientY - drag.fromY,
          )
        ) {
          return;
        }
        drag.moved = true;
        setDragging(true);
        settle(positionFrom(drag, event));
      }}
      onPointerUp={(event) => {
        const drag = dragRef.current;
        dragRef.current = null;
        if (!drag || drag.pointerId !== event.pointerId) return;
        event.currentTarget.releasePointerCapture?.(event.pointerId);
        setDragging(false);
        // A press that never travelled is a click: leave it alone so the mute
        // toggle still works.
        if (!drag.moved) return;
        suppressClickRef.current = true;
        const parked = positionFrom(drag, event);
        settle(parked);
        void setAmbientIndicatorPosition(parked)
          .then(setReport)
          .catch(() => {
            /* the pill stays where it was dropped for this session */
          });
      }}
      ref={buttonRef}
      style={{ left: placement.x, top: placement.y }}
      title={label}
      type="button"
    >
      {report.muted ? (
        <MicOff className="h-3.5 w-3.5" />
      ) : (
        <Mic className={cn("h-3.5 w-3.5", live && "text-primary")} />
      )}
      <span className="truncate">{label}</span>
    </button>
  );
}

function viewport(): IndicatorBox {
  return { width: window.innerWidth, height: window.innerHeight };
}
