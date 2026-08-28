//! The single producer of relay-facing filters — **and their only consumer**.
//!
//! This module is private, its items are `pub(super)`, and
//! [`ConstrainedFilters`]'s field is private to it. Crucially, the `Vec<Value>`
//! never leaves: there is no accessor that hands the filters out, so the
//! generic relay helper — which accepts any `&[serde_json::Value]` — cannot be
//! reached with them from anywhere in §5. Sending happens *inside* here, in
//! [`ConstrainedFilters::send`].
//!
//! An earlier revision exposed `as_filters()` and called the generic helper
//! from `query_events`. That documented a seal it did not have: unwrapping the
//! type one line before the send leaves an unconstrained read expressible, and
//! the type is then only a convention the current caller happens to honour.
//! R2 rejected exactly that shape — a check beside a freely constructible
//! value — so the containment is structural now rather than described.

use super::{is_channel_readable_kind, is_read_denied_kind, QueryError};
use super::{
    ValidatedRequest, Value, MAX_EMITTED_FILTERS, MAX_FETCHED_CANDIDATES, MAX_READ_PAIRS,
    MAX_RESPONSE_BYTES, MAX_REWRITTEN_QUERY_BYTES,
};

/// Relay filters built from granted pairs, and the pairs that produced them.
pub(super) struct ConstrainedFilters {
    filters: Vec<Value>,
    pairs: Vec<(u32, String)>,
}

impl ConstrainedFilters {
    /// The surviving pairs, for the post-response recheck and the verifier.
    pub(super) fn pairs(&self) -> &[(u32, String)] {
        &self.pairs
    }

    /// Test-only view of the emitted filters.
    ///
    /// **Deliberately `cfg(test)`.** Handing these out in production is
    /// what broke the seal: one `as_filters()` before the send and the
    /// generic relay helper is reachable again. Tests need the *shape* of
    /// what was built — one `#h` per filter, kinds grouped by channel, the
    /// scalar axes copied — which `matches_any` cannot show. Compiling it
    /// out of the shipped binary keeps the containment structural where it
    /// matters and honest where it does not.
    #[cfg(test)]
    pub(super) fn as_filters(&self) -> &[Value] {
        &self.filters
    }

    /// Does the event satisfy every axis of at least one filter that was
    /// actually built? Matching lives here because it needs the filters,
    /// and handing them out to do it elsewhere is what broke the seal.
    pub(super) fn matches_any(&self, event: &nostr::Event, channel: &str, kind: u32) -> bool {
        self.filters
            .iter()
            .any(|filter| super::matches_constructed_filter(event, filter, channel, kind))
    }

    /// Send these filters, and read the answer under a hard ceiling.
    ///
    /// **The only caller of the raw relay helper in the §5 path.** The
    /// generic `query_relay_at_with_keys_no_wait` downloads and
    /// deserialises the entire body before anything can cap it, which is
    /// fine for a caller talking to its own relay and wrong here: §5's
    /// whole premise is that the relay is untrusted. A hostile or defective
    /// one can ignore every emitted `limit` and answer with an arbitrarily
    /// large array, and `.take(N)` applied to the parsed vector is a cap on
    /// the wrong side of the allocation.
    ///
    /// So: refuse an over-cap `Content-Length` before reading, accumulate
    /// bytes under [`MAX_RESPONSE_BYTES`] and abort the moment it is
    /// exceeded, deserialise only those bytes, and **refuse** more than
    /// [`MAX_FETCHED_CANDIDATES`] events rather than silently keeping the
    /// first N — a relay that returns more than it was asked for is
    /// misbehaving, and quietly trimming its answer hides that.
    pub(super) async fn send(
        &self,
        state: &crate::AppState,
        keys: &nostr::Keys,
    ) -> Result<Vec<nostr::Event>, QueryError> {
        let mut response = crate::relay::send_query_no_wait(
            state,
            &crate::relay::relay_api_base_url_with_override(state),
            &self.filters,
            keys,
            None,
        )
        .await
        .map_err(|_| QueryError::Relay)?;

        if response
            .content_length()
            .is_some_and(|len| len > MAX_RESPONSE_BYTES as u64)
        {
            return Err(QueryError::Relay);
        }

        let mut body: Vec<u8> = Vec::new();
        while let Some(chunk) = response.chunk().await.map_err(|_| QueryError::Relay)? {
            if body.len().saturating_add(chunk.len()) > MAX_RESPONSE_BYTES {
                return Err(QueryError::Relay);
            }
            body.extend_from_slice(&chunk);
        }

        let events: Vec<nostr::Event> =
            serde_json::from_slice(&body).map_err(|_| QueryError::Relay)?;
        if events.len() > MAX_FETCHED_CANDIDATES {
            return Err(QueryError::Relay);
        }
        Ok(events)
    }
}

