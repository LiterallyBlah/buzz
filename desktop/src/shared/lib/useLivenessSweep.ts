import { useEffect, useEffectEvent } from "react";

/**
 * Re-evaluate a liveness map on a timer, but only while it can matter.
 *
 * Expiry is a function of the clock, so without a tick an entry whose last
 * frame aged out stays on screen until some unrelated event forces a render.
 * The tick is therefore not optional — but it must not be unconditional
 * either: a component holding no live entries has nothing to expire, and an
 * app full of such components would hold a timer each for a state that cannot
 * change. `active` is the "there is something to expire" condition, and unmount
 * is the "nobody is watching" one — the React counterpart of the store's rule
 * that the sweep runs only with both work and an audience.
 *
 * `onSweep` is read as an effect event, so a fresh closure every render does
 * not restart the interval and reset its phase.
 */
export function useLivenessSweep(
  active: boolean,
  intervalMs: number,
  onSweep: () => void,
): void {
  const sweep = useEffectEvent(() => {
    onSweep();
  });

  useEffect(() => {
    if (!active) return;
    const timer = setInterval(() => sweep(), intervalMs);
    return () => clearInterval(timer);
  }, [active, intervalMs]);
}
