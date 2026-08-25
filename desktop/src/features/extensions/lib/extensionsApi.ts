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
