use buzz_core::kind::KIND_IA_ARCHIVED_LIST;
use buzz_core::peer_call::{
    derive_call_id, onward_context, PeerCallRoute, CALL_WINDOW_SECS, KIND_PEER_CALL,
    KIND_PEER_CALL_RESULT, MAX_FANOUT,
};
use buzz_sdk::builders::{
    build_archive_identity_request, build_peer_call, build_peer_call_result,
    build_unarchive_identity_request, PeerCallMeta,
};
use nostr::PublicKey;
use serde_json::json;
use uuid::Uuid;

use crate::agent_management::{build_create, build_update, CreateAgentDraft, UpdateAgentDraft};
use crate::client::BuzzClient;
use crate::error::CliError;
use crate::validate::{read_or_stdin, validate_hex64, validate_uuid};
use crate::{AgentsCmd, RespondToArg};

/// Resolve the one route form NIP-PC permits from the CLI's four options.
///
/// Clap already refuses `--channel` together with `--project`, and ties
/// `--thread` to `--channel` and `--root` to `--project`. What it cannot
/// express is that *one* of the two must be present, so that is checked here.
/// Returning an error rather than defaulting to anything is deliberate: a call
/// with no route names no conversation, and guessing one would publish a call
/// whose result lands somewhere the operator never asked for.
fn resolve_route(
    channel: Option<String>,
    thread: Option<String>,
    project: Option<String>,
    root: Option<String>,
) -> Result<PeerCallRoute, CliError> {
    match (channel, project, root) {
        (Some(channel), None, _) => {
            validate_uuid(&channel)?;
            if let Some(ref t) = thread {
                validate_hex64(t)?;
            }
            Ok(PeerCallRoute::Channel {
                channel: channel.to_ascii_lowercase(),
                thread_root: thread.map(|t| t.to_ascii_lowercase()),
            })
        }
        (None, Some(coordinate), Some(root)) => {
            validate_hex64(&root)?;
            Ok(PeerCallRoute::Project {
                coordinate,
                root: root.to_ascii_lowercase(),
            })
        }
        _ => Err(CliError::Usage(
            "give exactly one route: --channel [--thread], or --project --root".into(),
        )),
    }
}

// ── NIP-PC issuing gate ───────────────────────────────────────────────────────
//
// The fan-out ceiling has to be enforced *here*, before the call is signed and
// submitted. A caller that publishes first and counts afterwards has already
// invoked the callee: the task runs, the work is done, and the only thing left
// to decide is whether to listen to the answer. That is not a bound on fan-out,
// it is a bound on how many answers the caller is willing to hear.
//
// The harness cannot supply the count either. `buzz agents call` is a separate
// one-shot process from the ACP harness and shares no memory with it, so the
// authority on "what have I already published on this route" is the relay's own
// record. Reconstructing it from there also survives an agent restart, which
// an in-process counter does not.

/// The lone value of a tag on a stored event, or `None` if absent or repeated.
fn sole_tag(event: &serde_json::Value, name: &str) -> Option<String> {
    let mut found = event
        .get("tags")?
        .as_array()?
        .iter()
        .filter_map(|t| {
            let arr = t.as_array()?;
            (arr.first()?.as_str()? == name).then(|| arr.get(1)?.as_str().map(str::to_owned))?
        })
        .collect::<Vec<_>>();
    (found.len() == 1).then(|| found.remove(0))
}

/// The callee and id of a stored call **if it was made on `route`**.
///
/// Route membership is decided by recomputing the call id rather than by
/// re-reading the route tags, because the id is already derived from the route:
/// a call whose id recomputes under this route was made on this route, and one
/// that does not, was not. Reusing the derivation means there is no second route
/// parser here to disagree with the harness's.
fn call_on_route(
    event: &serde_json::Value,
    caller: &str,
    route: &PeerCallRoute,
) -> Option<(String, String)> {
    if event.get("kind")?.as_u64()? != u64::from(KIND_PEER_CALL) {
        return None;
    }
    let callee = sole_tag(event, "p")?.to_ascii_lowercase();
    let nonce = sole_tag(event, "nonce")?.to_ascii_lowercase();
    let call_id = sole_tag(event, "call")?.to_ascii_lowercase();
    (derive_call_id(caller, &callee, route, &nonce) == call_id).then_some((call_id, callee))
}

/// The ids of this caller's calls on `route` that nobody has answered.
///
/// A result frees the slot only when it is *the callee's* result: the caller
/// asked one specific agent, and a third party publishing a `43004` carrying the
/// same call id must not be able to buy back fan-out capacity on its behalf.
/// That check is the same one `admit_result` makes before it resumes anything,
/// so a result that would free a slot here is a result that would actually have
/// closed the call.
fn outstanding_call_ids(
    calls: &[serde_json::Value],
    results: &[serde_json::Value],
    caller: &str,
    route: &PeerCallRoute,
) -> Vec<String> {
    let caller = caller.to_ascii_lowercase();
    let answered: Vec<(String, String)> = results
        .iter()
        .filter(|e| {
            e.get("kind").and_then(serde_json::Value::as_u64)
                == Some(u64::from(KIND_PEER_CALL_RESULT))
        })
        .filter_map(|e| {
            let call_id = sole_tag(e, "call")?.to_ascii_lowercase();
            let author = e.get("pubkey")?.as_str()?.to_ascii_lowercase();
            Some((call_id, author))
        })
        .collect();

    let mut outstanding = Vec::new();
    for event in calls {
        let author = event
            .get("pubkey")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default()
            .to_ascii_lowercase();
        if author != caller {
            continue;
        }
        let Some((call_id, callee)) = call_on_route(event, &caller, route) else {
            continue;
        };
        let closed = answered
            .iter()
            .any(|(id, by)| *id == call_id && *by == callee);
        if !closed && !outstanding.contains(&call_id) {
            outstanding.push(call_id);
        }
    }
    outstanding
}

