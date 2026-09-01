// Typed wrappers over the Rust extension-management commands.
// Distribution is local-only: directory or ZIP, prepared into host-owned bytes.

import { invoke } from "@tauri-apps/api/core";

export type ExtensionSignScope = { kind: number; channels: string[] };
export type ExtensionReadScope = { kinds: number[]; channels: string[] };
export type ExtensionScopes = {
  identity: boolean;
  storage: boolean;
  agentConverse: boolean;
  extensionData: boolean;
  sign: ExtensionSignScope[];
  read: ExtensionReadScope[];
};

export type ExtensionGrantPair = { kind: number; channel: string };
export type ExtensionGrantSelection = {
  identity: boolean;
  storage: boolean;
  agentConverse: boolean;
  extensionData: boolean;
  sign: ExtensionGrantPair[];
  read: ExtensionGrantPair[];
  egress: string[];
};

export type InstalledExtension = {
  id: string;
  name: string;
  version: string;
  entry: string;
  path: string;
  installedAt: number;
  scopes: ExtensionScopes;
  egress: string[];
  digest: string;
  enabled: boolean;
  granted: ExtensionGrantSelection;
};

export type PreparedExtension = {
  token: string;
  digest: string;
  manifest: {
    id: string;
    name: string;
    version: string;
    entry: string;
    scopes: ExtensionScopes;
    egress: string[];
  };
  sourceType: "directory" | "zip";
  expiresAt: number;
};

export function emptyGrantSelection(): ExtensionGrantSelection {
  return {
    identity: false,
    storage: false,
    agentConverse: false,
    extensionData: false,
    sign: [],
    read: [],
    egress: [],
  };
}

export function pickExtensionDirectory(): Promise<string | null> {
  return invoke<string | null>("pick_extension_directory");
}

export function pickExtensionZip(): Promise<string | null> {
  return invoke<string | null>("pick_extension_zip");
}

export type ExtensionPackagePreview = {
  source: string;
  manifestJson: string;
};

export function previewExtensionPackage(
  source: string,
): Promise<ExtensionPackagePreview> {
  return invoke<ExtensionPackagePreview>("preview_extension_package", {
    source,
  });
}

export function prepareExtensionFromDirectory(
  sourceDir: string,
): Promise<PreparedExtension> {
  return invoke<PreparedExtension>("prepare_extension_from_directory", {
    sourceDir,
  });
}

export function prepareExtensionFromZip(
  archivePath: string,
): Promise<PreparedExtension> {
  return invoke<PreparedExtension>("prepare_extension_from_zip", {
    archivePath,
  });
}

export function approvePreparedExtension(
  token: string,
  selected: ExtensionGrantSelection,
): Promise<InstalledExtension> {
  return invoke<InstalledExtension>("approve_prepared_extension", {
    token,
    selected,
  });
}

export function cancelPreparedExtension(token: string): Promise<void> {
  return invoke<void>("cancel_prepared_extension", { token });
}

export function listInstalledExtensions(): Promise<InstalledExtension[]> {
  return invoke<InstalledExtension[]>("list_installed_extensions");
}

export function setExtensionEnabled(
  id: string,
  enabled: boolean,
): Promise<InstalledExtension> {
  return invoke<InstalledExtension>("set_extension_enabled", { id, enabled });
}

export function updateExtensionGrants(
  id: string,
  selected: ExtensionGrantSelection,
): Promise<InstalledExtension> {
  return invoke<InstalledExtension>("update_extension_grants", {
    id,
    selected,
  });
}

export type RemoveExtensionResult = {
  removed: boolean;
  recoveryPath: string | null;
};

export function removeExtension(id: string): Promise<RemoveExtensionResult> {
  return invoke<RemoveExtensionResult>("remove_extension", { id });
}

export type ExtensionFrameTarget = {
  url: string;
  origin: string;
  lease: string;
};

export function openExtensionFrame(id: string): Promise<ExtensionFrameTarget> {
  return invoke<ExtensionFrameTarget>("open_extension_frame", { id });
}

export function closeExtensionFrame(lease: string): Promise<void> {
  return invoke<void>("close_extension_frame", { lease });
}
