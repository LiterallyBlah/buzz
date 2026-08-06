import { ExternalLink } from "lucide-react";

import { Button } from "@/shared/ui/button";

/**
 * The project name and its optional web link, above the workspace tabs.
 *
 * Rendered only when no work item is open. Inside an issue thread the
 * breadcrumb already names both the project and the item, so the hero is a
 * repeat of information the reader has — and a screen of vertical space
 * between them and the conversation they came for.
 *
 * Lives in its own file because `ProjectDetailScreen` is over the repo's
 * file-size limit; the rule there is that an over-limit file may not grow, and
 * a self-contained block of presentation is the cheapest thing to lift out.
 */
export function ProjectDetailHero({
  name,
  webUrl,
}: {
  name: string;
  /** Already validated by `isSafeUrl`; null when absent or rejected. */
  webUrl: string | null;
}) {
  return (
    <section className="space-y-3">
      <div className="flex min-w-0 items-start justify-between gap-3">
        <div className="min-w-0 flex-1 space-y-0.5">
          <div className="flex min-w-0 items-center gap-1.5">
            <h2 className="truncate text-xl font-semibold tracking-tight">
              {name}
            </h2>
            {webUrl ? (
              <Button
                asChild
                aria-label="Open project web page"
                className="h-6 w-6 shrink-0 text-muted-foreground hover:text-foreground"
                size="icon-xs"
                variant="ghost"
              >
                <a href={webUrl} rel="noopener noreferrer" target="_blank">
                  <ExternalLink className="h-3.5 w-3.5" />
                </a>
              </Button>
            ) : null}
          </div>
        </div>
      </div>
    </section>
  );
}
