//! P5 extension preparation, consent, activation and removal.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard, OnceLock};

use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};
use tauri::{AppHandle, Manager as _};

use super::grants::GrantSelection;
use super::manifest::ExtensionManifest;

const PREPARED_PREFIX: &str = ".prepared-";
const REMOVED_PREFIX: &str = ".removed-";
const PREPARED_TTL_SECONDS: u64 = 10 * 60;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct PreparedExtension {
    pub token: String,
    pub digest: String,
    pub manifest: ExtensionManifest,
    pub source_type: String,
    pub expires_at: u64,
}

#[derive(Debug)]
struct PreparedPackage {
    identity_pubkey: String,
    digest: String,
    manifest: ExtensionManifest,
    source_type: String,
    staged_path: PathBuf,
    expires_at: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct RemoveExtensionResult {
    pub removed: bool,
    pub recovery_path: Option<String>,
}

static PREPARED: OnceLock<Mutex<HashMap<String, PreparedPackage>>> = OnceLock::new();
static LIFECYCLE_FENCE: OnceLock<tokio::sync::RwLock<()>> = OnceLock::new();

#[cfg(test)]
static PACKAGE_TREE_WALKS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
#[cfg(test)]
static PACKAGE_TREE_WALK_ROOT: OnceLock<Mutex<Option<PathBuf>>> = OnceLock::new();

pub(crate) async fn lifecycle_read_fence() -> tokio::sync::RwLockReadGuard<'static, ()> {
    LIFECYCLE_FENCE
        .get_or_init(|| tokio::sync::RwLock::new(()))
        .read()
        .await
}

pub(crate) async fn lifecycle_write_fence() -> tokio::sync::RwLockWriteGuard<'static, ()> {
    LIFECYCLE_FENCE
        .get_or_init(|| tokio::sync::RwLock::new(()))
        .write()
        .await
}

#[cfg(test)]
pub(crate) fn reset_package_tree_walks(root: &Path) {
    *PACKAGE_TREE_WALK_ROOT
        .get_or_init(|| Mutex::new(None))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(root.to_path_buf());
    PACKAGE_TREE_WALKS.store(0, std::sync::atomic::Ordering::SeqCst);
}

#[cfg(test)]
pub(crate) fn package_tree_walks() -> usize {
    PACKAGE_TREE_WALKS.load(std::sync::atomic::Ordering::SeqCst)
}

fn prepared_registry() -> MutexGuard<'static, HashMap<String, PreparedPackage>> {
    let lock = PREPARED.get_or_init(|| Mutex::new(HashMap::new()));
    match lock.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    }
}

fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |duration| duration.as_secs())
}

fn current_identity<R: tauri::Runtime>(app: &AppHandle<R>) -> Result<String, String> {
    let state = app.state::<crate::AppState>();
    super::dispatch::resolve_identity_pubkey(&state)
        .ok_or_else(|| "no usable identity is available".to_string())
}

fn remove_staged(path: &Path) {
    let _ = fs::remove_dir_all(path);
}

fn expire_prepared() {
    let now = now_unix();
    let expired: Vec<PathBuf> = {
        let mut registry = prepared_registry();
        let expired_tokens: Vec<String> = registry
            .iter()
            .filter(|(_, package)| package.expires_at <= now)
            .map(|(token, _)| token.clone())
            .collect();
        expired_tokens
            .into_iter()
            .filter_map(|token| registry.remove(&token).map(|package| package.staged_path))
            .collect()
    };
    for path in expired {
        remove_staged(&path);
    }
}

pub(crate) fn clear_prepared() {
    let paths: Vec<PathBuf> = prepared_registry()
        .drain()
        .map(|(_, package)| package.staged_path)
        .collect();
    for path in paths {
        remove_staged(&path);
    }
}

fn walk_package(
    root: &Path,
    current: &Path,
    entries: &mut Vec<(String, Vec<u8>)>,
) -> Result<(), String> {
    let children = fs::read_dir(current)
        .map_err(|error| format!("could not read the prepared package: {error}"))?;
    for child in children {
        let child = child.map_err(|error| format!("could not read a package entry: {error}"))?;
        let path = child.path();
        let metadata = fs::symlink_metadata(&path)
            .map_err(|error| format!("could not inspect a package entry: {error}"))?;
        if metadata.is_symlink() {
            return Err("prepared package contains a symlink".to_string());
        }
        if metadata.is_dir() {
            walk_package(root, &path, entries)?;
        } else if metadata.is_file() {
            let relative = path
                .strip_prefix(root)
                .map_err(|_| "prepared package escaped its staging root".to_string())?;
            let relative = relative
                .to_str()
                .ok_or_else(|| "prepared package contains a non-UTF-8 path".to_string())?
                .replace('\\', "/");
            let bytes = fs::read(&path)
                .map_err(|error| format!("could not read a prepared package file: {error}"))?;
            entries.push((relative, bytes));
        } else {
            return Err("prepared package contains an unsupported file type".to_string());
        }
    }
    Ok(())
}

