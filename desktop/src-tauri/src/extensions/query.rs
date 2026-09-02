//! §5 `query.events` — the channel-scoped read path.
//!
//! # Enforcement is constructive, not a filter that gets checked
//!
//! A single NIP-01 filter ANDs its axes, so intersecting axis-wise
//! (`kinds ∩ granted-kinds` × `#h ∩ granted-channels`) leaks the **cross
//! product**: a kind granted only in channel B becomes readable in channel A.
//! The host therefore never edits the caller's filter. It enumerates the
//! surviving `(kind, channel)` pairs, groups them by channel, and *builds* one
//! filter per channel.
//!
//! That is why [`construction`] is a private module exposing a single producer.
//! [`construction::ConstrainedFilters`] has private fields and no constructor
//! other than [`construction::construct_filters`], which takes granted pairs
//! and a validated request — never a caller-assembled filter list. The
//! relay-facing send accepts only that type, so "query the relay with filters
//! nobody constrained" is not a thing this module can express. A boolean check
//! placed next to a freely-constructible value would be a different, weaker
//! design: it can be forgotten at one call site, and one forgotten call site is
//! the whole vulnerability.
//!
//! # Exactly one `#h` per emitted filter is load-bearing
//!
//! The relay pushes `#h` down to the strict `channel_id = C` SQL predicate only
//! when the filter carries a **single** value. With two, it falls back to a
//! predicate that admits `channel_id IS NULL` rows and post-matches on the
//! event's literal *signed* `h` tags — so a global-only event carrying a stray
//! `h` naming a granted channel matches (`READ_KIND_AUDIT.md` §3.1). One value
//! per filter closes that class by construction rather than by hoping the
//! relay's planner keeps its current shape.

use serde_json::Value;

use super::dispatch::{code, BridgeReply};
use super::manifest::{is_canonical_channel_uuid, is_channel_readable_kind, is_read_denied_kind};

/// Most `(kind, channel)` pairs one extension may hold. Fail-closed.
const MAX_READ_PAIRS: usize = 256;
/// Most relay filters one request may be rewritten into — one per channel.
const MAX_EMITTED_FILTERS: usize = 32;
/// Most values on any one axis (`kinds`, `ids`, `authors`, a tag filter).
const MAX_AXIS_VALUES: usize = 64;
/// Most bytes the rewritten query may occupy on the wire.
const MAX_REWRITTEN_QUERY_BYTES: usize = 64 * 1024;
/// Most events the relay may be asked to produce across every emitted filter.
///
/// Bounds relay and database *work*, not merely output: the relay runs each
/// emitted filter with its own `limit` and appends, so the aggregate is what
/// costs. Checked before any network work, which is why it is a pre-flight
/// multiplication rather than a cap applied while reading the response.
const MAX_FETCHED_CANDIDATES: usize = 4096;
/// Most events one `query.events` call may return to the extension.
const OVERALL_RESULT_CAP: usize = 500;
/// Most bytes of `params` this module will look at. Size before shape.
const MAX_REQUEST_BYTES: usize = 16 * 1024;
/// Hard ceiling on the relay's **response** body.
///
/// Separate from [`MAX_FETCHED_CANDIDATES`], which bounds how much work the
/// relay was *asked* for. This bounds what the host is willing to allocate and
/// parse from what it actually receives, because an untrusted relay is under no
/// obligation to honour the limits it was sent.
const MAX_RESPONSE_BYTES: usize = 8 * 1024 * 1024;
/// Longest value the free-form tag axes (`#t`, `#d`) may carry.
const MAX_TAG_VALUE_LEN: usize = 256;

/// Tag filters an extension may name. `#h` is handled separately: it selects
/// channels and is rewritten, never copied through.
const ALLOWED_TAG_FILTERS: &[&str] = &["#e", "#p", "#q", "#t", "#d"];

