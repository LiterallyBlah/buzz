//! Frame host — serves installed extension packages over localhost HTTP.
//!
//! Decision 002 (BX-09) hosts an extension as a `sandbox="allow-scripts"`
//! iframe pointed at a **remote-class** origin. That origin is this server: an
//! axum listener on `127.0.0.1:0`, mirroring `crate::media_proxy`.
//!
//! The origin class is the whole point. Tauri classifies every registered
//! custom URI scheme as *local*, and local origins bypass the app ACL — so a
//! registered scheme would hand the page all of Buzz's app commands. A plain
//! localhost HTTP origin is remote-class, its `Origin` header fails to parse at
//! Tauri's IPC boundary, and the invoke is rejected. That is what the BX-09
//! probe observed on Windows; see decision 002.
//!
//! **This server is therefore security-load-bearing in one direction only:** it
//! must never become a registered scheme, and it must never serve a byte from
//! outside an installed package. It grants no Buzz capability at all — there is
//! no bridge here, and P4's `window.buzz` is not injected by this module.
//!
//! # Lifecycle
//!
//! The listener is reference-counted by live frames, not started at boot:
//!
//! - [`acquire`] starts it on the first frame and hands out the port.
//! - [`release`] stops it when the last frame goes away — a closed tab, a
//!   navigation, or the preview flag being switched off (the frame unmounts in
//!   every one of those cases).
//! - [`shutdown_now`] stops it unconditionally on app exit, so a leaked holder
//!   count cannot outlive the process.
//!
//! Nothing listens when no extension frame is open, which is the state the app
//! is in almost all the time.

use std::net::Ipv4Addr;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard, OnceLock};

use axum::body::Body;
use axum::extract::{Path as RoutePath, State};
use axum::http::{header, HeaderValue, StatusCode};
use axum::response::Response;
use axum::routing::get;
use axum::Router;
use tokio::net::TcpListener;
use tokio::sync::oneshot;

use super::manifest::is_valid_extension_id;
use super::package_path::check_package_relative_path;

/// Path prefix every extension asset is served under.
pub(crate) const EXTENSION_ROUTE_PREFIX: &str = "ext";

/// Why a request did not resolve to a servable file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AssetError {
    /// The id did not match the install grammar.
    InvalidId,
    /// The path was absolute, traversing, or escaped the package root.
    UnsafePath,
    /// No such file inside the package (including "it is a directory").
    NotFound,
}

impl AssetError {
    fn status(self) -> StatusCode {
        // Every failure is a 404. Distinguishing "invalid id" from "no such
        // package" would let a caller enumerate what is installed.
        StatusCode::NOT_FOUND
    }
}

/// Resolve `<base>/<id>/<asset>` to a real file, or explain why not.
///
/// Separated from the server so the rules that matter can be tested without
/// binding a socket. Three independent gates, in order:
///
/// 1. **The id grammar** (`[a-z0-9_][a-z0-9_-]*`) — the same rule the installer
///    uses to name the directory. This is the wall: an id can never contain a
///    separator or `..`, so it cannot address anything but its own folder.
/// 2. **The relative-path rules** — platform-neutral, shared with the
///    installer, rejecting rooted, drive-prefixed and traversing paths.
/// 3. **Canonical containment** — both sides are canonicalised and the target
///    must still sit under the root. This is what catches a symlink planted in
///    an installed tree *after* install, which the first two gates cannot see.
pub(crate) fn resolve_asset(base_dir: &Path, id: &str, asset: &str) -> Result<PathBuf, AssetError> {
    if !is_valid_extension_id(id) {
        return Err(AssetError::InvalidId);
    }
    if check_package_relative_path(asset).is_err() {
        return Err(AssetError::UnsafePath);
    }

    let root = base_dir
        .join(id)
        .canonicalize()
        .map_err(|_| AssetError::NotFound)?;
    let target = root
        .join(asset)
        .canonicalize()
        .map_err(|_| AssetError::NotFound)?;

    if !target.starts_with(&root) {
        return Err(AssetError::UnsafePath);
    }
    if !target.is_file() {
        // Directories included: the host never lists a directory.
        return Err(AssetError::NotFound);
    }
    Ok(target)
}

