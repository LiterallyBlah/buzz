import {
  useProjectsQuery,
  useProjectsWorkItemsQuery,
} from "@/features/projects/hooks";

/**
 * Keeps the work-items query cache populated app-wide.
 *
 * The watch set that drives project notifications ("roots I authored or
 * commented on") is derived from this query's cache. Without a mount outside
 * the Projects screen that cache is empty until the user visits Projects,
 * which would make the badge useless exactly when it matters — you would find
 * out an agent replied only by going to look, which is the problem the feature
 * exists to solve.
 *
 * The cost is one `fetchProjectsWorkItems` fan-out (four relay queries) per
 * session, deduped with the Projects screen's own mount whenever the query
 * keys coincide. A cheaper future shape is a pair of author-scoped relay
 * queries ("roots I signed", "comments I signed"), which would drop the
 * dependency on this cache entirely.
 *
 * Lives in its own module so the bridge can lazy-import it: a static import
 * would pull `features/projects/hooks` and its transitive relay/git graph into
 * the startup chunk for every user, including those who never enable the
 * Projects preview feature.
 */
export function ProjectWorkItemsSeed() {
  const projectsQuery = useProjectsQuery();
  useProjectsWorkItemsQuery(projectsQuery.data ?? []);
  return null;
}
