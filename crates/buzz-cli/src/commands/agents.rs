use buzz_core::kind::KIND_IA_ARCHIVED_LIST;
use buzz_core::observer::{encrypt_observer_payload, OBSERVER_FRAME_TELEMETRY};
use buzz_core::peer_call::{
    derive_call_id, onward_context, PeerCallRoute, CALL_WINDOW_SECS, KIND_PEER_CALL,
    KIND_PEER_CALL_RESULT, MAX_FANOUT,
};
use buzz_sdk::builders::{
    build_archive_identity_request, build_peer_call, build_peer_call_result,
    build_project_activity, build_unarchive_identity_request, PeerCallMeta, ProjectActivityState,
};
use nostr::{Kind, PublicKey, Tag};
use serde_json::json;
use uuid::Uuid;

use crate::agent_drain::{build_drain, DRAIN_CONTROL_TYPE};
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

/// Validate the structural part of one NIP-AO telemetry payload.
///
/// The CLI deliberately does not reinterpret ACP payloads: `payload` remains
/// protocol data, and unknown `kind` values remain forward-compatible as NIP-AO
/// requires. It does enforce the routing and correlation fields that would make
/// a frame impossible to place or dangerously ambiguous.
fn validate_observer_event(event: &serde_json::Value) -> Result<(), CliError> {
    let object = event
        .as_object()
        .ok_or_else(|| CliError::Usage("--event must be one JSON object".into()))?;

    object
        .get("seq")
        .and_then(serde_json::Value::as_u64)
        .filter(|seq| *seq > 0)
        .ok_or_else(|| CliError::Usage("observer event seq must be a positive integer".into()))?;

    let timestamp = object
        .get("timestamp")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| CliError::Usage("observer event timestamp must be RFC3339 text".into()))?;
    chrono::DateTime::parse_from_rfc3339(timestamp)
        .map_err(|_| CliError::Usage("observer event timestamp must be RFC3339 text".into()))?;

    let kind = object
        .get("kind")
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|kind| !kind.is_empty() && kind.len() <= 64)
        .ok_or_else(|| CliError::Usage("observer event kind must be 1 to 64 characters".into()))?;
    if kind.chars().any(char::is_control) {
        return Err(CliError::Usage(
            "observer event kind must not contain control characters".into(),
        ));
    }

    if !object
        .get("payload")
        .is_some_and(serde_json::Value::is_object)
    {
        return Err(CliError::Usage(
            "observer event payload must be a JSON object".into(),
        ));
    }

    let channel = object.get("channelId").filter(|value| !value.is_null());
    if let Some(channel) = channel {
        let channel = channel.as_str().ok_or_else(|| {
            CliError::Usage("observer event channelId must be a UUID or null".into())
        })?;
        validate_uuid(channel)?;
    }

    let project = object.get("project").filter(|value| !value.is_null());
    if channel.is_some() && project.is_some() {
        return Err(CliError::Usage(
            "observer event cannot name both channelId and project".into(),
        ));
    }
    if let Some(project) = project {
        let project = project.as_object().ok_or_else(|| {
            CliError::Usage("observer event project must be an object or null".into())
        })?;
        let coordinate = project
            .get("coordinate")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| CliError::Usage("observer project.coordinate is required".into()))?;
        buzz_sdk::GitRepoCoord::from_a_tag_value(coordinate).ok_or_else(|| {
            CliError::Usage("observer project.coordinate must be 30617:<owner>:<identifier>".into())
        })?;
        let root = project
            .get("root")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| CliError::Usage("observer project.root is required".into()))?;
        validate_hex64(root)?;
    }

    for name in ["sessionId", "turnId"] {
        if let Some(value) = object.get(name).filter(|value| !value.is_null()) {
            let value = value.as_str().ok_or_else(|| {
                CliError::Usage(format!("observer event {name} must be text or null"))
            })?;
            if value.trim().is_empty() || value.len() > 256 {
                return Err(CliError::Usage(format!(
                    "observer event {name} must be 1 to 256 characters"
                )));
            }
        }
    }

    Ok(())
}

fn verified_profile_owner(event: &nostr::Event, agent: &PublicKey) -> Option<(PublicKey, Tag)> {
    if event.pubkey != *agent || event.kind != Kind::Metadata || event.verify().is_err() {
        return None;
    }
    let auth_tags: Vec<&Tag> = event
        .tags
        .iter()
        .filter(|tag| tag.as_slice().first().is_some_and(|value| value == "auth"))
        .collect();
    if auth_tags.len() != 1 {
        return None;
    }
    let auth_json = serde_json::to_string(auth_tags[0]).ok()?;
    let owner = buzz_sdk::nip_oa::verify_auth_tag(&auth_json, agent).ok()?;
    Some((owner, auth_tags[0].clone()))
}