/// Refuse to publish an eleventh concurrent call on one route.
///
/// Runs before the envelope is built or signed, so a refusal means no event
/// exists: the callee is never invoked, rather than being invoked and ignored.
///
/// Fails **closed**. If the outstanding set cannot be established the call is
/// not published — a ceiling that yields to a failed query is not a ceiling, and
/// the relay that could not answer this query is the same one the call was about
/// to be submitted to.
async fn issuing_gate(
    client: &BuzzClient,
    caller: &str,
    route: &PeerCallRoute,
) -> Result<(), CliError> {
    let since = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs().saturating_sub(CALL_WINDOW_SECS))
        .unwrap_or(0);

    // One round trip, two filters ORed by the relay: the calls this caller
    // published, and the results addressed back to it.
    let raw = client
        .query_multi(&[
            json!({"kinds": [KIND_PEER_CALL], "authors": [caller], "since": since}),
            json!({"kinds": [KIND_PEER_CALL_RESULT], "#p": [caller], "since": since}),
        ])
        .await?;
    let events: Vec<serde_json::Value> = serde_json::from_str(&raw)
        .map_err(|e| CliError::Other(format!("could not read outstanding calls: {e}")))?;

    let outstanding = outstanding_call_ids(&events, &events, caller, route);
    if outstanding.len() >= MAX_FANOUT {
        return Err(CliError::Usage(format!(
            "fan-out limit reached: {} calls are already outstanding on this route \
             (maximum {MAX_FANOUT}). Wait for a result, or call from a different \
             conversation. A call is released after {CALL_WINDOW_SECS}s if it is \
             never answered.",
            outstanding.len()
        )));
    }
    Ok(())
}

pub async fn dispatch(command: AgentsCmd, client: &BuzzClient) -> Result<(), CliError> {
    match command {
        AgentsCmd::Call {
            to,
            task,
            channel,
            thread,
            project,
            root,
            visited,
            nonce,
        } => {
            validate_hex64(&to)?;
            let caller = client.keys().public_key().to_hex().to_ascii_lowercase();
            let callee = to.to_ascii_lowercase();
            let route = resolve_route(channel, thread, project, root)?;

            // Before anything is built, signed or submitted. A call that trips
            // the ceiling must never reach the relay — once it does, the callee
            // has been invoked and the limit is retrospective.
            issuing_gate(client, &caller, &route).await?;

            // A v4 UUID is 16 random bytes, which is exactly the nonce width.
            // Reusing it avoids adding a second source of randomness to a crate
            // that already has one that is fit for the purpose.
            let nonce = match nonce {
                Some(n) => n.to_ascii_lowercase(),
                None => Uuid::new_v4().simple().to_string(),
            };

            // The caller is always in its own call path, and the hop count is
            // the size of that path. Both come from the one shared derivation
            // rather than from operator input: a hand-written call that omits
            // itself, or states a hop that disagrees with its path, is refused
            // by every callee for a reason invisible from the command typed.
            for entry in &visited {
                validate_hex64(entry)?;
            }
            let (hop, path) = onward_context(&visited, &caller);

            let builder = build_peer_call(
                &caller,
                &read_or_stdin(&task)?,
                &PeerCallMeta {
                    callee: callee.clone(),
                    route: route.clone(),
                    nonce: nonce.clone(),
                    hop,
                    visited: path,
                },
            )
            .map_err(|e| CliError::Usage(format!("invalid call: {e}")))?;

            let event = client.sign_event_unchecked(builder)?;
            let event_id = event.id.to_hex();
            let call_id = derive_call_id(&caller, &callee, &route, &nonce);
            client.submit_event(event).await?;
            println!(
                "{}",
                json!({
                    "ok": true,
                    "event_id": event_id,
                    "call_id": call_id,
                    "callee": callee,
                    "hop": hop,
                })
            );
            Ok(())
        }

        AgentsCmd::CallResult {
            to,
            call,
            body,
            channel,
            thread,
            project,
            root,
        } => {
            validate_hex64(&to)?;
            validate_hex64(&call)?;
            let route = resolve_route(channel, thread, project, root)?;
            let builder = build_peer_call_result(
                &to.to_ascii_lowercase(),
                &call.to_ascii_lowercase(),
                &read_or_stdin(&body)?,
                &route,
            )
            .map_err(|e| CliError::Usage(format!("invalid result: {e}")))?;

            let event = client.sign_event_unchecked(builder)?;
            let event_id = event.id.to_hex();
            client.submit_event(event).await?;
            println!(
                "{}",
                json!({
                    "ok": true,
                    "event_id": event_id,
                    "call_id": call.to_ascii_lowercase(),
                    "to": to.to_ascii_lowercase(),
                })
            );
            Ok(())
        }

        AgentsCmd::DraftCreate {
            channel,
            display_name,
            system_prompt,
        } => {
            let owner = require_owner(client)?;
            let built = build_create(
                client.keys(),
                &owner,
                CreateAgentDraft {
                    channel_id: channel,
                    display_name,
                    system_prompt: read_or_stdin(&system_prompt)?,
                },
            )?;
            let response = client.publish_ephemeral_event(built.event).await?;
            let mut output: serde_json::Value = serde_json::from_str(&response)
                .map_err(|e| CliError::Other(format!("invalid relay response: {e}")))?;
            if let Some(obj) = output.as_object_mut() {
                obj.insert("request_id".into(), built.request_id.into());
                obj.insert("action".into(), built.action.into());
                obj.insert("saved".into(), false.into());
                obj.insert(
                    "message".into(),
                    "Draft sent to Buzz Desktop for owner review. Nothing changes until the owner saves it."
                        .into(),
                );
            }
            println!("{output}");
            Ok(())
        }

        AgentsCmd::DraftUpdate {
            channel,
            agent_name,
            display_name,
            system_prompt,
            runtime,
            provider,
            model,
            respond_to,
        } => {
            let owner = require_owner(client)?;
            let built = build_update(
                client.keys(),
                &owner,
                UpdateAgentDraft {
                    channel_id: channel,
                    agent_name,
                    display_name,
                    system_prompt: system_prompt.map(|v| read_or_stdin(&v)).transpose()?,
                    runtime,
                    provider,
                    model,
                    respond_to: respond_to.map(RespondToArg::to_wire),
                },
            )?;
            let response = client.publish_ephemeral_event(built.event).await?;
            let mut output: serde_json::Value = serde_json::from_str(&response)
                .map_err(|e| CliError::Other(format!("invalid relay response: {e}")))?;
            if let Some(obj) = output.as_object_mut() {
                obj.insert("request_id".into(), built.request_id.into());
                obj.insert("action".into(), built.action.into());
                obj.insert("saved".into(), false.into());
                obj.insert(
                    "message".into(),
                    "Draft sent to Buzz Desktop for owner review. Nothing changes until the owner saves it."
                        .into(),
                );
            }
            println!("{output}");
            Ok(())
        }

        AgentsCmd::Archive {
            target_pubkey,
            reason,
            replaced_by,
            content,
        } => {
            validate_hex64(&target_pubkey)?;
            let signer_hex = client.keys().public_key().to_hex();
            let auth = resolve_auth(client, &target_pubkey, &signer_hex).await?;
            let builder = build_archive_identity_request(
                &target_pubkey,
                &content,
                reason.as_deref(),
                replaced_by.as_deref(),
                auth.as_ref(),
            )
            .map_err(|e| CliError::Usage(format!("invalid archive request: {e}")))?;
            let event = client.sign_event_unchecked(builder)?;
            let event_id = event.id.to_hex();
            client.submit_event(event).await?;
            println!(
                "{}",
                json!({
                    "ok": true,
                    "event_id": event_id,
                    "action": "archive",
                    "target": target_pubkey,
                })
            );
            Ok(())
        }

        AgentsCmd::Unarchive {
            target_pubkey,
            reason,
            content,
        } => {
            validate_hex64(&target_pubkey)?;
            let signer_hex = client.keys().public_key().to_hex();
            let auth = resolve_auth(client, &target_pubkey, &signer_hex).await?;
            let builder = build_unarchive_identity_request(
                &target_pubkey,
                &content,
                reason.as_deref(),
                auth.as_ref(),
            )
            .map_err(|e| CliError::Usage(format!("invalid unarchive request: {e}")))?;
            let event = client.sign_event_unchecked(builder)?;
            let event_id = event.id.to_hex();
            client.submit_event(event).await?;
            println!(
                "{}",
                json!({
                    "ok": true,
                    "event_id": event_id,
                    "action": "unarchive",
                    "target": target_pubkey,
                })
            );
            Ok(())
        }

        AgentsCmd::Archived => cmd_archived(client).await,
    }
}

