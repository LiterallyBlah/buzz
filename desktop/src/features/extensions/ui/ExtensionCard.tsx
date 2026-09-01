import type { InstalledExtension } from "@/features/extensions/lib/extensionsApi";
import { summarizeScopes } from "@/features/extensions/lib/scopeSummary";
import { Badge } from "@/shared/ui/badge";
import { Button } from "@/shared/ui/button";
import { Card } from "@/shared/ui/card";

type Props = {
  extension: InstalledExtension;
  pending: boolean;
  onOpen: (id: string) => void;
  onToggle: (extension: InstalledExtension) => void;
  onReview: (extension: InstalledExtension) => void;
  onRemove: (extension: InstalledExtension) => void;
};

function grantedSummary(extension: InstalledExtension): string[] {
  const granted: string[] = [];
  if (extension.granted.identity) granted.push("Identity");
  if (extension.granted.storage) granted.push("Storage");
  if (extension.granted.extensionData) granted.push("Extension data");
  granted.push(
    ...extension.granted.sign.map(
      (pair) => `Sign ${pair.kind} @ ${pair.channel}`,
    ),
    ...extension.granted.read.map(
      (pair) => `Read ${pair.kind} @ ${pair.channel}`,
    ),
    ...extension.granted.egress.map((origin) => `Egress ${origin}`),
  );
  return granted;
}

export function ExtensionCard({
  extension,
  pending,
  onOpen,
  onToggle,
  onReview,
  onRemove,
}: Props) {
  const requested = summarizeScopes(extension.scopes);
  const granted = grantedSummary(extension);

  return (
    <Card className="p-4" data-testid={`installed-extension-${extension.id}`}>
      <div className="min-w-0 space-y-3">
        <div className="flex min-w-0 flex-wrap items-center gap-2">
          <h3 className="min-w-0 truncate text-sm font-semibold">
            {extension.name}
          </h3>
          <Badge variant="secondary">{extension.version}</Badge>
          <Badge variant={extension.enabled ? "default" : "outline"}>
            {extension.enabled ? "Enabled" : "Disabled"}
          </Badge>
          <span className="truncate text-xs text-muted-foreground">
            {extension.id}
          </span>
          <div className="ml-auto flex flex-wrap gap-2">
            <Button
              data-testid={`review-extension-${extension.id}`}
              disabled={pending}
              onClick={() => onReview(extension)}
              size="sm"
              variant="ghost"
            >
              Grants
            </Button>
            <Button
              data-testid={`toggle-extension-${extension.id}`}
              disabled={pending}
              onClick={() => onToggle(extension)}
              size="sm"
              variant="outline"
            >
              {extension.enabled ? "Disable" : "Enable"}
            </Button>
            <Button
              data-testid={`remove-extension-${extension.id}`}
              disabled={pending}
              onClick={() => onRemove(extension)}
              size="sm"
              variant="ghost"
            >
              Remove
            </Button>
            <Button
              data-testid={`open-extension-${extension.id}`}
              disabled={!extension.enabled || pending}
              onClick={() => onOpen(extension.id)}
              size="sm"
            >
              Open
            </Button>
          </div>
        </div>

        <div className="grid gap-3 md:grid-cols-2">
          <div>
            <p className="text-xs font-medium text-muted-foreground">
              Requested
            </p>
            <div className="mt-1 flex flex-wrap gap-1.5">
              {requested.length > 0 ? (
                requested.map((scope) => (
                  <Badge key={scope} variant="outline">
                    {scope}
                  </Badge>
                ))
              ) : (
                <span className="text-sm text-muted-foreground">None</span>
              )}
            </div>
          </div>
          <div>
            <p className="text-xs font-medium text-muted-foreground">
              Granted now
            </p>
            <div className="mt-1 flex flex-wrap gap-1.5">
              {granted.length > 0 ? (
                granted.map((scope) => (
                  <Badge key={scope} variant="secondary">
                    {scope}
                  </Badge>
                ))
              ) : (
                <span className="text-sm text-muted-foreground">None</span>
              )}
            </div>
          </div>
        </div>

        <p className="truncate font-mono text-xs text-muted-foreground">
          Digest {extension.digest}
        </p>
      </div>
    </Card>
  );
}
