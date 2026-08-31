// Local picker → host-owned preparation → zod review shape.

import {
  type PreparedExtension,
  pickExtensionDirectory,
  pickExtensionZip,
  prepareExtensionFromDirectory,
  prepareExtensionFromZip,
} from "@/features/extensions/lib/extensionsApi";
import { parseExtensionManifest } from "@/features/extensions/lib/manifestSchema";

export type InstallSource = "directory" | "zip";

export class ManifestShapeError extends Error {
  readonly issues: string[];

  constructor(issues: string[]) {
    super(issues[0] ?? "extension.json is not valid");
    this.name = "ManifestShapeError";
    this.issues = issues;
  }
}

export async function prepareFromPickedSource(
  source: InstallSource,
): Promise<PreparedExtension | null> {
  const path =
    source === "directory"
      ? await pickExtensionDirectory()
      : await pickExtensionZip();
  if (path === null) {
    return null;
  }

  const prepared =
    source === "directory"
      ? await prepareExtensionFromDirectory(path)
      : await prepareExtensionFromZip(path);
  const parsed = parseExtensionManifest(prepared.manifest);
  if (!parsed.ok) {
    throw new ManifestShapeError(parsed.errors);
  }
  return prepared;
}
