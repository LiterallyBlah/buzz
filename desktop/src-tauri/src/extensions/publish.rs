//! The mediated signer (BRIDGE_SPEC §4, decision 003).
//!
//! This is the real authority boundary. Decision 002 bounds what an extension
//! can *reach*; this module decides what the host will put the user's
//! signature on, which is the part with real-world consequence.
//!
//! # Checked on the canonical event, never on the page's description
//!
//! §4 is explicit that the checks run on "the canonical event the host will
//! actually sign". So [`canonicalise`] builds that event first — normalising
//! tags, resolving `created_at` — and every check reads *it*. A check that
//! read the inbound template could pass while the thing actually signed
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

/// How far from now a caller-supplied `created_at` may sit before the host
/// pulls it back (§4: "the host clamps it to a sane window").
///
/// Clamped rather than refused: a client with a skewed clock should still be
/// able to post, and a timestamp is not an authority claim. Five minutes is
/// wide enough for ordinary skew and narrow enough that an extension cannot
/// backdate an event into someone's scrollback or park one in the future.
const CREATED_AT_SKEW_SECONDS: i64 = 300;

/// What the extension supplies (§4 `template`).
#[derive(Debug, Clone, Default)]
pub(crate) struct EventTemplate {
    pub(crate) kind: u32,
    pub(crate) content: String,
    pub(crate) tags: Vec<Vec<String>>,
    pub(crate) created_at: Option<i64>,
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
    /// The single `h` value, if the event carries exactly one.
    fn channel(&self) -> Option<&str> {
        let mut found = None;
        for tag in &self.tags {
            if tag.first().map(String::as_str) == Some(CHANNEL_TAG) {
                if found.is_some() {
                    return None;
                }
                found = tag.get(1).map(String::as_str);
            }
        }
        found
    }
}

/// Build the event the host will sign, from the template it was handed.
///
/// Drops nothing and interprets nothing: the only transformation is resolving
/// and clamping `created_at`. Tags are carried across verbatim so that what the
/// checks inspect is exactly what gets signed.
pub(crate) fn canonicalise(template: &EventTemplate, now: i64) -> CanonicalEvent {
    let created_at = template
        .created_at
        .unwrap_or(now)
        .clamp(now - CREATED_AT_SKEW_SECONDS, now + CREATED_AT_SKEW_SECONDS);
    CanonicalEvent {
        kind: template.kind,
        content: template.content.clone(),
        tags: template.tags.clone(),
        created_at,
    }
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
    let Some(channel) = event.channel() else {
        return Err(Refusal::ChannelTagNotSingular);
    };
    if channel.is_empty() || !has_sign_scope(event.kind, channel) {
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
fn parse_template(params: Option<Value>) -> Result<EventTemplate, super::dispatch::BridgeReply> {
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

    let created_at = match map.get("created_at") {
        None | Some(Value::Null) => None,
        Some(value) => Some(
            value
                .as_i64()
                .ok_or_else(|| invalid("created_at must be a unix timestamp"))?,
        ),
    };

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

/// §4 `publish.event(template) → { event, relay }`.
///
/// The ordering is the security property: parse, canonicalise, **authorise**,
/// and only then sign. Nothing touches the user's key until every check has
/// passed on the exact event that will be signed.
pub(crate) async fn publish_event<R: tauri::Runtime>(
    app: &tauri::AppHandle<R>,
    extension_id: &str,
    params: Option<Value>,
) -> super::dispatch::BridgeReply {
    use super::dispatch::{code, BridgeReply};
    use tauri::Manager as _;

    let template = match parse_template(params) {
        Ok(template) => template,
        Err(reply) => return reply,
    };
    let event = canonicalise(&template, now_unix());

    let state = app.state::<crate::AppState>();
    let keys = match state.keys.lock() {
        Ok(keys) => keys.clone(),
        Err(_) => return BridgeReply::err(code::INTERNAL, "identity is not readable"),
    };
    let identity_pubkey = keys.public_key().to_hex();

    // Fail closed: a store we cannot open has granted nothing, so the closure
    // below answers `false` for every (kind, channel) rather than erroring.
    let grant_db = super::dispatch::grant_db_path(app)
        .ok()
        .and_then(|path| super::grants::open_grant_db(&path).ok());

    if let Err(refusal) = authorise(&event, |kind_value, channel| {
        grant_db.as_ref().is_some_and(|conn| {
            super::grants::has_sign_scope(conn, &identity_pubkey, extension_id, kind_value, channel)
        })
    }) {
        return refusal.into();
    }

    match sign_and_publish(&event, &keys, &state).await {
        Ok(signed) => BridgeReply::ok(signed),
        Err(reply) => reply,
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

    let signed = builder
        .sign_with_keys(keys)
        .map_err(|_| BridgeReply::err(code::INTERNAL, "the event could not be signed"))?;

    // Every relay failure collapses to one §8 code with a fixed message. The
    // relay's own text is discarded rather than wrapped — it is written for an
    // operator, not for an extension.
    let accepted = crate::relay::submit_signed_event_with_keys(&signed, state, keys, None)
        .await
        .map_err(|_| BridgeReply::err(code::RELAY_ERROR, "the relay did not accept the event"))?;

    Ok(serde_json::json!({
        "event": {
            "id": signed.id.to_hex(),
            "pubkey": signed.pubkey.to_hex(),
            "kind": event.kind,
            "content": signed.content,
            "created_at": event.created_at,
            "tags": event.tags,
        },
        "relay": { "accepted": accepted.accepted },
    }))
}

#[cfg(test)]
#[path = "publish_tests.rs"]
mod publish_tests;
