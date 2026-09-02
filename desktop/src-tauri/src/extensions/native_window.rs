//! Windows-only dedicated extension WebView2 window lifecycle.
//!
//! The public planning functions remain platform-neutral so Linux CI can prove
//! the exact builder inputs. Only the final `WebviewWindowBuilder` call is
//! compiled for Windows; Linux keeps the accepted iframe path.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard, OnceLock};

use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};
use tauri::{AppHandle, Emitter, Manager};

use super::frame_authority::LeaseAuthority;

pub(crate) const NATIVE_WINDOW_LABEL_PREFIX: &str = "extension-secure-";
pub(crate) const NATIVE_STATUS_EVENT: &str = "extension-native-window-status";
pub(crate) const NATIVE_READY_TIMEOUT_SECONDS: u64 = 12;
pub(crate) const ACCEPTED_SCRIPT_SHA256: &str =
    "c4328966c35974dc87a7a43a55b470633819d38956289e33601072e47c319324";

/// Exact bytes accepted by the terminal owner WebView2 measurement.
pub(crate) const WEBRTC_DISABLE_SCRIPT: &str = r#"(() => {
  "use strict";
  const names = [
    "RTCPeerConnection", "webkitRTCPeerConnection", "mozRTCPeerConnection",
    "RTCDataChannel", "webkitRTCDataChannel", "mozRTCDataChannel"
  ];
  for (const name of names) {
    try {
      Object.defineProperty(globalThis, name, {
        value: undefined,
        writable: false,
        enumerable: false,
        configurable: false
      });
    } catch (_) {
      try { globalThis[name] = undefined; } catch (_) {}
    }
  }
})();"#;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ExtensionSurfaceMode {
    LinuxIframe,
    WindowsNativeWindow,
}