async fn resolve_observer_owner(
    client: &BuzzClient,
    explicit: Option<&str>,
) -> Result<(PublicKey, Option<Tag>), CliError> {
    let explicit = explicit
        .map(PublicKey::parse)
        .transpose()
        .map_err(|e| CliError::Usage(format!("--owner must be a pubkey or npub: {e}")))?;

    // An explicit owner is only a routing hint: the relay's authoritative
    // agent_owner_pubkey mapping still rejects a mismatched recipient. This is
    // the path for externally managed agents that do not hold the owner's
    // NIP-OA attestation locally.
    if let Some(owner) = explicit {
        return Ok((owner, None));
    }

    if let Some(owner) = client.auth_tag_owner_hex() {
        let owner = PublicKey::parse(&owner)
            .map_err(|e| CliError::Usage(format!("BUZZ_AUTH_TAG names an invalid owner: {e}")))?;
        return Ok((owner, None));
    }

    let agent = client.keys().public_key();
    let events = client
        .query_all(serde_json::json!({
            "kinds": [0],
            "authors": [agent.to_hex()],
            "limit": 10,
        }))
        .await?;
    let latest = events
        .into_iter()
        .filter_map(|value| serde_json::from_value::<nostr::Event>(value).ok())
        .filter(|event| {
            event.pubkey == agent && event.kind == Kind::Metadata && event.verify().is_ok()
        })
        .max_by_key(|event| event.created_at);

    let (owner, auth_tag) = latest
        .as_ref()
        .and_then(|event| verified_profile_owner(event, &agent))
        .ok_or_else(|| {
            CliError::Usage(
                "agents observe could not verify one owner: configure BUZZ_AUTH_TAG or publish a signed kind-0 profile with exactly one valid NIP-OA auth tag"
                    .into(),
            )
        })?;
    Ok((owner, Some(auth_tag)))
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

// ── NIP-OA sibling verification ───────────────────────────────────────────────

/// Ceiling on one `agents siblings` question. Callers ask about the authors
/// they have actually seen, and a runtime that needs more than this in one
/// breath is asking a different question.
const MAX_SIBLING_QUERY: usize = 50;

/// Does this profile carry an owner-signed NIP-OA attestation for `agent`?
///
/// The preimage covers the agent pubkey, so a relay cannot lift a valid
/// attestation from one agent onto another: the signature is verified against
/// the pubkey *asked about*, not the one the event claims. Withholding the
/// profile produces `false`, which is the fail-closed direction — an
/// unverifiable caller is untrusted, not trusted-by-default.
fn profile_attests(profiles: &[serde_json::Value], agent: &str, owner: &str) -> bool {
    let Ok(agent_pk) = PublicKey::from_hex(agent) else {
        return false;
    };
    let Some(event) = profiles.iter().find(|e| {
        e.get("pubkey")
            .and_then(serde_json::Value::as_str)
            .is_some_and(|author| author.eq_ignore_ascii_case(agent))
    }) else {
        return false;
    };
    let Some(tags) = event.get("tags").and_then(serde_json::Value::as_array) else {
        return false;
    };
    tags.iter().any(|tag| {
        let Some(parts) = tag.as_array() else {
            return false;
        };
        if parts.len() < 4 || parts[0].as_str() != Some("auth") {
            return false;
        }
        // Only an attestation naming *our* owner is worth a signature check;
        // a valid attestation to somebody else's owner is not a sibling.
        if !parts[1]
            .as_str()
            .is_some_and(|claimed| claimed.eq_ignore_ascii_case(owner))
        {
            return false;
        }
        let tag_json = serde_json::to_string(tag).unwrap_or_default();
        buzz_sdk::nip_oa::verify_auth_tag(&tag_json, &agent_pk).is_ok()
    })
}

/// Answer, for each pubkey, whether it is a verified same-owner sibling.
///
/// The owner is this process's own — the one its verified `BUZZ_AUTH_TAG`
/// names — so the question is always "sibling of me", never "sibling of a
/// pubkey the caller supplied". An agent with no auth tag has no owner and
/// therefore no siblings; it answers `false` for everything without asking the
/// relay anything, because there is nothing the answer could be checked
/// against.
async fn siblings_report(
    client: &BuzzClient,
    pubkeys: &[String],
) -> Result<serde_json::Value, CliError> {
    if pubkeys.is_empty() || pubkeys.len() > MAX_SIBLING_QUERY {
        return Err(CliError::Usage(format!(
            "--pubkey: give 1 to {MAX_SIBLING_QUERY} pubkeys"
        )));
    }
    let mut wanted: Vec<String> = Vec::with_capacity(pubkeys.len());
    for pubkey in pubkeys {
        validate_hex64(pubkey)?;
        let pubkey = pubkey.to_ascii_lowercase();
        if !wanted.contains(&pubkey) {
            wanted.push(pubkey);
        }
    }

    let me = client.keys().public_key().to_hex().to_ascii_lowercase();
    let owner = client.auth_tag_owner_hex().map(|o| o.to_ascii_lowercase());

    let profiles: Vec<serde_json::Value> = match owner {
        None => Vec::new(),
        Some(_) => {
            let raw = client
                .query(&json!({
                    "kinds": [0],
                    "authors": wanted,
                    "limit": wanted.len(),
                }))
                .await?;
            serde_json::from_str(&raw)
                .map_err(|e| CliError::Other(format!("could not read profiles: {e}")))?
        }
    };

    let results: Vec<serde_json::Value> = wanted
        .iter()
        .map(|pubkey| {
            // An agent is not its own sibling. Its own attestation verifies,
            // so without this it would report itself as a peer it may accept
            // calls from — the one caller class every runtime refuses first.
            let sibling = pubkey.as_str() != me.as_str()
                && owner
                    .as_deref()
                    .is_some_and(|owner| profile_attests(&profiles, pubkey, owner));
            json!({"pubkey": pubkey, "sibling": sibling})
        })
        .collect();

    Ok(json!({"owner": owner, "results": results}))
}

/// The command: the report above, printed. Kept this thin deliberately — a
/// test that asserts on the report is asserting on what the caller receives,
/// not on a parallel reconstruction of it.
async fn cmd_siblings(client: &BuzzClient, pubkeys: &[String]) -> Result<(), CliError> {
    println!("{}", siblings_report(client, pubkeys).await?);
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

        AgentsCmd::Activity {
            project,
            root,
            state,
            turn,
            stage,
        } => {
            validate_hex64(&root)?;
            let repo = buzz_sdk::GitRepoCoord::from_a_tag_value(&project).ok_or_else(|| {
                CliError::Usage(format!(
                    "--project must be 30617:<owner>:<identifier> (got {project:?})"
                ))
            })?;
            // Clap's value parser is the only source of these two spellings, so
            // an unknown one cannot arrive here; refusing rather than
            // defaulting keeps it that way if the parser ever widens.
            let state = match state.as_str() {
                "working" => ProjectActivityState::Working,
                "idle" => ProjectActivityState::Idle,
                other => {
                    return Err(CliError::Usage(format!(
                        "--state must be working or idle (got {other:?})"
                    )))
                }
            };

            // The signer *is* the agent the signal is about. Taking it from the
            // keys rather than from a flag is what stops one agent announcing
            // that another is working: the `agent` tag and the authorship
            // cannot disagree, so a consumer that checks either is checking
            // both.
            let agent = client.keys().public_key().to_hex().to_ascii_lowercase();
            let builder = build_project_activity(
                &repo,
                &root.to_ascii_lowercase(),
                &agent,
                state,
                &turn,
                stage.as_deref(),
            )
            .map_err(|e| CliError::Usage(format!("invalid activity: {e}")))?;

            // `sign_event_unchecked`, like the peer-call envelopes beside it:
            // an activity signal is published to everyone who can read the
            // issue, and the ambient NIP-OA auth tag is an owner attestation
            // that belongs on the agent's own profile, not stamped onto every
            // ephemeral frame.
            let event = client.sign_event_unchecked(builder)?;
            let event_id = event.id.to_hex();
            client.submit_event(event).await?;
            println!(
                "{}",
                json!({
                    "ok": true,
                    "event_id": event_id,
                    "root": root.to_ascii_lowercase(),
                    // Read back off the state itself, not restated here: the
                    // report and the event it reports must not be able to
                    // disagree.
                    "state": state.as_tag(),
                    "turn": turn,
                })
            );
            Ok(())
        }

        AgentsCmd::Observe { event, owner } => {
            let (owner, observer_auth) = resolve_observer_owner(client, owner.as_deref()).await?;
            let owner_hex = owner.to_hex();
            let event: serde_json::Value = serde_json::from_str(&read_or_stdin(&event)?)
                .map_err(|e| CliError::Usage(format!("--event is not valid JSON: {e}")))?;
            validate_observer_event(&event)?;

            let encrypted = encrypt_observer_payload(client.keys(), &owner, &event)
                .map_err(|e| CliError::Usage(format!("invalid observer event: {e}")))?;
            let agent = client.keys().public_key().to_hex().to_ascii_lowercase();
            let builder = buzz_sdk::build_agent_observer_frame(
                &owner_hex.to_ascii_lowercase(),
                &agent,
                OBSERVER_FRAME_TELEMETRY,
                &encrypted,
            )
            .map_err(|e| CliError::Usage(format!("invalid observer frame: {e}")))?;
            // NIP-AO kind 24200 is ephemeral. The relay refuses it over the
            // stored-event HTTP door, and its WS authorisation needs the same
            // verified NIP-OA attestation that established the recipient.
            let signed = client.sign_event_unchecked(builder)?;
            let event_id = signed.id.to_hex();
            client
                .publish_ephemeral_event_with_auth(signed, observer_auth.as_ref())
                .await?;
            println!(
                "{}",
                json!({
                    "ok": true,
                    "event_id": event_id,
                    "agent": agent,
                    "owner": owner_hex.to_ascii_lowercase(),
                    "frame": OBSERVER_FRAME_TELEMETRY,
                })
            );
            Ok(())
        }

        AgentsCmd::Siblings { pubkeys } => cmd_siblings(client, &pubkeys).await,

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

        AgentsCmd::Drain { agent, reason } => {
            validate_hex64(&agent)?;
            let built = build_drain(client.keys(), &agent, reason.as_deref())?;
            // Kind 24200 is ephemeral, and the relay rejects ephemeral kinds
            // over HTTP — the same WebSocket path `draft-create` uses.
            let response = client.publish_ephemeral_event(built.event).await?;
            let owner = client.keys().public_key().to_hex().to_ascii_lowercase();
            println!("{}", drain_ack(&response, &built.agent, &owner)?);
            Ok(())
        }
    }
}

