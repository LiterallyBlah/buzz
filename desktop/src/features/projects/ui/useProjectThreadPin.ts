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
 * cost a scroll to get there or a decision to stay. So the thread opens
 * pinned, stays pinned while the reader is at the floor, and — the part that
 * makes leaving the floor safe — counts what arrived while they were away
 * instead of silently growing underneath them.
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
      scrollToBottom("auto");
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
  }, [commentCount, rootId, scrollToBottom]);

  // The first commit is not a root change — the refs were initialised from
  // this root — so the opening pin is its own effect.
  React.useLayoutEffect(() => {
    scrollToBottom("auto");
  }, [scrollToBottom]);

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

  // Content that grows after layout — an image resolving, a code block
  // wrapping, the composer expanding under a click — moves the floor without
  // emitting a scroll event. Re-pinning only while already pinned is what
  // keeps this from yanking a reader who has deliberately scrolled up.
  React.useEffect(() => {
    const content = contentRef.current;
    if (!content || typeof ResizeObserver === "undefined") return;
    const observer = new ResizeObserver(() => {
      if (isAtBottomRef.current) scrollToBottom("auto");
    });
    observer.observe(content);
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