/// Why a read produced no value.
///
/// `Debug` is for test assertions only. It is never reached from
/// [`QueryError::into_reply`], so no Rust error text can ride out to an
/// extension on it.
#[derive(Debug)]
pub(crate) enum QueryError {
    /// The request was not well-formed. Never used for an authority failure.
    InvalidParams(String),
    /// An authority failure: not granted, or granted authority went away.
    Denied(&'static str),
    /// The request is well-formed but asks for more work than the host allows.
    QuotaExceeded(&'static str),
    /// The relay was unreachable or answered unusably.
    Relay,
}

impl QueryError {
    fn into_reply(self) -> BridgeReply {
        match self {
            QueryError::InvalidParams(message) => BridgeReply::err(code::INVALID_PARAMS, message),
            QueryError::Denied(message) => BridgeReply::err(code::DENIED, message),
            QueryError::QuotaExceeded(message) => BridgeReply::err(code::QUOTA_EXCEEDED, message),
            QueryError::Relay => {
                BridgeReply::err(code::RELAY_ERROR, "the relay could not answer the query")
            }
        }
    }
}

/// A caller's filter after the §5 grammar has accepted it.
///
/// Every field is already in the host's own representation: `kinds` are `u32`,
/// channels are canonical UUIDs, hex axes are exactly 64 lowercase hex. Nothing
/// downstream re-parses the caller's JSON, so there is one place where a value
/// becomes trusted.
pub(crate) struct ValidatedRequest {
    /// `None` means the axis was absent — "every granted kind", not "no kind".
    kinds: Option<Vec<u32>>,
    /// From `#h`. `None` means absent — "every granted channel".
    channels: Option<Vec<String>>,
    ids: Option<Vec<String>>,
    authors: Option<Vec<String>>,
    /// `#e`/`#p`/`#q`/`#t`/`#d`, copied into every emitted filter unchanged.
    tags: Vec<(String, Vec<String>)>,
    since: Option<u64>,
    until: Option<u64>,
    /// The overall extension-visible cap, already defaulted and range-checked.
    limit: usize,
}

/// Validate `params.filter` against §5's bounded, default-deny grammar.
///
/// **Reject, never clamp, and never treat an empty array as absent.** Silently
/// reading `"kinds": []` as "unset" would *widen* the request to every granted
/// kind, which is the one direction a filter mistake must never move. So an
/// explicitly empty array is a parameter error, not a synonym for omission.
pub(crate) fn validate_request(params: &Value) -> Result<ValidatedRequest, QueryError> {
    // Size before shape: a huge document should be refused without walking it.
    let encoded = serde_json::to_vec(params)
        .map_err(|_| QueryError::InvalidParams("filter is not encodable".to_string()))?;
    if encoded.len() > MAX_REQUEST_BYTES {
        return Err(QueryError::InvalidParams(format!(
            "filter exceeds {MAX_REQUEST_BYTES} bytes"
        )));
    }

    let filter = params
        .get("filter")
        .ok_or_else(|| QueryError::InvalidParams("filter is required".to_string()))?;
    let object = filter
        .as_object()
        .ok_or_else(|| QueryError::InvalidParams("filter must be an object".to_string()))?;

    let mut request = ValidatedRequest {
        kinds: None,
        channels: None,
        ids: None,
        authors: None,
        tags: Vec::new(),
        since: None,
        until: None,
        limit: OVERALL_RESULT_CAP,
    };
    let mut limit_seen = false;

    for (key, value) in object {
        match key.as_str() {
            "kinds" => {
                let raw = bounded_array(value, "kinds")?;
                let mut kinds = Vec::with_capacity(raw.len());
                for entry in raw {
                    let number = entry
                        .as_u64()
                        .and_then(|n| u32::try_from(n).ok())
                        .ok_or_else(|| {
                            QueryError::InvalidParams("kinds must be integers".to_string())
                        })?;
                    kinds.push(number);
                }
                kinds.sort_unstable();
                kinds.dedup();
                request.kinds = Some(kinds);
            }
            "ids" => request.ids = Some(hex_axis(value, "ids")?),
            "authors" => request.authors = Some(hex_axis(value, "authors")?),
            "since" => request.since = Some(integer(value, "since")?),
            "until" => request.until = Some(integer(value, "until")?),
            "limit" => {
                let requested = integer(value, "limit")?;
                let requested = usize::try_from(requested).unwrap_or(usize::MAX);
                if requested == 0 {
                    return Err(QueryError::InvalidParams(
                        "limit must be at least 1".to_string(),
                    ));
                }
                // Over the cap is refused before any network work rather than
                // quietly lowered: a caller that asked for more than it may
                // have should learn that, not receive a short page it reads as
                // the whole answer.
                if requested > OVERALL_RESULT_CAP {
                    return Err(QueryError::QuotaExceeded(
                        "limit exceeds the maximum number of events a query may return",
                    ));
                }
                request.limit = requested;
                limit_seen = true;
            }
            "#h" => {
                let raw = bounded_array(value, "#h")?;
                let mut channels = Vec::with_capacity(raw.len());
                for entry in raw {
                    let channel = entry.as_str().ok_or_else(|| {
                        QueryError::InvalidParams("#h values must be strings".to_string())
                    })?;
                    if !is_canonical_channel_uuid(channel) {
                        return Err(QueryError::InvalidParams(
                            "#h values must be channel UUIDs".to_string(),
                        ));
                    }
                    channels.push(channel.to_string());
                }
                channels.sort();
                channels.dedup();
                request.channels = Some(channels);
            }
            other if ALLOWED_TAG_FILTERS.contains(&other) => {
                let raw = bounded_array(value, other)?;
                let mut values = Vec::with_capacity(raw.len());
                for entry in raw {
                    let text = entry.as_str().ok_or_else(|| {
                        QueryError::InvalidParams(format!("{other} values must be strings"))
                    })?;
                    // Every tag axis has a grammar. An axis that accepts any
                    // string is a hole in a "bounded, default-deny" filter: it
                    // is copied verbatim into every emitted filter and sent to
                    // the relay, so "we only forward it" is not a defence.
                    match other {
                        // Event-id and quote references are ids.
                        "#e" | "#q" => require_lowercase_hex64(text, other)?,
                        // Pubkey references.
                        "#p" => require_lowercase_hex64(text, other)?,
                        // Free-form axes: bounded, printable, non-empty. Not
                        // "anything", which is what they accepted before.
                        "#t" | "#d" => require_bounded_label(text, other)?,
                        _ => {
                            return Err(QueryError::InvalidParams(format!(
                                "{other} is not a filter key an extension may use"
                            )))
                        }
                    }
                    values.push(text.to_string());
                }
                values.sort();
                values.dedup();
                request.tags.push((other.to_string(), values));
            }
            // Never silently dropped: an unknown key may be the caller's whole
            // intent, and answering a different question than the one asked is
            // worse than refusing.
            other => {
                return Err(QueryError::InvalidParams(format!(
                    "{other} is not a filter key an extension may use"
                )))
            }
        }
    }

    if let (Some(since), Some(until)) = (request.since, request.until) {
        if since > until {
            return Err(QueryError::InvalidParams(
                "since must not be after until".to_string(),
            ));
        }
    }
    let _ = limit_seen; // absent ⇒ the cap above is already injected.
    request.tags.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(request)
}

/// An array that is present, non-empty and within the per-axis bound.
fn bounded_array<'a>(value: &'a Value, field: &str) -> Result<&'a Vec<Value>, QueryError> {
    let array = value
        .as_array()
        .ok_or_else(|| QueryError::InvalidParams(format!("{field} must be an array")))?;
    if array.is_empty() {
        return Err(QueryError::InvalidParams(format!(
            "{field} must not be empty; omit it instead of sending an empty array"
        )));
    }
    if array.len() > MAX_AXIS_VALUES {
        return Err(QueryError::InvalidParams(format!(
            "{field} carries more than {MAX_AXIS_VALUES} values"
        )));
    }
    Ok(array)
}

