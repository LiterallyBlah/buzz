//! The mediated signer (BRIDGE_SPEC §4, decision 003).
//!
//! This is the real authority boundary. Decision 002 bounds what an extension
//! can *reach*; this module decides what the host will put the user's
//! signature on, which is the part with real-world consequence.
//!
//! # Checked on the canonical event, never on the page's description
//!
//! §4 is explicit that the checks run on "the canonical event the host will
//! actually sign". So [`canonicalise`] builds that event first and every check
//! reads *it*. It normalises nothing and resolves nothing — `created_at` is
//! required and window-checked during parsing, and tags cross verbatim —
//! because any adjustment here would change the event id the caller will retry
//! with. A check that read the inbound template could pass while the thing
//! actually signed
//! differed from what was inspected.
//!
//! # Two independent gates, not one
//!
//! [`is_never_grantable_kind`] (§4 check 1) and the allowlist (§4 check 2) both
//! have to pass. The allowlist already excludes every never-grantable kind, so
//! the denylist is redundant *today* — that is the point. It is defence in
//! depth against the allowlist being widened by someone who did not realise
//! what a kind carries. A test proves the two gates are independent by
//! mis-widening the allowlist and confirming the denylist still refuses.
//!
//! # Sourced from buzz-core, not copied
//!
//! The denylist is built from `buzz-core`'s own classification predicates
//! wherever one exists, so a kind reclassified there is reclassified here
//! without anyone remembering to update a list. Only the classes buzz-core has
//! no predicate for are named individually, and those use its constants rather
//! than integer literals.

use buzz_core_pkg::kind;
use serde_json::Value;

/// Kinds refused to any extension at any grant level (§4 check 1, D-2a).
///
/// These carry real-world authority: the relay executes them with the
/// signer's own standing, so a signature is not a message but an action —
/// a deploy, a membership change, a login, a deletion.
///
/// Ordered as D-2a enumerates them. Predicates come first because they track
/// buzz-core's classification automatically; the literal arms below exist only
/// where buzz-core has no predicate to borrow.
///
/// The set is exactly `is_relay_only_kind ∪ D-2a` — no more.
///
/// It is deliberately **not** widened to every kind that looks dangerous.
/// Kinds outside it are already refused by the allowlist (§4 check 2 is
/// default-deny), so a speculative extra arm buys nothing and costs the thing
/// that makes this list worth reading: that every entry traces to a line of
/// D-2a. `manifest_tests.rs` records what happens otherwise — an earlier
/// read-deny predicate carried "an extra `is_relay_only_kind` clause the spec
/// does not have", and nobody could tell whether that was policy or accident.
pub(crate) fn is_never_grantable_kind(kind_value: u32) -> bool {
    // Predicates first: these track buzz-core's own classification, so a kind
    // added to one of these families is covered here the day it is added.
    if kind::is_relay_only_kind(kind_value)
        // Deploy / workflow / approval, and DM membership — "a signature
        // triggers a deploy". Covers 30620/46020/46030/46031 + 41010–41012.
        || kind::is_command_kind(kind_value)
        // Relay membership admin, 9030–9033.
        || kind::is_relay_admin_kind(kind_value)
        // Moderation, 9040–9044.
        || kind::is_moderation_command_kind(kind_value)
        // Identity archival, 9035–9036.
        || kind::is_identity_archive_request_kind(kind_value)
    {
        return true;
    }

    // The remainder of D-2a, which buzz-core has no predicate for. Named by
    // constant rather than by integer so a renumbering is a compile error.
    matches!(
        kind_value,
        // Deletion — could remove the user's own history.
        kind::KIND_DELETION
        // NIP-29 membership and group administration.
        | kind::KIND_NIP29_PUT_USER
        | kind::KIND_NIP29_REMOVE_USER
        | kind::KIND_NIP29_EDIT_METADATA
        | kind::KIND_NIP29_DELETE_EVENT
        | kind::KIND_NIP29_CREATE_GROUP
        | kind::KIND_NIP29_DELETE_GROUP
        | kind::KIND_NIP29_CREATE_INVITE
        // Auth / bearer credential: a signature here *is* a login as the user.
        | kind::KIND_AUTH
        | kind::KIND_BLOSSOM_AUTH
        | kind::KIND_HTTP_AUTH
        | kind::KIND_NOSTR_IDENTITY_BINDING
        // Agent control: observer frames drive agents.
        | kind::KIND_AGENT_OBSERVER_FRAME
        // Git push / ref authority.
        | kind::KIND_GIT_REPO_ANNOUNCEMENT
        | kind::KIND_GIT_REPO_STATE
        | kind::KIND_GIT_STATUS_MERGED
        | kind::KIND_GIT_STATUS_CLOSED
    )
}

