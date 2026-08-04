import { Clock, Loader2 } from "lucide-react";

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
 *
 * `queued` is rendered as its own phrase rather than folded into "working".
 * The whole reason the state exists is that a person cannot tell "nobody was
 * addressed" from "somebody was addressed and is waiting", and calling the
 * second one "working" would replace that ambiguity with a claim that is
 * simply false for as long as the pool stays busy.
 */
export function ProjectActivityIndicator({
  profiles,
  rootEventId,
}: {
  profiles?: UserProfileLookup;
  rootEventId: string;
}) {
  const live = useProjectAgentActivity(rootEventId);
  if (live.length === 0) return null;

  const label = (pubkey: string) => resolveUserLabel({ profiles, pubkey });
  const working = live.filter((entry) => entry.state === "working");
  const queued = live.filter((entry) => entry.state === "queued");

  // The stage is only shown for a lone announcement. With two agents it would
  // be one of their captions attached to both names, which reads as a statement
  // about both and is wrong about one — and a caption beside a queued name is
  // wrong about the only thing queued means, which is that nothing has started.
  const stage =
    live.length === 1 && working.length === 1 ? working[0].stage : null;

  // One phrase per state, so a root where one agent is working and another is
  // waiting says exactly that instead of choosing a verb for both.
  const phrases: string[] = [];
  if (working.length > 0) {
    phrases.push(
      `${working.map((entry) => label(entry.agent)).join(", ")} ${
        working.length === 1 ? "is" : "are"
      } working${stage ? ` — ${stage}` : ""}`,
    );
  }
  if (queued.length > 0) {
    phrases.push(
      `${queued.map((entry) => label(entry.agent)).join(", ")} ${
        queued.length === 1 ? "is" : "are"
      } queued`,
    );
  }

  return (
    <div
      className="flex items-center gap-2 px-4 py-2 text-xs text-muted-foreground"
      data-testid="project-activity-indicator"
    >
      {working.length > 0 ? (
        <Loader2 className="h-3.5 w-3.5 animate-spin" aria-hidden />
      ) : (
        // Waiting, not running: a spinner beside "queued" would animate work
        // that has not begun.
        <Clock className="h-3.5 w-3.5" aria-hidden />
      )}
      <span>{phrases.join(" · ")}</span>
    </div>
  );
}