/// Require `BUZZ_AUTH_TAG` and parse the owner pubkey from it. Used only by
/// the `draft-create` and `draft-update` paths.
fn require_owner(client: &BuzzClient) -> Result<PublicKey, CliError> {
    let hex = client
        .auth_tag_owner_hex()
        .ok_or_else(|| CliError::Auth("agent draft requests require BUZZ_AUTH_TAG".into()))?;
    PublicKey::parse(&hex).map_err(|e| CliError::Auth(format!("invalid owner attestation: {e}")))
}

/// Resolve the optional NIP-OA `auth` tag for archive/unarchive requests.
///
/// Mirrors the desktop's `maybe_owner_auth_tag`:
/// - `target == signer`: self path — no auth needed → `Ok(None)`.
/// - Otherwise: fetch target's kind:0, look for an `auth` tag whose owner
///   (index 1) matches the signer. Return it when present; `Ok(None)` when
///   absent or structurally malformed. Query/network failures surface as
///   `Err` — silent degradation to bare would make the relay reject the
///   request with a misleading error.
async fn resolve_auth(
    client: &BuzzClient,
    target_hex: &str,
    signer_hex: &str,
) -> Result<Option<[String; 4]>, CliError> {
    if target_hex.eq_ignore_ascii_case(signer_hex) {
        return Ok(None);
    }
    let filter = json!({"kinds": [0], "authors": [target_hex], "limit": 1});
    let raw = client
        .query(&filter)
        .await
        .map_err(|e| CliError::Other(format!("failed to fetch target kind:0: {e}")))?;
    let events: Vec<serde_json::Value> = serde_json::from_str(&raw)
        .map_err(|e| CliError::Other(format!("invalid kind:0 query response: {e}")))?;
    let event = match events.into_iter().next() {
        Some(e) => e,
        None => return Ok(None),
    };
    let tags = match event.get("tags").and_then(|v| v.as_array()) {
        Some(t) => t,
        None => return Ok(None),
    };
    Ok(extract_owner_auth_tag(tags, signer_hex))
}

/// Pure extraction helper: require exactly one kind:0 tag whose first
/// element is `"auth"` (a set-level rule — a valid tag alongside a second
/// malformed or duplicate `auth`-labeled tag is bare, not the valid one),
/// then structurally validate that sole tag as
/// `["auth", owner, conditions, sig]` matching `signer_hex`.
///
/// Malformed tags (wrong arity, non-string elements, non-hex fields) are
/// silently skipped — the contract is "bare" (None), not error.
fn extract_owner_auth_tag(tags: &[serde_json::Value], signer_hex: &str) -> Option<[String; 4]> {
    let auth_tags: Vec<&serde_json::Value> = tags
        .iter()
        .filter(|tag| {
            tag.as_array()
                .and_then(|elems| elems.first())
                .and_then(|v| v.as_str())
                == Some("auth")
        })
        .collect();
    if auth_tags.len() != 1 {
        return None;
    }

    let elems = auth_tags[0].as_array()?;
    if elems.len() != 4 {
        return None;
    }
    let label = elems[0].as_str()?;
    let owner = elems[1].as_str()?;
    if !owner.eq_ignore_ascii_case(signer_hex) {
        return None;
    }
    let conditions = elems[2].as_str()?;
    let sig = elems[3].as_str()?;
    if owner.len() != 64
        || !owner.chars().all(|c| c.is_ascii_hexdigit())
        || sig.len() != 128
        || !sig.chars().all(|c| c.is_ascii_hexdigit())
    {
        return None;
    }
    Some([
        label.to_owned(),
        owner.to_owned(),
        conditions.to_owned(),
        sig.to_owned(),
    ])
}

