import { FileArchive, FolderOpen, Puzzle } from "lucide-react";
import * as React from "react";
import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";

import { useAppNavigation } from "@/app/navigation/useAppNavigation";
import {
  approvePreparedExtension,
  cancelPreparedExtension,
  type ExtensionGrantSelection,
  type InstalledExtension,
  listInstalledExtensions,
  type PreparedExtension,
  removeExtension,
  setExtensionEnabled,
  updateExtensionGrants,
} from "@/features/extensions/lib/extensionsApi";
import {
  type InstallSource,
  ManifestShapeError,
  prepareFromPickedSource,
} from "@/features/extensions/lib/installFlow";
import { ExtensionCard } from "@/features/extensions/ui/ExtensionCard";
import { ExtensionGrantDialog } from "@/features/extensions/ui/ExtensionGrantDialog";
import {
  AlertDialog,
  AlertDialogAction,
  AlertDialogCancel,
  AlertDialogContent,
  AlertDialogDescription,
  AlertDialogFooter,
  AlertDialogHeader,
  AlertDialogTitle,
} from "@/shared/ui/alert-dialog";
import { Button } from "@/shared/ui/button";
import { Card } from "@/shared/ui/card";
import { Skeleton } from "@/shared/ui/skeleton";

const installedExtensionsQueryKey = ["installed-extensions"] as const;

type InstallFailure = { title: string; issues: string[] };

function toFailure(
  error: unknown,
  title = "Extension operation failed",
): InstallFailure {
  if (error instanceof ManifestShapeError) {
    return {
      title: "This package's extension.json isn't valid",
      issues: error.issues,
    };
  }
  if (typeof error === "string") return { title, issues: [error] };
  if (error instanceof Error) return { title, issues: [error.message] };
  return { title, issues: [String(error)] };
}

function ExtensionsListSkeleton() {
  return (
    <div className="space-y-2">
      {["first", "second"].map((card) => (
        <Card className="p-4" key={card}>
          <div className="space-y-3">
            <Skeleton className="h-5 w-40" />
            <Skeleton className="h-4 w-full max-w-xl" />
          </div>
        </Card>
      ))}
    </div>
  );
}

