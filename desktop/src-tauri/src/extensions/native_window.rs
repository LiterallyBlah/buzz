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

#[derive(Debug, Clone)]
struct NativeRecord {
    key: SurfaceKey,
    label: String,
    lease: String,
    wrapper_url: String,
    data_directory: PathBuf,
    state: NativeWindowState,
    error: Option<String>,
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

fn cleanup_record<R: tauri::Runtime>(
    app: &AppHandle<R>,
    label: &str,
    close_window: bool,
    state: NativeWindowState,
    error: Option<String>,
) -> Option<NativeExtensionWindowStatus> {
    let mut record = remove_record(label)?;
    if close_window {
        if let Some(window) = app.get_webview_window(label) {
            if let Err(close_error) = window.close() {
                eprintln!("buzz-desktop: failed to close native extension window: {close_error}");
            }
        }
    }
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
    Some(status)
}

pub(crate) fn caller_authorized(label: &str, lease: &str, wrapper_url: &str) -> bool {
    registry()
        .by_label
        .get(label)
        .is_some_and(|record| record.lease == lease && record.wrapper_url == wrapper_url)
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

    let status = {
        let mut state = registry();
        let record = state
            .by_label
            .get_mut(label)
            .filter(|record| record.lease == lease)
            .ok_or_else(|| "native extension window is no longer opening".to_string())?;
        record.state = NativeWindowState::Open;
        record.error = None;
        status_for(record)
    };
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
        let still_opening = registry()
            .by_label
            .get(&watchdog_label)
            .is_some_and(|record| record.state == NativeWindowState::Opening);
        if still_opening {
            cleanup_record(
                &watchdog_app,
                &watchdog_label,
                true,
                NativeWindowState::Failed,
                Some("the secure extension window did not become ready".to_string()),
            );
        }
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

#[tauri::command]
pub fn close_native_extension_window(
    app: AppHandle,
    id: String,
) -> Result<NativeExtensionWindowStatus, String> {
    if !super::manifest::is_valid_extension_id(&id) {
        return Err("extension id is not valid".to_string());
    }
    let identity = super::management::current_identity_for_app(&app)?;
    let labels: Vec<String> = registry()
        .by_label
        .values()
        .filter(|record| record.key.identity_pubkey == identity && record.key.extension_id == id)
        .map(|record| record.label.clone())
        .collect();
    for label in labels {
        cleanup_record(&app, &label, true, NativeWindowState::Closed, None);
    }
    Ok(NativeExtensionWindowStatus::closed(&id))
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
mod tests {
    use super::*;

    fn authority(identity: &str, digest: &str, generation: u64) -> LeaseAuthority {
        LeaseAuthority {
            extension_id: "equation-explorer".to_string(),
            identity_pubkey: identity.to_string(),
            package_digest: digest.to_string(),
            grant_generation: generation,
        }
    }

    #[test]
    fn accepted_all_frame_script_bytes_and_constructor_list_are_exact() {
        assert_eq!(
            hex::encode(Sha256::digest(WEBRTC_DISABLE_SCRIPT.as_bytes())),
            ACCEPTED_SCRIPT_SHA256
        );
        for name in [
            "RTCPeerConnection",
            "webkitRTCPeerConnection",
            "mozRTCPeerConnection",
            "RTCDataChannel",
            "webkitRTCDataChannel",
            "mozRTCDataChannel",
        ] {
            assert_eq!(
                WEBRTC_DISABLE_SCRIPT
                    .matches(&format!("\"{name}\""))
                    .count(),
                1,
                "{name}"
            );
        }
        assert!(WEBRTC_DISABLE_SCRIPT.contains("writable: false"));
        assert!(WEBRTC_DISABLE_SCRIPT.contains("configurable: false"));
    }

    #[test]
    fn plan_binds_unique_udf_to_identity_digest_generation_and_label() {
        let root = Path::new("C:/Buzz/private-webview2");
        let digest = "ab".repeat(32);
        let first = plan_native_window(
            root,
            authority("identity-a", &digest, 7),
            "extension-secure-first".to_string(),
            "http://127.0.0.1:3001/frame/context/digest/equation-explorer".to_string(),
        )
        .unwrap();
        let second = plan_native_window(
            root,
            authority("identity-a", &digest, 7),
            "extension-secure-second".to_string(),
            "http://127.0.0.1:3001/frame/context/digest/equation-explorer".to_string(),
        )
        .unwrap();
        assert_ne!(first.data_directory, second.data_directory);
        assert!(first.data_directory.ends_with(
            Path::new("equation-explorer")
                .join(&digest)
                .join("7")
                .join("extension-secure-first")
        ));
        assert_ne!(
            first.data_directory,
            plan_native_window(
                root,
                authority("identity-b", &digest, 7),
                "extension-secure-first".to_string(),
                first.wrapper_url.clone(),
            )
            .unwrap()
            .data_directory
        );
    }

    #[test]
    fn production_plan_contains_no_measurement_browser_arguments() {
        let source = include_str!("native_window.rs");
        let production = source.split("#[cfg(test)]").next().unwrap();
        let ignored_cert = ["ignore", "certificate", "errors"].join("-");
        let proxy_flag = ["disable", "non", "proxied", "udp"].join("_");
        let browser_args = ["additional", "browser", "args"].join("_");
        assert!(!production.contains(&ignored_cert));
        assert!(!production.contains(&proxy_flag));
        assert!(!production.contains(&browser_args));
        assert_eq!(
            production
                .matches("initialization_script_for_all_frames")
                .count(),
            1
        );
        assert!(production.contains("data_directory(plan.data_directory.clone())"));
        assert!(
            !include_str!("../huddle/window.rs").contains("initialization_script_for_all_frames")
        );
    }

    #[test]
    fn linux_surface_mode_retains_the_iframe_path() {
        #[cfg(not(target_os = "windows"))]
        assert_eq!(
            ExtensionSurfaceMode::current(),
            ExtensionSurfaceMode::LinuxIframe
        );
    }

    #[test]
    fn native_wrapper_originates_one_channel_and_rejects_frame_ports() {
        let source = include_str!("native_wrapper.js");
        assert_eq!(source.matches("new MessageChannel()").count(), 1);
        assert!(!source.contains("event.ports"));
        assert!(source.contains("event.source !== frame.contentWindow"));
        assert!(source.contains("plugin:extension-bridge|invoke"));
        assert!(source.contains("plugin:extension-bridge|stream_control"));
        assert!(source.contains("plugin:extension-bridge|native_ready"));
    }

    #[test]
    fn wrapper_policy_is_platform_specific_and_host_derived() {
        let origin = "http://127.0.0.1:43123";
        let linux = super::super::frame_host::wrapper_content_security_policy_for_mode(
            origin,
            super::super::frame_authority::WrapperMode::LinuxIframe,
        );
        let windows = super::super::frame_host::wrapper_content_security_policy_for_mode(
            origin,
            super::super::frame_authority::WrapperMode::WindowsTopLevel,
        );
        assert!(!linux.contains("frame-ancestors"));
        assert!(windows.contains("frame-ancestors 'none'"));
        assert!(linux.contains(&format!("frame-src {origin}")));
        assert!(windows.contains(&format!("frame-src {origin}")));
    }

    #[test]
    fn terminal_pass_fixture_pins_initial_srcdoc_controls_and_server_snapshot() {
        let result_bytes =
            include_bytes!("../../tests/fixtures/extensions/webview2-realm-disable-results.json");
        assert_eq!(
            hex::encode(Sha256::digest(result_bytes)),
            "41238014f32883efcae15b33b8c886d262f4cd560705da0648b5094b5f6d96f3"
        );
        let result: serde_json::Value = serde_json::from_slice(result_bytes).unwrap();
        assert_eq!(result["overall"], "PASS");
        assert_eq!(result["injected_script_sha256"], ACCEPTED_SCRIPT_SHA256);
        assert_eq!(result["rows"].as_array().unwrap().len(), 1);
        let evidence = &result["rows"][0]["evidence"];
        for field in [
            "matrix_complete",
            "snapshots_valid",
            "candidate_off_reports_live",
            "huddle_report_live",
            "loopback_controls_live",
            "offhost_controls_live",
            "protected_reports_blocked",
            "protected_sinks_zero",
        ] {
            assert_eq!(evidence[field], true, "{field}");
        }
        for lane in ["protected-initial", "protected-srcdoc"] {
            let report = &evidence["reports"][lane];
            for constructor in [
                "RTCPeerConnection",
                "webkitRTCPeerConnection",
                "mozRTCPeerConnection",
                "RTCDataChannel",
                "webkitRTCDataChannel",
                "mozRTCDataChannel",
            ] {
                assert_eq!(report["constructorTypes"][constructor], "undefined");
            }
        }
        let snapshot_bytes =
            include_bytes!("../../tests/fixtures/extensions/offhost-snapshot-at-intake.json");
        assert_eq!(
            hex::encode(Sha256::digest(snapshot_bytes)),
            "ae0e926765e9e34ce778269dc36581ab55bf44a6c978a4d40c7ae91562f8ff44"
        );
        let snapshot: serde_json::Value = serde_json::from_slice(snapshot_bytes).unwrap();
        assert_eq!(evidence["offhost_snapshot"], snapshot);
    }

    #[tokio::test]
    async fn lifecycle_close_is_exact_idempotent_and_reopen_uses_fresh_authority() {
        let _guard = super::super::frame_host::lifecycle_guard().await;
        let app = tauri::test::mock_app();
        let identity = "11".repeat(32);
        let digest = "ab".repeat(32);
        let extension_id = "equation-explorer";
        let first_label = "extension-secure-lifecycle-first".to_string();
        let first_lease = "44444444-4444-4444-8444-444444444444";
        super::super::frame_host::insert_authorized_lease_with_generation_for_test(
            first_lease,
            extension_id,
            &identity,
            &digest,
            9,
        );
        let first_key = SurfaceKey {
            identity_pubkey: identity.clone(),
            extension_id: extension_id.to_string(),
            package_digest: digest.clone(),
            grant_generation: 9,
        };
        {
            let mut state = registry();
            state
                .by_surface
                .insert(first_key.clone(), first_label.clone());
            state.by_label.insert(
                first_label.clone(),
                NativeRecord {
                    key: first_key,
                    label: first_label.clone(),
                    lease: first_lease.to_string(),
                    wrapper_url: "http://127.0.0.1:41000/frame/first".to_string(),
                    data_directory: std::env::temp_dir().join(&first_label),
                    state: NativeWindowState::Open,
                    error: None,
                },
            );
        }
        assert_eq!(
            close_for_identity_extension(app.handle(), &identity, extension_id),
            1
        );
        assert!(registry().by_label.is_empty());
        assert!(super::super::frame_host::lease_authority_snapshot(first_lease).is_none());
        assert_eq!(
            close_for_identity_extension(app.handle(), &identity, extension_id),
            0
        );

        let second_label = "extension-secure-lifecycle-second".to_string();
        assert_ne!(first_label, second_label);
        let second_lease = "55555555-5555-4555-8555-555555555555";
        super::super::frame_host::insert_authorized_lease_with_generation_for_test(
            second_lease,
            extension_id,
            &identity,
            &digest,
            10,
        );
        let second_key = SurfaceKey {
            identity_pubkey: identity.clone(),
            extension_id: extension_id.to_string(),
            package_digest: digest,
            grant_generation: 10,
        };
        {
            let mut state = registry();
            state
                .by_surface
                .insert(second_key.clone(), second_label.clone());
            state.by_label.insert(
                second_label.clone(),
                NativeRecord {
                    key: second_key,
                    label: second_label.clone(),
                    lease: second_lease.to_string(),
                    wrapper_url: "http://127.0.0.1:42000/frame/second".to_string(),
                    data_directory: std::env::temp_dir().join(&second_label),
                    state: NativeWindowState::Opening,
                    error: None,
                },
            );
        }
        assert_eq!(close_for_extension(app.handle(), extension_id), 1);
        assert!(registry().by_label.is_empty());
        assert!(super::super::frame_host::lease_authority_snapshot(second_lease).is_none());
    }

    #[test]
    fn native_capability_is_windows_remote_wrapper_only() {
        let capability: serde_json::Value = serde_json::from_str(include_str!(
            "../../capabilities/extension-native-bridge.json"
        ))
        .unwrap();
        assert_eq!(capability["local"], false);
        assert_eq!(
            capability["windows"],
            serde_json::json!(["extension-secure-*"])
        );
        assert_eq!(
            capability["remote"]["urls"],
            serde_json::json!(["http://127.0.0.1:*/frame/*"])
        );
        assert_eq!(capability["platforms"], serde_json::json!(["windows"]));
        let permissions = capability["permissions"].as_array().unwrap();
        assert_eq!(permissions.len(), 5);
        assert!(!capability.to_string().contains("/ext/*"));
        assert!(!capability.to_string().contains("*://*"));
    }
}