/// Validate the NIP-11 relay-info `self` field is a 64-hex pubkey and
/// normalize it to lowercase, so the archived-identities query filter and
/// the author comparison in [`verify_archived_event`] agree regardless of
/// the case the relay published `self` in.
fn normalize_relay_self_hex(self_hex: &str) -> Result<String, CliError> {
    if self_hex.len() != 64 || !self_hex.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err(CliError::Other(format!(
            "relay 'self' field is not a valid 64-hex pubkey: {self_hex}"
        )));
    }
    Ok(self_hex.to_ascii_lowercase())
}

/// Fetch and verify the relay's NIP-IA archived-identities snapshot (kind
/// 13535). Shared by `cmd_archived` (trust failures are fatal — verifying
/// repair state is the command's whole purpose) and the `--template`
/// resolver's archive filter, which fails open on a trust failure instead
/// (see `channels::resolve_roster_with_archive_filter`'s doc comment for
/// why).
///
/// Three trust states:
/// - State 1: no events — `Ok(vec![])`
/// - State 2: event passes all checks — `Ok(<pubkeys>)`
/// - State 3: trust failure — `Err`, naming the specific failure
pub(crate) async fn fetch_archived_snapshot(client: &BuzzClient) -> Result<Vec<String>, CliError> {
    // Fetch NIP-11 info to get the relay's self pubkey.
    let nip11_raw = client
        .get_public("/")
        .await
        .map_err(|e| CliError::Other(format!("failed to fetch relay info document: {e}")))?;
    let nip11: serde_json::Value = serde_json::from_str(&nip11_raw)
        .map_err(|e| CliError::Other(format!("relay info document is not valid JSON: {e}")))?;
    let self_hex = nip11
        .get("self")
        .and_then(|v| v.as_str())
        .ok_or_else(|| CliError::Other("relay info document missing 'self' field".into()))?;
    let self_hex = normalize_relay_self_hex(self_hex)?;

    // Query for the archived-identities list.
    let filter = json!({"kinds": [KIND_IA_ARCHIVED_LIST], "authors": [self_hex], "limit": 1});
    let raw = client
        .query(&filter)
        .await
        .map_err(|e| CliError::Other(format!("failed to query archived-identities list: {e}")))?;
    let events: Vec<serde_json::Value> = serde_json::from_str(&raw)
        .map_err(|e| CliError::Other(format!("invalid query response: {e}")))?;

    // State 1: no events.
    if events.is_empty() {
        return Ok(Vec::new());
    }

    // State 2 or 3: verify then collect.
    let raw_event = events.into_iter().next().unwrap();
    let event: nostr::Event = serde_json::from_value(raw_event)
        .map_err(|e| CliError::Other(format!("archived-identities event is malformed: {e}")))?;
    let archived = verify_archived_event(&event, &self_hex)?;

    Ok(archived.into_iter().map(str::to_string).collect())
}

/// `buzz agents archived`: read path over [`fetch_archived_snapshot`] for
/// direct invocation — a trust failure (state 3) is fatal here so a
/// verification command can never look like success.
async fn cmd_archived(client: &BuzzClient) -> Result<(), CliError> {
    let archived = fetch_archived_snapshot(client).await?;
    println!("{}", json!({"archived": archived}));
    Ok(())
}

/// Pure verification of a kind:13535 archived-identities event.
///
/// Returns the list of valid hex64 pubkeys from `p` tags on success, or a
/// named trust-failure error (State 3).
fn verify_archived_event<'a>(
    event: &'a nostr::Event,
    relay_self_hex: &str,
) -> Result<Vec<&'a str>, CliError> {
    if event.kind != nostr::Kind::Custom(KIND_IA_ARCHIVED_LIST as u16) {
        return Err(CliError::Other(format!(
            "archived-identities event has wrong kind: {}",
            event.kind.as_u16()
        )));
    }

    if event.pubkey.to_hex() != relay_self_hex {
        return Err(CliError::Other(format!(
            "archived-identities event author {} does not match relay self {}",
            event.pubkey.to_hex(),
            relay_self_hex
        )));
    }

    let mut nip70_count = 0usize;
    for t in event.tags.iter() {
        let s = t.as_slice();
        if s.first().map(String::as_str) != Some("-") {
            continue;
        }
        if s.len() != 1 {
            return Err(CliError::Other(
                "archived-identities event has a malformed NIP-70 '-' tag (expected arity 1)"
                    .into(),
            ));
        }
        nip70_count += 1;
    }
    if nip70_count != 1 {
        return Err(CliError::Other(format!(
            "archived-identities event must have exactly one NIP-70 '-' tag, found {nip70_count}"
        )));
    }

    event.verify().map_err(|e| {
        CliError::Other(format!(
            "archived-identities event failed cryptographic verification: {e}"
        ))
    })?;

    let archived: Vec<&str> = event
        .tags
        .iter()
        .filter_map(|t| {
            let s = t.as_slice();
            if s.first().map(String::as_str) == Some("p") {
                let pk = s.get(1).map(String::as_str)?;
                if pk.len() == 64 && pk.chars().all(|c| c.is_ascii_hexdigit()) {
                    return Some(pk);
                }
            }
            None
        })
        .collect();

    Ok(archived)
}

#[cfg(test)]
mod tests {
    use super::*;
    use buzz_core::kind::KIND_IA_ARCHIVED_LIST;
    use nostr::{EventBuilder, Keys, Kind, Tag};
    use serde_json::json;

