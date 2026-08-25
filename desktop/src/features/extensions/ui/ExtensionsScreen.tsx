import { FileArchive, FolderOpen, Puzzle } from "lucide-react";
import * as React from "react";
import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";

import {
  type InstalledExtension,
  listInstalledExtensions,
} from "@/features/extensions/lib/extensionsApi";
import {
  type InstallSource,
  ManifestShapeError,
  installFromPickedSource,
} from "@/features/extensions/lib/installFlow";
import { ExtensionCard } from "@/features/extensions/ui/ExtensionCard";
import { Button } from "@/shared/ui/button";
import { Card } from "@/shared/ui/card";
import { Skeleton } from "@/shared/ui/skeleton";

const installedExtensionsQueryKey = ["installed-extensions"] as const;

type InstallFailure = {
  /** Headline: which half of validation refused the package. */
  title: string;
  /** One line per problem. */
  issues: string[];
};

/**
 * Rejections arrive from two places and both are already written for the user:
 * zod's shape complaints from the frontend preview, and the Rust loader's
 * string ("extension id \"../evil\" is not valid"). Show either verbatim rather
 * than replacing it with a generic failure — the specific reason is the whole
 * point of validating.
 */
function toInstallFailure(error: unknown): InstallFailure {
  if (error instanceof ManifestShapeError) {
    return {
      title: "This package's extension.json isn't valid",
      issues: error.issues,
    };
  }
  if (typeof error === "string") {
    return { title: "Install failed", issues: [error] };
  }
  if (error instanceof Error) {
    return { title: "Install failed", issues: [error.message] };
  }
  return { title: "Install failed", issues: [String(error)] };
}

function ExtensionsListSkeleton() {
  return (
    <div className="space-y-2">
      {["first", "second"].map((card) => (
        <Card className="p-4" key={card}>
          <div className="space-y-3">
            <div className="flex items-center gap-2">
              <Skeleton className="h-5 w-40" />
              <Skeleton className="h-5 w-16 rounded-full" />
            </div>
            <Skeleton className="h-4 w-full max-w-xl" />
          </div>
        </Card>
      ))}
    </div>
  );
}

export function ExtensionsScreen() {
  const queryClient = useQueryClient();
  const [installError, setInstallError] = React.useState<InstallFailure | null>(
    null,
  );

  const installedQuery = useQuery({
    queryKey: installedExtensionsQueryKey,
    queryFn: listInstalledExtensions,
    staleTime: 30_000,
  });

  const installMutation = useMutation({
    // pick → preview → zod shape check → install. The preview step is what
    // makes decision 006's frontend validation real rather than ornamental;
    // Rust validates again and remains authoritative.
    mutationFn: (source: InstallSource) => installFromPickedSource(source),
    onMutate: () => setInstallError(null),
    onSuccess: (installed: InstalledExtension | null) => {
      if (installed === null) {
        return;
      }
      void queryClient.invalidateQueries({
        queryKey: installedExtensionsQueryKey,
      });
    },
    onError: (error: unknown) => setInstallError(toInstallFailure(error)),
  });

  const isInstalling = installMutation.isPending;
  const installed = installedQuery.data ?? [];

  return (
    <div
      className="relative flex min-h-0 flex-1 overflow-hidden"
      data-testid="extensions-view"
    >
      <div
        className="flex min-h-0 flex-1 flex-col overflow-y-auto px-4 pb-4 pt-4"
        data-scroll-restoration-id="extensions-list"
      >
        <div className="mb-4 flex items-center justify-between gap-4">
          <h2 className="text-lg font-semibold">Extensions</h2>
          <div className="flex shrink-0 gap-2">
            <Button
              data-testid="install-extension-from-folder"
              disabled={isInstalling}
              onClick={() => installMutation.mutate("directory")}
              size="sm"
              variant="outline"
            >
              <FolderOpen className="mr-1 h-4 w-4" />
              Install from folder
            </Button>
            <Button
              data-testid="install-extension-from-zip"
              disabled={isInstalling}
              onClick={() => installMutation.mutate("zip")}
              size="sm"
            >
              <FileArchive className="mr-1 h-4 w-4" />
              Install from zip
            </Button>
          </div>
        </div>

        {installError ? (
          <Card
            className="mb-4 border-destructive/50 p-4"
            data-testid="extension-install-error"
          >
            <p className="text-sm font-medium text-destructive">
              {installError.title}
            </p>
            <ul className="mt-1 space-y-1">
              {installError.issues.map((issue) => (
                <li
                  className="break-words text-sm text-muted-foreground"
                  key={issue}
                >
                  {issue}
                </li>
              ))}
            </ul>
          </Card>
        ) : null}

        {installedQuery.isLoading ? (
          <ExtensionsListSkeleton />
        ) : installedQuery.isError ? (
          <div className="flex flex-1 flex-col items-center justify-center gap-2 text-muted-foreground">
            <p className="text-sm text-destructive">
              Failed to load installed extensions
            </p>
            <Button
              onClick={() => void installedQuery.refetch()}
              size="sm"
              variant="outline"
            >
              Retry
            </Button>
          </div>
        ) : installed.length === 0 ? (
          <div className="flex flex-1 flex-col items-center justify-center gap-3 text-muted-foreground">
            <Puzzle className="h-10 w-10 opacity-30" />
            <p className="text-sm">No extensions installed</p>
            <p className="max-w-md text-center text-sm">
              Install a package from a local folder or zip. Installed extensions
              are listed here; running them arrives in a later preview.
            </p>
          </div>
        ) : (
          <div className="space-y-2">
            {installed.map((extension) => (
              <ExtensionCard extension={extension} key={extension.id} />
            ))}
          </div>
        )}
      </div>
    </div>
  );
}
