import { Trash2 } from "lucide-react";
import * as React from "react";
import { toast } from "sonner";

import {
  canDeleteProjectEvent,
  type ProjectDeletionSubject,
  useDeleteProjectEventMutation,
} from "@/features/projects/deletionMutations";
import type { Project } from "@/features/projects/hooks";
import { useIdentityQuery } from "@/shared/api/hooks";
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
import { DropdownMenuItem } from "@/shared/ui/dropdown-menu";
import { ProjectListRowMenu } from "./ProjectListRowMenu";

/**
 * The project row menu, with an author-gated "Delete …" entry appended.
 *
 * One component for every deletable project item — issue rows, pull-request
 * rows, and each comment surface — because the thing that must not drift is the
 * gate: the entry appears only when the signed-in pubkey *is* the author of the
 * event being deleted. A second implementation of that check is how a panel
 * ends up offering a button whose event the relay refuses.
 *
 * The relay stays the authority. This is a UX gate: it decides what is worth
 * offering, never what is permitted, and a rejection surfaces as the toast the
 * other project writes use while the caches keep what they had.
 *
 * `children` are menu entries the surface already had (for example "View
 * issue"); when there are none and the viewer is not the author there is
 * nothing to open, so nothing is rendered.
 */
export function ProjectItemDeleteMenu({
  author,
  children,
  label,
  project,
  rootId,
  subject,
  targetId,
  testId,
  title,
}: {
  author: string;
  children?: React.ReactNode;
  label: string;
  project: Project | null | undefined;
  rootId: string;
  subject: ProjectDeletionSubject;
  targetId: string;
  testId: string;
  /** Item name shown in the confirmation, when the item has one. */
  title?: string;
}) {
  const [confirmOpen, setConfirmOpen] = React.useState(false);
  const identityQuery = useIdentityQuery();
  const { isPending, mutateAsync } = useDeleteProjectEventMutation(project);
  const canDelete = canDeleteProjectEvent(author, identityQuery.data?.pubkey);

  const handleDelete = React.useCallback(async () => {
    try {
      await mutateAsync({ author, rootId, subject, targetId });
      toast.success(`Deleted this ${subject}.`);
      setConfirmOpen(false);
    } catch (error) {
      toast.error(
        error instanceof Error
          ? error.message
          : `Failed to delete this ${subject}.`,
      );
    }
  }, [author, mutateAsync, rootId, subject, targetId]);

  if (!canDelete && !children) return null;

  return (
    <AlertDialog onOpenChange={setConfirmOpen} open={confirmOpen}>
      <ProjectListRowMenu label={label}>
        {children}
        {canDelete ? (
          <DropdownMenuItem
            className="text-destructive focus:text-destructive"
            data-testid={`project-delete-${testId}`}
            disabled={isPending}
            onSelect={(event) => {
              event.preventDefault();
              event.stopPropagation();
              if (!isPending) setConfirmOpen(true);
            }}
          >
            <Trash2 className="h-4 w-4" />
            Delete {subject}
          </DropdownMenuItem>
        ) : null}
      </ProjectListRowMenu>
      <AlertDialogContent data-testid={`project-delete-confirm-${testId}`}>
        <AlertDialogHeader>
          <AlertDialogTitle>Delete {subject}?</AlertDialogTitle>
          <AlertDialogDescription>
            {title
              ? `Delete “${title}” from Buzz for everyone in this community. Copies saved elsewhere may remain. This can only be done for ${subject}s you wrote and cannot be undone here.`
              : `Delete this ${subject} from Buzz for everyone in this community. Copies saved elsewhere may remain. This can only be done for ${subject}s you wrote and cannot be undone here.`}
          </AlertDialogDescription>
        </AlertDialogHeader>
        <AlertDialogFooter>
          <AlertDialogCancel asChild>
            <Button disabled={isPending} type="button" variant="outline">
              Cancel
            </Button>
          </AlertDialogCancel>
          <AlertDialogAction asChild>
            <Button
              data-testid={`project-delete-confirm-button-${testId}`}
              disabled={isPending}
              onClick={(event) => {
                event.preventDefault();
                void handleDelete();
              }}
              type="button"
              variant="destructive"
            >
              {isPending ? "Deleting…" : `Delete ${subject}`}
            </Button>
          </AlertDialogAction>
        </AlertDialogFooter>
      </AlertDialogContent>
    </AlertDialog>
  );
}