    fn hex64(c: char) -> String {
        std::iter::repeat_n(c, 64).collect()
    }

    fn hex128(c: char) -> String {
        std::iter::repeat_n(c, 128).collect()
    }

    // --- (b) auth-selection matrix: extract_owner_auth_tag ---

    #[test]
    fn auth_selection_owner_match_returns_tag() {
        let signer = hex64('a');
        let sig = hex128('b');
        let tags = vec![json!(["auth", signer, "conditions", sig])];
        let result = extract_owner_auth_tag(&tags, &signer);
        assert!(result.is_some());
        let tag = result.unwrap();
        assert_eq!(tag[0], "auth");
        assert_eq!(tag[1], signer);
        assert_eq!(tag[2], "conditions");
        assert_eq!(tag[3], sig);
    }

    #[test]
    fn auth_selection_non_owner_returns_none() {
        let signer = hex64('a');
        let other_owner = hex64('b');
        let tags = vec![json!(["auth", other_owner, "", hex128('c')])];
        assert!(extract_owner_auth_tag(&tags, &signer).is_none());
    }

    #[test]
    fn auth_selection_malformed_three_elements_returns_none() {
        let signer = hex64('a');
        let tags = vec![json!(["auth", signer, "conditions"])];
        assert!(extract_owner_auth_tag(&tags, &signer).is_none());
    }

    #[test]
    fn auth_selection_malformed_five_elements_returns_none() {
        let signer = hex64('a');
        let tags = vec![json!(["auth", signer, "conditions", hex128('b'), "extra"])];
        assert!(extract_owner_auth_tag(&tags, &signer).is_none());
    }

    #[test]
    fn auth_selection_malformed_non_hex_owner_returns_none() {
        let signer = "z".repeat(64);
        let tags = vec![json!(["auth", signer, "", hex128('a')])];
        assert!(extract_owner_auth_tag(&tags, &signer).is_none());
    }

    #[test]
    fn auth_selection_malformed_non_hex_sig_returns_none() {
        let signer = hex64('a');
        let bad_sig = "z".repeat(128);
        let tags = vec![json!(["auth", signer, "", bad_sig])];
        assert!(extract_owner_auth_tag(&tags, &signer).is_none());
    }

    #[test]
    fn auth_selection_malformed_short_sig_returns_none() {
        let signer = hex64('a');
        let short_sig = hex128('a')[..64].to_string();
        let tags = vec![json!(["auth", signer, "", short_sig])];
        assert!(extract_owner_auth_tag(&tags, &signer).is_none());
    }

    #[test]
    fn auth_selection_case_insensitive_owner_match() {
        let signer_lower = hex64('a');
        let signer_upper = signer_lower.to_uppercase();
        let sig = hex128('b');
        let tags = vec![json!(["auth", signer_upper, "cond", sig])];
        let result = extract_owner_auth_tag(&tags, &signer_lower);
        assert!(result.is_some());
    }

    #[test]
    fn auth_selection_non_string_elements_returns_none() {
        let signer = hex64('a');
        let tags = vec![json!(["auth", signer, 42, hex128('b')])];
        assert!(extract_owner_auth_tag(&tags, &signer).is_none());
    }

    #[test]
    fn auth_selection_non_array_tag_skipped() {
        let signer = hex64('a');
        let tags = vec![
            json!("not an array"),
            json!(["auth", signer, "", hex128('b')]),
        ];
        let result = extract_owner_auth_tag(&tags, &signer);
        assert!(result.is_some());
    }

    #[test]
    fn auth_selection_no_tags_returns_none() {
        assert!(extract_owner_auth_tag(&[], &hex64('a')).is_none());
    }

    #[test]
    fn auth_selection_wrong_label_returns_none() {
        let signer = hex64('a');
        let tags = vec![json!(["delegation", signer, "", hex128('b')])];
        assert!(extract_owner_auth_tag(&tags, &signer).is_none());
    }

    #[test]
    fn auth_selection_valid_plus_duplicate_auth_tag_returns_none() {
        // Set-level rule (F6): a structurally valid, owner-matching `auth`
        // tag alongside a second `auth`-labeled tag (malformed or a
        // duplicate) must not be selected — the whole kind:0 is bare.
        let signer = hex64('a');
        let sig = hex128('b');
        let tags = vec![
            json!(["auth", signer, "conditions", sig]),
            json!(["auth", signer, "conditions", sig]),
        ];
        assert!(extract_owner_auth_tag(&tags, &signer).is_none());
    }

    #[test]
    fn auth_selection_valid_plus_malformed_second_auth_tag_returns_none() {
        let signer = hex64('a');
        let sig = hex128('b');
        let tags = vec![
            json!(["auth", signer, "conditions", sig]),
            json!(["auth", "not-hex", "conditions"]),
        ];
        assert!(extract_owner_auth_tag(&tags, &signer).is_none());
    }

    // --- (d) NIP-11 self normalization: normalize_relay_self_hex ---

    #[test]
    fn normalize_self_lowercases_uppercase_hex() {
        let upper = hex64('A');
        let result = normalize_relay_self_hex(&upper).expect("should pass");
        assert_eq!(result, hex64('a'));
    }

    #[test]
    fn normalize_self_rejects_wrong_length() {
        assert!(normalize_relay_self_hex(&hex64('a')[..63]).is_err());
    }

    #[test]
    fn normalize_self_rejects_non_hex() {
        assert!(normalize_relay_self_hex(&"z".repeat(64)).is_err());
    }

    #[test]
    fn archived_uppercase_self_matches_lowercase_event_author() {
        // F7: an uppercase NIP-11 `self` must still resolve to the same
        // relay identity as the event's (always-lowercase) author hex once
        // normalized — before the fix this was a case-sensitive mismatch.
        let keys = Keys::generate();
        let self_hex_lower = keys.public_key().to_hex();
        let self_hex_upper = self_hex_lower.to_uppercase();
        let normalized = normalize_relay_self_hex(&self_hex_upper).expect("valid hex");
        let event = build_archived_event(&keys, KIND_IA_ARCHIVED_LIST as u16, &[], true);
        let result = verify_archived_event(&event, &normalized).expect("should pass");
        assert!(result.is_empty());
    }

