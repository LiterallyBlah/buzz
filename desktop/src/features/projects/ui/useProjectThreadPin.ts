import * as React from "react";

import {
  hasTurnSettled,
  isThreadAtBottom,
  nextUnreadBelowFold,
} from "@/features/projects/lib/projectThreadPin";

/**
 * Keeps a project thread standing on its newest comment.
 *
 * The whole point of the surface: with an agent replying every few seconds,
 * the bottom of the thread is the only place worth being, and it should not
 * cost a scroll to get there or a decision to stay. So the thread opens on its
 * newest comment, stays there while the reader is at the floor, and — the part
 * that makes leaving the floor safe — counts what arrived while they were away
 * instead of silently growing underneath them.
 *
 * "Newest comment" and "the floor" are the same place only while comments
 * exist; with none, the floor is the end of the issue description and opening
 * there is wrong. See `openThread`.
 *
 * Three things drive a re-pin, and the third is the one that is easy to omit
 * and then spend an afternoon on: new comments, opening a different issue, and
 * the *existing* content changing height. Markdown images, an expanding
 * composer and a rendered code block all settle after layout, and a thread
 * that pinned once on mount ends up a few hundred pixels short of the bottom
 * with no scroll event to tell it so.
 */
export function useProjectThreadPin({
  commentCount,
  rootId,
  workingAgents,
}: {
  commentCount: number;
  /** Opening a different issue re-opens at that issue's newest comment. */
  rootId: string;
  /**
   * Who is working on this root right now, from
   * `workingAgentsKey`. An agent leaving this set is a turn ending, which is
   * activity a scrolled-up reader is owed even when it produced no comment.
   */
  workingAgents: string;
}) {
  const scrollRef = React.useRef<HTMLDivElement | null>(null);
  const contentRef = React.useRef<HTMLDivElement | null>(null);
  const [isAtBottom, setIsAtBottom] = React.useState(true);
  const [unreadBelow, setUnreadBelow] = React.useState(0);
  const [activitySettledBelow, setActivitySettledBelow] = React.useState(false);

  // Mirrors of the two pieces of state the observers need without re-running:
  // a ResizeObserver that re-subscribed on every scroll would rebuild itself
  // once per frame while the reader is dragging.
  const isAtBottomRef = React.useRef(true);
  const previousCountRef = React.useRef(commentCount);
  const previousRootRef = React.useRef(rootId);
  const previousWorkingRef = React.useRef(workingAgents);

  const scrollToBottom = React.useCallback(
    (behavior: ScrollBehavior = "auto") => {
      const element = scrollRef.current;
      if (!element) return;
      element.scrollTo({ top: element.scrollHeight, behavior });
      isAtBottomRef.current = true;
      setIsAtBottom(true);
      setUnreadBelow(0);
      setActivitySettledBelow(false);
    },
    [],
  );

  const handleScroll = React.useCallback(() => {
    const element = scrollRef.current;
    if (!element) return;
    const atBottom = isThreadAtBottom(element);
    isAtBottomRef.current = atBottom;
    setIsAtBottom(atBottom);
    if (atBottom) {
      setUnreadBelow(0);
      setActivitySettledBelow(false);
    }
  }, []);

  /**
   * Open on the newest comment — or, when there is no comment, at the top.
   *
   * "Pinned to the newest comment" has no referent on a thread with none, and
   * scrolling to the floor anyway lands the reader at the *end* of the issue
   * description with its opening lines behind the sticky header. On a long
   * description that means the first thing they see is a mid-sentence
   * fragment, and the label identifying it as the description is one of the
   * things scrolled out of sight.
   *
   * The at-bottom state is still measured rather than assumed, so a short
   * description that does not overflow is at the floor exactly as before and
   * the first arriving comment pins normally; a long one is honestly not at
   * the floor, and a comment arriving while it is being read offers the pill
   * instead of yanking the page.
   */
  const openThread = React.useCallback((commentsPresent: boolean) => {
    const element = scrollRef.current;
    if (!element) return;
    if (commentsPresent) {
      element.scrollTo({ top: element.scrollHeight, behavior: "auto" });
    }
    const atBottom = isThreadAtBottom(element);
    isAtBottomRef.current = atBottom;
    setIsAtBottom(atBottom);
    setUnreadBelow(0);
    setActivitySettledBelow(false);
  }, []);

  // Opening and arrival are one effect rather than two, because switching
  // issues changes the root and the comment count in the same commit: split
  // across two effects, the second one sees a count that jumped from the old
  // thread's to the new thread's and reports the whole of the new issue's
  // history as comments that arrived while the reader was away.
  React.useLayoutEffect(() => {
    const previousCommentCount = previousCountRef.current;
    const isNewRoot = previousRootRef.current !== rootId;
    previousRootRef.current = rootId;
    previousCountRef.current = commentCount;

    if (isNewRoot) {
      openThread(commentCount > 0);
      return;
    }
    if (commentCount === previousCommentCount) return;
    if (isAtBottomRef.current) {
      scrollToBottom("auto");
      return;
    }
    setUnreadBelow((unread) =>
      nextUnreadBelowFold({
        atBottom: false,
        commentCount,
        previousCommentCount,
        unread,
      }),
    );
  }, [commentCount, openThread, rootId, scrollToBottom]);

  // The first commit is not a root change — the refs were initialised from
  // this root — so the opening pin is its own effect. Guarded by a ref rather
  // than an empty dependency list because it needs the comment count, and a
  // count that arrives in a later commit must not re-open the thread under a
  // reader who has since scrolled.
  const openedRef = React.useRef(false);
  React.useLayoutEffect(() => {
    if (openedRef.current) return;
    openedRef.current = true;
    openThread(commentCount > 0);
  }, [commentCount, openThread]);

  // A turn ending while the reader is away. Kept apart from the arrival effect
  // above because it is not a count: an agent can finish without commenting,
  // and folding it into the comment total would both invent a comment to jump
  // to and double-count the ordinary case where the turn ends by posting one.
  const settledRootRef = React.useRef(rootId);
  React.useEffect(() => {
    const previousWorking = previousWorkingRef.current;
    const isNewRoot = settledRootRef.current !== rootId;
    settledRootRef.current = rootId;
    previousWorkingRef.current = workingAgents;
    // Opening a different issue empties the activity subscription, so every
    // agent working on the issue just left would read as a turn that settled
    // on the issue just opened.
    if (isNewRoot) return;
    if (
      !isAtBottomRef.current &&
      hasTurnSettled(previousWorking, workingAgents)
    )
      setActivitySettledBelow(true);
  }, [rootId, workingAgents]);

  // Anything that moves the floor without emitting a scroll event.
  //
  // Two boxes, because the floor is a function of both and they move for
  // different reasons. The content grows — an image resolves, a code block
  // wraps — and the distance to the bottom grows with it. The *container*
  // shrinks — the docked composer expands from its one-line bar, and it is a
  // flex sibling outside this region, so its growth is taken out of the
  // thread's height. Measured: focusing the compact composer takes 28px off
  // `clientHeight` with `scrollTop` and `scrollHeight` unchanged, which is 28px
  // of the newest comment sliding above the floor at the moment the reader is
  // answering it.
  //
  // Observing only the content leaves that second case to whatever else
  // happens to re-pin. Something does today — the composer refocuses its editor
  // after expanding, and the second `focusin` re-runs the focus handler against
  // the new height — but that is another component's internal sequencing, not
  // an invariant this hook can rely on.
  //
  // Re-pinning only while already pinned is what keeps this from yanking a
  // reader who has deliberately scrolled up.
  React.useEffect(() => {
    const content = contentRef.current;
    const scroller = scrollRef.current;
    if (!content || !scroller || typeof ResizeObserver === "undefined") return;
    const observer = new ResizeObserver(() => {
      if (isAtBottomRef.current) scrollToBottom("auto");
    });
    observer.observe(content);
    observer.observe(scroller);
    return () => observer.disconnect();
  }, [scrollToBottom]);

  return {
    activitySettledBelow,
    contentRef,
    handleScroll,
    isAtBottom,
    scrollRef,
    scrollToBottom,
    unreadBelow,
  };
}
