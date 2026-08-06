import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";

import {
  cloneProjectRepository,
  getProjectRepoSyncStatus,
  pullProjectLocalRepository,
  pushProjectLocalRepository,
} from "@/shared/api/projectGit";
import type { Project, ProjectPullRequest } from "@/features/projects/hooks";
import { projectCheckoutCloneUrl } from "./lib/pullRequestRepoContext";
import { publishProjectPullRequestUpdate } from "./pullRequestMutations";

/**
 * Every hook here is checkout-scoped: it operates on the local clone named by
 * `project.dtag`, so without a selected project there is genuinely nothing to
 * act on and "No project selected." is the truth. A missing *clone URL* is a
 * different failure entirely — the project is right there, its announcement
 * (or the relay-origin cache behind it) just never yielded a URL. Reporting
 * that as "No project selected." sent people hunting for the wrong problem, so
 * the two conditions are now split.
 */
const NO_CLONE_URL_ERROR =
  "This project has no clone URL. Add a clone URL to the repository announcement, or reconnect to the relay that hosts it.";

/** Local-vs-remote git sync status for a project checkout (ahead/behind
 * counts, push/pull availability). Polls gently — each check runs a
 * `git fetch` — and refetches on focus to catch the common "committed in
 * a terminal, switched back to the app" flow. */
export function useProjectRepoSyncStatusQuery(
  project: Project | null | undefined,
  reposDir?: string | null,
  branchName?: string | null,
  baseBranch?: string | null,
) {
  const selectedBranch = branchName ?? project?.defaultBranch ?? null;
  const selectedBaseBranch = baseBranch ?? project?.defaultBranch ?? null;

  return useQuery({
    enabled: Boolean(project?.cloneUrls[0]),
    queryKey: [
      "project",
      project?.id ?? "none",
      "repo-sync-status",
      reposDir ?? "default",
      selectedBranch ?? "default",
      selectedBaseBranch ?? "default",
    ],
    queryFn: () => {
      if (!project) throw new Error("No project selected.");
      const cloneUrl = project.cloneUrls[0];
      if (!cloneUrl) throw new Error(NO_CLONE_URL_ERROR);
      return getProjectRepoSyncStatus({
        reposDir,
        projectDtag: project.dtag,
        cloneUrl,
        branchName: selectedBranch,
        baseBranch: selectedBaseBranch,
      });
    },
    staleTime: 10_000,
    refetchInterval: 60_000,
    refetchOnWindowFocus: true,
    retry: 1,
  });
}

/** Pushes local commits to the project remote. */
export function usePushProjectLocalRepositoryMutation(
  project: Project | null | undefined,
  reposDir?: string | null,
  branchName?: string | null,
  pullRequest?: ProjectPullRequest | null,
) {
  const queryClient = useQueryClient();
  const selectedBranch = branchName ?? project?.defaultBranch ?? null;

  return useMutation({
    mutationFn: async () => {
      if (!project) throw new Error("No project selected.");
      // An open PR on this branch carries the same repo's `clone` tags, so it
      // can supply the URL when the announcement's own list came up empty.
      const cloneUrl = projectCheckoutCloneUrl(project, pullRequest);
      if (!cloneUrl) throw new Error(NO_CLONE_URL_ERROR);
      const result = await pushProjectLocalRepository({
        reposDir,
        projectDtag: project.dtag,
        cloneUrl,
        branchName: selectedBranch,
        baseBranch: project.defaultBranch,
      });
      let pullRequestUpdate:
        | { status: "skipped" | "unchanged" | "updated" }
        | { status: "failed"; error: string } = { status: "skipped" };
      if (
        pullRequest &&
        (pullRequest.status === "Open" || pullRequest.status === "Draft")
      ) {
        try {
          const updated = await publishProjectPullRequestUpdate({
            commit: result.commit,
            mergeBase: result.mergeBase,
            project,
            pullRequest,
          });
          pullRequestUpdate = {
            status: updated ? "updated" : "unchanged",
          };
        } catch (error) {
          pullRequestUpdate = {
            status: "failed",
            error:
              error instanceof Error
                ? error.message
                : "The pull request update could not be published.",
          };
        }
      }
      return { ...result, pullRequestUpdate };
    },
    onSuccess: () => {
      void queryClient.invalidateQueries({
        queryKey: ["project", project?.id ?? "none"],
      });
      void queryClient.invalidateQueries({ queryKey: ["projects"] });
    },
  });
}

/** Clones a project into the workspace repositories directory. */
export function useCloneProjectRepositoryMutation(
  project: Project | null | undefined,
  reposDir?: string | null,
) {
  const queryClient = useQueryClient();
  return useMutation({
    mutationFn: () => {
      if (!project) throw new Error("No project selected.");
      const cloneUrl = project.cloneUrls[0];
      if (!cloneUrl) throw new Error(NO_CLONE_URL_ERROR);
      return cloneProjectRepository({
        reposDir,
        projectDtag: project.dtag,
        cloneUrl,
        defaultBranch: project.defaultBranch,
      });
    },
    onSuccess: async () => {
      await Promise.all([
        queryClient.invalidateQueries({
          queryKey: ["project", project?.id ?? "none", "local-repo-snapshot"],
        }),
        queryClient.invalidateQueries({
          queryKey: ["project", project?.id ?? "none", "repo-sync-status"],
        }),
        queryClient.invalidateQueries({
          queryKey: ["projects", "local-repositories"],
        }),
      ]);
    },
  });
}

/** Fast-forwards the local checkout to the remote branch head. */
export function usePullProjectLocalRepositoryMutation(
  project: Project | null | undefined,
  reposDir?: string | null,
  branchName?: string | null,
) {
  const queryClient = useQueryClient();
  const selectedBranch = branchName ?? project?.defaultBranch ?? null;

  return useMutation({
    mutationFn: () => {
      if (!project) throw new Error("No project selected.");
      const cloneUrl = project.cloneUrls[0];
      if (!cloneUrl) throw new Error(NO_CLONE_URL_ERROR);
      return pullProjectLocalRepository({
        reposDir,
        projectDtag: project.dtag,
        cloneUrl,
        branchName: selectedBranch,
      });
    },
    onSuccess: () => {
      void queryClient.invalidateQueries({
        queryKey: ["project", project?.id ?? "none"],
      });
      void queryClient.invalidateQueries({ queryKey: ["projects"] });
    },
  });
}
