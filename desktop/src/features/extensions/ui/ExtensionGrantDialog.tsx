import * as React from "react";

import type {
  ExtensionGrantSelection,
  ExtensionScopes,
} from "@/features/extensions/lib/extensionsApi";
import { emptyGrantSelection } from "@/features/extensions/lib/extensionsApi";
import { Button } from "@/shared/ui/button";
import { Checkbox } from "@/shared/ui/checkbox";
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from "@/shared/ui/dialog";

export type GrantReviewManifest = {
  id: string;
  name: string;
  version: string;
  scopes: ExtensionScopes;
  egress: string[];
};

type Props = {
  open: boolean;
  mode: "install" | "review";
  manifest: GrantReviewManifest | null;
  digest: string;
  initial?: ExtensionGrantSelection;
  pending: boolean;
  error: string | null;
  onCancel: () => void;
  onConfirm: (selected: ExtensionGrantSelection) => void;
};

function pairKey(pair: { kind: number; channel: string }) {
  return `${pair.kind}@${pair.channel}`;
}

function requestedSign(manifest: GrantReviewManifest) {
  return manifest.scopes.sign.flatMap((scope) =>
    scope.channels.map((channel) => ({ channel, kind: scope.kind })),
  );
}

function requestedRead(manifest: GrantReviewManifest) {
  return manifest.scopes.read.flatMap((scope) =>
    scope.kinds.flatMap((kind) =>
      scope.channels.map((channel) => ({ channel, kind })),
    ),
  );
}

export function ExtensionGrantDialog({
  open,
  mode,
  manifest,
  digest,
  initial,
  pending,
  error,
  onCancel,
  onConfirm,
}: Props) {
  const [selected, setSelected] =
    React.useState<ExtensionGrantSelection>(emptyGrantSelection);

  React.useEffect(() => {
    if (open) {
      setSelected(initial ?? emptyGrantSelection());
    }
  }, [initial, open]);

  if (!manifest) {
    return null;
  }

  const togglePair = (
    field: "sign" | "read",
    pair: { kind: number; channel: string },
    checked: boolean,
  ) => {
    setSelected((current) => ({
      ...current,
      [field]: checked
        ? [
            ...current[field].filter(
              (value) => pairKey(value) !== pairKey(pair),
            ),
            pair,
          ]
        : current[field].filter((value) => pairKey(value) !== pairKey(pair)),
    }));
  };
  const toggleOrigin = (origin: string, checked: boolean) => {
    setSelected((current) => ({
      ...current,
      egress: checked
        ? [...current.egress.filter((value) => value !== origin), origin]
        : current.egress.filter((value) => value !== origin),
    }));
  };

  return (
    <Dialog
      onOpenChange={(next) => {
        if (!next && !pending) onCancel();
      }}
      open={open}
    >
      <DialogContent
        className="flex max-h-[85vh] flex-col overflow-hidden sm:max-w-2xl"
        data-testid="extension-consent-dialog"
      >
        <DialogHeader>
          <DialogTitle>
            {mode === "install"
              ? "Review extension access"
              : "Change extension access"}
          </DialogTitle>
          <DialogDescription>
            {manifest.name} {manifest.version} · {manifest.id}. Nothing is
            granted by default. Package digest {digest.slice(0, 16)}…
          </DialogDescription>
        </DialogHeader>

        <div className="min-h-0 space-y-4 overflow-y-auto py-2 text-sm">
          <p className="rounded-md border border-border/70 bg-muted/40 p-3 text-muted-foreground">
            Select only the scopes, concrete kind/channel pairs, and origins
            this package should receive. Unselected requests remain denied.
          </p>

          <section className="space-y-2" aria-label="Boolean scopes">
            <h3 className="font-medium">Boolean scopes</h3>
            {(
              [
                ["identity", "Identity public key", manifest.scopes.identity],
                ["storage", "Device-local storage", manifest.scopes.storage],
                [
                  "extensionData",
                  "Namespaced kind 30800 data",
                  manifest.scopes.extensionData,
                ],
              ] as const
            )
              .filter(([, , requested]) => requested)
              .map(([field, label]) => (
                <div className="flex items-center gap-2" key={field}>
                  <Checkbox
                    checked={selected[field]}
                    data-testid={`grant-${field}`}
                    onCheckedChange={(checked) =>
                      setSelected((current) => ({
                        ...current,
                        [field]: checked === true,
                      }))
                    }
                  />
                  <span>{label}</span>
                </div>
              ))}
            {!manifest.scopes.identity &&
            !manifest.scopes.storage &&
            !manifest.scopes.extensionData ? (
              <p className="text-muted-foreground">
                No boolean scopes requested.
              </p>
            ) : null}
          </section>

          <section className="space-y-2" aria-label="Signing grants">
            <h3 className="font-medium">Signing pairs</h3>
            {requestedSign(manifest).map((pair) => (
              <div className="flex items-start gap-2" key={pairKey(pair)}>
                <Checkbox
                  checked={selected.sign.some(
                    (value) => pairKey(value) === pairKey(pair),
                  )}
                  data-testid={`grant-sign-${pair.kind}-${pair.channel}`}
                  onCheckedChange={(checked) =>
                    togglePair("sign", pair, checked === true)
                  }
                />
                <span>
                  Kind {pair.kind} in channel <code>{pair.channel}</code>
                </span>
              </div>
            ))}
            {requestedSign(manifest).length === 0 ? (
              <p className="text-muted-foreground">
                No signing pairs requested.
              </p>
            ) : null}
          </section>

          <section className="space-y-2" aria-label="Read grants">
            <h3 className="font-medium">Read pairs</h3>
            {requestedRead(manifest).map((pair) => (
              <div className="flex items-start gap-2" key={pairKey(pair)}>
                <Checkbox
                  checked={selected.read.some(
                    (value) => pairKey(value) === pairKey(pair),
                  )}
                  data-testid={`grant-read-${pair.kind}-${pair.channel}`}
                  onCheckedChange={(checked) =>
                    togglePair("read", pair, checked === true)
                  }
                />
                <span>
                  Kind {pair.kind} in channel <code>{pair.channel}</code>
                </span>
              </div>
            ))}
            {requestedRead(manifest).length === 0 ? (
              <p className="text-muted-foreground">No read pairs requested.</p>
            ) : null}
          </section>

          <section className="space-y-2" aria-label="Egress origins">
            <h3 className="font-medium">External network origins</h3>
            {manifest.egress.map((origin) => (
              <div className="flex items-start gap-2" key={origin}>
                <Checkbox
                  checked={selected.egress.includes(origin)}
                  data-testid={`grant-egress-${origin}`}
                  onCheckedChange={(checked) =>
                    toggleOrigin(origin, checked === true)
                  }
                />
                <code>{origin}</code>
              </div>
            ))}
            {manifest.egress.length === 0 ? (
              <p className="text-muted-foreground">
                None requested; network egress remains default-deny.
              </p>
            ) : null}
          </section>

          {error ? (
            <p className="rounded-md border border-destructive/40 bg-destructive/10 p-3 text-destructive">
              {error}
            </p>
          ) : null}
        </div>

        <DialogFooter>
          <Button
            disabled={pending}
            onClick={onCancel}
            type="button"
            variant="outline"
          >
            Cancel
          </Button>
          <Button
            data-testid="extension-consent-confirm"
            disabled={pending}
            onClick={() => onConfirm(selected)}
            type="button"
          >
            {pending
              ? "Saving…"
              : mode === "install"
                ? "Install disabled"
                : "Save grants"}
          </Button>
        </DialogFooter>
      </DialogContent>
    </Dialog>
  );
}