/// Build one filter per surviving channel, or refuse.
///
/// A granted `(k, c)` survives iff `k` is in the request's `kinds` (or
/// `kinds` is absent) and `c` is in the request's `#h` (or `#h` is absent).
/// Survivors are grouped by channel, so every emitted filter carries one
/// channel and only the kinds granted *in that channel* — the pairing is
/// preserved and the cross product is unreachable.
pub(super) fn construct_filters(
    granted: &[(u32, String)],
    request: &ValidatedRequest,
) -> Result<ConstrainedFilters, QueryError> {
    if granted.len() > MAX_READ_PAIRS {
        return Err(QueryError::QuotaExceeded(
            "this extension holds more read grants than a single query may span",
        ));
    }

    // Mixed requests fail whole (§5). A request naming any floor or
    // non-readable kind is refused outright rather than narrowed to the
    // remainder — silently answering a smaller question than the one asked
    // is how a caller comes to believe a kind is simply absent.
    if let Some(kinds) = request.kinds.as_ref() {
        for kind in kinds {
            if is_read_denied_kind(*kind) {
                return Err(QueryError::Denied(
                    "the request names a kind extensions may never read",
                ));
            }
            if !is_channel_readable_kind(*kind) {
                return Err(QueryError::Denied(
                    "the request names a kind that is not channel-readable",
                ));
            }
        }
    }

    // Survivors, grouped by channel and kept in the granted order so the
    // emitted query is deterministic for a given grant set.
    let mut grouped: Vec<(String, Vec<u32>)> = Vec::new();
    for (kind, channel) in granted {
        // The floor and the allowlist are re-checked against the *grant*
        // too. A row that predates a kind leaving the allowlist must not
        // become readable because validation only ran at install time.
        if is_read_denied_kind(*kind) || !is_channel_readable_kind(*kind) {
            continue;
        }
        if let Some(kinds) = request.kinds.as_ref() {
            if !kinds.contains(kind) {
                continue;
            }
        }
        if let Some(channels) = request.channels.as_ref() {
            if !channels.contains(channel) {
                continue;
            }
        }
        match grouped.iter_mut().find(|(existing, _)| existing == channel) {
            Some((_, kinds)) => {
                if !kinds.contains(kind) {
                    kinds.push(*kind);
                }
            }
            None => grouped.push((channel.clone(), vec![*kind])),
        }
    }

    // An all-ungranted request is an authority failure and must read as
    // one. An empty success would tell the extension the channel is empty,
    // which is a different and false statement.
    if grouped.is_empty() {
        return Err(QueryError::Denied(
            "missing scope: read (no granted kind/channel pair matches this filter)",
        ));
    }
    if grouped.len() > MAX_EMITTED_FILTERS {
        return Err(QueryError::QuotaExceeded(
            "the request spans more channels than one query may emit filters for",
        ));
    }
    // Bounds relay/DB work before any of it happens: the relay runs each
    // emitted filter with its own limit and appends the results.
    if grouped.len().saturating_mul(request.limit) > MAX_FETCHED_CANDIDATES {
        return Err(QueryError::QuotaExceeded(
            "the request would ask the relay for more candidate events than the host allows",
        ));
    }

    let mut filters = Vec::with_capacity(grouped.len());
    let mut pairs = Vec::new();
    for (channel, mut kinds) in grouped {
        kinds.sort_unstable();
        for kind in &kinds {
            pairs.push((*kind, channel.clone()));
        }
        let mut filter = serde_json::Map::new();
        filter.insert("kinds".to_string(), serde_json::json!(kinds));
        // Exactly one value. Not incidental — see the module header.
        filter.insert("#h".to_string(), serde_json::json!([channel]));
        if let Some(ids) = request.ids.as_ref() {
            filter.insert("ids".to_string(), serde_json::json!(ids));
        }
        if let Some(authors) = request.authors.as_ref() {
            filter.insert("authors".to_string(), serde_json::json!(authors));
        }
        for (name, values) in &request.tags {
            filter.insert(name.clone(), serde_json::json!(values));
        }
        if let Some(since) = request.since {
            filter.insert("since".to_string(), serde_json::json!(since));
        }
        if let Some(until) = request.until {
            filter.insert("until".to_string(), serde_json::json!(until));
        }
        filter.insert("limit".to_string(), serde_json::json!(request.limit));
        filters.push(Value::Object(filter));
    }

    let encoded = serde_json::to_vec(&filters)
        .map_err(|_| QueryError::QuotaExceeded("the rewritten query could not be encoded"))?;
    if encoded.len() > MAX_REWRITTEN_QUERY_BYTES {
        return Err(QueryError::QuotaExceeded(
            "the rewritten query exceeds the maximum size the host will send",
        ));
    }

    pairs.sort();
    Ok(ConstrainedFilters { filters, pairs })
}