/// Content type for a package asset, chosen by extension.
///
/// Deliberately a fixed table rather than content sniffing: what the host
/// claims a byte stream is should be a function of the package's own naming,
/// not of attacker-controlled bytes. Anything unrecognised is served as an
/// opaque download rather than guessed at.
fn content_type_for(path: &Path) -> &'static str {
    let extension = path
        .extension()
        .and_then(|value| value.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase();
    match extension.as_str() {
        "html" | "htm" => "text/html; charset=utf-8",
        "js" | "mjs" => "text/javascript; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "json" => "application/json; charset=utf-8",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "avif" => "image/avif",
        "ico" => "image/x-icon",
        "woff2" => "font/woff2",
        "woff" => "font/woff",
        "ttf" => "font/ttf",
        "wasm" => "application/wasm",
        "txt" | "md" => "text/plain; charset=utf-8",
        _ => "application/octet-stream",
    }
}

/// The `Content-Security-Policy` served with every extension document.
///
/// Decision 004: egress is **default-deny**. The host controls the document
/// bytes, so the host sets the policy; CSPs combine as an intersection, so an
/// extension cannot loosen this by injecting its own `<meta>`.
///
/// `'self'` is deliberately absent. A `sandbox="allow-scripts"` document has an
/// **opaque** origin, and `'self'` matches the document's origin — which for an
/// opaque origin matches nothing at all. A policy written with `'self'` would
/// silently block the package's own scripts. The serving origin is therefore
/// named explicitly.
///
/// `frame-ancestors` is omitted on purpose: it has no `default-src` fallback,
/// so leaving it out is what permits the Buzz window to frame this document at
/// all, and the parent's origin differs across platforms (`tauri://localhost`
/// vs `http://tauri.localhost`) — naming it would be a portability bug.
///
/// Manifest-declared `egress` origins are **not** honoured yet: an extension
/// that declares them is simply denied, which is the fail-closed direction.
fn content_security_policy(origin: &str) -> String {
    format!(
        "default-src 'none'; \
         script-src {origin} 'unsafe-inline'; \
         style-src {origin} 'unsafe-inline'; \
         img-src {origin} data: blob:; \
         font-src {origin}; \
         media-src {origin}; \
         connect-src 'none'; \
         base-uri 'none'; \
         form-action 'none'"
    )
}

#[derive(Clone)]
struct HostState {
    base_dir: PathBuf,
    origin: String,
}

fn build_router(base_dir: PathBuf, port: u16) -> Router {
    let state = HostState {
        base_dir,
        origin: origin_for_port(port),
    };
    Router::new()
        .route(
            &format!("/{EXTENSION_ROUTE_PREFIX}/{{id}}/{{*asset}}"),
            get(serve_asset),
        )
        .with_state(state)
}

/// The origin an extension frame is served from.
///
/// A literal `127.0.0.1` HTTP origin, which is **remote-class** to Tauri. It
/// must never become a `register_uri_scheme_protocol` scheme: Tauri classifies
/// registered schemes as local, local origins bypass the app ACL, and the BX-09
/// evidence would no longer describe what ships (decision 002's explicit
/// caveat).
pub(crate) fn origin_for_port(port: u16) -> String {
    format!("http://127.0.0.1:{port}")
}

/// The URL of an installed extension's entry document.
///
/// Built host-side from the validated manifest so the frontend never composes a
/// URL into this boundary. Entry paths have already passed the installer's
/// relative-path rules, so the only thing needed here is percent-encoding of
/// each segment — a filename with a space or `#` would otherwise truncate or
/// mis-address the request.
pub(crate) fn frame_url(origin: &str, id: &str, entry: &str) -> String {
    let encoded: Vec<String> = entry
        .split('/')
        .filter(|segment| !segment.is_empty())
        .map(|segment| {
            segment
                .bytes()
                .map(|byte| match byte {
                    b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                        (byte as char).to_string()
                    }
                    other => format!("%{other:02X}"),
                })
                .collect::<String>()
        })
        .collect();
    format!(
        "{origin}/{EXTENSION_ROUTE_PREFIX}/{id}/{}",
        encoded.join("/")
    )
}

