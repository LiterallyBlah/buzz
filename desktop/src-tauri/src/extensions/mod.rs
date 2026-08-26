//! Extension packages — install and inventory.
//!
//! This is the install half of the Buzz Extensions frame (project home:
//! `buzz-extensions`). It installs a **local** directory or zip package into
//! `<app-data>/extensions/<id>`, validates its `extension.json`, and lists what
//! is installed.
//!
//! What is deliberately **not** here: hosting, the webview, the `window.buzz`
//! bridge, signing, the query surface, and any relay change. An installed
//! extension is inventory, not yet something that runs.
//!
//! Design inputs, all from the buzz-extensions repository:
//!
//! - decision 006 — strict JSON manifest, unknown fields rejected, id grammar.
//! - decision 008 — local directory/zip only; an update is a manual re-install.
//! - decision 004 — network egress default-deny, widened per declared origin.
//! - `docs/BRIDGE_SPEC.md` §4/§5/§7 — signable-kind allowlist, read denylist
//!   floor, and the manifest shape.
//!
//! The directory convention (`<app-data>/<feature>/`) matches
//! `custom_harnesses` and `managed_agents`.

mod frame_host;
mod install;
// `pub(crate)` so the signer enforcement in the bridge (P4) can import
// `manifest::EXTENSION_SIGNABLE_KINDS` rather than re-declare the allowlist.
// One writer, two consumers.
pub(crate) mod bridge;
pub(crate) mod dispatch;
pub(crate) mod grants;
pub(crate) mod manifest;
mod package_path;
mod preview;
pub(crate) mod publish;

use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Manager};

pub use manifest::ExtensionScopes;
pub use preview::ExtensionPackagePreview;

use manifest::ExtensionManifest;

/// Directory under `<app-data>` that holds installed packages.
const EXTENSIONS_DIR_NAME: &str = "extensions";

/// An extension package present in `<app-data>/extensions/`.
///
/// Every field except `path` and `installedAt` comes straight from the
/// package's validated `extension.json`; the frontend renders these as the
/// grant summary at install and in the management list.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InstalledExtension {
    /// Package id — also the directory name under `<app-data>/extensions/`.
    pub id: String,
    /// Human-readable name.
    pub name: String,
    /// Version string as declared.
    pub version: String,
    /// Package-relative path of the document the extension is hosted from.
    pub entry: String,
    /// Absolute path of the installed package directory.
    pub path: String,
    /// Unix seconds the package was installed.
    ///
    /// Read from the install directory's modification time, which the
    /// stage-then-swap sets while the package is being staged. No separate
    /// bookkeeping file is written, so there is no state that can disagree
    /// with what is actually on disk.
    pub installed_at: u64,
    /// Scopes the manifest requests.
    pub scopes: ExtensionScopes,
    /// Egress origins the manifest declares (empty is the default and the norm).
    pub egress: Vec<String>,
}

/// `<app-data>/extensions`, created if absent.
///
/// Mirrors `managed_agents::storage::managed_agents_base_dir` and the
/// `custom_harnesses` convention.
pub(crate) fn extensions_base_dir<R: tauri::Runtime>(
    app: &AppHandle<R>,
) -> Result<PathBuf, String> {
    let dir = app
        .path()
        .app_data_dir()
        .map_err(|error| format!("failed to resolve app data dir: {error}"))?
        .join(EXTENSIONS_DIR_NAME);
    std::fs::create_dir_all(&dir)
        .map_err(|error| format!("failed to create extensions dir: {error}"))?;
    Ok(dir)
}

/// Install an extension package from a local directory.
///
/// The package is staged, validated, and only then swapped into
/// `<app-data>/extensions/<id>`. Re-installing over an existing id replaces it
/// (decision 008: updates are a manual re-install).
#[tauri::command]
pub async fn install_extension_from_directory(
    app: AppHandle,
    source_dir: String,
) -> Result<InstalledExtension, String> {
    let base_dir = extensions_base_dir(&app)?;
    let source = PathBuf::from(source_dir);
    tokio::task::spawn_blocking(move || install_directory_in(&base_dir, &source))
        .await
        .map_err(|error| format!("extension install task failed: {error}"))?
}

/// Install an extension package from a local zip archive.
///
/// Entry names are validated against the platform-neutral traversal rules
/// before anything is written, and the expanded package is capped in entry
/// count and total size.
#[tauri::command]
pub async fn install_extension_from_zip(
    app: AppHandle,
    archive_path: String,
) -> Result<InstalledExtension, String> {
    let base_dir = extensions_base_dir(&app)?;
    let archive = PathBuf::from(archive_path);
    tokio::task::spawn_blocking(move || install_zip_in(&base_dir, &archive))
        .await
        .map_err(|error| format!("extension install task failed: {error}"))?
}

/// Stop the frame host, whatever the live-frame count says.
///
/// Called from app shutdown so a listener can never outlive the process, even
/// if a frame never released — a crashed or reloaded webview does exactly that.
pub(crate) fn shutdown_frame_host() {
    frame_host::shutdown_now();
}