/// Tag names an extension-signed event may carry (§4 check 4).
///
/// An **allowlist**, for the same reason kinds are: §4's prose enumerates what
/// is rejected (`role`, admin, authority `expiration`), but enumerating
/// rejections means a privilege tag added to Buzz next month is permitted until
/// somebody remembers to add it. Decision 003 chose default-deny for kinds with
/// exactly this argument — "nightly-added authority kinds are safe until
/// reviewed" — and a tag is no different.
///
/// `a` is deliberately absent. §4 rejects "cross-channel `e`-root/`a`", and
/// since no allowlisted content kind needs an addressable coordinate, refusing
/// `a` outright satisfies that clause by construction rather than by parsing
/// coordinates and hoping the parse is right.
const PERMITTED_TAG_NAMES: &[&str] = &["h", "p", "e", "q", "t", "emoji"];

/// The channel tag. Exactly one, and it must name a granted channel.
const CHANNEL_TAG: &str = "h";

/// How far from now a caller-supplied `created_at` may sit.
///
/// §4 says the host "clamps it to a sane window"; this **rejects** instead,
/// and that refinement is the whole idempotency mechanism rather than a
/// stylistic preference. A nostr event id is a hash *over* `created_at`, so
/// silently adjusting a caller's timestamp changes the id — and a retry that
/// rebuilt the same template would then produce a *different* event, publish a
/// second time, and defeat the relay's deduplication. A clamp here would be an
/// invisible double-publish.
///
/// Five minutes is wide enough for ordinary clock skew and narrow enough that
/// an extension cannot backdate an event into someone's scrollback or park one
/// in the future.
const CREATED_AT_SKEW_SECONDS: i64 = 300;

/// What the extension supplies (§4 `template`).
#[derive(Debug, Clone, Default)]
pub(crate) struct EventTemplate {
    pub(crate) kind: u32,
    pub(crate) content: String,
    pub(crate) tags: Vec<Vec<String>>,
    /// Required, and validated against the window before this exists.
    ///
    /// Not an `Option`: there is no default-to-now path, because a caller who
    /// omits it gets a different id on every retry and double-publishes the
    /// first time anything goes wrong.
    ///
    /// **The v1 contract is that the caller retains and resubmits the exact
    /// template.** There is no client shim on this branch — `window.buzz` is
    /// injected nowhere — so nothing else is holding this value on the
    /// extension's behalf. A §11 shim may later offer same-frame convenience,
    /// but it cannot be relied on across a frame reopen: the terminal port
    /// lifecycle can retire a frame, and identity held only in that frame's
    /// JavaScript goes with it.
    ///
    /// The horizon is bounded and worth stating precisely, because the two
    /// numbers are different mechanisms rather than one policy: the **host**
    /// accepts a five-minute window around now, and the **relay** accepts
    /// timestamps within a ±15-minute drift window. Neither is a retention
    /// policy — the relay does not discard the event after fifteen minutes.
    ///
    /// Past the host window a reused template is **rejected**, which is safe —
    /// it will not duplicate — but `publish.event` alone can no longer confirm
    /// the earlier commit, and a read is needed instead. Bounded idempotency,
    /// not indefinite result retrieval.
    pub(crate) created_at: i64,
}

/// The event the host will actually sign.
///
/// Constructed *before* the checks run, because §4 requires the checks to read
/// the canonical event rather than the page's description of it. A check that
/// read the template could pass while the signed bytes differed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CanonicalEvent {
    pub(crate) kind: u32,
    pub(crate) content: String,
    pub(crate) tags: Vec<Vec<String>>,
    pub(crate) created_at: i64,
}