/// `ids`/`authors`: exactly 64 lowercase hex. No prefixes — Buzz's matcher
/// round-trips typed ids to hex, so a prefix could not be honoured faithfully
/// and v1 refuses rather than pretending.
fn hex_axis(value: &Value, field: &str) -> Result<Vec<String>, QueryError> {
    let raw = bounded_array(value, field)?;
    let mut out = Vec::with_capacity(raw.len());
    for entry in raw {
        let text = entry
            .as_str()
            .ok_or_else(|| QueryError::InvalidParams(format!("{field} values must be strings")))?;
        if text.len() != 64
            || !text
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        {
            return Err(QueryError::InvalidParams(format!(
                "{field} values must be 64 lowercase hex characters"
            )));
        }
        out.push(text.to_string());
    }
    out.sort();
    out.dedup();
    Ok(out)
}

/// Exactly 64 lowercase hex characters — the form Buzz's matcher round-trips
/// ids and pubkeys to. No prefixes, for the same reason `ids`/`authors` refuse
/// them: a prefix cannot be honoured faithfully, and v1 refuses rather than
/// pretending.
fn require_lowercase_hex64(text: &str, field: &str) -> Result<(), QueryError> {
    let ok = text.len() == 64
        && text
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b));
    if ok {
        Ok(())
    } else {
        Err(QueryError::InvalidParams(format!(
            "{field} values must be 64 lowercase hex characters"
        )))
    }
}