impl ExtensionSurfaceMode {
    pub(crate) const fn current() -> Self {
        if cfg!(target_os = "windows") {
            Self::WindowsNativeWindow
        } else {
            Self::LinuxIframe
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum NativeWindowState {
    Opening,
    Open,
    Closed,
    Failed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NativeExtensionWindowStatus {
    pub extension_id: String,
    pub state: NativeWindowState,
    pub label: Option<String>,
    pub error: Option<String>,
}

impl NativeExtensionWindowStatus {
    fn closed(extension_id: &str) -> Self {
        Self {
            extension_id: extension_id.to_string(),
            state: NativeWindowState::Closed,
            label: None,
            error: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct NativeWindowPlan {
    pub(crate) label: String,
    pub(crate) wrapper_url: String,
    pub(crate) data_directory: PathBuf,
    pub(crate) initialization_script: &'static str,
    pub(crate) authority: LeaseAuthority,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct SurfaceKey {
    identity_pubkey: String,
    extension_id: String,
    package_digest: String,
    grant_generation: u64,
}

impl From<&LeaseAuthority> for SurfaceKey {
    fn from(authority: &LeaseAuthority) -> Self {
        Self {
            identity_pubkey: authority.identity_pubkey.clone(),
            extension_id: authority.extension_id.clone(),
            package_digest: authority.package_digest.clone(),
            grant_generation: authority.grant_generation,
        }
    }
}

#[derive(Clone)]
struct NativeRecord {
    key: SurfaceKey,
    label: String,
    lease: String,
    wrapper_url: String,
    data_directory: PathBuf,
    state: NativeWindowState,
    error: Option<String>,
    stream_sink: Option<super::query::StreamSink>,
}

#[derive(Default)]
struct NativeRegistry {
    by_label: BTreeMap<String, NativeRecord>,
    by_surface: BTreeMap<SurfaceKey, String>,
}

static NATIVE_WINDOWS: OnceLock<Mutex<NativeRegistry>> = OnceLock::new();
static NATIVE_OPEN_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

fn registry() -> MutexGuard<'static, NativeRegistry> {
    NATIVE_WINDOWS
        .get_or_init(|| Mutex::new(NativeRegistry::default()))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn identity_directory(identity_pubkey: &str) -> String {
    hex::encode(Sha256::digest(identity_pubkey.as_bytes()))
}

pub(crate) fn plan_native_window(
    udf_root: &Path,
    authority: LeaseAuthority,
    label: String,
    wrapper_url: String,
) -> Result<NativeWindowPlan, String> {
    if hex::encode(Sha256::digest(WEBRTC_DISABLE_SCRIPT.as_bytes())) != ACCEPTED_SCRIPT_SHA256 {
        return Err("production WebRTC isolation script bytes drifted".to_string());
    }
    if !label.starts_with(NATIVE_WINDOW_LABEL_PREFIX)
        || authority.identity_pubkey.is_empty()
        || authority.package_digest.len() != 64
        || authority.grant_generation == 0
        || !super::manifest::is_valid_extension_id(&authority.extension_id)
    {
        return Err("native extension window authority is incomplete".to_string());
    }
    let data_directory = udf_root
        .join(identity_directory(&authority.identity_pubkey))
        .join(&authority.extension_id)
        .join(&authority.package_digest)
        .join(authority.grant_generation.to_string())
        .join(&label);
    Ok(NativeWindowPlan {
        label,
        wrapper_url,
        data_directory,
        initialization_script: WEBRTC_DISABLE_SCRIPT,
        authority,
    })
}

fn status_for(record: &NativeRecord) -> NativeExtensionWindowStatus {
    NativeExtensionWindowStatus {
        extension_id: record.key.extension_id.clone(),
        state: record.state,
        label: Some(record.label.clone()),
        error: record.error.clone(),
    }
}

fn emit_status<R: tauri::Runtime>(app: &AppHandle<R>, status: &NativeExtensionWindowStatus) {
    if let Err(error) = app.emit_to("main", NATIVE_STATUS_EVENT, status) {
        eprintln!("buzz-desktop: failed to emit native extension status: {error}");
    }
}

fn remove_record(label: &str) -> Option<NativeRecord> {
    let mut state = registry();
    let record = state.by_label.remove(label)?;
    state.by_surface.remove(&record.key);
    Some(record)
}

fn remove_record_if_state(label: &str, expected: NativeWindowState) -> Option<NativeRecord> {
    let mut state = registry();
    if state.by_label.get(label)?.state != expected {
        return None;
    }
    let record = state.by_label.remove(label)?;
    state.by_surface.remove(&record.key);
    Some(record)
}

fn finish_cleanup<R: tauri::Runtime>(
    app: &AppHandle<R>,
    mut record: NativeRecord,
    close_window: bool,
    state: NativeWindowState,
    error: Option<String>,
) -> NativeExtensionWindowStatus {
    if close_window {
        if let Some(window) = app.get_webview_window(&record.label) {
            if let Err(close_error) = window.close() {
                eprintln!("buzz-desktop: failed to close native extension window: {close_error}");
            }
        }
    }
    // The native stream sink disappears from the registry before lease teardown,
    // so any already-cloned sender rechecks and fails without delivering bytes.
    record.stream_sink.take();
    super::frame_host::release(&record.lease);
    record.state = state;
    record.error = error;
    let status = status_for(&record);
    emit_status(app, &status);

    let data_directory = record.data_directory;
    tauri::async_runtime::spawn_blocking(move || {
        if let Err(remove_error) = std::fs::remove_dir_all(&data_directory) {
            if remove_error.kind() != std::io::ErrorKind::NotFound {
                eprintln!(
                    "buzz-desktop: could not remove retired extension WebView2 data directory: {remove_error}"
                );
            }
        }
    });
    status
}

fn cleanup_record<R: tauri::Runtime>(
    app: &AppHandle<R>,
    label: &str,
    close_window: bool,
    state: NativeWindowState,
    error: Option<String>,
) -> Option<NativeExtensionWindowStatus> {
    remove_record(label).map(|record| finish_cleanup(app, record, close_window, state, error))
}

fn cleanup_record_if_state<R: tauri::Runtime>(
    app: &AppHandle<R>,
    label: &str,
    expected: NativeWindowState,
    close_window: bool,
    state: NativeWindowState,
    error: Option<String>,
) -> Option<NativeExtensionWindowStatus> {
    remove_record_if_state(label, expected)
        .map(|record| finish_cleanup(app, record, close_window, state, error))
}

pub(crate) fn caller_authorized(label: &str, lease: &str, wrapper_url: &str) -> bool {
    registry()
        .by_label
        .get(label)
        .is_some_and(|record| record.lease == lease && record.wrapper_url == wrapper_url)
}

fn stream_delivery_authorized(label: &str, lease: &str, wrapper_url: &str) -> bool {
    registry().by_label.get(label).is_some_and(|record| {
        record.lease == lease
            && record.wrapper_url == wrapper_url
            && record.stream_sink.is_some()
            && matches!(
                record.state,
                NativeWindowState::Opening | NativeWindowState::Open
            )
    })
}

fn bind_stream_sink(
    label: &str,
    lease: &str,
    wrapper_url: &str,
    authority: &LeaseAuthority,
    sink: super::query::StreamSink,
) -> Result<(), String> {
    if !caller_authorized(label, lease, wrapper_url) {
        return Err("native extension stream caller is not authorised".to_string());
    }
    let mut state = registry();
    let record = state
        .by_label
        .get_mut(label)
        .filter(|record| {
            record.lease == lease
                && record.wrapper_url == wrapper_url
                && record.key == SurfaceKey::from(authority)
                && record.state == NativeWindowState::Opening
                && record.stream_sink.is_none()
        })
        .ok_or_else(|| "native extension stream is not bindable".to_string())?;
    record.stream_sink = Some(sink);
    Ok(())
}

fn guarded_stream_sink(
    label: &str,
    lease: &str,
    wrapper_url: &str,
    deliver: impl Fn(&super::query::StreamBatch) -> Result<(), ()> + Send + Sync + 'static,
) -> super::query::StreamSink {
    let sink_label = label.to_string();
    let sink_lease = lease.to_string();
    let sink_url = wrapper_url.to_string();
    std::sync::Arc::new(move |batch| {
        if batch.generation != sink_lease
            || !stream_delivery_authorized(&sink_label, &sink_lease, &sink_url)
        {
            return Err(());
        }
        deliver(batch)
    })
}

pub(crate) fn bind_stream_channel<R: tauri::Runtime>(
    app: &AppHandle<R>,
    label: &str,
    lease: &str,
    wrapper_url: &str,
    channel: tauri::ipc::Channel<super::query::StreamBatch>,
) -> Result<(), String> {
    let authority = super::frame_host::lease_authority_for_caller(lease, label)
        .ok_or_else(|| "native extension stream caller is not authorised".to_string())?;
    if !super::management::lease_authority_current_for_app(app, &authority) {
        return Err("native extension authority changed while binding its stream".to_string());
    }
    let sink = guarded_stream_sink(label, lease, wrapper_url, move |batch| {
        channel.send(batch.clone()).map_err(|_| ())
    });
    bind_stream_sink(label, lease, wrapper_url, &authority, sink)
}

pub(crate) fn stream_sink_for_lease(lease: &str) -> Option<super::query::StreamSink> {
    registry()
        .by_label
        .values()
        .find(|record| {
            record.lease == lease
                && record.stream_sink.is_some()
                && matches!(
                    record.state,
                    NativeWindowState::Opening | NativeWindowState::Open
                )
        })
        .and_then(|record| record.stream_sink.clone())
}

fn transition_to_open(label: &str, lease: &str) -> Result<NativeExtensionWindowStatus, String> {
    let mut state = registry();
    let record = state
        .by_label
        .get_mut(label)
        .filter(|record| {
            record.lease == lease
                && record.state == NativeWindowState::Opening
                && record.stream_sink.is_some()
        })
        .ok_or_else(|| "native extension window is no longer opening".to_string())?;
    record.state = NativeWindowState::Open;
    record.error = None;
    Ok(status_for(record))
}

pub(crate) fn mark_ready<R: tauri::Runtime>(
    app: &AppHandle<R>,
    label: &str,
    lease: &str,
) -> Result<NativeExtensionWindowStatus, String> {
    let authority = super::frame_host::lease_authority_for_caller(lease, label)
        .ok_or_else(|| "native extension caller is not authorised".to_string())?;
    if !super::management::lease_authority_current_for_app(app, &authority) {
        cleanup_record(
            app,
            label,
            true,
            NativeWindowState::Failed,
            Some("extension authority changed while opening".to_string()),
        );
        return Err("extension authority changed while opening".to_string());
    }

    let status = transition_to_open(label, lease)?;
    emit_status(app, &status);
    Ok(status)
}

#[tauri::command]
pub fn extension_surface_mode() -> ExtensionSurfaceMode {
    ExtensionSurfaceMode::current()
}

#[tauri::command]
pub fn native_extension_window_status(
    app: AppHandle,
    id: String,
) -> Result<NativeExtensionWindowStatus, String> {
    if !super::manifest::is_valid_extension_id(&id) {
        return Err("extension id is not valid".to_string());
    }
    let identity = super::management::current_identity_for_app(&app)?;
    let state = registry();
    Ok(state
        .by_label
        .values()
        .find(|record| record.key.identity_pubkey == identity && record.key.extension_id == id)
        .map(status_for)
        .unwrap_or_else(|| NativeExtensionWindowStatus::closed(&id)))
}

#[tauri::command]
pub async fn open_native_extension_window(
    app: AppHandle,
    id: String,
) -> Result<NativeExtensionWindowStatus, String> {
    if ExtensionSurfaceMode::current() != ExtensionSurfaceMode::WindowsNativeWindow {
        return Err("native extension windows are available only on Windows".to_string());
    }
    open_native_extension_window_for(app, id).await
}

async fn open_native_extension_window_for<R: tauri::Runtime>(
    app: AppHandle<R>,
    id: String,
) -> Result<NativeExtensionWindowStatus, String> {
    let _open_guard = NATIVE_OPEN_LOCK.lock().await;
    let _lifecycle = super::management::lifecycle_read_fence().await;
    let base_dir = super::extensions_base_dir(&app)?;
    let manifest = {
        let base_dir = base_dir.clone();
        let id = id.clone();
        tokio::task::spawn_blocking(move || super::resolve_frame_manifest(&base_dir, &id))
            .await
            .map_err(|error| format!("extension window task failed: {error}"))??
    };
    let (identity, digest, generation, entry, egress) =
        super::management::enabled_context_for_app(&app, &manifest.id)?;
    if entry != manifest.entry {
        return Err("installed extension entry changed while opening".to_string());
    }
    let authority = LeaseAuthority {
        extension_id: manifest.id.clone(),
        identity_pubkey: identity,
        package_digest: digest.clone(),
        grant_generation: generation,
    };
    let key = SurfaceKey::from(&authority);

    if let Some(label) = registry().by_surface.get(&key).cloned() {
        if let Some(window) = app.get_webview_window(&label) {
            window.show().map_err(|error| error.to_string())?;
            window.set_focus().map_err(|error| error.to_string())?;
            return registry()
                .by_label
                .get(&label)
                .map(status_for)
                .ok_or_else(|| "native extension registry changed while focusing".to_string());
        }
        cleanup_record(&app, &label, false, NativeWindowState::Closed, None);
    }

    let label = format!("{NATIVE_WINDOW_LABEL_PREFIX}{}", uuid::Uuid::new_v4());
    let claim = super::frame_host::acquire_authorized_with_generation_and_label(
        base_dir,
        &manifest.id,
        &authority.identity_pubkey,
        &digest,
        generation,
        &entry,
        egress,
        &label,
        super::frame_authority::WrapperMode::WindowsTopLevel,
    )
    .await?;
    let wrapper_origin = super::frame_host::origin_for_port(claim.wrapper_port);
    let wrapper_url = super::frame_host::wrapper_url(
        &wrapper_origin,
        &claim.static_context,
        &digest,
        &manifest.id,
    );
    let prepared_plan = (|| {
        let udf_root = app
            .path()
            .app_data_dir()
            .map_err(|error| format!("failed to resolve app data dir: {error}"))?
            .join("extension-webview2");
        let plan = plan_native_window(&udf_root, authority, label.clone(), wrapper_url)?;
        std::fs::create_dir_all(&plan.data_directory).map_err(|error| {
            format!("could not create extension WebView2 data directory: {error}")
        })?;
        Ok::<NativeWindowPlan, String>(plan)
    })();
    let plan = match prepared_plan {
        Ok(plan) => plan,
        Err(error) => {
            super::frame_host::release(&claim.lease);
            return Err(error);
        }
    };

    {
        let mut state = registry();
        if state.by_surface.contains_key(&key) || state.by_label.contains_key(&label) {
            super::frame_host::release(&claim.lease);
            let _ = std::fs::remove_dir_all(&plan.data_directory);
            return Err("native extension window already exists".to_string());
        }
        state.by_surface.insert(key.clone(), label.clone());
        state.by_label.insert(
            label.clone(),
            NativeRecord {
                key,
                label: label.clone(),
                lease: claim.lease.clone(),
                wrapper_url: plan.wrapper_url.clone(),
                data_directory: plan.data_directory.clone(),
                state: NativeWindowState::Opening,
                error: None,
                stream_sink: None,
            },
        );
    }

    if let Err(error) = build_native_window(&app, &plan) {
        cleanup_record(
            &app,
            &label,
            true,
            NativeWindowState::Failed,
            Some(error.clone()),
        );
        return Err(error);
    }

    let watchdog_app = app.clone();
    let watchdog_label = label.clone();
    tauri::async_runtime::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_secs(NATIVE_READY_TIMEOUT_SECONDS)).await;
        cleanup_record_if_state(
            &watchdog_app,
            &watchdog_label,
            NativeWindowState::Opening,
            true,
            NativeWindowState::Failed,
            Some("the secure extension window did not become ready".to_string()),
        );
    });

    let status = registry()
        .by_label
        .get(&label)
        .map(status_for)
        .ok_or_else(|| "native extension window disappeared during creation".to_string())?;
    emit_status(&app, &status);
    Ok(status)
}

#[cfg(target_os = "windows")]
fn build_native_window<R: tauri::Runtime>(
    app: &AppHandle<R>,
    plan: &NativeWindowPlan,
) -> Result<(), String> {
    use tauri::{WebviewUrl, WebviewWindowBuilder};

    let url = plan
        .wrapper_url
        .parse()
        .map_err(|error| format!("invalid extension wrapper URL: {error}"))?;
    WebviewWindowBuilder::new(app, plan.label.clone(), WebviewUrl::External(url))
        .title(format!("Buzz Extension — {}", plan.authority.extension_id))
        .data_directory(plan.data_directory.clone())
        .initialization_script_for_all_frames(plan.initialization_script)
        .inner_size(960.0, 720.0)
        .min_inner_size(720.0, 520.0)
        .build()
        .map_err(|error| format!("could not create the secure extension window: {error}"))?;
    Ok(())
}

#[cfg(not(target_os = "windows"))]
fn build_native_window<R: tauri::Runtime>(
    _app: &AppHandle<R>,
    _plan: &NativeWindowPlan,
) -> Result<(), String> {
    Err("native extension windows are available only on Windows".to_string())
}

async fn close_native_extension_window_serialized<R, F>(
    app: &AppHandle<R>,
    id: &str,
    resolve_identity: F,
) -> Result<NativeExtensionWindowStatus, String>
where
    R: tauri::Runtime,
    F: FnOnce() -> Result<String, String>,
{
    let _open_guard = NATIVE_OPEN_LOCK.lock().await;
    let identity = resolve_identity()?;
    close_for_identity_extension(app, &identity, id);
    Ok(NativeExtensionWindowStatus::closed(id))
}

#[tauri::command]
pub async fn close_native_extension_window(
    app: AppHandle,
    id: String,
) -> Result<NativeExtensionWindowStatus, String> {
    if !super::manifest::is_valid_extension_id(&id) {
        return Err("extension id is not valid".to_string());
    }
    close_native_extension_window_serialized(&app, &id, || {
        super::management::current_identity_for_app(&app)
    })
    .await
}

pub(crate) fn close_for_identity_extension<R: tauri::Runtime>(
    app: &AppHandle<R>,
    identity: &str,
    extension_id: &str,
) -> usize {
    let labels: Vec<String> = registry()
        .by_label
        .values()
        .filter(|record| {
            record.key.identity_pubkey == identity && record.key.extension_id == extension_id
        })
        .map(|record| record.label.clone())
        .collect();
    for label in &labels {
        cleanup_record(app, label, true, NativeWindowState::Closed, None);
    }
    labels.len()
}

pub(crate) fn close_for_extension<R: tauri::Runtime>(
    app: &AppHandle<R>,
    extension_id: &str,
) -> usize {
    let labels: Vec<String> = registry()
        .by_label
        .values()
        .filter(|record| record.key.extension_id == extension_id)
        .map(|record| record.label.clone())
        .collect();
    for label in &labels {
        cleanup_record(app, label, true, NativeWindowState::Closed, None);
    }
    labels.len()
}

pub(crate) fn close_all<R: tauri::Runtime>(app: &AppHandle<R>) -> usize {
    let labels: Vec<String> = registry().by_label.keys().cloned().collect();
    for label in &labels {
        cleanup_record(app, label, true, NativeWindowState::Closed, None);
    }
    labels.len()
}

pub(crate) fn handle_window_closed<R: tauri::Runtime>(app: &AppHandle<R>, label: &str) -> bool {
    cleanup_record(app, label, false, NativeWindowState::Closed, None).is_some()
}

#[cfg(test)]
#[path = "native_window_tests.rs"]
mod tests;
