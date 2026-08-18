/**
 * Where the listening pill is allowed to sit.
 *
 * The clamp is the safety property: the pill carries the only one-click mute
 * in the app, so a stored position that lands outside the window is not a
 * cosmetic defect — it is an unreachable control. The threshold is the
 * usability property: without it every click would read as a drag.
 */

import assert from "node:assert/strict";
import test from "node:test";

import {
  clampIndicatorPosition,
  defaultIndicatorPosition,
  isIndicatorDrag,
  INDICATOR_DRAG_THRESHOLD_PX,
  INDICATOR_EDGE_MARGIN_PX,
} from "./indicatorPosition.ts";

const VIEWPORT = { width: 1200, height: 800 };
const PILL = { width: 176, height: 26 };

test("the default corner is the bottom right, not the occupied bottom left", () => {
  const position = defaultIndicatorPosition(VIEWPORT, PILL);
  assert.deepEqual(position, {
    x: VIEWPORT.width - PILL.width - INDICATOR_EDGE_MARGIN_PX,
    y: VIEWPORT.height - PILL.height - INDICATOR_EDGE_MARGIN_PX,
  });
  // The bottom-left corner is the sidebar profile card's and the reconnect
  // overlay's; landing on the same inset is the bug being fixed.
  assert.notEqual(position.x, INDICATOR_EDGE_MARGIN_PX);
});

test("a position inside the window is left alone", () => {
  assert.deepEqual(clampIndicatorPosition({ x: 400, y: 300 }, VIEWPORT, PILL), {
    x: 400,
    y: 300,
  });
});

test("a position saved on a larger display is pulled back inside", () => {
  // Reopening the app on a smaller screen must not strand the mute control.
  assert.deepEqual(
    clampIndicatorPosition({ x: 5_000, y: 4_000 }, VIEWPORT, PILL),
    {
      x: VIEWPORT.width - PILL.width - INDICATOR_EDGE_MARGIN_PX,
      y: VIEWPORT.height - PILL.height - INDICATOR_EDGE_MARGIN_PX,
    },
  );
});

test("a negative position is pushed off the top-left edges", () => {
  assert.deepEqual(clampIndicatorPosition({ x: -900, y: -5 }, VIEWPORT, PILL), {
    x: INDICATOR_EDGE_MARGIN_PX,
    y: INDICATOR_EDGE_MARGIN_PX,
  });
});

test("a window narrower than the pill still shows its leading edge", () => {
  // The legal range inverts here (max < min). Pinning to the margin keeps the
  // pill's left edge and its mute affordance on screen.
  const tiny = { width: 100, height: 40 };
  assert.deepEqual(clampIndicatorPosition({ x: 90, y: 90 }, tiny, PILL), {
    x: INDICATOR_EDGE_MARGIN_PX,
    y: INDICATOR_EDGE_MARGIN_PX,
  });
  assert.deepEqual(defaultIndicatorPosition(tiny, PILL), {
    x: INDICATOR_EDGE_MARGIN_PX,
    y: INDICATOR_EDGE_MARGIN_PX,
  });
});

test("a corrupted coordinate falls back to the near edge", () => {
  assert.deepEqual(
    clampIndicatorPosition({ x: Number.NaN, y: Number.NaN }, VIEWPORT, PILL),
    { x: INDICATOR_EDGE_MARGIN_PX, y: INDICATOR_EDGE_MARGIN_PX },
  );
});

test("only travel past the threshold counts as a drag", () => {
  assert.equal(isIndicatorDrag(0, 0), false);
  assert.equal(
    isIndicatorDrag(INDICATOR_DRAG_THRESHOLD_PX - 1, 0),
    false,
    "a click that wandered a pixel must still reach the mute toggle",
  );
  assert.equal(isIndicatorDrag(INDICATOR_DRAG_THRESHOLD_PX, 0), true);
  assert.equal(isIndicatorDrag(0, -INDICATOR_DRAG_THRESHOLD_PX), true);
  assert.equal(isIndicatorDrag(Number.NaN, 0), false);
});