    // --- (c) snapshot tri-state: verify_archived_event ---

    fn build_archived_event(
        keys: &Keys,
        kind: u16,
        p_tags: &[&str],
        include_nip70: bool,
    ) -> nostr::Event {
        let mut tags: Vec<Tag> = Vec::new();
        if include_nip70 {
            tags.push(Tag::parse(["-"]).unwrap());
        }
        for pk in p_tags {
            tags.push(Tag::parse(["p", pk]).unwrap());
        }
        EventBuilder::new(Kind::Custom(kind), "")
            .tags(tags)
            .sign_with_keys(keys)
            .expect("sign")
    }

    #[test]
    fn archived_state2_valid_event_returns_pubkeys() {
        let keys = Keys::generate();
        let self_hex = keys.public_key().to_hex();
        let pk1 = hex64('a');
        let pk2 = hex64('b');
        let event = build_archived_event(&keys, KIND_IA_ARCHIVED_LIST as u16, &[&pk1, &pk2], true);
        let result = verify_archived_event(&event, &self_hex).expect("should pass");
        assert_eq!(result, vec![pk1.as_str(), pk2.as_str()]);
    }

    #[test]
    fn archived_state2_empty_p_tags_returns_empty() {
        let keys = Keys::generate();
        let self_hex = keys.public_key().to_hex();
        let event = build_archived_event(&keys, KIND_IA_ARCHIVED_LIST as u16, &[], true);
        let result = verify_archived_event(&event, &self_hex).expect("should pass");
        assert!(result.is_empty());
    }

    #[test]
    fn archived_state3_wrong_kind_errors() {
        let keys = Keys::generate();
        let self_hex = keys.public_key().to_hex();
        let event = build_archived_event(&keys, 9999, &[], true);
        let err = verify_archived_event(&event, &self_hex).unwrap_err();
        assert!(
            err.to_string().contains("wrong kind"),
            "error should name wrong kind: {err}"
        );
    }

    #[test]
    fn archived_state3_wrong_author_errors() {
        let event_keys = Keys::generate();
        let other_self = hex64('f');
        let event = build_archived_event(&event_keys, KIND_IA_ARCHIVED_LIST as u16, &[], true);
        let err = verify_archived_event(&event, &other_self).unwrap_err();
        assert!(
            err.to_string().contains("does not match relay self"),
            "error should name author mismatch: {err}"
        );
    }

    #[test]
    fn archived_state3_no_nip70_tag_errors() {
        let keys = Keys::generate();
        let self_hex = keys.public_key().to_hex();
        let event = build_archived_event(&keys, KIND_IA_ARCHIVED_LIST as u16, &[], false);
        let err = verify_archived_event(&event, &self_hex).unwrap_err();
        assert!(
            err.to_string().contains("NIP-70"),
            "error should name missing NIP-70 tag: {err}"
        );
    }

    #[test]
    fn archived_state3_duplicate_nip70_tags_errors() {
        let keys = Keys::generate();
        let self_hex = keys.public_key().to_hex();
        let event = EventBuilder::new(Kind::Custom(KIND_IA_ARCHIVED_LIST as u16), "")
            .tags([Tag::parse(["-"]).unwrap(), Tag::parse(["-"]).unwrap()])
            .sign_with_keys(&keys)
            .expect("sign");
        let err = verify_archived_event(&event, &self_hex).unwrap_err();
        assert!(
            err.to_string().contains("found 2"),
            "error should report 2 NIP-70 tags: {err}"
        );
    }

    #[test]
    fn archived_state3_lone_malformed_nip70_tag_errors() {
        let keys = Keys::generate();
        let self_hex = keys.public_key().to_hex();
        let event = EventBuilder::new(Kind::Custom(KIND_IA_ARCHIVED_LIST as u16), "")
            .tags([Tag::parse(["-", "extra"]).unwrap()])
            .sign_with_keys(&keys)
            .expect("sign");
        let err = verify_archived_event(&event, &self_hex).unwrap_err();
        assert!(
            err.to_string().contains("malformed NIP-70"),
            "error should name the malformed NIP-70 tag: {err}"
        );
    }

    #[test]
    fn archived_state3_exact_marker_plus_malformed_marker_errors() {
        // F5 (IMPORTANT, discriminating): a valid `["-"]` alongside a
        // malformed `["-", "extra"]` must still poison the snapshot — the
        // old count-of-exact-shape-only check let this bypass through with
        // nip70_count == 1.
        let keys = Keys::generate();
        let self_hex = keys.public_key().to_hex();
        let event = EventBuilder::new(Kind::Custom(KIND_IA_ARCHIVED_LIST as u16), "")
            .tags([
                Tag::parse(["-"]).unwrap(),
                Tag::parse(["-", "extra"]).unwrap(),
            ])
            .sign_with_keys(&keys)
            .expect("sign");
        let err = verify_archived_event(&event, &self_hex).unwrap_err();
        assert!(
            err.to_string().contains("malformed NIP-70"),
            "error should name the malformed NIP-70 tag: {err}"
        );
    }

    #[test]
    fn archived_non_hex_p_tag_dropped() {
        let keys = Keys::generate();
        let self_hex = keys.public_key().to_hex();
        let valid_pk = hex64('a');
        let event = EventBuilder::new(Kind::Custom(KIND_IA_ARCHIVED_LIST as u16), "")
            .tags([
                Tag::parse(["-"]).unwrap(),
                Tag::parse(["p", &valid_pk]).unwrap(),
                Tag::parse(["p", "not-hex-at-all"]).unwrap(),
                Tag::parse(["p", &"z".repeat(64)]).unwrap(),
            ])
            .sign_with_keys(&keys)
            .expect("sign");
        let result = verify_archived_event(&event, &self_hex).expect("should pass");
        assert_eq!(result, vec![valid_pk.as_str()]);
    }