impl CanonicalEvent {
    /// The one channel this event names, or why it does not name exactly one.
    ///
    /// **Occurrences are counted independently of whether they carry a value.**
    /// An earlier version used a single `Option` as both the count and the
    /// extracted value, so a valueless `["h"]` left it `None` and the *next*
    /// `["h", granted]` read as the first channel tag — a two-`h` event slipped
    /// through the exactly-one gate. `nostr::Tag::parse(["h"])` succeeds and the
    /// relay's `extract_channel_id` skips the valueless one and takes the later
    /// UUID, so that event would have been signed and ingested.
    ///
    /// A valueless `h` is classified as malformed rather than as a missing
    /// channel: the caller sent a channel tag, it is simply not usable, and
    /// saying so is more honest than reporting the event as unscoped.
    fn channel(&self) -> Result<&str, Refusal> {
        let mut seen = 0usize;
        let mut value: Option<&str> = None;
        for tag in &self.tags {
            if tag.first().map(String::as_str) == Some(CHANNEL_TAG) {
                seen += 1;
                value = tag.get(1).map(String::as_str);
            }
        }
        // One decision point. An early `return` for `seen > 1` inside the loop
        // would be redundant with the match below and — being unobservable
        // through it — could be deleted without a single test noticing, which
        // is not a property a channel-scope guard should have.
        match (seen, value) {
            (1, Some(channel)) if !channel.is_empty() => Ok(channel),
            (1, _) => Err(Refusal::MalformedTag),
            _ => Err(Refusal::ChannelTagNotSingular),
        }
    }
}

/// Build the event the host will sign, from the template it was handed.
///
/// Drops nothing, interprets nothing, and transforms nothing. `created_at` is
/// required and window-checked during parsing, so by the time a template exists
/// there is no resolving or clamping left to do — this is a projection, and it
/// has to be, because any adjustment here would change the event id the caller
/// will retry with. Tags are carried across verbatim so that what the checks
/// inspect is exactly what gets signed.
pub(crate) fn canonicalise(template: &EventTemplate) -> CanonicalEvent {
    CanonicalEvent {
        kind: template.kind,
        content: template.content.clone(),
        tags: template.tags.clone(),
        created_at: template.created_at,
    }
}

/// Is this timestamp inside the window the host will sign?
///
/// Separate and pure so the boundary is testable on both sides without
/// building a template.
pub(crate) fn timestamp_in_window(created_at: i64, now: i64) -> bool {
    created_at >= now - CREATED_AT_SKEW_SECONDS && created_at <= now + CREATED_AT_SKEW_SECONDS
}

/// Does this kind-9 content match the agent-control directive convention?
///
/// §4 check 6: a kind-9 message whose content is `!shutdown` with a `p` tag is
/// the agent-shutdown convention (`buzz-core/src/kind.rs`). Publishing a
/// message must not smuggle an agent-control action through the content field.
///
/// The `p` tag is **not** required for a refusal. The convention pairs the two,
/// but the directive is the `!`-prefix, and refusing only the exact pair would
/// leave the host guessing which harnesses match on content alone.
fn is_agent_control_directive(content: &str) -> bool {
    let trimmed = content.trim_start();
    let Some(rest) = trimmed.strip_prefix('!') else {
        return false;
    };
    rest.chars().next().is_some_and(char::is_alphabetic)
}

/// Which §4 check refused, as a value rather than a sentence.
///
/// The gates are ordered so that an earlier one is *redundant* for the final
/// outcome — a never-grantable kind is not in the allowlist either, so both
/// refuse it. That redundancy is the point of defence in depth, and it is also
/// what makes a test asserting only "denied" useless: deleting the denylist
/// leaves every such test green, because the next gate catches the same case.
///
/// Naming the refusing gate is what makes each one independently observable,
/// so a deleted check fails a test instead of being silently covered for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Refusal {
    /// Check 1 — D-2a: refused to any extension at any grant level.
    NeverGrantable,
    /// Check 2 — not in the v1 signable allowlist.
    NotAllowlisted,
    /// Check 2 — allowlisted, but §4 routes it through `extensionData`.
    WrongMethodForKind,
    /// Check 4 — a tag an extension may not set.
    TagNotPermitted,
    /// Check 4 — a tag with no name at all: malformed, not unauthorised.
    MalformedTag,
    /// Check 3 — no channel tag, or more than one.
    ChannelTagNotSingular,
    /// Check 3 — the channel is not one this extension may sign in.
    ChannelNotGranted,
    /// Check 5 — content matches the agent-control directive convention.
    AgentControlDirective,
}

