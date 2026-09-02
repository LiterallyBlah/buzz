//! The extension bridge, as a **plugin** command.
//!
//! # Why a plugin and not an app command
//!
//! Tauri checks the ACL when the command is a plugin command, when the app
//! declares an ACL manifest, or when the origin is remote
//! (`tauri-2.11.5/src/webview/mod.rs:1823`). Buzz has **no `__app-acl__` manifest**
//! — the generated `acl-manifests.json` reports `has_app: false` — so an *app*
//! command cannot be granted narrowly to one origin. Measured: adding a
//! capability that grants an app command fails outright with
//! `UnknownManifest { key: "app manifest" }` (the key Tauri 2.11.5 looks for is
//! `__app-acl__`).
//!
//! A **plugin** command is always ACL-checked and resolves against the manifest
//! `tauri_build::InlinedPlugin` generates, so it can be granted to exactly one
//! origin. That is what keeps in-app tabs available without migrating Buzz's
//! ~350 app commands into an AppManifest.
//!
//! # Identity has a single producer
//!
//! The command accepts the opaque **lease** the host minted and handed to the
//! trusted wrapper, and resolves the extension id from host state. It never
//! accepts an extension id, package path or permission set as a parameter.
//!
//! The distinction matters: the wrapper is trusted to be *our code*, not
//! trusted to be *uncompromised*. If a bug in it ever lets the extension
//! influence a parameter, parameter-derived identity becomes attacker
//! controlled — whereas a token the host minted cannot be guessed, and the map
//! that interprets it lives in Rust.
//!
//! # What this is, and what still is not
//!
//! The §2 mediator contracts are **present**: request/response schemas and
//! id correlation in `bridgeDispatch`, the method allowlist in
//! [`super::dispatch::route`], byte and shape limits in `bridgeFrame`, and
//! per-port admission, replay-safe ids and teardown settlement in
//! `bridgeRegistry`.
//!
//! Still absent, and honestly so: **subscription ownership and backpressure**
//! (§5, a later increment), and **cancellation** — closing a port does not
//! recall work already running in Rust, which is why `publish.event` rests on
//! content-addressed idempotency rather than on cancelling anything.

use tauri::plugin::TauriPlugin;
use tauri::Runtime;

use super::frame_host;

/// The identity the host resolved for a caller, from its lease alone.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct BridgeIdentity {
    /// The extension the lease was issued for. Host-derived, never echoed from
    /// a caller parameter.
    pub extension_id: String,
}

/// Resolve who is calling, from the host-minted lease.
///
/// One entry point among several: the §2 contracts now dispatch through
/// [`super::dispatch`], which serves `identity.getPublicKey` and
/// `publish.event` over the framed port. This command remains the plumbing
/// proof — plugin command, ACL-gated, identity from host state. Subscriptions
/// and request cancellation are what is still held.
fn caller_authorized_parts(label: &str, url: Option<&str>, lease: &str) -> bool {
    frame_host::lease_authority_for_caller(lease, label).is_some()
        && (!label.starts_with(super::native_window::NATIVE_WINDOW_LABEL_PREFIX)
            || url.is_some_and(|url| super::native_window::caller_authorized(label, lease, url)))
}

fn caller_authorized<R: Runtime>(webview: &tauri::Webview<R>, lease: &str) -> bool {
    let url = webview.url().ok();
    caller_authorized_parts(webview.label(), url.as_ref().map(|url| url.as_str()), lease)
}

async fn resolve_identity_for_label(label: &str, lease: String) -> Result<BridgeIdentity, String> {
    match frame_host::lease_authority_for_caller(&lease, label)
        .is_some()
        .then(|| frame_host::extension_for_lease(&lease))
        .flatten()
    {
        Some(extension_id) => Ok(BridgeIdentity { extension_id }),
        // One message for unknown and released alike: distinguishing them would
        // let a caller probe which leases have existed.
        None => Err("no live extension frame for this lease".to_string()),
    }
}

#[tauri::command]
pub(crate) async fn resolve_identity<R: Runtime>(
    webview: tauri::Webview<R>,
    lease: String,
) -> Result<BridgeIdentity, String> {
    resolve_identity_for_label(webview.label(), lease).await
}

/// The §2 request entry point: one frame in, one reply out.
///
/// `params` is threaded to the handler that needs it and **never** to
/// attribution. §2's "attribute by the held handle, never by the payload" stays
/// true by signature rather than by discipline:
/// [`super::dispatch::route`] — which decides *who the caller is* — takes no
/// `params` argument at all, so no payload can reach that decision. The
/// template travels separately, to the signer alone.
///
/// The frontend correlates: it holds the port, so it knows which `id` this
/// answers. Nothing here needs the request id.
#[tauri::command]
pub(crate) async fn invoke<R: Runtime>(
    app: tauri::AppHandle<R>,
    webview: tauri::Webview<R>,
    lease: String,
    v: u32,
    method: String,
    params: Option<serde_json::Value>,
) -> super::dispatch::BridgeReply {
    if !caller_authorized(&webview, &lease) {
        return super::dispatch::BridgeReply::err(
            super::dispatch::code::DENIED,
            "extension caller is not authorised for this lease",
        );
    }
    super::dispatch::dispatch(&app, &lease, v, &method, params).await
}

