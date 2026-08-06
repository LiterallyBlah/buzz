import type * as React from "react";

import { cn } from "@/shared/lib/cn";

/**
 * The opening text of an issue or pull request, dressed as the document it is.
 *
 * It used to be bare rich content in the same gutter the comments use, with no
 * container, no tint and no boundary — so on a surface where every other block
 * of prose is a message, it read as an unattributed first comment whose byline
 * had gone missing.
 *
 * Two signals do the work, and they are deliberately the inverse of a comment's:
 *
 *  - **Contained and tinted.** A comment is loose in the gutter; this sits in an
 *    inset panel. The panel's own edges are what end the description, which is
 *    the boundary the reader was missing between it and the first reply.
 *  - **Labelled, not attributed.** "Description" says what the block is. It
 *    pointedly does not get the avatar-and-name row a comment opens with —
 *    attribution is what makes something look like a message, and the thread
 *    header already carries "Issue from {author}". Adding a byline here would
 *    argue for exactly the reading this component exists to prevent.
 *
 * Takes children rather than content, so the issue panel can clamp its body and
 * hang "Show more" inside the same panel while the pull request panel passes
 * its rich content straight through.
 */
export function ProjectWorkItemDescription({
  children,
  className,
}: {
  children: React.ReactNode;
  /** Outer spacing, which differs between a scroll region and a padded section. */
  className?: string;
}) {
  return (
    <section
      className={cn("space-y-1.5 px-4 pb-4 pt-3", className)}
      data-testid="project-work-item-description"
    >
      <p className="text-2xs font-semibold uppercase tracking-[0.18em] text-muted-foreground">
        Description
      </p>
      <div className="space-y-1.5 rounded-lg border border-border/60 bg-muted/20 px-3 py-2.5">
        {children}
      </div>
    </section>
  );
}