pub(crate) fn package_digest(root: &Path) -> Result<String, String> {
    #[cfg(test)]
    if PACKAGE_TREE_WALK_ROOT
        .get_or_init(|| Mutex::new(None))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .as_deref()
        == Some(root)
    {
        PACKAGE_TREE_WALKS.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    }
    let mut entries = Vec::new();
    walk_package(root, root, &mut entries)?;
    entries.sort_by(|left, right| left.0.cmp(&right.0));
    let mut digest = Sha256::new();
    for (path, bytes) in entries {
        digest.update(b"file\0");
        digest.update(path.as_bytes());
        digest.update(b"\0");
        digest.update((bytes.len() as u64).to_be_bytes());
        digest.update(&bytes);
    }
    Ok(hex::encode(digest.finalize()))
}

fn prepare_in(
    base_dir: &Path,
    source: &Path,
    source_type: &str,
    identity_pubkey: String,
) -> Result<PreparedExtension, String> {
    fs::create_dir_all(base_dir)
        .map_err(|error| format!("could not create the extensions folder: {error}"))?;
    let staging = tempfile::Builder::new()
        .prefix(PREPARED_PREFIX)
        .tempdir_in(base_dir)
        .map_err(|error| format!("could not create a private preparation folder: {error}"))?;
    match source_type {
        "directory" => super::install::stage_directory(staging.path(), source)?,
        "zip" => super::install::stage_zip(staging.path(), source)?,
        _ => return Err("unsupported extension source type".to_string()),
    }
    let manifest = super::manifest::load_and_validate_manifest(staging.path())?;
    let digest = package_digest(staging.path())?;
    let staged_path = staging.keep();
    let token = uuid::Uuid::new_v4().to_string();
    let expires_at = now_unix().saturating_add(PREPARED_TTL_SECONDS);
    let prepared = PreparedExtension {
        token: token.clone(),
        digest: digest.clone(),
        manifest: manifest.clone(),
        source_type: source_type.to_string(),
        expires_at,
    };
    prepared_registry().insert(
        token,
        PreparedPackage {
            identity_pubkey,
            digest,
            manifest,
            source_type: source_type.to_string(),
            staged_path,
            expires_at,
        },
    );
    Ok(prepared)
}

async fn prepare<R: tauri::Runtime>(
    app: AppHandle<R>,
    source: String,
    source_type: &'static str,
) -> Result<PreparedExtension, String> {
    expire_prepared();
    let identity = current_identity(&app)?;
    let base_dir = super::extensions_base_dir(&app)?;
    let source = PathBuf::from(source);
    tokio::task::spawn_blocking(move || prepare_in(&base_dir, &source, source_type, identity))
        .await
        .map_err(|error| format!("extension preparation task failed: {error}"))?
}

#[tauri::command]
pub async fn prepare_extension_from_directory(
    app: AppHandle,
    source_dir: String,
) -> Result<PreparedExtension, String> {
    prepare(app, source_dir, "directory").await
}

#[tauri::command]
pub async fn prepare_extension_from_zip(
    app: AppHandle,
    archive_path: String,
) -> Result<PreparedExtension, String> {
    prepare(app, archive_path, "zip").await
}

fn take_prepared(token: &str, identity: &str) -> Result<PreparedPackage, String> {
    expire_prepared();
    let mut registry = prepared_registry();
    let Some(package) = registry.remove(token) else {
        return Err("prepared extension token is invalid, expired, or already used".to_string());
    };
    if package.identity_pubkey != identity {
        registry.insert(token.to_string(), package);
        return Err("prepared extension token belongs to another identity".to_string());
    }
    Ok(package)
}

#[tauri::command]
pub async fn cancel_prepared_extension(app: AppHandle, token: String) -> Result<(), String> {
    let identity = current_identity(&app)?;
    let package = take_prepared(&token, &identity)?;
    remove_staged(&package.staged_path);
    Ok(())
}

