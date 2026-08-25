// Typed wrappers over the Rust extension-install commands.
//
// Distribution is local-only (decision 008): a directory or a zip the user
// picked themselves. There is deliberately no URL, repo or update surface here.
//
// The file pickers are Rust commands: this app ships no
// `@tauri-apps/plugin-dialog` JS binding, so every picker in the tree is a
// command the frontend invokes (see `export_util.rs`, `media.rs`).

import { invoke } from "@tauri-apps/api/core";

export type ExtensionSignScope = {
  kind: number;
  channels: string[];
};

export type ExtensionReadScope = {
  kinds: number[];
  channels: string[];
};

export type ExtensionScopes = {
  identity: boolean;
  storage: boolean;
  extensionData: boolean;
  sign: ExtensionSignScope[];
  read: ExtensionReadScope[];
};

export type InstalledExtension = {
  id: string;
  name: string;
  version: string;
  entry: string;
  /** Absolute path of the installed package under `<app-data>/extensions/`. */
  path: string;
  /** Unix seconds. */
  installedAt: number;
  scopes: ExtensionScopes;
  egress: string[];
};

/** Open a directory picker. Resolves to `null` when the user cancels. */
export function pickExtensionDirectory(): Promise<string | null> {
  return invoke<string | null>("pick_extension_directory");
}

/** Open a zip file picker. Resolves to `null` when the user cancels. */
export function pickExtensionZip(): Promise<string | null> {
  return invoke<string | null>("pick_extension_zip");
}

export type ExtensionPackagePreview = {
  /** The directory or zip that was inspected. */
  source: string;
  /** Raw `extension.json` contents — not parsed and not validated by the host. */
  manifestJson: string;
};

/**
 * Read a candidate package's manifest without installing it.
 *
 * The webview cannot read local paths, so this is how decision 006's frontend
 * validation half gets something to validate. Read-only and non-authoritative:
 * a package that previews cleanly can still be rejected at install. P5's
 * grant-review UI reads the same preview.
 */
export function previewExtensionPackage(
  source: string,
): Promise<ExtensionPackagePreview> {
  return invoke<ExtensionPackagePreview>("preview_extension_package", {
    source,
  });
}

export function installExtensionFromDirectory(
  sourceDir: string,
): Promise<InstalledExtension> {
  return invoke<InstalledExtension>("install_extension_from_directory", {
    sourceDir,
  });
}

export function installExtensionFromZip(
  archivePath: string,
): Promise<InstalledExtension> {
  return invoke<InstalledExtension>("install_extension_from_zip", {
    archivePath,
  });
}

export function listInstalledExtensions(): Promise<InstalledExtension[]> {
  return invoke<InstalledExtension[]>("list_installed_extensions");
}