/// Where an installed extension's page is served from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ExtensionFrameTarget {
    /// Absolute URL of the package's entry document.
    pub url: String,
    /// The origin that URL sits on, for the caller to assert against.
    pub origin: String,
    /// Opaque claim on the frame host, to be handed back on close.
    ///
    /// The caller must return *this* lease and no other. A frame whose open
    /// failed has none, so its cleanup cannot release a lease it never held.
    pub lease: String,
}

/// Start (or join) the frame host and resolve an installed extension's page.
///
/// The URL is built host-side from the *validated installed manifest*, never
/// from anything the caller supplies beyond the id — the frontend should not be
/// in the business of composing URLs into a security boundary.
///
/// Pairs with [`close_extension_frame`]: every successful call registers a live
/// frame, and the host stops when the last one is released (decision 002's
/// containment says nothing about lifetime, but a listener serving a user's
/// files with no frame open is surface for nothing).
#[tauri::command]
pub async fn open_extension_frame(
    app: AppHandle,
    id: String,
) -> Result<ExtensionFrameTarget, String> {
    let base_dir = extensions_base_dir(&app)?;
    let manifest = {
        let base_dir = base_dir.clone();
        tokio::task::spawn_blocking(move || resolve_frame_manifest(&base_dir, &id))
            .await
            .map_err(|error| format!("extension frame task failed: {error}"))??
    };

    let claim = frame_host::acquire(base_dir, &manifest.id).await?;
    // Buzz frames the *wrapper*, so the origin the caller asserts against is
    // the wrapper origin — a different origin from the one serving package
    // content, which is the point of the split.
    let origin = frame_host::origin_for_port(claim.wrapper_port);
    Ok(ExtensionFrameTarget {
        url: frame_host::wrapper_url(&origin, &manifest.id),
        origin,
        lease: claim.lease,
    })
}

/// The validated manifest of the installed package `id` names.
///
/// The grammar check comes **before** the join, not after. `id` arrives from the
/// webview, and `base_dir.join(id)` with an unchecked `id` is a path traversal
/// waiting for a directory to exist — validating the manifest afterwards would
/// be checking the wrong thing, since by then the read has already happened
/// somewhere it should not have.
///
/// The manifest's own id must also match the directory, so a package cannot
/// claim to be one extension while installed as another.
pub(crate) fn resolve_frame_manifest(
    base_dir: &Path,
    id: &str,
) -> Result<ExtensionManifest, String> {
    if !manifest::is_valid_extension_id(id) {
        return Err(format!("extension id {id:?} is not valid"));
    }
    let manifest = manifest::load_and_validate_manifest(&base_dir.join(id))?;
    if manifest.id != id {
        return Err(format!(
            "extension folder {id:?} holds a manifest claiming id {:?}",
            manifest.id
        ));
    }
    Ok(manifest)
}

/// Release one live extension frame, stopping the host when it was the last.
///
/// Takes the lease `open_extension_frame` issued. Releasing an unknown lease is
/// a no-op rather than an error: cleanup runs on paths where opening failed,
/// and making that noisy would only teach callers to ignore it.
#[tauri::command]
pub async fn close_extension_frame(lease: String) -> Result<(), String> {
    frame_host::release(&lease);
    Ok(())
}

/// Read a candidate package's `extension.json` without installing it.
///
/// This is the seam decision 006's frontend half needs: the webview cannot read
/// local paths, so it asks the host for the manifest, validates its shape with
/// zod, and only then offers to install. It is also what P5's grant-review UI
/// reads to show what an extension is asking for *before* the user agrees —
/// hence a whole manifest rather than a summary.
///
/// Read-only and non-authoritative: nothing is written, nothing is validated
/// here, and a package that previews cleanly can still be rejected at install.
#[tauri::command]
pub async fn preview_extension_package(source: String) -> Result<ExtensionPackagePreview, String> {
    let path = PathBuf::from(source);
    tokio::task::spawn_blocking(move || preview::preview_package(&path))
        .await
        .map_err(|error| format!("extension preview task failed: {error}"))?
}

/// List every installed extension package.
///
/// A directory whose manifest no longer validates is skipped with a warning
/// rather than failing the whole list — one broken package must not make the
/// feature unusable. This matches `custom_harnesses`, which warns on invalid
/// entries and never propagates them to the caller.
#[tauri::command]
pub async fn list_installed_extensions(app: AppHandle) -> Result<Vec<InstalledExtension>, String> {
    let base_dir = extensions_base_dir(&app)?;
    tokio::task::spawn_blocking(move || list_installed_in(&base_dir))
        .await
        .map_err(|error| format!("extension list task failed: {error}"))?
}

/// Ask the user for an extension package folder.
///
/// Returns `Ok(None)` when the user cancels — cancelling is not an error.
///
/// There is no `@tauri-apps/plugin-dialog` JS binding in this app, so the
/// picker has to be a Rust command the frontend invokes. The non-blocking
/// callback form is used (as everywhere else in the tree — `export_util`,
/// `commands::media`, `huddle::tts_voice_import`) because the blocking form
/// deadlocks when it runs on the main thread.
#[tauri::command]
pub async fn pick_extension_directory(app: AppHandle) -> Result<Option<String>, String> {
    use tauri_plugin_dialog::DialogExt;

    let (sender, receiver) = tokio::sync::oneshot::channel();
    app.dialog()
        .file()
        .set_title("Choose an extension folder")
        .pick_folder(move |folder| {
            let _ = sender.send(folder);
        });

    let Some(folder) = receiver
        .await
        .map_err(|_| "the folder picker closed unexpectedly".to_string())?
    else {
        return Ok(None);
    };
    picked_path_to_string(folder).map(Some)
}

