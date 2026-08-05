import { Clock, Loader2 } from "lucide-react";

import { useOpenAgentActivity } from "@/features/agents/useOpenAgentActivity";
import {
  resolveUserLabel,
  type UserProfileLookup,
} from "@/features/profile/lib/identity";
import type { ProjectAgentActivity } from "@/features/projects/projectAgentActivity";
import { useProjectAgentActivity } from "@/features/projects/useProjectAgentActivity";

/**
 * One run of the announcement sentence.
 *
 * The sentence is built as data rather than as a string because half of it is
 * a handle and half of it is prose: a name identifies an agent whose activity
 * can be opened, the words around it describe what that agent is doing. Making
 * the split explicit means the rule for which characters are clickable is
 * testable without rendering, and a two-agent announcement cannot hand one
 * agent's name the other agent's pubkey — the mistake that a string built with
 * `join(", ")` and then re-split for hit-testing invites.
 */
export type ProjectActivitySegment =
  | { kind: "agent"; agent: string; label: string }
  | { kind: "text"; text: string };

/**
 * Append prose, merging into the preceding run.
 *
 * Without the merge the same sentence would come out as one text segment or as
 * three depending only on which phrases happened to be present, so every test
 * of the segment shape would really be a test of the builder's call order.
 */
function appendText(segments: ProjectActivitySegment[], text: string) {
  const last = segments.at(-1);
  if (last?.kind === "text") {
    segments[segments.length - 1] = { kind: "text", text: last.text + text };
    return;
  }
  segments.push({ kind: "text", text });
}

/**
 * The announcement for one root, as an ordered list of runs.
 *
 * Empty when nothing is happening, which is the same thing the component
 * treats as "render nothing at all" — see below for why that is the honest
 * rendering rather than an idle row.
 */
export function buildProjectActivitySegments({
  entries,
  label,
}: {
  entries: readonly ProjectAgentActivity[];
  label: (agentPubkey: string) => string;
}): ProjectActivitySegment[] {
  const working = entries.filter((entry) => entry.state === "working");
  const queued = entries.filter((entry) => entry.state === "queued");

  // The stage is only shown for a lone announcement. With two agents it would
  // be one of their captions attached to both names, which reads as a statement
  // about both and is wrong about one — and a caption beside a queued name is
  // wrong about the only thing queued means, which is that nothing has started.
  const stage =
    entries.length === 1 && working.length === 1 ? working[0].stage : null;

  const segments: ProjectActivitySegment[] = [];

  const appendPhrase = (
    group: readonly ProjectAgentActivity[],
    predicate: string,
  ) => {
    if (group.length === 0) return;
    if (segments.length > 0) appendText(segments, " · ");
    group.forEach((entry, index) => {
      if (index > 0) appendText(segments, ", ");
      segments.push({
        kind: "agent",
        agent: entry.agent,
        label: label(entry.agent),
      });
    });
    appendText(segments, ` ${group.length === 1 ? "is" : "are"} ${predicate}`);
  };

  // One phrase per state, so a root where one agent is working and another is
  // waiting says exactly that instead of choosing a verb for both.
  appendPhrase(working, `working${stage ? ` — ${stage}` : ""}`);
  appendPhrase(queued, "queued");

  return segments;
}

/**
 * The sentence itself.
 *
 * Hook-free on purpose: what is worth pinning is that each name carries its own
 * agent's pubkey into the open handler, and that is a property of this
 * function's output rather than of a rendered tree with a relay behind it.
 */
export function ProjectActivitySegments({
  onOpenAgent,
  segments,
}: {
  onOpenAgent: (agentPubkey: string) => void;
  segments: readonly ProjectActivitySegment[];
}) {
  return (
    <>
      {segments.map((segment) =>
        segment.kind === "agent" ? (
          <button
            className="rounded-sm font-medium text-foreground underline-offset-2 hover:underline focus-visible:outline-hidden focus-visible:ring-1 focus-visible:ring-ring"
            data-testid={`project-activity-agent-${segment.agent}`}
            key={`agent-${segment.agent}`}
            onClick={() => onOpenAgent(segment.agent)}
            title={`View ${segment.label}'s activity`}
            type="button"
          >
            {segment.label}
          </button>
        ) : (
          // Prose goes out as bare text, not a wrapped element. "is working —
          // reading files" describes the turn; it is not a second destination,
          // and wrapping the whole phrase in one button would leave a reader
          // looking at two working agents unable to say which one they were
          // about to open. Text nodes also need no key, which is the honest
          // answer here: a run of prose has no identity of its own, so any key
          // for it would be its position dressed up as one.
          segment.text
        ),
      )}
    </>
  );
}

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
 *
 * Each name opens that agent's ACP activity through `useOpenAgentActivity` —
 * the same ingress the channel-side working indicator uses (the working rows in
 * `ChannelActivityPopover`), so an agent reached from an issue lands on the
 * surface an agent reached from the sidebar lands on. No channel id is passed,
 * and that is a decision rather than an omission: a project root is not a
 * channel, its route key names an issue or a pull request, so any channel id
 * supplied from here would be invented. Without one the ingress opens the
 * agent's own scope — the channel it is currently working in, else one the
 * viewer shares with it — and says so plainly when there is nowhere reachable.
 */
export function ProjectActivityIndicator({
  profiles,
  rootEventId,
}: {
  profiles?: UserProfileLookup;
  rootEventId: string;
}) {
  const live = useProjectAgentActivity(rootEventId);
  // Resolved above the early return: a root with nothing to announce still has
  // to run the same hooks as one that does.
  const { openAgentActivity } = useOpenAgentActivity();

  const segments = buildProjectActivitySegments({
    entries: live,
    label: (pubkey) => resolveUserLabel({ profiles, pubkey }),
  });
  if (segments.length === 0) return null;

  return (
    <div
      className="flex items-center gap-2 px-4 py-2 text-xs text-muted-foreground"
      data-testid="project-activity-indicator"
    >
      {live.some((entry) => entry.state === "working") ? (
        <Loader2 className="h-3.5 w-3.5 animate-spin" aria-hidden />
      ) : (
        // Waiting, not running: a spinner beside "queued" would animate work
        // that has not begun.
        <Clock className="h-3.5 w-3.5" aria-hidden />
      )}
      <span>
        <ProjectActivitySegments
          onOpenAgent={openAgentActivity}
          segments={segments}
        />
      </span>
    </div>
  );
}