/// The documented v1 grammar for the free-form tag axes (`#t`, `#d`).
///
/// Non-empty, at most [`MAX_TAG_VALUE_LEN`] bytes, and printable ASCII with no
/// control characters. Deliberately narrow: these are copied into the emitted
/// filter, so the bound is on what the host is willing to *send*, and control
/// bytes in a value the relay will log or match on are nobody's legitimate
/// filter.
fn require_bounded_label(text: &str, field: &str) -> Result<(), QueryError> {
    if text.is_empty() {
        return Err(QueryError::InvalidParams(format!(
            "{field} values must not be empty"
        )));
    }
    if text.len() > MAX_TAG_VALUE_LEN {
        return Err(QueryError::InvalidParams(format!(
            "{field} values must be at most {MAX_TAG_VALUE_LEN} bytes"
        )));
    }
    if !text.bytes().all(|b| (0x20..=0x7e).contains(&b)) {
        return Err(QueryError::InvalidParams(format!(
            "{field} values must be printable ASCII"
        )));
    }
    Ok(())
}

fn integer(value: &Value, field: &str) -> Result<u64, QueryError> {
    value
        .as_u64()
        .ok_or_else(|| QueryError::InvalidParams(format!("{field} must be a non-negative integer")))
}

/// The single producer of relay-facing filters — **and their only consumer**.
///
/// Private, and its items are `pub(super)`, so the constrained type cannot be
/// unwrapped anywhere in §5. The module's own header records why that seal had
/// to become structural rather than described.
mod construction;

use construction::{construct_filters, ConstrainedFilters};

/// The authority recheck for a read, run after the gate wait and again after
/// every returned event has been verified.
///
/// Owns its own decision rather than borrowing the extension-data one: that
/// checks a `d`-tag coordinate inside the `extensionData` scope, which is a
/// different grant and a different wall.
pub(crate) struct QueryRevalidation<'a> {
    pub(crate) lease: &'a str,
    pub(crate) extension_id: &'a str,
    pub(crate) identity_at_entry: &'a str,
    /// Exactly the pairs that produced the filters now in flight.
    pub(crate) pairs_at_entry: &'a [(u32, String)],
    pub(crate) state: &'a crate::AppState,
    pub(crate) grant_db: Option<std::path::PathBuf>,
}