impl Refusal {
    /// The §8 code. Everything here is an authority failure except a tag that
    /// is not even well formed, which is a malformed template.
    pub(crate) fn code(self) -> &'static str {
        use super::dispatch::code;
        match self {
            Refusal::MalformedTag => code::INVALID_PARAMS,
            _ => code::DENIED,
        }
    }

    /// Human-readable, and deliberately free of anything the caller supplied:
    /// no kind number, no tag name, no channel id. §8 requires the message not
    /// to disclose what the extension was not granted.
    pub(crate) fn message(self) -> &'static str {
        match self {
            Refusal::NeverGrantable => "this kind can never be signed by an extension",
            Refusal::NotAllowlisted => "this kind is not in the v1 signable allowlist",
            Refusal::WrongMethodForKind => "this kind must be published through its own method",
            Refusal::TagNotPermitted => "this event carries a tag an extension may not set",
            Refusal::MalformedTag => "a tag must have a name",
            Refusal::ChannelTagNotSingular => {
                "a channel-scoped event must carry exactly one channel tag"
            }
            Refusal::ChannelNotGranted => "this extension may not sign this kind in this channel",
            Refusal::AgentControlDirective => "message content matches an agent-control directive",
        }
    }
}

impl From<Refusal> for super::dispatch::BridgeReply {
    fn from(refusal: Refusal) -> Self {
        super::dispatch::BridgeReply::err(refusal.code(), refusal.message())
    }
}

/// Run §4's ordered checks on the canonical event.
///
/// `has_sign_scope(kind, channel)` is the granted `(kind, channel)` lookup —
/// injected so every rule here is testable without a database, and so this
/// function cannot accidentally consult anything else.
pub(crate) fn authorise(
    event: &CanonicalEvent,
    has_sign_scope: impl Fn(u32, &str) -> bool,
) -> Result<(), Refusal> {
    // Check 1 — denylist, unconditionally, before anything else is consulted.
    if is_never_grantable_kind(event.kind) {
        return Err(Refusal::NeverGrantable);
    }

    // Check 2 — allowlist, then the extension's own grant.
    if !super::manifest::EXTENSION_SIGNABLE_KINDS.contains(&event.kind) {
        return Err(Refusal::NotAllowlisted);
    }
    // Kind 30800 is allowlisted, but §4 routes it through `publish.extensionData`
    // so the host — not the extension — builds the `d` namespace tag. Reaching
    // it through the generic path would let an extension name another's
    // namespace, so it is refused here rather than partially handled.
    if event.kind == kind::KIND_EXTENSION_DATA {
        return Err(Refusal::WrongMethodForKind);
    }

    // Check 4 (tag shape) runs before the channel comparison, because the
    // channel check needs to know there is exactly one `h` to compare.
    for tag in &event.tags {
        let Some(name) = tag.first() else {
            return Err(Refusal::MalformedTag);
        };
        if !PERMITTED_TAG_NAMES.contains(&name.as_str()) {
            return Err(Refusal::TagNotPermitted);
        }
    }

    // Check 3 — channel scope. Exactly one `h`, naming a granted channel.
    let channel = event.channel()?;
    if !has_sign_scope(event.kind, channel) {
        return Err(Refusal::ChannelNotGranted);
    }

    // Check 5 — content-level side-effect guard.
    if event.kind == kind::KIND_STREAM_MESSAGE && is_agent_control_directive(&event.content) {
        return Err(Refusal::AgentControlDirective);
    }

    Ok(())
}