    #[test]
    fn archived_short_p_tag_dropped() {
        let keys = Keys::generate();
        let self_hex = keys.public_key().to_hex();
        let event = EventBuilder::new(Kind::Custom(KIND_IA_ARCHIVED_LIST as u16), "")
            .tags([
                Tag::parse(["-"]).unwrap(),
                Tag::parse(["p", &hex64('a')[..32]]).unwrap(),
            ])
            .sign_with_keys(&keys)
            .expect("sign");
        let result = verify_archived_event(&event, &self_hex).expect("should pass");
        assert!(result.is_empty());
    }

    // ── NIP-PC route resolution ──────────────────────────────────────────────

    const CHANNEL: &str = "8f377516-7391-47bf-bcc4-249a1028b212";

    #[test]
    fn a_channel_route_carries_its_optional_thread() {
        assert_eq!(
            resolve_route(Some(CHANNEL.into()), None, None, None).unwrap(),
            PeerCallRoute::Channel {
                channel: CHANNEL.into(),
                thread_root: None,
            }
        );
        assert_eq!(
            resolve_route(
                Some(CHANNEL.into()),
                Some(hex64('b').to_uppercase()),
                None,
                None
            )
            .unwrap(),
            PeerCallRoute::Channel {
                channel: CHANNEL.into(),
                thread_root: Some(hex64('b')),
            },
            "a thread root is normalised, because the call id is derived from it"
        );
    }

    #[test]
    fn a_project_route_needs_its_root() {
        let coordinate = format!("30617:{}:buzz", hex64('a'));
        assert_eq!(
            resolve_route(None, None, Some(coordinate.clone()), Some(hex64('c'))).unwrap(),
            PeerCallRoute::Project {
                coordinate,
                root: hex64('c'),
            }
        );
    }

    /// No route names no conversation. Clap refuses `--channel` with
    /// `--project`; what it cannot express is that one of them is required, so
    /// the absence of both has to fail here rather than default to anything.
    #[test]
    fn a_call_with_no_route_is_a_usage_error() {
        assert!(matches!(
            resolve_route(None, None, None, None),
            Err(CliError::Usage(_))
        ));
        assert!(matches!(
            resolve_route(None, None, Some("30617:x:buzz".into()), None),
            Err(CliError::Usage(_))
        ));
    }

    #[test]
    fn a_malformed_route_component_is_refused_before_anything_is_published() {
        assert!(resolve_route(Some("not-a-uuid".into()), None, None, None).is_err());
        assert!(resolve_route(Some(CHANNEL.into()), Some("short".into()), None, None).is_err());
        assert!(resolve_route(
            None,
            None,
            Some(format!("30617:{}:buzz", hex64('a'))),
            Some("short".into())
        )
        .is_err());
    }
}

/// The NIP-PC fan-out ceiling, proved where it has to hold: before publication.
///
/// Every test here drives the real `dispatch(AgentsCmd::Call { .. })` against a
/// local HTTP relay and counts what reached `/events`. The assertion that
/// matters is not that an error was returned — it is that the refused call
/// produced **no event**, so the callee was never invoked and never did the
/// work. A ceiling that discards the answer to a task that already ran is not a
/// ceiling, and a test that only checks the error message cannot tell the two
/// apart.
#[cfg(test)]
mod issuing_gate_tests {
    use std::net::SocketAddr;
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::sync::Arc;

    use axum::extract::State;
    use axum::routing::post;
    use axum::{Json, Router};
    use buzz_sdk::builders::{build_peer_call, build_peer_call_result, PeerCallMeta};
    use nostr::Keys;
    use serde_json::Value;
    use tokio::net::TcpListener;

    use super::*;
    use crate::AgentsCmd;

    const CHANNEL: &str = "8f377516-7391-47bf-bcc4-249a1028b212";
    const OTHER_CHANNEL: &str = "1b2c3d4e-5f60-4718-8293-a4b5c6d7e8f9";

    #[derive(Clone)]
    struct Relay {
        stored: Arc<Vec<Value>>,
        published: Arc<AtomicU32>,
    }

    /// A relay that answers `/query` from a fixed history and counts `/events`.
    async fn relay_with(stored: Vec<Value>) -> (String, Arc<AtomicU32>) {
        let published = Arc::new(AtomicU32::new(0));
        let state = Relay {
            stored: Arc::new(stored),
            published: published.clone(),
        };
        let app = Router::new()
            .route(
                "/query",
                post(|State(s): State<Relay>, _body: String| async move {
                    Json(Value::Array(s.stored.as_ref().clone()))
                }),
            )
            .route(
                "/events",
                post(|State(s): State<Relay>, _body: String| async move {
                    s.published.fetch_add(1, Ordering::SeqCst);
                    Json(serde_json::json!({"accepted": true, "message": ""}))
                }),
            )
            .with_state(state);

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr: SocketAddr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        (format!("http://{addr}"), published)
    }

    fn channel_route(uuid: &str) -> PeerCallRoute {
        PeerCallRoute::Channel {
            channel: uuid.to_string(),
            thread_root: None,
        }
    }

    /// A published call, exactly as this CLI would have written it.
    fn stored_call(caller: &Keys, callee: &Keys, route: &PeerCallRoute, i: usize) -> Value {
        let caller_hex = caller.public_key().to_hex().to_ascii_lowercase();
        let (hop, visited) = onward_context(&[], &caller_hex);
        let event = build_peer_call(
            &caller_hex,
            "an earlier task",
            &PeerCallMeta {
                callee: callee.public_key().to_hex().to_ascii_lowercase(),
                route: route.clone(),
                nonce: format!("{i:032x}"),
                hop,
                visited,
            },
        )
        .expect("well-formed call")
        .sign_with_keys(caller)
        .expect("sign");
        serde_json::to_value(event).expect("serialise")
    }