impl QueryRevalidation<'_> {
    pub(crate) fn check(&self) -> Result<(), &'static str> {
        // The lease must still resolve to *this* extension.
        match super::frame_host::extension_for_lease(self.lease) {
            Some(current) if current == self.extension_id => {}
            _ => return Err(code::DENIED),
        }
        if !super::management::revalidation_current(
            self.state,
            self.grant_db.as_deref(),
            self.lease,
            self.extension_id,
            self.identity_at_entry,
        ) {
            return Err(code::DENIED);
        }

        // Identity still available and unchanged. Recovery swaps in an
        // ephemeral key, so "available" is not enough on its own.
        let now_pubkey =
            super::dispatch::resolve_identity_pubkey(self.state).ok_or(code::DENIED)?;
        if now_pubkey != self.identity_at_entry {
            return Err(code::DENIED);
        }

        // Every pair that authorised a filter in flight must still authorise
        // it. Checking the *set that was used* rather than re-listing grants is
        // the point: a revocation of one pair must refuse the whole read, not
        // quietly narrow the answer to whatever remains granted.
        let Some(path) = self.grant_db.as_deref() else {
            return Err(code::DENIED);
        };
        let Ok(conn) = super::grants::open_grant_db(path) else {
            return Err(code::DENIED);
        };
        for (kind, channel) in self.pairs_at_entry {
            // Floor and allowlist are re-checked here too, independently, so a
            // kind that left either set stops being readable immediately
            // rather than at the next install.
            if is_read_denied_kind(*kind) || !is_channel_readable_kind(*kind) {
                return Err(code::DENIED);
            }
            if !super::grants::has_read_scope(&conn, &now_pubkey, self.extension_id, *kind, channel)
            {
                return Err(code::DENIED);
            }
        }
        Ok(())
    }
}

/// Is this event one the extension is allowed to see, on its own evidence?
///
/// Defence in depth over the constrained filter: the relay is untrusted, so
/// every clause here re-derives from the event's signed bytes rather than
/// trusting that the filter was honoured.
fn verify_event(
    event: &nostr::Event,
    filters: &ConstrainedFilters,
    conn: &rusqlite::Connection,
    identity_pubkey: &str,
    extension_id: &str,
) -> bool {
    // The signature covers these bytes. Nothing below means anything until the
    // event is known to be authentic.
    if event.verify().is_err() {
        return false;
    }
    let kind = u32::from(event.kind.as_u16());
    if !is_channel_readable_kind(kind) {
        return false;
    }
    if is_read_denied_kind(kind) {
        return false;
    }

    // Count `h` occurrences by tag **name**, then classify once.
    //
    // Filtering to values first would silently drop a valueless `["h"]` before
    // the count, so a crafted `[["h"], ["h", granted]]` would read as one `h`.
    // Two occurrences make placement ambiguous, and an ambiguous event is
    // refused rather than resolved by picking one — picking is exactly how a
    // crafted event carrying both a granted channel and a foreign one gets in.
    let mut seen = 0usize;
    let mut value: Option<String> = None;
    for tag in event.tags.iter() {
        let parts = tag.clone().to_vec();
        if parts.first().map(String::as_str) == Some("h") {
            seen += 1;
            value = parts.get(1).cloned();
        }
    }
    if seen != 1 {
        return false;
    }
    let Some(channel) = value else {
        return false;
    };
    if !is_canonical_channel_uuid(&channel) {
        return false;
    }

    // The concrete pair, read live from the store — not from the pair set
    // captured at construction. A revocation between construction and here
    // must drop the event.
    if !super::grants::has_read_scope(conn, identity_pubkey, extension_id, kind, &channel) {
        return false;
    }

    // And it must match at least one *complete* filter the host built. Matching
    // the pair alone would re-admit the cross product: `(45001, A)` can be a
    // granted pair while no emitted filter ever asked for 45001 in A.
    filters.matches_any(event, &channel, kind)
}