/// Parse the caller's `params` into a template (§4 `{ kind, content, tags?, created_at? }`).
///
/// Deliberately strict and hand-written rather than a serde derive: a derive
/// would silently accept unknown fields, and an unknown field on the signer's
/// input is exactly the thing worth refusing. `params.extensionId` is not
/// merely ignored here — it makes the whole call `invalid_params`, because a
/// caller sending one has misunderstood who decides identity.
fn parse_template(
    params: Option<Value>,
    now: i64,
) -> Result<EventTemplate, super::dispatch::BridgeReply> {
    use super::dispatch::{code, BridgeReply};

    let invalid = |message: &str| BridgeReply::err(code::INVALID_PARAMS, message.to_string());

    let Some(Value::Object(map)) = params else {
        return Err(invalid("publish.event requires a template object"));
    };

    for key in map.keys() {
        if !matches!(key.as_str(), "kind" | "content" | "tags" | "created_at") {
            return Err(invalid("the template carries an unrecognised field"));
        }
    }

    let kind = map
        .get("kind")
        .and_then(Value::as_u64)
        .and_then(|k| u32::try_from(k).ok())
        .ok_or_else(|| invalid("the template needs an integer kind"))?;

    let content = match map.get("content") {
        Some(Value::String(text)) => text.clone(),
        None => String::new(),
        Some(_) => return Err(invalid("content must be a string")),
    };

    let tags = match map.get("tags") {
        None | Some(Value::Null) => Vec::new(),
        Some(Value::Array(rows)) => {
            let mut parsed = Vec::with_capacity(rows.len());
            for row in rows {
                let Value::Array(parts) = row else {
                    return Err(invalid("each tag must be an array of strings"));
                };
                let mut tag = Vec::with_capacity(parts.len());
                for part in parts {
                    let Value::String(text) = part else {
                        return Err(invalid("each tag must be an array of strings"));
                    };
                    tag.push(text.clone());
                }
                parsed.push(tag);
            }
            parsed
        }
        Some(_) => return Err(invalid("tags must be an array")),
    };

    // Required. Omitting it is the double-publish footgun: without a stable
    // timestamp a retry rebuilds a different event id, so the relay cannot
    // recognise it as the same operation.
    let created_at = map
        .get("created_at")
        .and_then(Value::as_i64)
        .ok_or_else(|| invalid("created_at is required and must be a unix timestamp"))?;
    if !timestamp_in_window(created_at, now) {
        // Rejected, never adjusted. Silently moving it would change the event
        // id the caller will retry with, which is exactly the deduplication
        // this requires.
        return Err(invalid("created_at is outside the acceptable window"));
    }

    Ok(EventTemplate {
        kind,
        content,
        tags,
        created_at,
    })
}

fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or_default()
}

/// The identity the signer may act under, or a refusal.
///
/// Extracted so the recovery path is testable against a production-shaped
/// `AppState` rather than only through a Tauri command. Increment 2 removed
/// this exact bypass from the identity handler and the signer reintroduced it,
/// so the seam is worth having a name and a test of its own.
///
/// `signing_keys()` refuses while `identity_lost` or `keyring_locked` is set;
/// `state.keys` does not, and hands back the real-looking **ephemeral** key
/// those states boot with. Signing under that would publish, with a valid
/// signature, as an identity the user does not control.
///
/// The refusal is `denied`, not `identity_unavailable`: §7 grants are keyed by
/// identity, so with no usable identity nothing can be granted, and the caller
/// is refused exactly as an ungranted one is. Recovery stays invisible.
pub(crate) fn signing_identity(
    state: &crate::AppState,
) -> Result<nostr::Keys, super::dispatch::BridgeReply> {
    use super::dispatch::{code, BridgeReply};
    state
        .signing_keys()
        .map_err(|_| BridgeReply::err(code::DENIED, "missing scope: sign"))
}

