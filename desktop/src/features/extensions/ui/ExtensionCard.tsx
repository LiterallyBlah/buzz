import type { InstalledExtension } from "@/features/extensions/lib/extensionsApi";
import { summarizeScopes } from "@/features/extensions/lib/scopeSummary";
import { Badge } from "@/shared/ui/badge";
import { Card } from "@/shared/ui/card";

type ExtensionCardProps = {
  extension: InstalledExtension;
};

export function ExtensionCard({ extension }: ExtensionCardProps) {
  const scopes = summarizeScopes(extension.scopes);

  return (
    <Card className="p-4" data-testid={`installed-extension-${extension.id}`}>
      <div className="min-w-0 space-y-3">
        <div className="flex min-w-0 flex-wrap items-center gap-2">
          <h3 className="min-w-0 truncate text-sm font-semibold">
            {extension.name}
          </h3>
          <Badge variant="secondary">{extension.version}</Badge>
          <span className="truncate text-xs text-muted-foreground">
            {extension.id}
          </span>
        </div>

        {scopes.length > 0 ? (
          <div className="flex flex-wrap gap-1.5">
            {scopes.map((scope) => (
              <Badge key={scope} variant="outline">
                {scope}
              </Badge>
            ))}
          </div>
        ) : (
          <p className="text-sm text-muted-foreground">No scopes requested</p>
        )}

        {extension.egress.length > 0 ? (
          <p className="break-words text-xs text-muted-foreground">
            Declared egress: {extension.egress.join(", ")}
          </p>
        ) : null}
      </div>
    </Card>
  );
}