/// Does the event satisfy every axis of one filter this host constructed?
///
/// Only the axes this module emits are considered, because only those can
/// appear — the filter is not caller-supplied, so there is no unknown axis to
/// fall open on.
fn matches_constructed_filter(
    event: &nostr::Event,
    filter: &Value,
    channel: &str,
    kind: u32,
) -> bool {
    let Some(object) = filter.as_object() else {
        return false;
    };
    for (key, value) in object {
        let matched = match key.as_str() {
            "kinds" => value
                .as_array()
                .is_some_and(|kinds| kinds.iter().any(|k| k.as_u64() == Some(u64::from(kind)))),
            "#h" => value
                .as_array()
                .is_some_and(|hs| hs.iter().any(|h| h.as_str() == Some(channel))),
            "ids" => {
                let id = event.id.to_hex();
                value
                    .as_array()
                    .is_some_and(|ids| ids.iter().any(|i| i.as_str() == Some(id.as_str())))
            }
            "authors" => {
                let author = event.pubkey.to_hex();
                value
                    .as_array()
                    .is_some_and(|a| a.iter().any(|x| x.as_str() == Some(author.as_str())))
            }
            "since" => value
                .as_u64()
                .is_some_and(|since| event.created_at.as_secs() >= since),
            "until" => value
                .as_u64()
                .is_some_and(|until| event.created_at.as_secs() <= until),
            // `limit` bounds how many the relay returns; it is not a property
            // of any single event, so it cannot fail one.
            "limit" => true,
            // A `#x` tag filter: the event must carry that tag with one of the
            // named values.
            name if name.starts_with('#') && name.len() == 2 => {
                let Some(wanted) = value.as_array() else {
                    return false;
                };
                let tag_name = &name[1..];
                event.tags.iter().any(|tag| {
                    let parts = tag.clone().to_vec();
                    parts.first().map(String::as_str) == Some(tag_name)
                        && parts
                            .get(1)
                            .is_some_and(|v| wanted.iter().any(|w| w.as_str() == Some(v.as_str())))
                })
            }
            // Unreachable for a host-built filter. Fail closed anyway: if this
            // module ever grows an axis, the verifier refuses until it is
            // taught the axis rather than silently ignoring it.
            _ => false,
        };
        if !matched {
            return false;
        }
    }
    true
}

/// Dedup by id → verify → order → truncate, in that fixed order.
///
/// **Extracted so each step can be defended.** Inside the handler these lines
/// were unreachable from any test — no fixture could reach them without a Tauri
/// app and a relay — and an unreachable step is one that can be deleted without
/// a single test noticing. Each of the four does distinct work:
///
/// - **dedup first**, because the relay runs each emitted filter separately and
///   appends, so one event can arrive once per filter and would otherwise eat
///   several slots of the caller's cap;
/// - **verify before ordering**, so an invalid event is gone before it can
///   influence position;
/// - **order** `created_at` descending with an id-ascending tiebreak, so the
///   page is deterministic when timestamps collide;
/// - **truncate last**, strictly after verification, so a dropped invalid event
///   frees a slot deterministically instead of the caller silently receiving
///   fewer events than it asked for.
fn compose_results(
    events: Vec<nostr::Event>,
    filters: &ConstrainedFilters,
    conn: &rusqlite::Connection,
    identity_pubkey: &str,
    extension_id: &str,
    limit: usize,
) -> Vec<nostr::Event> {
    let mut seen_ids = std::collections::HashSet::new();
    let mut verified: Vec<nostr::Event> = Vec::new();
    for event in events {
        if !seen_ids.insert(event.id) {
            continue;
        }
        if verify_event(&event, filters, conn, identity_pubkey, extension_id) {
            verified.push(event);
        }
    }
    verified.sort_by(|a, b| {
        b.created_at
            .cmp(&a.created_at)
            .then_with(|| a.id.cmp(&b.id))
    });
    verified.truncate(limit);
    verified
}