/// Internal stream transport control. It never enters the public request-id
/// registry and exposes no method authority.
#[allow(clippy::too_many_arguments)]
#[tauri::command]
pub(crate) async fn stream_control<R: Runtime>(
    app: tauri::AppHandle<R>,
    webview: tauri::Webview<R>,
    lease: String,
    action: String,
    sub: String,
    seq: Option<u64>,
    token: Option<String>,
    frame_count: Option<usize>,
    encoded_bytes: Option<usize>,
) {
    if !caller_authorized(&webview, &lease)
        || lease.len() > 128
        || sub.is_empty()
        || sub.len() > 128
    {
        return;
    }
    match action.as_str() {
        "activate" => super::query::activate_subscription(&app, &lease, &sub),
        "ack" => match (seq, token, frame_count, encoded_bytes) {
            (Some(seq), Some(token), Some(frame_count), Some(encoded_bytes)) => {
                super::query::acknowledge_subscription_batch(
                    &app,
                    &lease,
                    &sub,
                    seq,
                    &token,
                    frame_count,
                    encoded_bytes,
                );
            }
            _ => super::query::stream_flow_violation(&app, &lease, &sub),
        },
        "violation" => super::query::stream_flow_violation(&app, &lease, &sub),
        _ => super::query::stream_flow_violation(&app, &lease, &sub),
    }
}

/// The trusted native wrapper calls this only after it owns the host-created
/// MessageChannel and installed its bounded stream listener. The hostile child
/// has neither the matching URL capability nor the exact invoking label.
#[tauri::command]
pub(crate) async fn native_ready<R: Runtime>(
    app: tauri::AppHandle<R>,
    webview: tauri::Webview<R>,
    lease: String,
) -> Result<super::native_window::NativeExtensionWindowStatus, String> {
    let url = webview
        .url()
        .map_err(|_| "native extension wrapper URL is unavailable".to_string())?;
    native_ready_for_caller(&app, webview.label(), url.as_str(), &lease)
}

pub(crate) fn native_ready_for_caller<R: Runtime>(
    app: &tauri::AppHandle<R>,
    label: &str,
    wrapper_url: &str,
    lease: &str,
) -> Result<super::native_window::NativeExtensionWindowStatus, String> {
    if !caller_authorized_parts(label, Some(wrapper_url), lease) {
        return Err("native extension caller is not authorised for this lease".to_string());
    }
    super::native_window::mark_ready(app, label, lease)
}

#[tauri::command]
pub(crate) async fn native_stream_bind<R: Runtime>(
    app: tauri::AppHandle<R>,
    webview: tauri::Webview<R>,
    lease: String,
    channel: tauri::ipc::Channel<super::query::StreamBatch>,
) -> Result<(), String> {
    if !caller_authorized(&webview, &lease) {
        return Err("native extension caller is not authorised for this lease".to_string());
    }
    let url = webview
        .url()
        .map_err(|_| "native extension wrapper URL is unavailable".to_string())?;
    super::native_window::bind_stream_channel(&app, webview.label(), &lease, url.as_str(), channel)
}

/// Plugin name. Must match the `tauri_build::InlinedPlugin` entry in `build.rs`
/// or the generated ACL manifest will not resolve and every grant fails.
pub(crate) const PLUGIN_NAME: &str = "extension-bridge";

pub(crate) fn init<R: Runtime>() -> TauriPlugin<R> {
    tauri::plugin::Builder::new(PLUGIN_NAME)
        .invoke_handler(tauri::generate_handler![
            resolve_identity,
            invoke,
            stream_control,
            native_ready,
            native_stream_bind
        ])
        .build()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn an_unknown_lease_resolves_to_nothing() {
        let outcome = resolve_identity_for_label("main", "not-a-lease".to_string()).await;
        assert!(
            outcome.is_err(),
            "an invented lease must not resolve to an identity"
        );
    }

    #[tokio::test]
    async fn identity_comes_from_the_lease_not_a_parameter() {
        // The command takes no extension id at all, which is the point: there
        // is no parameter a caller could set to influence the answer. This test
        // pins that shape — if someone adds an `id` parameter later, the
        // signature change breaks it and the reason is recorded here.
        let base = tempfile::tempdir().expect("tempdir");
        let root = base.path().join("demo");
        std::fs::create_dir_all(&root).expect("root");
        std::fs::write(
            root.join("extension.json"),
            br#"{ "id": "demo", "name": "Demo", "version": "1", "entry": "index.html" }"#,
        )
        .expect("manifest");
        std::fs::write(root.join("index.html"), b"<!doctype html>").expect("entry");

        // The frame host is process-wide. Without this guard, this test's
        // `shutdown_now()` tore down the host a lifecycle test was mid-way
        // through using — the cause of a ~14% flake in `frame_host_tests`.
        let _guard = frame_host::lifecycle_guard().await;
        let claim = frame_host::acquire(base.path().to_path_buf(), "demo")
            .await
            .expect("acquire");

        let identity = resolve_identity_for_label("main", claim.lease.clone())
            .await
            .expect("a live lease must resolve");
        assert_eq!(identity.extension_id, "demo");
        assert!(
            resolve_identity_for_label("extension-secure-wrong", claim.lease.clone())
                .await
                .is_err(),
            "a different invoking label must not reuse the lease"
        );

        // Release is terminal: the same token stops working.
        frame_host::release(&claim.lease);
        assert!(
            resolve_identity_for_label("main", claim.lease)
                .await
                .is_err(),
            "a released lease must not keep resolving"
        );
    }
}
