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

export type ExtensionFrameTarget = {
  /** Absolute URL of the package's entry document. */
  url: string;
  /** The origin that URL sits on. */
  origin: string;
  /**
   * Opaque claim on the frame host. Hand back exactly this on close.
   *
   * A frame whose open failed has no lease, so its cleanup must not call close
   * at all — releasing "a" holder rather than "your" holder is what let a
   * failed frame stop the server serving a healthy one.
   */
  lease: string;
};

/**
 * Start (or join) the frame host and get the URL of an extension's page.
 *
 * The URL is composed host-side from the validated installed manifest — the
 * frontend deliberately does not build URLs into this boundary.
 *
 * Every successful call registers a live frame; pair it with
 * {@link closeExtensionFrame} or the localhost listener outlives the tab.
 */
export function openExtensionFrame(id: string): Promise<ExtensionFrameTarget> {
  return invoke<ExtensionFrameTarget>("open_extension_frame", { id });
}

/** Release the lease from a successful {@link openExtensionFrame}. */
export function closeExtensionFrame(lease: string): Promise<void> {
  return invoke<void>("close_extension_frame", { lease });
}