/// §5 `query.events({ filter }) → { events }`.
///
/// Pipeline order is fixed by the spec and by what each step may assume:
/// construct → bound-check → fetch → dedup by id → verify → recheck authority →
/// order → truncate. Truncation is strictly after verification, so a dropped
/// invalid event frees a slot deterministically instead of the caller silently
/// receiving fewer events than it could have had.
pub(crate) async fn query_events<R: tauri::Runtime>(
    app: &tauri::AppHandle<R>,
    extension_id: &str,
    lease: &str,
    params: Option<Value>,
) -> BridgeReply {
    use tauri::Manager as _;

    let Some(params) = params else {
        return BridgeReply::err(code::INVALID_PARAMS, "params must be an object");
    };
    let request = match validate_request(&params) {
        Ok(request) => request,
        Err(error) => return error.into_reply(),
    };

    let state = app.state::<crate::AppState>();
    // Never a raw `state.keys` read: after an unbounded wait that can be the
    // ephemeral recovery key rather than the identity whose grant admitted the
    // request.
    let keys = match super::publish::signing_identity(&state) {
        Ok(keys) => keys,
        Err(_) => return BridgeReply::err(code::DENIED, "missing scope: read"),
    };
    let identity_pubkey = keys.public_key().to_hex();
    let grant_db = super::dispatch::grant_db_path(app).ok();

    let Some(path) = grant_db.clone() else {
        return BridgeReply::err(code::DENIED, "missing scope: read");
    };
    let Ok(conn) = super::grants::open_grant_db(&path) else {
        return BridgeReply::err(code::DENIED, "missing scope: read");
    };
    let granted = super::grants::list_read_pairs(&conn, &identity_pubkey, extension_id);

    // Construction *is* the initial authority check: it refuses unless granted
    // pairs survive the request.
    let filters = match construct_filters(&granted, &request) {
        Ok(filters) => filters,
        Err(error) => return error.into_reply(),
    };

    let revalidation = QueryRevalidation {
        lease,
        extension_id,
        identity_at_entry: &identity_pubkey,
        pairs_at_entry: filters.pairs(),
        state: &state,
        grant_db: grant_db.clone(),
    };

    // The gate is process-global and unbounded from here, so authority checked
    // before it proves nothing about authority after it.
    crate::relay_admission::wait_for_rate_limit().await;
    if revalidation.check().is_err() {
        return BridgeReply::err(code::DENIED, "missing scope: read");
    }

    // No await between that recheck and the send. The keys are the ones the
    // grant admitted; nothing here re-reads `state` for identity. The send is
    // the constrained type's own method — the filters are never handed to the
    // generic relay helper, which would accept any values at all.
    let answered = filters.send(&state, &keys).await;

    // Boundary-1's precedence, on the failure branch: authority is rechecked
    // *before* the failure is classified, so a relay error can never outrank a
    // refusal for a caller who is no longer entitled to ask.
    let events = match answered {
        Ok(events) => events,
        Err(_) => {
            if revalidation.check().is_err() {
                return BridgeReply::err(code::DENIED, "missing scope: read");
            }
            return QueryError::Relay.into_reply();
        }
    };

    let verified = compose_results(
        events,
        &filters,
        &conn,
        &identity_pubkey,
        extension_id,
        request.limit,
    );

    // Order, truncate and serialise **before** the last recheck.
    //
    // The recheck has to be the last authority-bearing operation before the
    // reply, and none of this work is cheap: sorting and tiebreaking run over
    // every verified event, and `as_json` + reparse serialises up to the
    // overall cap. Rechecking first and then doing all of it reopens exactly
    // the window Boundary 1 closed — "no `await`" is not a mutex, and another
    // thread or another process writing the WAL-backed grant store can revoke
    // authority while synchronous work runs.
    use nostr::JsonUtil as _;
    let mut encoded: Result<Vec<Value>, ()> = Ok(Vec::with_capacity(verified.len()));
    for event in &verified {
        match (
            &mut encoded,
            serde_json::from_str::<Value>(&event.as_json()),
        ) {
            (Ok(out), Ok(value)) => out.push(value),
            (slot, Err(_)) => *slot = Err(()),
            (Err(_), _) => {}
        }
    }

    // Now, with the reply fully built and nothing left to do but hand it over.
    if revalidation.check().is_err() {
        return BridgeReply::err(code::DENIED, "missing scope: read");
    }

    // Authority outranks an internal encoding failure, for the same reason it
    // outranks a relay failure: a caller who is no longer entitled to ask must
    // not learn that the host had trouble encoding something it held.
    match encoded {
        Ok(out) => BridgeReply::ok(serde_json::json!({ "events": out })),
        Err(()) => BridgeReply::err(code::INTERNAL, "could not encode an event"),
    }
}