async fn serve_asset(
    State(state): State<HostState>,
    RoutePath((id, asset)): RoutePath<(String, String)>,
) -> Response {
    let path = match resolve_asset(&state.base_dir, &id, &asset) {
        Ok(path) => path,
        Err(error) => return empty(error.status()),
    };
    let Ok(bytes) = tokio::fs::read(&path).await else {
        return empty(StatusCode::NOT_FOUND);
    };

    let mut response = Response::new(Body::from(bytes));
    let headers = response.headers_mut();
    insert(headers, header::CONTENT_TYPE, content_type_for(&path));
    insert(
        headers,
        header::CONTENT_SECURITY_POLICY,
        &content_security_policy(&state.origin),
    );
    // The package's own naming decides the type; never let a browser re-guess.
    insert(headers, header::X_CONTENT_TYPE_OPTIONS, "nosniff");
    insert(headers, header::CACHE_CONTROL, "no-store");
    response
}

fn insert(headers: &mut header::HeaderMap, name: header::HeaderName, value: &str) {
    if let Ok(value) = HeaderValue::from_str(value) {
        headers.insert(name, value);
    }
}

fn empty(status: StatusCode) -> Response {
    let mut response = Response::new(Body::empty());
    *response.status_mut() = status;
    response
}

// ── Lifecycle ────────────────────────────────────────────────────────────────

struct RunningHost {
    port: u16,
    shutdown: oneshot::Sender<()>,
}

#[derive(Default)]
struct FrameHostState {
    running: Option<RunningHost>,
    /// Live frames that have acquired the host and not yet released it.
    holders: usize,
}

static FRAME_HOST: OnceLock<Mutex<FrameHostState>> = OnceLock::new();

/// The shared state, recovering rather than panicking on a poisoned lock.
fn host_state() -> MutexGuard<'static, FrameHostState> {
    let lock = FRAME_HOST.get_or_init(|| Mutex::new(FrameHostState::default()));
    match lock.lock() {
        Ok(guard) => guard,
        // A panic while holding this lock leaves the counters readable and the
        // data structurally fine; refusing to serve afterwards would be worse.
        Err(poisoned) => poisoned.into_inner(),
    }
}

/// Start the host if it is not running and register one more live frame.
///
/// Returns the port. Idempotent: a second frame reuses the running listener.
pub(crate) async fn acquire(base_dir: PathBuf) -> Result<u16, String> {
    {
        let mut state = host_state();
        if let Some(running) = &state.running {
            let port = running.port;
            state.holders += 1;
            return Ok(port);
        }
    }

    // Bind outside the lock: this is the only await in the path, and holding a
    // std Mutex across it would be a deadlock waiting to happen.
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
        .await
        .map_err(|error| format!("could not start the extension frame host: {error}"))?;
    let port = listener
        .local_addr()
        .map_err(|error| format!("could not read the frame host address: {error}"))?
        .port();

    let (shutdown, shutdown_rx) = oneshot::channel();
    let router = build_router(base_dir, port);
    tokio::spawn(async move {
        axum::serve(listener, router)
            .with_graceful_shutdown(async move {
                shutdown_rx.await.ok();
            })
            .await
            .ok();
    });

    let mut state = host_state();
    if let Some(running) = &state.running {
        // Another frame won the race while we were binding. Keep theirs and
        // retire the listener we just created rather than leaking it.
        let port = running.port;
        let _ = shutdown.send(());
        state.holders += 1;
        return Ok(port);
    }
    state.running = Some(RunningHost { port, shutdown });
    state.holders += 1;
    Ok(port)
}

/// Drop one live frame, stopping the host when the last one goes.
pub(crate) fn release() {
    let mut state = host_state();
    state.holders = state.holders.saturating_sub(1);
    if state.holders == 0 {
        if let Some(running) = state.running.take() {
            let _ = running.shutdown.send(());
        }
    }
}

/// Stop the host unconditionally, whatever the holder count says.
///
/// Called on app shutdown. A frontend that never released — a crashed webview,
/// a reload — must not leave a listener behind the process.
pub(crate) fn shutdown_now() {
    let mut state = host_state();
    state.holders = 0;
    if let Some(running) = state.running.take() {
        let _ = running.shutdown.send(());
    }
}

/// The running port, if any. Test and diagnostic use.
#[cfg(test)]
pub(crate) fn running_port() -> Option<u16> {
    host_state().running.as_ref().map(|running| running.port)
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
#[path = "frame_host_tests.rs"]
mod frame_host_tests;
