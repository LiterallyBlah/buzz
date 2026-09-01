// Renders an installed extension's declared scopes in human terms.
//
// M1 P1/P2 lists what a package *declares*. Granting, revoking and the consent
// dialog are package P5 — nothing here implies the user has approved anything,
// so the wording is "requests", not "can".

import type { ExtensionScopes } from "@/features/extensions/lib/extensionsApi";

function channelCount(channels: string[]): string {
  return channels.length === 1 ? "1 channel" : `${channels.length} channels`;
}

/** One short phrase per declared scope, in a stable order. */
export function summarizeScopes(scopes: ExtensionScopes): string[] {
  const summary: string[] = [];

  if (scopes.identity) {
    summary.push("Identity");
  }
  if (scopes.storage) {
    summary.push("Storage");
  }
  if (scopes.agentConverse) {
    summary.push("Local agent conversations");
  }
  if (scopes.extensionData) {
    summary.push("Extension data");
  }
  for (const scope of scopes.sign) {
    summary.push(`Sign kind ${scope.kind} in ${channelCount(scope.channels)}`);
  }
  for (const scope of scopes.read) {
    summary.push(
      `Read kind${scope.kinds.length === 1 ? "" : "s"} ${scope.kinds.join(", ")} in ${channelCount(scope.channels)}`,
    );
  }

  return summary;
}