/// The `agents drain` acknowledgement: the relay's own response, plus the three
/// facts a caller cannot recover from it.
///
/// A separate function because the ack is a contract two very different readers
/// depend on — a human at a terminal and `deploy.sh` deciding whether to wait —
/// and a contract that lives inline in a match arm is a contract no test can
/// hold still.
///
/// `drain_confirmed` is a literal `false` and is not derived from anything.
/// That is the whole point: this process observed a *delivery*. The drain
/// happens afterwards, in another process, on a schedule set by whatever work
/// that process is holding. A field computed from the publish result would be
/// reporting the wrong event under the right name, and a deployer that believed
/// it would install a binary over a running turn.
fn drain_ack(response: &str, agent: &str, owner: &str) -> Result<serde_json::Value, CliError> {
    let mut output: serde_json::Value = serde_json::from_str(response)
        .map_err(|e| CliError::Other(format!("invalid relay response: {e}")))?;
    if let Some(obj) = output.as_object_mut() {
        obj.insert("agent".into(), agent.into());
        // The signer, restated because it is the field the agent checks: a
        // drain from the wrong key is accepted by the relay and then dropped by
        // the agent, and the only way to diagnose that afterwards is to know
        // which key was used.
        obj.insert("owner".into(), owner.into());
        obj.insert("type".into(), DRAIN_CONTROL_TYPE.into());
        obj.insert("drain_confirmed".into(), false.into());
        obj.insert(
            "note".into(),
            "The relay accepted the frame. Whether the agent drained is not observable \
             from here: watch its journal for 'drain requested by owner' and wait for the \
             process to exit 0. A drained agent stays down until something starts it."
                .into(),
        );
    }
    Ok(output)
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

/// NIP-PA activity and NIP-OA sibling verification, proved on the wire.
///
/// The activity tests read the **published event**, not the builder's return
/// value: the tag shape is the whole product here, and an `h` tag or a missing
/// marked root would leave every consumer keyed on the wrong thing while the
/// command still exited zero.
#[cfg(test)]
mod activity_tests {
    use std::net::SocketAddr;
    use std::sync::{Arc, Mutex};

    use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
    use axum::extract::State;
    use axum::response::Response;
    use axum::routing::{any, post};
    use axum::{Json, Router};
    use buzz_sdk::nip_oa::compute_auth_tag;
    use nostr::Keys;
    use serde_json::Value;
    use tokio::net::TcpListener;

    use super::*;
    use crate::AgentsCmd;

    const ROOT: &str = "3333333333333333333333333333333333333333333333333333333333333333";
    const TURN: &str = "turn-7";

    #[derive(Clone)]
    struct Relay {
        stored: Arc<Vec<Value>>,
        published: Arc<Mutex<Vec<Value>>>,
    }

    /// A relay that answers `/query` from a fixed history, keeps stored events,
    /// and accepts ephemeral observer frames over its WebSocket root.
    async fn relay_with(stored: Vec<Value>) -> (String, Arc<Mutex<Vec<Value>>>) {
        let published = Arc::new(Mutex::new(Vec::new()));
        let state = Relay {
            stored: Arc::new(stored),
            published: published.clone(),
        };
        let app = Router::new()
            .route(
                "/",
                any(
                    |State(s): State<Relay>, upgrade: WebSocketUpgrade| async move {
                        let response: Response =
                            upgrade.on_upgrade(move |socket| serve_observer(socket, s.published));
                        response
                    },
                ),
            )
            .route(
                "/query",
                post(|State(s): State<Relay>, _body: String| async move {
                    Json(Value::Array(s.stored.as_ref().clone()))
                }),
            )
            .route(
                "/events",
                post(|State(s): State<Relay>, body: String| async move {
                    if let Ok(event) = serde_json::from_str::<Value>(&body) {
                        s.published.lock().expect("lock").push(event);
                    }
                    Json(serde_json::json!({"accepted": true, "message": ""}))
                }),
            )
            .with_state(state);
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr: SocketAddr = listener.local_addr().expect("addr");
        tokio::spawn(async move { axum::serve(listener, app).await.expect("serve") });
        (format!("http://{addr}"), published)
    }

    async fn serve_observer(mut socket: WebSocket, published: Arc<Mutex<Vec<Value>>>) {
        let _ = socket
            .send(Message::Text(r#"["AUTH","deadbeef"]"#.into()))
            .await;
        while let Some(Ok(message)) = socket.recv().await {
            let Message::Text(text) = message else {
                continue;
            };
            let Ok(frame) = serde_json::from_str::<Vec<Value>>(&text) else {
                continue;
            };
            let verb = frame.first().and_then(Value::as_str).unwrap_or_default();
            let Some(event) = frame.get(1) else { continue };
            let id = event
                .get("id")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            if verb == "EVENT" {
                published.lock().expect("lock").push(event.clone());
            }
            let _ = socket
                .send(Message::Text(
                    serde_json::json!(["OK", id, true, ""]).to_string().into(),
                ))
                .await;
        }
    }

    fn coordinate(owner: &Keys) -> String {
        format!("30617:{}:demo", owner.public_key().to_hex())
    }

    fn tag_values(event: &Value, name: &str) -> Vec<String> {
        event
            .get("tags")
            .and_then(Value::as_array)
            .map(|tags| {
                tags.iter()
                    .filter_map(|t| {
                        let parts = t.as_array()?;
                        (parts.first()?.as_str()? == name)
                            .then(|| parts.get(1)?.as_str().map(str::to_owned))?
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    fn activity_command(owner: &Keys, state: &str, stage: Option<&str>) -> AgentsCmd {
        AgentsCmd::Activity {
            project: coordinate(owner),
            root: ROOT.to_string(),
            state: state.to_string(),
            turn: TURN.to_string(),
            stage: stage.map(str::to_owned),
        }
    }

    /// The exact event shape NIP-PA pins, read off the wire.
    #[tokio::test]
    async fn activity_publishes_the_repository_root_agent_state_and_turn() {
        let agent = Keys::generate();
        let owner = Keys::generate();
        let (url, published) = relay_with(vec![]).await;
        let client = BuzzClient::new(url, agent.clone(), None, None).expect("client");

        dispatch(activity_command(&owner, "working", None), &client)
            .await
            .expect("publishes");

        let published = published.lock().expect("lock");
        assert_eq!(published.len(), 1, "one activity event");
        let event = &published[0];
        assert_eq!(event["kind"], serde_json::json!(20003));
        assert_eq!(tag_values(event, "a"), vec![coordinate(&owner)]);
        assert_eq!(tag_values(event, "e"), vec![ROOT.to_string()]);
        assert_eq!(
            tag_values(event, "agent"),
            vec![agent.public_key().to_hex()],
            "the agent tag must be the signer, not a caller-supplied pubkey"
        );
        assert_eq!(tag_values(event, "state"), vec!["working".to_string()]);
        assert_eq!(tag_values(event, "turn"), vec![TURN.to_string()]);
        assert!(
            tag_values(event, "stage").is_empty(),
            "no stage was given, so none may be invented"
        );
        assert!(
            tag_values(event, "h").is_empty(),
            "an issue is not a channel — an h tag would key every consumer on a channel that does not exist"
        );
        // The `e` tag must be *marked* root, or a client that groups a thread
        // by its root marker never sees this signal.
        let marked: Vec<&Value> = event["tags"]
            .as_array()
            .expect("tags")
            .iter()
            .filter(|t| t[0] == serde_json::json!("e"))
            .collect();
        assert_eq!(marked.len(), 1);
        assert_eq!(marked[0][3], serde_json::json!("root"));
    }

    #[tokio::test]
    async fn activity_carries_an_optional_stage_label() {
        let agent = Keys::generate();
        let owner = Keys::generate();
        let (url, published) = relay_with(vec![]).await;
        let client = BuzzClient::new(url, agent, None, None).expect("client");

        dispatch(
            activity_command(&owner, "idle", Some("reading files")),
            &client,
        )
        .await
        .expect("publishes");

        let published = published.lock().expect("lock");
        let event = &published[0];
        assert_eq!(tag_values(event, "state"), vec!["idle".to_string()]);
        assert_eq!(
            tag_values(event, "stage"),
            vec!["reading files".to_string()]
        );
    }

    /// A refused activity publishes nothing. An indicator raised by a malformed
    /// coordinate would point at a repository nobody can resolve.
    #[tokio::test]
    async fn a_malformed_activity_never_reaches_the_relay() {
        let agent = Keys::generate();
        let owner = Keys::generate();
        let (url, published) = relay_with(vec![]).await;
        let client = BuzzClient::new(url, agent, None, None).expect("client");

        for command in [
            AgentsCmd::Activity {
                project: "not-a-coordinate".into(),
                root: ROOT.into(),
                state: "working".into(),
                turn: TURN.into(),
                stage: None,
            },
            AgentsCmd::Activity {
                project: coordinate(&owner),
                root: "nope".into(),
                state: "working".into(),
                turn: TURN.into(),
                stage: None,
            },
            AgentsCmd::Activity {
                project: coordinate(&owner),
                root: ROOT.into(),
                state: "working".into(),
                turn: "   ".into(),
                stage: None,
            },
        ] {
            dispatch(command, &client)
                .await
                .expect_err("a malformed activity must be refused");
        }
        assert!(published.lock().expect("lock").is_empty());
    }

    fn observer_event(owner: &Keys) -> Value {
        serde_json::json!({
            "seq": 7,
            "timestamp": "2026-08-05T12:00:00.125Z",
            "kind": "acp_read",
            "agentIndex": null,
            "channelId": null,
            "project": {
                "coordinate": coordinate(owner),
                "root": ROOT,
            },
            "sessionId": "session-7",
            "turnId": TURN,
            "payload": {
                "jsonrpc": "2.0",
                "method": "session/update",
                "params": {
                    "sessionId": "session-7",
                    "update": {
                        "sessionUpdate": "tool_call",
                        "toolCallId": "tool-7",
                        "title": "reading files",
                        "status": "in_progress",
                        "kind": "read",
                    }
                }
            }
        })
    }

    fn client_with_owner(url: String, agent: &Keys, owner: &Keys) -> BuzzClient {
        let auth_json = compute_auth_tag(owner, &agent.public_key(), "").expect("attestation");
        let auth = buzz_sdk::nip_oa::parse_auth_tag(&auth_json).expect("auth tag");
        BuzzClient::new(url, agent.clone(), Some(auth), Some(auth_json)).expect("client")
    }

    #[tokio::test]
    async fn observe_encrypts_one_structured_frame_to_the_relay_vetted_owner() {
        let agent = Keys::generate();
        let owner = Keys::generate();
        let (url, published) = relay_with(vec![]).await;
        let client = BuzzClient::new(url, agent.clone(), None, None).expect("client");
        let expected = observer_event(&owner);

        dispatch(
            AgentsCmd::Observe {
                event: expected.to_string(),
                owner: Some(owner.public_key().to_hex()),
            },
            &client,
        )
        .await
        .expect("publishes encrypted telemetry");

        let published = published.lock().expect("lock");
        assert_eq!(published.len(), 1);
        let raw = &published[0];
        assert_eq!(raw["kind"], serde_json::json!(24200));
        assert_eq!(
            tag_values(raw, "p"),
            vec![owner.public_key().to_hex()],
            "the recipient is explicit, encrypted, and remains subject to the relay's owner map"
        );
        assert_eq!(tag_values(raw, "agent"), vec![agent.public_key().to_hex()]);
        assert_eq!(tag_values(raw, "frame"), vec!["telemetry".to_string()]);
        assert!(tag_values(raw, "auth").is_empty());

        let wire: nostr::Event = serde_json::from_value(raw.clone()).expect("wire event");
        let decrypted: Value =
            buzz_core::observer::decrypt_observer_payload(&owner, &wire).expect("owner decrypts");
        assert_eq!(decrypted, expected);
    }

    #[tokio::test]
    async fn observe_resolves_owner_from_the_latest_signed_agent_profile() {
        let agent = Keys::generate();
        let owner = Keys::generate();
        let auth_json = compute_auth_tag(&owner, &agent.public_key(), "").expect("attestation");
        let auth = buzz_sdk::nip_oa::parse_auth_tag(&auth_json).expect("auth tag");
        let profile = nostr::EventBuilder::new(Kind::Metadata, "{}")
            .tags([auth])
            .sign_with_keys(&agent)
            .expect("signed profile");
        let (url, published) =
            relay_with(vec![serde_json::to_value(profile).expect("profile json")]).await;
        let client = BuzzClient::new(url, agent, None, None).expect("client");

        dispatch(
            AgentsCmd::Observe {
                event: observer_event(&owner).to_string(),
                owner: None,
            },
            &client,
        )
        .await
        .expect("observe");

        let events = published.lock().expect("lock");
        assert_eq!(events.len(), 1);
        assert_eq!(
            events[0]["tags"],
            serde_json::json!([
                ["p", owner.public_key().to_hex()],
                ["agent", client.keys().public_key().to_hex()],
                ["frame", "telemetry"]
            ])
        );
    }

    #[tokio::test]
    async fn observe_without_an_attested_owner_publishes_nothing() {
        let agent = Keys::generate();
        let owner = Keys::generate();
        let (url, published) = relay_with(vec![]).await;
        let client = BuzzClient::new(url, agent, None, None).expect("client");

        let error = dispatch(
            AgentsCmd::Observe {
                event: observer_event(&owner).to_string(),
                owner: None,
            },
            &client,
        )
        .await
        .expect_err("ownerless observer telemetry must fail closed");
        assert!(error.to_string().contains("BUZZ_AUTH_TAG"));
        assert!(published.lock().expect("lock").is_empty());
    }

    #[tokio::test]
    async fn observe_refuses_a_frame_that_claims_a_channel_and_a_project() {
        let agent = Keys::generate();
        let owner = Keys::generate();
        let (url, published) = relay_with(vec![]).await;
        let client = client_with_owner(url, &agent, &owner);
        let mut event = observer_event(&owner);
        event["channelId"] = Value::String("52a85618-0f8f-4542-94ec-599e6e1c6f2e".into());

        let error = dispatch(
            AgentsCmd::Observe {
                event: event.to_string(),
                owner: None,
            },
            &client,
        )
        .await
        .expect_err("ambiguous route must be refused");
        assert!(error.to_string().contains("both channelId and project"));
        assert!(published.lock().expect("lock").is_empty());
    }

    // ── Siblings ──────────────────────────────────────────────────────────

    /// A kind:0 profile carrying a NIP-OA attestation from `owner`.
    fn profile_with_auth(agent: &Keys, owner: &Keys) -> Value {
        let tag_json = compute_auth_tag(owner, &agent.public_key(), "").expect("attestation");
        let tag: Value = serde_json::from_str(&tag_json).expect("tag json");
        serde_json::json!({
            "pubkey": agent.public_key().to_hex(),
            "kind": 0,
            "content": "{}",
            "tags": [tag],
        })
    }

    /// The real command's report, verbatim: `cmd_siblings` is nothing but a
    /// `println!` of this, so what is asserted here is what a caller receives.
    async fn siblings_response(
        me: &Keys,
        owner: Option<&Keys>,
        stored: Vec<Value>,
        asked: &[&Keys],
    ) -> Vec<(String, bool)> {
        let (url, _published) = relay_with(stored).await;
        let auth = owner.map(|owner| {
            let json = compute_auth_tag(owner, &me.public_key(), "").expect("attestation");
            (buzz_sdk::nip_oa::parse_auth_tag(&json).expect("tag"), json)
        });
        let client = BuzzClient::new(
            url,
            me.clone(),
            auth.as_ref().map(|(tag, _)| tag.clone()),
            auth.as_ref().map(|(_, json)| json.clone()),
        )
        .expect("client");

        let pubkeys: Vec<String> = asked.iter().map(|k| k.public_key().to_hex()).collect();
        let report = siblings_report(&client, &pubkeys)
            .await
            .expect("siblings report");
        assert_eq!(
            report["owner"],
            owner
                .map(|o| Value::String(o.public_key().to_hex()))
                .unwrap_or(Value::Null),
            "the report must name the owner it verified against"
        );
        report["results"]
            .as_array()
            .expect("results")
            .iter()
            .map(|row| {
                (
                    row["pubkey"].as_str().unwrap_or_default().to_string(),
                    row["sibling"].as_bool().unwrap_or_default(),
                )
            })
            .collect()
    }

    /// The enum wiring: `buzz agents siblings` reaches the report at all.
    #[tokio::test]
    async fn the_siblings_subcommand_is_wired_to_the_report() {
        let owner = Keys::generate();
        let me = Keys::generate();
        let peer = Keys::generate();
        let (url, _published) = relay_with(vec![profile_with_auth(&peer, &owner)]).await;
        let client = BuzzClient::new(url, me, None, None).expect("client");
        dispatch(
            AgentsCmd::Siblings {
                pubkeys: vec![peer.public_key().to_hex()],
            },
            &client,
        )
        .await
        .expect("siblings");

        dispatch(AgentsCmd::Siblings { pubkeys: vec![] }, &client)
            .await
            .expect_err("a question about nobody is not a question");
    }

    #[tokio::test]
    async fn a_same_owner_attestation_makes_a_sibling() {
        let owner = Keys::generate();
        let me = Keys::generate();
        let peer = Keys::generate();
        let results = siblings_response(
            &me,
            Some(&owner),
            vec![profile_with_auth(&peer, &owner)],
            &[&peer],
        )
        .await;
        assert_eq!(results, vec![(peer.public_key().to_hex(), true)]);
    }

    #[tokio::test]
    async fn an_attestation_from_a_different_owner_is_not_a_sibling() {
        let owner = Keys::generate();
        let stranger_owner = Keys::generate();
        let me = Keys::generate();
        let peer = Keys::generate();
        let results = siblings_response(
            &me,
            Some(&owner),
            vec![profile_with_auth(&peer, &stranger_owner)],
            &[&peer],
        )
        .await;
        assert_eq!(results, vec![(peer.public_key().to_hex(), false)]);
    }

    /// The forgery the preimage exists to stop: a valid attestation for one
    /// agent, served on another agent's profile.
    #[tokio::test]
    async fn an_attestation_lifted_onto_another_agents_profile_is_refused() {
        let owner = Keys::generate();
        let me = Keys::generate();
        let real = Keys::generate();
        let impostor = Keys::generate();
        let lifted = compute_auth_tag(&owner, &real.public_key(), "").expect("attestation");
        let tag: Value = serde_json::from_str(&lifted).expect("tag json");
        let profile = serde_json::json!({
            "pubkey": impostor.public_key().to_hex(),
            "kind": 0,
            "content": "{}",
            "tags": [tag],
        });

        let results = siblings_response(&me, Some(&owner), vec![profile], &[&impostor]).await;
        assert_eq!(results, vec![(impostor.public_key().to_hex(), false)]);
    }

    /// No owner, no siblings. An unowned agent has nothing for a caller to be
    /// verified against, so the answer is `false` rather than an error.
    #[tokio::test]
    async fn an_agent_without_an_auth_tag_has_no_siblings() {
        let owner = Keys::generate();
        let me = Keys::generate();
        let peer = Keys::generate();
        let results =
            siblings_response(&me, None, vec![profile_with_auth(&peer, &owner)], &[&peer]).await;
        assert_eq!(results, vec![(peer.public_key().to_hex(), false)]);
    }

    /// An agent is not its own sibling: its own attestation verifies, and
    /// reporting it would name the one caller class every runtime refuses first.
    #[tokio::test]
    async fn an_agent_is_not_its_own_sibling() {
        let owner = Keys::generate();
        let me = Keys::generate();
        let results = siblings_response(
            &me,
            Some(&owner),
            vec![profile_with_auth(&me, &owner)],
            &[&me],
        )
        .await;
        assert_eq!(results, vec![(me.public_key().to_hex(), false)]);
    }
}

/// The drain publish path, driven end to end against a WebSocket relay.
///
/// The frame's *shape* is proved in `crate::agent_drain`; what is proved here
/// is the part only the command owns: that `agents drain` reaches the relay at
/// all, and that it reaches it over WebSocket. That distinction is not
/// pedantry — kind 24200 is ephemeral, the relay refuses ephemeral kinds over
/// HTTP, and a drain sent down the HTTP path would be rejected at the one
/// moment nobody is watching (a deploy, unattended, at 3am). A test relay that
/// only spoke HTTP would have passed.
#[cfg(test)]
mod drain_publish_tests {
    use std::net::SocketAddr;
    use std::sync::{Arc, Mutex};

    use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
    use axum::extract::State;
    use axum::response::Response;
    use axum::routing::any;
    use axum::Router;
    use nostr::Keys;
    use serde_json::Value;
    use tokio::net::TcpListener;

    use super::*;
    use crate::AgentsCmd;

    /// Every EVENT the relay was handed, in order.
    type Received = Arc<Mutex<Vec<Value>>>;

    /// A NIP-42 relay that accepts everything and remembers what it was sent.
    ///
    /// It answers `AUTH` and `EVENT` with `OK … true` and nothing else, which
    /// is the whole protocol `buzz_ws_client::publish_event` drives. Accepting
    /// unconditionally is deliberate: this test is about what the CLI sends,
    /// and a relay with opinions would let a policy failure masquerade as a
    /// send failure.
    async fn ws_relay() -> (String, Received) {
        let received: Received = Arc::new(Mutex::new(Vec::new()));
        let app = Router::new()
            .route(
                "/",
                any(
                    |State(seen): State<Received>, upgrade: WebSocketUpgrade| async move {
                        let response: Response =
                            upgrade.on_upgrade(move |socket| serve(socket, seen));
                        response
                    },
                ),
            )
            .with_state(received.clone());

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr: SocketAddr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        (format!("http://{addr}"), received)
    }

    async fn serve(mut socket: WebSocket, seen: Received) {
        // The challenge comes first, unprompted: that is what the client waits
        // for before it will authenticate.
        let _ = socket
            .send(Message::Text(r#"["AUTH","deadbeef"]"#.into()))
            .await;
        while let Some(Ok(message)) = socket.recv().await {
            let Message::Text(text) = message else {
                continue;
            };
            let Ok(frame) = serde_json::from_str::<Vec<Value>>(&text) else {
                continue;
            };
            let verb = frame.first().and_then(Value::as_str).unwrap_or_default();
            let Some(event) = frame.get(1) else { continue };
            let id = event
                .get("id")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            if verb == "EVENT" {
                seen.lock().unwrap().push(event.clone());
            }
            let _ = socket
                .send(Message::Text(
                    serde_json::json!(["OK", id, true, ""]).to_string().into(),
                ))
                .await;
        }
    }

    fn tag_value(event: &Value, name: &str) -> Option<String> {
        event.get("tags")?.as_array()?.iter().find_map(|tag| {
            let tag = tag.as_array()?;
            (tag.first()?.as_str()? == name).then(|| tag.get(1)?.as_str().map(str::to_owned))?
        })
    }

    #[tokio::test]
    async fn a_drain_reaches_the_relay_over_websocket_signed_by_the_caller() {
        let owner = Keys::generate();
        let agent = Keys::generate();
        let (url, received) = ws_relay().await;
        let client = BuzzClient::new(url, owner.clone(), None, None).unwrap();

        dispatch(
            AgentsCmd::Drain {
                agent: agent.public_key().to_hex(),
                reason: Some("deploy projects-merge".into()),
            },
            &client,
        )
        .await
        .expect("the drain publishes");

        let events = received.lock().unwrap().clone();
        assert_eq!(events.len(), 1, "exactly one frame is published");
        let event = &events[0];
        assert_eq!(event["kind"].as_u64(), Some(24_200));
        assert_eq!(
            event["pubkey"].as_str(),
            Some(owner.public_key().to_hex().as_str()),
            "the caller signs — the agent drops a control frame from anyone but its owner"
        );
        assert_eq!(
            tag_value(event, "p").as_deref(),
            Some(agent.public_key().to_hex().as_str()),
            "the relay routes on `p`, so the drain must be p-tagged to the agent"
        );
        assert_eq!(tag_value(event, "frame").as_deref(), Some("control"));
    }

    /// The ack, field by field. `deploy.sh` and a human both read this, and the
    /// one field that matters most is the one that is always false.
    #[test]
    fn the_ack_reports_delivery_and_refuses_to_claim_more() {
        let relay = r#"{"event_id":"abc","accepted":true,"message":""}"#;
        let ack = drain_ack(relay, "d".repeat(64).as_str(), "0".repeat(64).as_str()).unwrap();

        assert_eq!(ack["event_id"], "abc");
        assert_eq!(ack["accepted"], true);
        assert_eq!(ack["agent"], "d".repeat(64));
        assert_eq!(ack["owner"], "0".repeat(64));
        assert_eq!(ack["type"], "drain");
        assert_eq!(
            ack["drain_confirmed"], false,
            "this process saw a delivery, never a drain — the field is a constant"
        );
        assert!(ack["note"].as_str().unwrap().contains("exit 0"));
    }

    /// A relay answer that is not JSON is an error, not an ack with holes in
    /// it: a caller parsing `{}` would read `drain_confirmed` as absent and
    /// could not tell that from false.
    #[test]
    fn an_unparseable_relay_response_does_not_become_an_ack() {
        assert!(drain_ack("not json", "a", "b").is_err());
    }

    /// A malformed pubkey must not open a socket. The gate is argument
    /// validation, and it has to sit before the publish for the same reason the
    /// peer-call ceiling does: an event that reaches the relay has already
    /// happened.
    #[tokio::test]
    async fn a_bad_agent_pubkey_never_reaches_the_relay() {
        let (url, received) = ws_relay().await;
        let client = BuzzClient::new(url, Keys::generate(), None, None).unwrap();

        let error = dispatch(
            AgentsCmd::Drain {
                agent: "not-a-pubkey".into(),
                reason: None,
            },
            &client,
        )
        .await
        .expect_err("a malformed agent is refused");
        assert!(matches!(error, CliError::Usage(_)), "unexpected: {error:?}");
        assert!(received.lock().unwrap().is_empty());
    }
}
