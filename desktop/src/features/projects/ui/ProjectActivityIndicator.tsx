import { Loader2 } from "lucide-react";

import {
  resolveUserLabel,
  type UserProfileLookup,
} from "@/features/profile/lib/identity";
import { useProjectAgentActivity } from "@/features/projects/useProjectAgentActivity";

/**
 * "An agent is working on this issue", live (NIP-PA).
 *
 * Renders nothing when nothing is happening. An always-present row saying
 * "idle" would be a permanent piece of furniture reporting the absence of an
 * event, and the absence is the normal case.
 */
export function ProjectActivityIndicator({
  profiles,
  rootEventId,
}: {
  profiles?: UserProfileLookup;
  rootEventId: string;
}) {
  const working = useProjectAgentActivity(rootEventId);
  if (working.length === 0) return null;

  const names = working.map((entry) =>
    resolveUserLabel({ profiles, pubkey: entry.agent }),
  );
  // The stage is only shown for a single worker. With two agents it would be
  // one of their captions attached to both names, which reads as a statement
  // about both and is wrong about one.
  const stage = working.length === 1 ? working[0].stage : null;

  return (
    <div
      className="flex items-center gap-2 px-4 py-2 text-xs text-muted-foreground"
      data-testid="project-activity-indicator"
    >
      <Loader2 className="h-3.5 w-3.5 animate-spin" aria-hidden />
      <span>
        {names.join(", ")} {working.length === 1 ? "is" : "are"} working
        {stage ? ` — ${stage}` : ""}
      </span>
    </div>
  );
}