pub(crate) fn revalidation_current(
    state: &crate::AppState,
    grant_db: Option<&Path>,
    lease: &str,
    extension_id: &str,
    identity_at_entry: &str,
) -> bool {
    let Some((leased_extension, leased_identity, leased_digest)) =
        super::frame_host::lease_authority(lease)
    else {
        return false;
    };
    if leased_extension != extension_id {
        return false;
    }
    #[cfg(test)]
    if leased_identity.is_empty() && leased_digest.is_empty() {
        return true;
    }
    if leased_identity != identity_at_entry
        || super::dispatch::resolve_identity_pubkey(state).as_deref() != Some(identity_at_entry)
    {
        return false;
    }
    grant_db
        .and_then(|path| super::grants::open_grant_db(path).ok())
        .is_some_and(|conn| {
            super::grants::is_enabled(&conn, identity_at_entry, extension_id, &leased_digest)
        })
}

pub(crate) fn lease_authority_current_for_app<R: tauri::Runtime>(
    app: &AppHandle<R>,
    authority: &super::frame_authority::LeaseAuthority,
) -> bool {
    if current_identity(app).as_deref() != Ok(authority.identity_pubkey.as_str()) {
        return false;
    }
    super::dispatch::grant_db_path(app)
        .ok()
        .and_then(|path| super::grants::open_grant_db(&path).ok())
        .is_some_and(|conn| {
            super::grants::is_enabled(
                &conn,
                &authority.identity_pubkey,
                &authority.extension_id,
                &authority.package_digest,
            )
        })
}

pub(crate) fn enabled_context_for_app<R: tauri::Runtime>(
    app: &AppHandle<R>,
    id: &str,
) -> Result<(String, String, String, Vec<String>), String> {
    let identity = current_identity(app)?;
    let base = super::extensions_base_dir(app)?;
    let manifest = super::resolve_frame_manifest(&base, id)?;
    let digest = package_digest(&base.join(id))?;
    let db_path = super::dispatch::grant_db_path(app)?;
    let conn = super::grants::open_grant_db(&db_path)?;
    if !super::grants::is_enabled(&conn, &identity, &manifest.id, &digest) {
        return Err("extension is disabled for the current identity or package".to_string());
    }
    let selected = super::grants::list_selection(&conn, &identity, &manifest.id, &digest);
    Ok((identity, digest, manifest.entry, selected.egress))
}

fn decorate<R: tauri::Runtime>(
    app: &AppHandle<R>,
    mut installed: super::InstalledExtension,
) -> super::InstalledExtension {
    let identity = current_identity(app).ok();
    if let Some(identity) = identity {
        if let Ok(path) = super::dispatch::grant_db_path(app) {
            if let Ok(conn) = super::grants::open_grant_db(&path) {
                installed.enabled =
                    super::grants::is_enabled(&conn, &identity, &installed.id, &installed.digest);
                installed.granted = super::grants::list_selection(
                    &conn,
                    &identity,
                    &installed.id,
                    &installed.digest,
                );
            }
        }
    }
    installed
}

#[tauri::command]
pub async fn approve_prepared_extension(
    app: AppHandle,
    token: String,
    selected: GrantSelection,
) -> Result<super::InstalledExtension, String> {
    let identity = current_identity(&app)?;
    let package = take_prepared(&token, &identity)?;
    let base = super::extensions_base_dir(&app)?;
    let staged = package.staged_path.clone();
    let _fence = lifecycle_write_fence().await;
    let result = (|| {
        if package.expires_at <= now_unix() {
            return Err("prepared extension token expired".to_string());
        }
        let current_digest = package_digest(&staged)?;
        if current_digest != package.digest {
            return Err("prepared extension bytes changed after review".to_string());
        }
        let current_manifest = super::manifest::load_and_validate_manifest(&staged)?;
        if current_manifest != package.manifest || package.source_type.is_empty() {
            return Err("prepared extension manifest changed after review".to_string());
        }
        super::grants::validate_selection(&current_manifest, &selected)?;
        let db_path = super::dispatch::grant_db_path(&app)?;
        let mut conn = super::grants::open_grant_db(&db_path)?;

        // Fence activation before the first teardown. If replacement fails, the
        // predecessor conservatively remains disabled.
        super::grants::disable_all_for_extension(&conn, &current_manifest.id)?;
        super::frame_host::release_for_extension_id(&current_manifest.id);

        let destination = base.join(&current_manifest.id);
        let replacement = (|| {
            super::install::swap_into_place(&base, &staged, &destination)?;
            super::grants::replace_for_install(
                &mut conn,
                &identity,
                &current_manifest,
                &current_digest,
                &selected,
            )?;
            let installed = super::installed_from(current_manifest.clone(), &destination);
            Ok(decorate(&app, installed))
        })();

        // Exact final sweep closes the pre-swap/open race, including binds that
        // were paused before they could publish a lease.
        super::frame_host::release_for_extension_id(&current_manifest.id);
        replacement
    })();
    if staged.exists() {
        remove_staged(&staged);
    }
    result
}