    fn call_id_of(event: &Value) -> String {
        sole_tag(event, "call").expect("a call carries its id")
    }

    fn stored_result(
        answerer: &Keys,
        caller: &Keys,
        call_id: &str,
        route: &PeerCallRoute,
    ) -> Value {
        let event = build_peer_call_result(
            &caller.public_key().to_hex().to_ascii_lowercase(),
            call_id,
            "done",
            route,
        )
        .expect("well-formed result")
        .sign_with_keys(answerer)
        .expect("sign");
        serde_json::to_value(event).expect("serialise")
    }

    fn call_command(to: &Keys, channel: &str) -> AgentsCmd {
        AgentsCmd::Call {
            to: to.public_key().to_hex(),
            task: "one more thing".into(),
            channel: Some(channel.to_string()),
            thread: None,
            project: None,
            root: None,
            visited: vec![],
            nonce: None,
        }
    }

    /// Ten outstanding calls on one route publish; the eleventh does not exist.
    #[tokio::test]
    async fn the_eleventh_concurrent_call_never_reaches_the_relay() {
        let caller = Keys::generate();
        let callee = Keys::generate();
        let route = channel_route(CHANNEL);
        let history: Vec<Value> = (0..MAX_FANOUT)
            .map(|i| stored_call(&caller, &callee, &route, i))
            .collect();

        let (url, published) = relay_with(history).await;
        let client = BuzzClient::new(url, caller.clone(), None, None).unwrap();

        let err = dispatch(call_command(&callee, CHANNEL), &client)
            .await
            .expect_err("the eleventh call must be refused");
        assert!(
            matches!(err, CliError::Usage(ref m) if m.contains("fan-out limit")),
            "unexpected error: {err:?}"
        );
        assert_eq!(
            published.load(Ordering::SeqCst),
            0,
            "a refused call still reached the relay — the callee would have run it"
        );
    }

    /// The control. One slot short of the ceiling, the same command publishes —
    /// so the refusal above is the ceiling and not a broken call path.
    #[tokio::test]
    async fn the_tenth_concurrent_call_publishes_normally() {
        let caller = Keys::generate();
        let callee = Keys::generate();
        let route = channel_route(CHANNEL);
        let history: Vec<Value> = (0..MAX_FANOUT - 1)
            .map(|i| stored_call(&caller, &callee, &route, i))
            .collect();

        let (url, published) = relay_with(history).await;
        let client = BuzzClient::new(url, caller.clone(), None, None).unwrap();

        dispatch(call_command(&callee, CHANNEL), &client)
            .await
            .expect("under the ceiling");
        assert_eq!(published.load(Ordering::SeqCst), 1);
    }

    /// A correlated result frees the slot it occupied.
    #[tokio::test]
    async fn a_completed_call_returns_its_slot_to_the_route() {
        let caller = Keys::generate();
        let callee = Keys::generate();
        let route = channel_route(CHANNEL);
        let mut history: Vec<Value> = (0..MAX_FANOUT)
            .map(|i| stored_call(&caller, &callee, &route, i))
            .collect();
        let answered = call_id_of(&history[0]);
        history.push(stored_result(&callee, &caller, &answered, &route));

        let (url, published) = relay_with(history).await;
        let client = BuzzClient::new(url, caller.clone(), None, None).unwrap();

        dispatch(call_command(&callee, CHANNEL), &client)
            .await
            .expect("one call was answered, so one slot is free");
        assert_eq!(published.load(Ordering::SeqCst), 1);
    }

    /// Only the callee's own result frees the slot. A stranger who saw the call
    /// id on the relay cannot hand the caller back capacity it never spent.
    #[tokio::test]
    async fn a_result_from_anyone_but_the_callee_frees_nothing() {
        let caller = Keys::generate();
        let callee = Keys::generate();
        let stranger = Keys::generate();
        let route = channel_route(CHANNEL);
        let mut history: Vec<Value> = (0..MAX_FANOUT)
            .map(|i| stored_call(&caller, &callee, &route, i))
            .collect();
        let target = call_id_of(&history[0]);
        history.push(stored_result(&stranger, &caller, &target, &route));

        let (url, published) = relay_with(history).await;
        let client = BuzzClient::new(url, caller.clone(), None, None).unwrap();

        dispatch(call_command(&callee, CHANNEL), &client)
            .await
            .expect_err("a third party's result must not free a slot");
        assert_eq!(published.load(Ordering::SeqCst), 0);
    }

    /// The budget is per originating route, not a global throttle on the agent.
    #[tokio::test]
    async fn a_different_route_keeps_its_own_budget() {
        let caller = Keys::generate();
        let callee = Keys::generate();
        let route = channel_route(CHANNEL);
        let history: Vec<Value> = (0..MAX_FANOUT)
            .map(|i| stored_call(&caller, &callee, &route, i))
            .collect();

        let (url, published) = relay_with(history).await;
        let client = BuzzClient::new(url, caller.clone(), None, None).unwrap();

        dispatch(call_command(&callee, OTHER_CHANNEL), &client)
            .await
            .expect("a full route does not exhaust every other conversation");
        assert_eq!(published.load(Ordering::SeqCst), 1);
    }

    /// The gate fails closed. A relay that cannot say what is outstanding is not
    /// a relay this caller may publish an eleventh call to on faith.
    #[tokio::test]
    async fn an_unreadable_history_refuses_rather_than_publishing() {
        let (url, published) = relay_with(vec![]).await;
        // Point the client at a path that answers nothing at all.
        let client =
            BuzzClient::new(format!("{url}/nowhere"), Keys::generate(), None, None).unwrap();
        dispatch(call_command(&Keys::generate(), CHANNEL), &client)
            .await
            .expect_err("an unanswerable query must not publish");
        assert_eq!(published.load(Ordering::SeqCst), 0);
    }
}
