//! HTTP bridge query helpers (`POST /query`).
//!
//! Split out of `relay.rs` verbatim: these bodies are byte-identical to the
//! ones that lived there, and the parent re-exports every name, so no call
//! site changed.

use super::*;

/// Execute a one-shot query via the relay's HTTP bridge (`POST /query`).
///
/// Filters are serialized as a JSON array. The request is authenticated with
/// a NIP-98 event signed by the user's keys. Returns the deserialized array of
/// events.
pub async fn query_relay(
    state: &AppState,
    filters: &[serde_json::Value],
) -> Result<Vec<nostr::Event>, String> {
    query_relay_at(state, &relay_api_base_url_with_override(state), filters).await
}

/// Like [`query_relay`] but targets an explicit HTTP API base URL instead of
/// the workspace override. Used when a query must hit a specific relay (e.g.
/// reconciling an agent's profile on the relay where it was published).
pub async fn query_relay_at(
    state: &AppState,
    api_base_url: &str,
    filters: &[serde_json::Value],
) -> Result<Vec<nostr::Event>, String> {
    crate::relay_admission::wait_for_rate_limit().await;
    let url = format!("{}/query", api_base_url);
    let body_bytes =
        serde_json::to_vec(filters).map_err(|e| format!("filter serialization failed: {e}"))?;
    let auth = build_nip98_auth_header(&Method::POST, &url, &body_bytes, state)?;

    let response = state
        .http_client
        .post(&url)
        .header("Authorization", auth)
        .header("Content-Type", "application/json")
        .body(body_bytes)
        .send()
        .await
        .map_err(|e| classify_request_error(&e))?;

    if !response.status().is_success() {
        return Err(relay_error_message(response).await);
    }

    parse_json_response(response).await
}

pub async fn query_relay_at_with_keys(
    state: &AppState,
    api_base_url: &str,
    filters: &[serde_json::Value],
    keys: &Keys,
    auth_tag: Option<&str>,
) -> Result<Vec<nostr::Event>, String> {
    crate::relay_admission::wait_for_rate_limit().await;
    query_relay_at_with_keys_no_wait(state, api_base_url, filters, keys, auth_tag).await
}

/// [`query_relay_at_with_keys`] without the admission wait.
///
/// **The caller must already have waited on the admission gate and revalidated
/// its authority afterwards.** That is the entire reason this exists: the gate
/// wait is unbounded, so authority checked before it can be stale by the time
/// the request goes out, and a caller that needs to recheck in between has
/// nowhere to stand if the wait is buried inside the send.
///
/// Keys are taken explicitly and are the ones used to sign NIP-98 — nothing
/// here re-reads `state` for identity. A caller that resolved its identity
/// through the authority gate therefore signs with *that* identity, not with
/// whatever `state.keys` holds by the time the request is built.
pub async fn query_relay_at_with_keys_no_wait(
    state: &AppState,
    api_base_url: &str,
    filters: &[serde_json::Value],
    keys: &Keys,
    auth_tag: Option<&str>,
) -> Result<Vec<nostr::Event>, String> {
    let response = send_query_no_wait(state, api_base_url, filters, keys, auth_tag).await?;
    parse_json_response(response).await
}

/// Send the query and hand back the **unread** response.
///
/// The shared half of [`query_relay_at_with_keys_no_wait`]: URL, body, NIP-98
/// signing with the explicit keys, the optional auth tag, and the status check
/// — everything except deciding how much of the body to believe.
///
/// It exists so a caller that must bound the *response* can do so without
/// re-implementing authentication. `parse_json_response` downloads and
/// deserialises the whole body, which is the right default for callers talking
/// to their own relay, and the wrong one for a caller whose threat model says
/// the relay is untrusted: by the time a cap could be applied to the parsed
/// vector, the allocation has already happened. Such a caller takes the
/// response from here and reads it under its own ceiling.
pub(crate) async fn send_query_no_wait(
    state: &AppState,
    api_base_url: &str,
    filters: &[serde_json::Value],
    keys: &Keys,
    auth_tag: Option<&str>,
) -> Result<reqwest::Response, String> {
    let url = format!("{}/query", api_base_url);
    let body_bytes =
        serde_json::to_vec(filters).map_err(|e| format!("filter serialization failed: {e}"))?;
    let auth = build_nip98_auth_header_for_keys(keys, &Method::POST, &url, &body_bytes)?;
    let mut request = state
        .http_client
        .post(&url)
        .header("Authorization", auth)
        .header("Content-Type", "application/json");
    if let Some(tag) = auth_tag {
        request = request.header("x-auth-tag", tag);
    }
    let response = request
        .body(body_bytes)
        .send()
        .await
        .map_err(|e| classify_request_error(&e))?;
    if !response.status().is_success() {
        return Err(relay_error_message(response).await);
    }
    Ok(response)
}