/// §5 `subscribe` — aggregation, quota and public-subscription lifecycle.
///
/// A **private** child, so it inherits this module's seal: it can reach
/// `construct_filters`/`matches_any`/`verify_event`, and nothing outside can
/// reach them through it.
mod subscription;

/// Who owns which live stream, and what dies with it.
///
/// A sibling of [`subscription`] rather than part of it only because the two
/// together exceed the repo's 1000-line ratchet; both are private children of
/// this module and the seal covers them identically.
mod registry;

/// End-to-end ACK/window flow control from Rust through the browser port.
mod flow;

/// The shared, authenticated relay socket the branches are opened on.
mod connection;

pub(crate) use flow::StreamBatch;
pub(crate) use registry::StreamSink;

/// §5 `unsubscribe({ sub }) → { ok }` — the crate-visible bridge handler.
///
/// Re-exported from the private child so `dispatch` can route to it without
/// the sealed internals becoming reachable.
pub(crate) fn unsubscribe(lease: &str, params: Option<Value>) -> BridgeReply {
    registry::unsubscribe(lease, params)
}

/// Production lease wall, called by the real frame-host lifecycle.
///
/// Teardown performs the physical relay CLOSE burst before dropping each
/// reservation, then emits one terminal batch for every activated stream. A
/// pre-reply stream keeps only a terminated tombstone until its exact receipt,
/// so `closed` cannot overtake the correlated reply. Idempotence comes from
/// registry removal or the tombstone's terminated state.
pub(crate) fn close_subscriptions_for_lease(lease: &str) -> usize {
    let closed =
        registry::registry().close_for_lease(lease, subscription::CloseReason::Unsubscribed);
    for (sink, delivery) in closed.deliveries {
        connection::deliver(&sink, delivery);
    }
    closed.closed
}

#[cfg(test)]
pub(crate) fn live_subscription_count_for_test() -> usize {
    registry::registry().live_count()
}

pub(crate) fn activate_subscription<R: tauri::Runtime>(
    app: &tauri::AppHandle<R>,
    lease: &str,
    sub: &str,
) {
    connection::activate_prepared(app, lease, sub);
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn acknowledge_subscription_batch<R: tauri::Runtime>(
    app: &tauri::AppHandle<R>,
    lease: &str,
    sub: &str,
    seq: u64,
    token: &str,
    frame_count: usize,
    encoded_bytes: usize,
) {
    connection::acknowledge_batch(app, lease, sub, seq, token, frame_count, encoded_bytes);
}

pub(crate) fn stream_flow_violation<R: tauri::Runtime>(
    app: &tauri::AppHandle<R>,
    lease: &str,
    sub: &str,
) {
    connection::report_flow_violation(app, lease, sub);
}

/// §5 `subscribe({ filter }) → { sub }` — the crate-visible bridge handler.
pub(crate) async fn subscribe<R: tauri::Runtime>(
    app: &tauri::AppHandle<R>,
    extension_id: &str,
    lease: &str,
    params: Option<Value>,
) -> BridgeReply {
    connection::subscribe(app, extension_id, lease, params).await
}

#[cfg(test)]
#[path = "query_tests.rs"]
mod query_tests;

#[cfg(test)]
#[path = "construction_tests.rs"]
mod construction_tests;

#[cfg(test)]
#[path = "query_authority_tests.rs"]
mod query_authority_tests;

#[cfg(test)]
#[path = "query_live_proof_tests.rs"]
mod query_live_proof_tests;
