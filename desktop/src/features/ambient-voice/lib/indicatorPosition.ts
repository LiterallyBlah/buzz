import type { AmbientIndicatorPosition } from "./ambientVoiceApi";

/**
 * Geometry for the draggable listening indicator.
 *
 * Pure on purpose: where the pill may sit, and what separates a click from a
 * drag, are the two things that decide whether the control is usable, and
 * neither of them needs a DOM to be decided or tested.
 */

/** A window, or the pill itself, in CSS pixels. */
export type IndicatorBox = { width: number; height: number };

/**
 * Gap kept between the pill and every window edge.
 *
 * 12px matches the `bottom-3` inset the app's other floating overlays already
 * use (`RelayConnectionOverlay`), so a pill left in its default corner lines up
 * with them rather than sitting a few pixels off.
 */
export const INDICATOR_EDGE_MARGIN_PX = 12;

/**
 * How far the pointer must travel before a press counts as a drag.
 *
 * Without this the mute toggle would be unusable: a click almost always moves
 * the pointer a pixel or two between press and release, and treating that as a
 * drag would swallow the click.
 */
export const INDICATOR_DRAG_THRESHOLD_PX = 4;

/**
 * Size assumed for the pill before it has been measured.
 *
 * Only ever used for the frame between first render and the layout effect that
 * measures the real element; a wrong guess shifts the default corner, it never
 * strands the pill, because the measured value is clamped in before paint.
 */
export const FALLBACK_INDICATOR_BOX: IndicatorBox = { width: 176, height: 26 };

/**
 * Where the pill sits for a user who has never dragged it: the bottom-right
 * corner.
 *
 * Bottom-**left** is the corner the dogfood report was about — the sidebar's
 * profile card sits there, and `RelayConnectionOverlay` claims the same inset
 * for the reconnect card. The bottom-right corner is the only one of the four
 * with no fixed chrome of its own (the top edge carries the window controls
 * and the channel header's actions), and it keeps the pill on the bottom rail
 * where the user already looks for session state.
 */
export function defaultIndicatorPosition(
  viewport: IndicatorBox,
  indicator: IndicatorBox,
): AmbientIndicatorPosition {
  return clampIndicatorPosition(
    {
      x: viewport.width - indicator.width - INDICATOR_EDGE_MARGIN_PX,
      y: viewport.height - indicator.height - INDICATOR_EDGE_MARGIN_PX,
    },
    viewport,
    indicator,
  );
}

/**
 * Pull a position back inside the window.
 *
 * Applied on restore and on every resize, so a pill parked at the edge of a
 * large display cannot be stranded off-screen by reopening the app on a small
 * one — the failure mode that would leave the user with no mute control and no
 * way to reach it.
 */
export function clampIndicatorPosition(
  position: AmbientIndicatorPosition,
  viewport: IndicatorBox,
  indicator: IndicatorBox,
): AmbientIndicatorPosition {
  return {
    x: clampAxis(position.x, viewport.width, indicator.width),
    y: clampAxis(position.y, viewport.height, indicator.height),
  };
}

function clampAxis(value: number, viewport: number, indicator: number): number {
  const min = INDICATOR_EDGE_MARGIN_PX;
  // A window narrower than the pill has no legal range at all. Taking the
  // margin as the floor keeps the pill's leading edge visible instead of
  // letting an inverted range push it off the other side.
  const max = Math.max(min, viewport - indicator - INDICATOR_EDGE_MARGIN_PX);
  // A non-finite coordinate can only come from a corrupted store or a
  // synthetic pointer event; the near edge is the safe answer.
  if (!Number.isFinite(value)) return min;
  return Math.round(Math.min(Math.max(value, min), max));
}

/** Whether pointer travel since the press counts as a drag rather than a click. */
export function isIndicatorDrag(dx: number, dy: number): boolean {
  if (!Number.isFinite(dx) || !Number.isFinite(dy)) return false;
  return (
    Math.abs(dx) >= INDICATOR_DRAG_THRESHOLD_PX ||
    Math.abs(dy) >= INDICATOR_DRAG_THRESHOLD_PX
  );
}
