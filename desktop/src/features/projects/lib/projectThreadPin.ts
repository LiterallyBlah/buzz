/**
 * When a project thread counts as "at the bottom", and what a reader who is
 * not at the bottom is owed.
 *
 * Split out from the hook that drives it because both rules are arithmetic on
 * numbers a scroll container happens to supply, and the failures worth
 * catching — a threshold that disagrees with the chat timeline's, a pill that
 * counts a deletion as an arrival — are visible in the arithmetic and invisible
 * in a rendered tree.
 *
 * Deliberately not shared with `features/messages/ui/anchoredScrollPolicy.ts`.
 * That module is coupled to the message virtualizer, which an issue thread of
 * a few dozen comments does not need; what the two surfaces must agree on is
 * the *number* below, not the implementation around it.
 */

/**
 * How close to the floor still counts as standing on it.
 *
 * Mirrors `AT_BOTTOM_THRESHOLD_PX` in `features/messages/ui/useAnchoredScroll.ts`
 * on purpose: a reader who is "at the bottom" of a channel and a reader who is
 * "at the bottom" of an issue are making the same claim about their own
 * attention, and two different numbers would mean one of the two surfaces
 * silently stops auto-following a few pixels sooner than the other.
 */
export const THREAD_AT_BOTTOM_THRESHOLD_PX = 32;

export type ThreadScrollMetrics = {
  clientHeight: number;
  scrollHeight: number;
  scrollTop: number;
};

/** Whether the reader is close enough to the newest comment to be following it. */
export function isThreadAtBottom({
  clientHeight,
  scrollHeight,
  scrollTop,
}: ThreadScrollMetrics): boolean {
  return (
    scrollHeight - clientHeight - scrollTop <= THREAD_AT_BOTTOM_THRESHOLD_PX
  );
}

/**
 * The next value of the "N new ↓" count.
 *
 * Three rules, and each one exists because the obvious implementation gets it
 * wrong:
 *
 *  - At the bottom the count is zero, not "unchanged". The reader is looking
 *    at the arrivals, so a surviving count would be a pill offering to take
 *    them somewhere they already are.
 *  - A shrinking thread decrements. A comment deleted while the reader is
 *    scrolled up was very likely one of the comments being counted, and a pill
 *    promising three comments that resolve to two is worse than undercounting.
 *  - The count can never exceed the thread. Clamping to `commentCount` is what
 *    keeps a deletion of an already-read comment from stranding the count
 *    above what a jump to the bottom could possibly reveal.
 */
export function nextUnreadBelowFold({
  atBottom,
  commentCount,
  previousCommentCount,
  unread,
}: {
  atBottom: boolean;
  commentCount: number;
  previousCommentCount: number;
  unread: number;
}): number {
  if (atBottom) return 0;
  const arrived = commentCount - previousCommentCount;
  return Math.max(0, Math.min(unread + arrived, commentCount));
}

/**
 * The set of agents working on a root, as one comparable value.
 *
 * A string rather than an array because it is used as an effect dependency:
 * the activity hook rebuilds its result on every two-second staleness tick, so
 * an array would wake the comparison a few dozen times per minute to discover
 * that the same two agents are still working.
 */
export function workingAgentsKey(
  entries: readonly { agent: string; state: string }[],
): string {
  return entries
    .filter((entry) => entry.state === "working")
    .map((entry) => entry.agent)
    .sort()
    .join(",");
}

/**
 * Whether an agent stopped working between two observations.
 *
 * This is the other half of what a scrolled-up reader is owed, and it is not
 * expressible as a comment count. An agent handed an issue by a peer call
 * announces NIP-PA activity for the length of its turn and may leave no
 * comment at all, so a pill that only counted comments would let the one
 * event the reader is waiting for — the agent finishing — pass in silence.
 *
 * Only removals count. An agent *starting* is not something to be caught up
 * on: the reader can already see it in the rail, and it resolves into either a
 * comment or a settled turn, both of which are counted when they happen.
 */
export function hasTurnSettled(
  previousKey: string,
  currentKey: string,
): boolean {
  if (previousKey === currentKey || !previousKey) return false;
  const current = new Set(currentKey ? currentKey.split(",") : []);
  return previousKey.split(",").some((agent) => !current.has(agent));
}