/// §4 `publish.event(template) → { event, relay }`.
///
/// The ordering is the security property: parse, canonicalise, **authorise**,
/// and only then sign. Nothing touches the user's key until every check has
/// passed on the exact event that will be signed.
pub(crate) async fn publish_event<R: tauri::Runtime>(
    app: &tauri::AppHandle<R>,
    extension_id: &str,
    lease: &str,
    params: Option<Value>,
) -> super::dispatch::BridgeReply {
    use super::dispatch::{code, BridgeReply};
    use tauri::Manager as _;

    let template = match parse_template(params, now_unix()) {
        Ok(template) => template,
        Err(reply) => return reply,
    };
    let event = canonicalise(&template);

    let state = app.state::<crate::AppState>();

    // Through the authority gate, never `state.keys` directly.
    //
    // This is increment 2's blocker 3, and it was reintroduced here because the
    // re-apply fixed the identity handler's key read and missed this parallel
    // one in the signer. `identity_lost` and `keyring_locked` both boot with a
    // real-looking **ephemeral** key, so locking `state.keys` yields 64 valid
    // hex characters that are not the user's — and signing under it would
    // publish, with a real signature, as an identity the user does not control.
    //
    // Grant-before-protected-state is preserved: §7 grants are keyed by
    // identity, so with no usable identity there is nothing to key a lookup by,
    // nothing can be granted, and the caller is `denied` exactly as an
    // ungranted one is. Recovery stays invisible.
    let keys = match signing_identity(&state) {
        Ok(keys) => keys,
        Err(reply) => return reply,
    };
    let identity_pubkey = keys.public_key().to_hex();

    // Fail closed: a store we cannot open has granted nothing, so the lookup
    // answers `false` for every (kind, channel) rather than erroring.
    //
    // Scoped, and re-opened per check rather than held: a `rusqlite`
    // `Connection` is not `Sync`, so keeping one alive across the submit await
    // would make this command's future non-`Send`. Re-opening also means the
    // revalidation below reads the grant store *as it is then*, which is the
    // point of revalidating at all.
    let has_grant = |kind_value: u32, channel: &str, pubkey: &str| -> bool {
        super::dispatch::grant_db_path(app)
            .ok()
            .and_then(|path| super::grants::open_grant_db(&path).ok())
            .is_some_and(|conn| {
                super::grants::has_sign_scope(&conn, pubkey, extension_id, kind_value, channel)
            })
    };

    if let Err(refusal) = authorise(&event, |kind_value, channel| {
        has_grant(kind_value, channel, &identity_pubkey)
    }) {
        return refusal.into();
    }

    // Authority is re-checked at the last moment before the POST, inside
    // `sign_and_publish`. It is deliberately **not** described as liveness or
    // cancellation: budget exhaustion closes the port without releasing the
    // lease, so a live lease does not mean a live port, and nothing here
    // recalls a request already on the wire. Correctness rests on the event id
    // being deterministic; this only avoids publishing under authority that
    // has since been withdrawn.
    let identity_at_entry = identity_pubkey.clone();
    let event_ref = &event;
    let state_ref = &*state;
    let revalidate = || -> Result<(), &'static str> {
        // The lease must still resolve to *this* extension — not merely to
        // something. A reissued lease pointing elsewhere is a different caller.
        match super::frame_host::extension_for_lease(lease) {
            Some(current) if current == extension_id => {}
            _ => return Err(code::DENIED),
        }
        // The signing identity must still be available and unchanged. Recovery
        // swaps in an ephemeral key, so "available" is not enough on its own.
        let now_pubkey = super::dispatch::resolve_identity_pubkey(state_ref).ok_or(code::DENIED)?;
        if now_pubkey != identity_at_entry {
            return Err(code::DENIED);
        }
        // The whole authority decision, re-run against the exact canonical
        // event — not just the grant row. Re-running `authorise` means a
        // revocation, a denylist change or a tag that no longer passes is
        // caught by the same code that admitted it, rather than by a
        // hand-copied subset of it that could drift.
        authorise(event_ref, |kind_value, channel| {
            has_grant(kind_value, channel, &now_pubkey)
        })
        .map_err(Refusal::code)?;

        // The wait is unbounded, so a template that was inside the window when
        // it arrived may not be now. Signing it anyway would put an event on
        // the relay that the host would refuse if asked again.
        if !timestamp_in_window(event_ref.created_at, now_unix()) {
            return Err(code::INVALID_PARAMS);
        }
        Ok(())
    };

    match sign_and_publish(&event, &keys, &state, revalidate).await {
        Ok(signed) => BridgeReply::ok(signed),
        Err(reply) => reply,
    }
}

