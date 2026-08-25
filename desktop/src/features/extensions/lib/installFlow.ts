// The install flow: pick → preview → shape-validate → install.
//
// Decision 006 splits validation across the bridge by authority. This module is
// the frontend half: it reads the candidate manifest through the read-only
// preview command and runs zod over it *before* offering to install, so a
// malformed package is explained in the UI rather than bounced back as a single
// opaque string from the loader.
//
// The Rust loader remains authoritative and runs again at install time. Passing
// shape validation here is not permission to install — it only means the UI has
// nothing more useful to say than the loader will.

import {
  type InstalledExtension,
  installExtensionFromDirectory,
  installExtensionFromZip,
  pickExtensionDirectory,
  pickExtensionZip,
  previewExtensionPackage,
} from "@/features/extensions/lib/extensionsApi";
import { parseExtensionManifest } from "@/features/extensions/lib/manifestSchema";

export type InstallSource = "directory" | "zip";

/**
 * A manifest that failed frontend shape validation.
 *
 * Carries every problem rather than the first, because an author fixing a
 * manifest wants the whole list, and because the loader would only ever report
 * one at a time.
 */
export class ManifestShapeError extends Error {
  readonly issues: string[];

  constructor(issues: string[]) {
    super(issues[0] ?? "extension.json is not valid");
    this.name = "ManifestShapeError";
    this.issues = issues;
  }
}

/**
 * Run the whole flow for one source. Resolves to `null` when the user cancels
 * the picker — cancelling is not an error and must not be reported as one.
 */
export async function installFromPickedSource(
  source: InstallSource,
): Promise<InstalledExtension | null> {
  const path =
    source === "directory"
      ? await pickExtensionDirectory()
      : await pickExtensionZip();
  if (path === null) {
    return null;
  }

  const preview = await previewExtensionPackage(path);

  let manifest: unknown;
  try {
    manifest = JSON.parse(preview.manifestJson);
  } catch (error) {
    const detail = error instanceof Error ? error.message : String(error);
    throw new ManifestShapeError([
      `extension.json is not valid JSON: ${detail}`,
    ]);
  }

  const parsed = parseExtensionManifest(manifest);
  if (!parsed.ok) {
    throw new ManifestShapeError(parsed.errors);
  }

  return source === "directory"
    ? await installExtensionFromDirectory(path)
    : await installExtensionFromZip(path);
}