/// Ask the user for an extension package zip.
///
/// Returns `Ok(None)` when the user cancels — cancelling is not an error.
#[tauri::command]
pub async fn pick_extension_zip(app: AppHandle) -> Result<Option<String>, String> {
    use tauri_plugin_dialog::DialogExt;

    let (sender, receiver) = tokio::sync::oneshot::channel();
    app.dialog()
        .file()
        .set_title("Choose an extension package")
        .add_filter("Extension package", &["zip"])
        .pick_file(move |file| {
            let _ = sender.send(file);
        });

    let Some(file) = receiver
        .await
        .map_err(|_| "the file picker closed unexpectedly".to_string())?
    else {
        return Ok(None);
    };
    picked_path_to_string(file).map(Some)
}

/// Convert a picked path to the `String` the IPC contract carries.
///
/// A non-UTF-8 path is an error rather than a lossy conversion: a mangled path
/// would fail later with a confusing "not found" instead of naming the problem.
fn picked_path_to_string(picked: tauri_plugin_dialog::FilePath) -> Result<String, String> {
    let path = picked
        .as_path()
        .ok_or_else(|| "the selected path is not a local file path".to_string())?;
    path.to_str()
        .map(str::to_string)
        .ok_or_else(|| "the selected path is not valid UTF-8".to_string())
}

// ── Testable cores (no `AppHandle` required) ─────────────────────────────────

/// Install from a directory into an explicit base directory.
pub(crate) fn install_directory_in(
    base_dir: &Path,
    source_dir: &Path,
) -> Result<InstalledExtension, String> {
    let (manifest, path) = install::install_from_directory(base_dir, source_dir)?;
    Ok(installed_from(manifest, &path))
}

/// Install from a zip into an explicit base directory.
pub(crate) fn install_zip_in(
    base_dir: &Path,
    archive_path: &Path,
) -> Result<InstalledExtension, String> {
    let (manifest, path) = install::install_from_zip(base_dir, archive_path)?;
    Ok(installed_from(manifest, &path))
}

/// List installed packages under an explicit base directory.
pub(crate) fn list_installed_in(base_dir: &Path) -> Result<Vec<InstalledExtension>, String> {
    if !base_dir.is_dir() {
        return Ok(Vec::new());
    }

    let entries = std::fs::read_dir(base_dir)
        .map_err(|error| format!("could not read the extensions folder: {error}"))?;

    let mut installed = Vec::new();
    for entry in entries {
        let entry =
            entry.map_err(|error| format!("could not read an extensions entry: {error}"))?;
        let Ok(name) = entry.file_name().into_string() else {
            continue;
        };
        // Staging and replaced-install holders are dot-prefixed, and so is
        // anything else the user dropped in; neither is an installed package.
        if name.starts_with('.') {
            continue;
        }
        // `symlink_metadata` so a symlinked directory is not treated as an
        // install — the package tree is Buzz-owned, not a pointer elsewhere.
        let path = entry.path();
        let Ok(metadata) = std::fs::symlink_metadata(&path) else {
            continue;
        };
        if !metadata.is_dir() {
            continue;
        }

        match load_installed(&path, &name) {
            Ok(extension) => installed.push(extension),
            Err(error) => {
                eprintln!("buzz-desktop: skipping extension {name:?}: {error}");
            }
        }
    }

    installed.sort_by(|left, right| left.id.cmp(&right.id));
    Ok(installed)
}

/// Read and re-validate one installed package directory.
fn load_installed(path: &Path, directory_name: &str) -> Result<InstalledExtension, String> {
    let manifest = manifest::load_and_validate_manifest(path)?;
    if manifest.id != directory_name {
        return Err(format!(
            "manifest id {:?} does not match its folder name {directory_name:?}",
            manifest.id
        ));
    }
    Ok(installed_from(manifest, path))
}

/// Build the IPC record for a validated manifest installed at `path`.
fn installed_from(manifest: ExtensionManifest, path: &Path) -> InstalledExtension {
    InstalledExtension {
        id: manifest.id,
        name: manifest.name,
        version: manifest.version,
        entry: manifest.entry,
        path: path.to_string_lossy().into_owned(),
        installed_at: installed_at_secs(path),
        scopes: manifest.scopes,
        egress: manifest.egress,
    }
}

/// Install time in unix seconds, from the package directory's mtime.
fn installed_at_secs(path: &Path) -> u64 {
    std::fs::metadata(path)
        .and_then(|metadata| metadata.modified())
        .ok()
        .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
        .map(|since| since.as_secs())
        .unwrap_or(0)
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
#[path = "mod_tests.rs"]
mod mod_tests;