/// Discriminants for the refusal carrier.
///
/// A plain integer rather than a lock: this cell exists only to move a §8 code
/// out past a submit path that speaks `String`, and a diagnostic channel must
/// not be able to panic the command it is diagnosing.
const REFUSAL_NONE: u8 = 0;
const REFUSAL_DENIED: u8 = 1;
const REFUSAL_INVALID_PARAMS: u8 = 2;
const REFUSAL_INTERNAL: u8 = 3;

fn refusal_tag(code: &str) -> u8 {
    use super::dispatch::code;
    if code == code::DENIED {
        REFUSAL_DENIED
    } else if code == code::INVALID_PARAMS {
        REFUSAL_INVALID_PARAMS
    } else {
        REFUSAL_INTERNAL
    }
}

fn refusal_code(tag: u8) -> &'static str {
    use super::dispatch::code;
    match tag {
        REFUSAL_DENIED => code::DENIED,
        REFUSAL_INVALID_PARAMS => code::INVALID_PARAMS,
        REFUSAL_INTERNAL => code::INTERNAL,
        // Nothing was recorded, so the failure came from the relay itself.
        _ => code::RELAY_ERROR,
    }
}

/// A §8 code and the message that goes with it.
///
/// Kept as one mapping so a refusal raised deep in `prepare` cannot arrive at
/// the caller wearing the wrong code — collapsing them all into `relay_error`
/// would have told a caller whose grant was revoked that the relay was at
/// fault, and a caller whose timestamp expired that it should retry.
fn message_for(code: &str) -> &'static str {
    match code {
        c if c == super::dispatch::code::DENIED => "publishing is no longer permitted",
        c if c == super::dispatch::code::INVALID_PARAMS => {
            "created_at is outside the acceptable window"
        }
        c if c == super::dispatch::code::INTERNAL => "the event could not be signed",
        _ => "the relay did not accept the event",
    }
}

