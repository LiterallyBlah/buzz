import { Search, X } from "lucide-react";
import * as React from "react";

import { useIsArchivedPredicate } from "@/features/identity-archive/hooks";
import { useUserSearchQuery } from "@/features/profile/hooks";
import type { UserProfileLookup } from "@/features/profile/lib/identity";
import type { UserSearchResult } from "@/shared/api/types";
import { normalizePubkey, truncatePubkey } from "@/shared/lib/pubkey";
import { Input } from "@/shared/ui/input";
import { UserAvatar } from "@/shared/ui/UserAvatar";
import {
  CreateProjectWorkItemDialog,
  type CreateProjectWorkItemDialogInput,
} from "./CreateProjectWorkItemDialog";

export type CreateIssueDialogInput = CreateProjectWorkItemDialogInput;

function searchLabel(user: UserSearchResult) {
  return (
    user.displayName?.trim() ||
    user.nip05Handle?.trim() ||
    truncatePubkey(user.pubkey)
  );
}

function profileForPubkey(pubkey: string, profiles?: UserProfileLookup) {
  return profiles?.[normalizePubkey(pubkey)] ?? null;
}

function selectionLabel(pubkey: string, profiles?: UserProfileLookup) {
  const profile = profileForPubkey(pubkey, profiles);
  return (
    profile?.displayName?.trim() ||
    profile?.nip05Handle?.trim() ||
    truncatePubkey(pubkey)
  );
}

/**
 * Mention picker for a new issue.
 *
 * Built from the pieces the reviewer picker already uses —
 * `useUserSearchQuery`, `UserAvatar`, the archived-identity filter — rather
 * than a second search surface with its own idea of who exists. Inline rather
 * than in the reviewer row's nested dialog because these people are chosen
 * *before* the issue exists, so the choice belongs in the form that creates it.
 */
function IssueMentionPicker({
  disabled,
  onChange,
  profiles,
  selected,
}: {
  disabled: boolean;
  onChange: (next: string[]) => void;
  profiles?: UserProfileLookup;
  selected: string[];
}) {
  const [query, setQuery] = React.useState("");
  const userSearchQuery = useUserSearchQuery(query);
  const isArchived = useIsArchivedPredicate();
  const chosen = React.useMemo(
    () => new Set(selected.map(normalizePubkey)),
    [selected],
  );

  const candidates = React.useMemo(
    () =>
      (userSearchQuery.data ?? []).filter((user) => {
        const pubkey = normalizePubkey(user.pubkey);
        return !chosen.has(pubkey) && !isArchived(pubkey);
      }),
    [chosen, isArchived, userSearchQuery.data],
  );

  return (
    <div className="space-y-1.5">
      <label
        className="text-sm font-medium text-foreground"
        htmlFor="create-issue-mentions"
      >
        Notify
        <span className="ml-1 text-xs font-normal text-muted-foreground/50">
          Optional
        </span>
      </label>
      {selected.length > 0 ? (
        <div
          className="flex flex-wrap gap-1.5"
          data-testid="create-issue-mentions-selected"
        >
          {selected.map((pubkey) => {
            const profile = profileForPubkey(pubkey, profiles);
            const label = selectionLabel(pubkey, profiles);
            return (
              <span
                className="inline-flex items-center gap-1.5 rounded-full bg-muted px-2 py-1 text-xs text-foreground"
                key={pubkey}
              >
                <UserAvatar
                  accent={profile?.isAgent === true}
                  avatarUrl={profile?.avatarUrl ?? null}
                  displayName={label}
                  size="xs"
                />
                {label}
                <button
                  aria-label={`Remove ${label}`}
                  className="text-muted-foreground transition-colors hover:text-foreground"
                  disabled={disabled}
                  onClick={() =>
                    onChange(selected.filter((entry) => entry !== pubkey))
                  }
                  type="button"
                >
                  <X className="h-3 w-3" />
                </button>
              </span>
            );
          })}
        </div>
      ) : null}
      <div className="flex min-h-11 items-center gap-2 rounded-xl border border-input bg-muted/40 px-3 transition-colors hover:border-muted-foreground/40 focus-within:border-muted-foreground/50">
        <Search className="h-3.5 w-3.5 shrink-0 text-muted-foreground" />
        <Input
          className="h-8 border-0 bg-transparent px-0 shadow-none outline-none ring-0 placeholder:text-muted-foreground/55 focus-visible:ring-0"
          data-testid="create-issue-mentions"
          disabled={disabled}
          id="create-issue-mentions"
          onChange={(event) => setQuery(event.target.value)}
          placeholder="Search people to notify"
          value={query}
        />
      </div>
      {query.trim().length > 0 && candidates.length > 0 ? (
        <ul className="max-h-40 overflow-y-auto rounded-xl border border-input">
          {candidates.map((user) => {
            const label = searchLabel(user);
            return (
              <li key={user.pubkey}>
                <button
                  className="flex w-full items-center gap-2 px-3 py-2 text-left text-sm transition-colors hover:bg-muted"
                  data-testid={`create-issue-mention-${user.pubkey}`}
                  disabled={disabled}
                  onClick={() => {
                    onChange([...selected, normalizePubkey(user.pubkey)]);
                    setQuery("");
                  }}
                  type="button"
                >
                  <UserAvatar
                    accent={user.isAgent}
                    avatarUrl={user.avatarUrl}
                    displayName={label}
                    size="xs"
                  />
                  <span className="truncate">{label}</span>
                </button>
              </li>
            );
          })}
        </ul>
      ) : null}
    </div>
  );
}

export function CreateIssueDialog({
  isCreating,
  onCreate,
  onOpenChange,
  open,
  profiles,
  projectName,
}: {
  isCreating: boolean;
  onCreate: (input: CreateIssueDialogInput) => Promise<void>;
  onOpenChange: (open: boolean) => void;
  open: boolean;
  profiles?: UserProfileLookup;
  projectName: string;
}) {
  const [recipients, setRecipients] = React.useState<string[]>([]);

  // The shared dialog clears its own fields whenever it opens. A selection
  // left behind here would silently notify the previous issue's people.
  React.useEffect(() => {
    if (open) setRecipients([]);
  }, [open]);

  return (
    <CreateProjectWorkItemDialog
      bodyPlaceholder="Add context, expected behavior, or reproduction steps"
      description={`Create a task in ${projectName}`}
      isCreating={isCreating}
      itemName="issue"
      onCreate={onCreate}
      onOpenChange={onOpenChange}
      open={open}
      recipients={recipients}
      title="Create a task"
      titlePlaceholder="Describe the task"
    >
      <IssueMentionPicker
        disabled={isCreating}
        onChange={setRecipients}
        profiles={profiles}
        selected={recipients}
      />
    </CreateProjectWorkItemDialog>
  );
}