export function ExtensionsScreen() {
  const queryClient = useQueryClient();
  const { goExtension } = useAppNavigation();
  const [failure, setFailure] = React.useState<InstallFailure | null>(null);
  const [prepared, setPrepared] = React.useState<PreparedExtension | null>(
    null,
  );
  const [reviewing, setReviewing] = React.useState<InstalledExtension | null>(
    null,
  );
  const [removing, setRemoving] = React.useState<InstalledExtension | null>(
    null,
  );
  const [dialogError, setDialogError] = React.useState<string | null>(null);

  const installedQuery = useQuery({
    queryKey: installedExtensionsQueryKey,
    queryFn: listInstalledExtensions,
    staleTime: 30_000,
  });
  const refresh = () =>
    queryClient.invalidateQueries({ queryKey: installedExtensionsQueryKey });

  const prepareMutation = useMutation({
    mutationFn: (source: InstallSource) => prepareFromPickedSource(source),
    onMutate: () => setFailure(null),
    onSuccess: (next) => {
      if (next) {
        setDialogError(null);
        setPrepared(next);
      }
    },
    onError: (error) => setFailure(toFailure(error, "Preparation failed")),
  });

  const approveMutation = useMutation({
    mutationFn: ({
      token,
      selected,
    }: {
      token: string;
      selected: ExtensionGrantSelection;
    }) => approvePreparedExtension(token, selected),
    onMutate: () => setDialogError(null),
    onSuccess: () => {
      setPrepared(null);
      void refresh();
    },
    onError: (error) => setDialogError(toFailure(error).issues.join(" ")),
  });

  const grantsMutation = useMutation({
    mutationFn: ({
      id,
      selected,
    }: {
      id: string;
      selected: ExtensionGrantSelection;
    }) => updateExtensionGrants(id, selected),
    onMutate: () => setDialogError(null),
    onSuccess: () => {
      setReviewing(null);
      void refresh();
    },
    onError: (error) => setDialogError(toFailure(error).issues.join(" ")),
  });

  const toggleMutation = useMutation({
    mutationFn: (extension: InstalledExtension) =>
      setExtensionEnabled(extension.id, !extension.enabled),
    onMutate: () => setFailure(null),
    onSuccess: () => void refresh(),
    onError: (error) => setFailure(toFailure(error)),
  });

  const removeMutation = useMutation({
    mutationFn: (extension: InstalledExtension) =>
      removeExtension(extension.id),
    onMutate: () => setFailure(null),
    onSuccess: (result) => {
      setRemoving(null);
      if (result.recoveryPath) {
        setFailure({
          title: "Extension removed; files require cleanup",
          issues: [`Files remain at ${result.recoveryPath}`],
        });
      }
      void refresh();
    },
    onError: (error) => setFailure(toFailure(error, "Remove failed")),
  });

  const cancelPrepared = () => {
    const token = prepared?.token;
    setPrepared(null);
    setDialogError(null);
    if (token) {
      void cancelPreparedExtension(token).catch((error) =>
        setFailure(toFailure(error, "Could not cancel prepared package")),
      );
    }
  };

  const busy =
    prepareMutation.isPending ||
    approveMutation.isPending ||
    grantsMutation.isPending ||
    toggleMutation.isPending ||
    removeMutation.isPending;
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
          <div>
            <h2 className="text-lg font-semibold">Extensions</h2>
            <p className="text-sm text-muted-foreground">
              Local packages only. New and replaced packages install disabled.
            </p>
          </div>
          <div className="flex shrink-0 gap-2">
            <Button
              data-testid="install-extension-from-folder"
              disabled={busy}
              onClick={() => prepareMutation.mutate("directory")}
              size="sm"
              variant="outline"
            >
              <FolderOpen className="mr-1 h-4 w-4" /> Install from folder
            </Button>
            <Button
              data-testid="install-extension-from-zip"
              disabled={busy}
              onClick={() => prepareMutation.mutate("zip")}
              size="sm"
            >
              <FileArchive className="mr-1 h-4 w-4" /> Install from zip
            </Button>
          </div>
        </div>

        {failure ? (
          <Card
            className="mb-4 border-destructive/50 p-4"
            data-testid="extension-install-error"
          >
            <p className="text-sm font-medium text-destructive">
              {failure.title}
            </p>
            <ul className="mt-1 space-y-1">
              {failure.issues.map((issue) => (
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
          <div className="flex flex-1 flex-col items-center justify-center gap-2">
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
              Choose a local folder or ZIP, review its exact prepared digest and
              requested access, then install it disabled.
            </p>
          </div>
        ) : (
          <div className="space-y-2">
            {installed.map((extension) => (
              <ExtensionCard
                extension={extension}
                key={extension.id}
                onOpen={(id) => void goExtension(id)}
                onRemove={setRemoving}
                onReview={(item) => {
                  setDialogError(null);
                  setReviewing(item);
                }}
                onToggle={(item) => toggleMutation.mutate(item)}
                pending={busy}
              />
            ))}
          </div>
        )}
      </div>

      <ExtensionGrantDialog
        digest={prepared?.digest ?? ""}
        error={dialogError}
        manifest={prepared?.manifest ?? null}
        mode="install"
        onCancel={cancelPrepared}
        onConfirm={(selected) => {
          if (prepared)
            approveMutation.mutate({ token: prepared.token, selected });
        }}
        open={prepared !== null}
        pending={approveMutation.isPending}
      />
      <ExtensionGrantDialog
        digest={reviewing?.digest ?? ""}
        error={dialogError}
        initial={reviewing?.granted}
        manifest={reviewing ?? null}
        mode="review"
        onCancel={() => {
          setReviewing(null);
          setDialogError(null);
        }}
        onConfirm={(selected) => {
          if (reviewing) grantsMutation.mutate({ id: reviewing.id, selected });
        }}
        open={reviewing !== null}
        pending={grantsMutation.isPending}
      />

      <AlertDialog
        onOpenChange={(open) => !open && setRemoving(null)}
        open={removing !== null}
      >
        <AlertDialogContent>
          <AlertDialogHeader>
            <AlertDialogTitle>Remove extension?</AlertDialogTitle>
            <AlertDialogDescription>
              Remove {removing?.name ?? "this extension"}, disable every live
              generation, and delete its grants and enable state for every
              identity.
            </AlertDialogDescription>
          </AlertDialogHeader>
          <AlertDialogFooter>
            <AlertDialogCancel asChild>
              <Button type="button" variant="outline">
                Cancel
              </Button>
            </AlertDialogCancel>
            <AlertDialogAction asChild>
              <Button
                data-testid="confirm-remove-extension"
                onClick={() => removing && removeMutation.mutate(removing)}
                type="button"
                variant="destructive"
              >
                Remove
              </Button>
            </AlertDialogAction>
          </AlertDialogFooter>
        </AlertDialogContent>
      </AlertDialog>
    </div>
  );
}