/// Build, sign and submit the authorised event.
///
/// Split out so the relay's error text has exactly one place it can reach the
/// wire, and does not: §8 requires a normalised `{ code, message }`, and a
/// relay string can carry internals a refused extension has no business
/// reading.
async fn sign_and_publish(
    event: &CanonicalEvent,
    keys: &nostr::Keys,
    state: &crate::AppState,
    revalidate: impl Fn() -> Result<(), &'static str>,
) -> Result<Value, super::dispatch::BridgeReply> {
    use super::dispatch::{code, BridgeReply};

    let kind_u16 = u16::try_from(event.kind)
        .map_err(|_| BridgeReply::err(code::INVALID_PARAMS, "kind is outside the nostr range"))?;

    let mut tags = Vec::with_capacity(event.tags.len());
    for parts in &event.tags {
        tags.push(
            nostr::Tag::parse(parts.clone())
                .map_err(|_| BridgeReply::err(code::INVALID_PARAMS, "a tag is malformed"))?,
        );
    }

    let created_at = u64::try_from(event.created_at)
        .map_err(|_| BridgeReply::err(code::INVALID_PARAMS, "created_at is out of range"))?;

    let builder = nostr::EventBuilder::new(nostr::Kind::Custom(kind_u16), event.content.clone())
        .tags(tags)
        .custom_created_at(nostr::Timestamp::from(created_at));

    // The id is computed from the *unsigned* event, before anything is signed.
    //
    // That is the whole idempotency claim in one line: an event id is a hash of
    // the canonical event and the author's pubkey, and carries nothing from the
    // signature — so a re-sign of the same template yields the same id even
    // though Schnorr signatures need not be byte-identical. It also gives us
    // something to check the relay's acknowledgement against without holding
    // the signed event across the await.
    let expected_id = builder.clone().build(keys.public_key()).id();

    // Signing is deferred into `prepare`, which runs after the rate-limit wait
    // and after revalidation — so the signature is made under an identity and
    // an authority that were both confirmed a moment earlier, with no await
    // between that confirmation and the POST.
    let signing_keys = keys.clone();
    // The refusal reason has to survive out through a submit path that only
    // speaks `String`, so `prepare` records a discriminant here on its way out.
    //
    // An atomic rather than a `Mutex`: the repository forbids new production
    // `unwrap`/`expect`, and a poisoned lock on a *diagnostic* cell would panic
    // the bridge command rather than produce a normalised refusal. A failure
    // path that can crash is a worse failure path than the one it reports.
    let refusal = std::sync::atomic::AtomicU8::new(REFUSAL_NONE);
    let refusal_ref = &refusal;
    let prepare = move || -> Result<nostr::Event, String> {
        use std::sync::atomic::Ordering;
        if let Err(code) = revalidate() {
            refusal_ref.store(refusal_tag(code), Ordering::Release);
            return Err("refused before submission".to_string());
        }
        let signed = builder.sign_with_keys(&signing_keys).map_err(|_| {
            refusal_ref.store(REFUSAL_INTERNAL, Ordering::Release);
            "the event could not be signed".to_string()
        })?;
        // Before the irreversible step: the bytes actually signed must be the
        // ones the id was precomputed from. The relay's acknowledgement is
        // checked *after* the POST and so cannot prevent a divergent
        // submission; this can.
        // Unreachable with a correct signer — `sign_with_keys` derives the id
        // from the same canonical projection `build()` did, so the two agree by
        // construction. No fixture can isolate this branch, and the mutation
        // battery does not claim one. It is kept because it guards the one
        // thing the relay acknowledgement check cannot: the acknowledgement is
        // read *after* the POST, so it can report a divergence but not prevent
        // the submission.
        if signed.id != expected_id {
            refusal_ref.store(REFUSAL_INTERNAL, Ordering::Release);
            return Err("the signed event diverged from the canonical projection".to_string());
        }
        Ok(signed)
    };

    // Every relay failure collapses to one §8 code with a fixed message. The
    // relay's own text is discarded rather than wrapped — it is written for an
    // operator, not for an extension.
    //
    // A **suppressed duplicate is a success**, and deliberately indistinguishable
    // from a fresh publish. The relay answers `accepted: true` with
    // `message: "duplicate:"` when its `ON CONFLICT DO NOTHING` recognised the
    // id, and the event is committed either way — so an idempotent operation
    // returns the same success on retry. The relay's `message` is not forwarded,
    // so "you already did this" is not even observable: the caller is told
    // "committed", which is the only fact that matters to it.
    let (accepted, signed) = crate::relay::submit_prepared_event(state, keys, None, prepare)
        .await
        .map_err(|_| {
            // A refusal recorded by `prepare` outranks the generic relay error:
            // the request never reached the relay, so blaming it would be
            // false.
            let code = refusal_code(refusal.load(std::sync::atomic::Ordering::Acquire));
            BridgeReply::err(code, message_for(code))
        })?;

    // The relay must be talking about the event we signed, and it must say so
    // for a duplicate exactly as for a fresh insert. A mismatch means something
    // between here and the store substituted an id, and reporting that as
    // success would tell the caller a *different* event had committed — under
    // an idempotent contract they would then stop retrying the one that never
    // landed.
    let signed_id = expected_id.to_hex();
    if !accepted.accepted || accepted.event_id != signed_id {
        return Err(BridgeReply::err(
            code::RELAY_ERROR,
            "the relay acknowledged a different event",
        ));
    }

    // §4's result is the **signed** event, `sig` included. Returning an
    // unsigned projection plus an id would be a different contract, and one the
    // caller cannot verify.
    Ok(serde_json::json!({
        "event": {
            "id": signed.id.to_hex(),
            "pubkey": signed.pubkey.to_hex(),
            "kind": event.kind,
            "content": signed.content,
            "created_at": event.created_at,
            "tags": event.tags,
            "sig": signed.sig.to_string(),
        },
        "relay": { "accepted": accepted.accepted },
    }))
}

#[cfg(test)]
#[path = "publish_test_support.rs"]
mod publish_test_support;

#[cfg(test)]
#[path = "publish_tests.rs"]
mod publish_tests;

#[cfg(test)]
#[path = "publish_wire_tests.rs"]
mod publish_wire_tests;

#[cfg(test)]
#[path = "publish_denylist_tests.rs"]
mod publish_denylist_tests;