fn load_installed_for_management<R: tauri::Runtime>(
    app: &AppHandle<R>,
    id: &str,
) -> Result<(PathBuf, ExtensionManifest, String), String> {
    if !super::manifest::is_valid_extension_id(id) {
        return Err("extension id is not valid".to_string());
    }
    let base = super::extensions_base_dir(app)?;
    let path = base.join(id);
    let manifest = super::resolve_frame_manifest(&base, id)?;
    let digest = package_digest(&path)?;
    Ok((path, manifest, digest))
}

#[tauri::command]
pub async fn set_extension_enabled(
    app: AppHandle,
    id: String,
    enabled: bool,
) -> Result<super::InstalledExtension, String> {
    let _fence = lifecycle_write_fence().await;
    let identity = current_identity(&app)?;
    let (path, manifest, digest) = load_installed_for_management(&app, &id)?;
    let db_path = super::dispatch::grant_db_path(&app)?;
    let conn = super::grants::open_grant_db(&db_path)?;
    super::grants::set_enabled(&conn, &identity, &manifest.id, &digest, enabled)?;
    if !enabled {
        super::frame_host::release_for_identity_extension(&identity, &manifest.id);
    }
    Ok(decorate(&app, super::installed_from(manifest, &path)))
}

#[tauri::command]
pub async fn update_extension_grants(
    app: AppHandle,
    id: String,
    selected: GrantSelection,
) -> Result<super::InstalledExtension, String> {
    let _fence = lifecycle_write_fence().await;
    let identity = current_identity(&app)?;
    let (path, manifest, digest) = load_installed_for_management(&app, &id)?;
    let db_path = super::dispatch::grant_db_path(&app)?;
    let mut conn = super::grants::open_grant_db(&db_path)?;
    super::grants::replace_for_identity(&mut conn, &identity, &manifest, &digest, &selected)?;
    super::frame_host::release_for_identity_extension(&identity, &manifest.id);
    Ok(decorate(&app, super::installed_from(manifest, &path)))
}

#[tauri::command]
pub async fn remove_extension(app: AppHandle, id: String) -> Result<RemoveExtensionResult, String> {
    let _fence = lifecycle_write_fence().await;
    if !super::manifest::is_valid_extension_id(&id) {
        return Err("extension could not be removed".to_string());
    }
    let base = super::extensions_base_dir(&app)?;
    let db_path = super::dispatch::grant_db_path(&app)?;
    let mut conn = super::grants::open_grant_db(&db_path)?;
    super::grants::disable_all_for_extension(&conn, &id)?;
    super::frame_host::release_for_extension_id(&id);
    let parked = park_extension_for_removal(&base, &id)?;
    if let Err(error) = super::grants::delete_all_for_extension(&mut conn, &id) {
        return Err(format!(
            "{error}. The package is disabled and preserved at {}",
            parked.display()
        ));
    }
    super::frame_host::release_for_extension_id(&id);
    match fs::remove_dir_all(&parked) {
        Ok(()) => Ok(RemoveExtensionResult {
            removed: true,
            recovery_path: None,
        }),
        Err(_) => Ok(RemoveExtensionResult {
            removed: true,
            recovery_path: Some(parked.to_string_lossy().into_owned()),
        }),
    }
}

fn park_extension_for_removal(base: &Path, id: &str) -> Result<PathBuf, String> {
    if !super::manifest::is_valid_extension_id(id) {
        return Err("extension could not be removed".to_string());
    }
    let source = base.join(id);
    let metadata =
        fs::symlink_metadata(&source).map_err(|_| "extension could not be removed".to_string())?;
    if !metadata.is_dir() || metadata.is_symlink() {
        return Err("extension could not be removed".to_string());
    }
    let parked = base.join(format!("{REMOVED_PREFIX}{}", uuid::Uuid::new_v4()));
    fs::rename(&source, &parked)
        .map_err(|error| format!("could not park the extension for removal: {error}"))?;
    Ok(parked)
}

pub(crate) fn decorate_list<R: tauri::Runtime>(
    app: &AppHandle<R>,
    installed: Vec<super::InstalledExtension>,
) -> Vec<super::InstalledExtension> {
    installed
        .into_iter()
        .map(|extension| decorate(app, extension))
        .collect()
}

#[cfg(test)]
#[path = "management_tests.rs"]
mod management_tests;

#[cfg(test)]
#[path = "management_live_tests.rs"]
mod management_live_tests;
