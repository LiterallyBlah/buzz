//! Generic, granted extension conversations through Buzz's configured local
//! ACP runtime.
//!
//! Authority remains exact to the frame lease tuple. The extension supplies a
//! bounded object-scoped context and receives only normalized assistant text;
//! it never sees provider credentials, ACP frames, process handles, tools, or
//! a relay transcript.

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tauri::AppHandle;
use tokio::sync::watch;

use super::dispatch::{code, BridgeReply};
use super::frame_authority::LeaseAuthority;

const MAX_CONTEXT_BYTES: usize = 8 * 1024;
const MAX_MESSAGE_BYTES: usize = 4 * 1024;
const MAX_HISTORY_ITEMS: usize = 6;
const MAX_HISTORY_ITEM_BYTES: usize = 2 * 1024;
const MAX_PROMPT_BYTES: usize = 24 * 1024;
const MAX_REQUESTS_PER_LEASE: u32 = 32;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ConversationParams {
    context: ConversationContext,
    message: String,
    #[serde(default)]
    history: Vec<HistoryMessage>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct HistoryMessage {
    role: String,
    content: String,
    #[serde(default)]
    at: Option<String>,
    #[serde(default)]
    evidence: Option<bool>,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ConversationContext {
    schema_version: u8,
    challenge_id: String,
    registry_entry_id: String,
    object: ConversationObject,
    parent: Option<RelatedObject>,
    children: Vec<RelatedObject>,
    confusion: String,
    learner_how: String,
    learner_why: String,
    instruction: String,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct ConversationObject {
    id: String,
    kind: String,
    label: String,
    status: String,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct RelatedObject {
    id: String,
    label: String,
}

struct LeaseAdmission {
    requests: u32,
    in_flight: bool,
    cancel: Option<watch::Sender<bool>>,
}

fn admissions() -> &'static Mutex<HashMap<String, LeaseAdmission>> {
    static ADMISSIONS: OnceLock<Mutex<HashMap<String, LeaseAdmission>>> = OnceLock::new();
    ADMISSIONS.get_or_init(|| Mutex::new(HashMap::new()))
}

#[derive(Debug)]
struct AdmissionGuard {
    lease: String,
}

impl Drop for AdmissionGuard {
    fn drop(&mut self) {
        if let Ok(mut all) = admissions().lock() {
            if let Some(state) = all.get_mut(&self.lease) {
                state.in_flight = false;
                state.cancel = None;
            }
        }
    }
}

fn admit(lease: &str) -> Result<(AdmissionGuard, watch::Receiver<bool>), BridgeReply> {
    let mut all = admissions()
        .lock()
        .map_err(|_| BridgeReply::err(code::INTERNAL, "conversation admission unavailable"))?;
    let state = all.entry(lease.to_string()).or_insert(LeaseAdmission {
        requests: 0,
        in_flight: false,
        cancel: None,
    });
    if state.in_flight {
        return Err(BridgeReply::err(
            code::RATE_LIMITED,
            "one conversation is already running for this frame",
        ));
    }
    if state.requests >= MAX_REQUESTS_PER_LEASE {
        return Err(BridgeReply::err(
            code::QUOTA_EXCEEDED,
            "conversation request budget exhausted for this frame",
        ));
    }
    state.requests += 1;
    state.in_flight = true;
    let (cancel, receiver) = watch::channel(false);
    state.cancel = Some(cancel);
    Ok((AdmissionGuard { lease: lease.to_string() }, receiver))
}

pub(crate) fn cancel_lease(lease: &str) {
    let state = admissions().lock().ok().and_then(|mut all| all.remove(lease));
    if let Some(cancel) = state.and_then(|state| state.cancel) {
        let _ = cancel.send(true);
    }
}

fn selection_has_converse<R: tauri::Runtime>(
    app: &AppHandle<R>,
    owner: &LeaseAuthority,
) -> bool {
    super::dispatch::grant_db_path(app)
        .ok()
        .and_then(|path| super::grants::open_grant_db(&path).ok())
        .is_some_and(|conn| {
            super::grants::list_selection(
                &conn,
                &owner.identity_pubkey,
                &owner.extension_id,
                &owner.package_digest,
            )
            .agent_converse
        })
}

fn valid_text(value: &str, max: usize, allow_empty: bool) -> bool {
    let len = value.len();
    (allow_empty || len > 0)
        && len <= max
        && value
            .chars()
            .all(|ch| ch == '\n' || ch == '\t' || (' '..='~').contains(&ch) || ch >= '\u{a0}')
}

fn valid_context(context: &ConversationContext) -> bool {
    context.schema_version == 1
        && valid_text(&context.challenge_id, 128, false)
        && valid_text(&context.registry_entry_id, 128, false)
        && valid_text(&context.object.id, 128, false)
        && valid_text(&context.object.kind, 64, false)
        && valid_text(&context.object.label, 256, false)
        && valid_text(&context.object.status, 32, false)
        && context.parent.as_ref().is_none_or(|item| {
            valid_text(&item.id, 128, false) && valid_text(&item.label, 256, true)
        })
        && context.children.len() <= 16
        && context.children.iter().all(|item| {
            valid_text(&item.id, 128, false) && valid_text(&item.label, 256, true)
        })
        && valid_text(&context.confusion, 2 * 1024, true)
        && valid_text(&context.learner_how, 2 * 1024, true)
        && valid_text(&context.learner_why, 2 * 1024, true)
        && valid_text(&context.instruction, 1024, false)
}

fn parse_prompt(params: Option<Value>) -> Result<String, BridgeReply> {
    let params: ConversationParams = serde_json::from_value(
        params.ok_or_else(|| BridgeReply::err(code::INVALID_PARAMS, "params are required"))?,
    )
    .map_err(|_| BridgeReply::err(code::INVALID_PARAMS, "conversation params are invalid"))?;
    if !valid_context(&params.context) {
        return Err(BridgeReply::err(
            code::INVALID_PARAMS,
            "conversation context is invalid",
        ));
    }
    let context = serde_json::to_string(&params.context)
        .map_err(|_| BridgeReply::err(code::INVALID_PARAMS, "conversation context is invalid"))?;
    let message = params.message.trim();
    if context.len() > MAX_CONTEXT_BYTES
        || message.is_empty()
        || message.len() > MAX_MESSAGE_BYTES
        || params.history.len() > MAX_HISTORY_ITEMS
    {
        return Err(BridgeReply::err(
            code::INVALID_PARAMS,
            "conversation input exceeds its object-scope limits",
        ));
    }
    let mut history = String::new();
    for item in params.history {
        let content = item.content.trim();
        if !matches!(item.role.as_str(), "learner" | "agent")
            || content.is_empty()
            || content.len() > MAX_HISTORY_ITEM_BYTES
            || !valid_text(content, MAX_HISTORY_ITEM_BYTES, false)
            || item
                .at
                .as_deref()
                .is_some_and(|at| !valid_text(at, 40, false))
            || (item.role == "agent" && item.evidence != Some(false))
            || (item.role == "learner" && item.evidence.is_some())
        {
            return Err(BridgeReply::err(
                code::INVALID_PARAMS,
                "conversation history is invalid",
            ));
        }
        history.push_str(&format!("{}: {}\n", item.role, content));
    }
    let prompt = format!(
        "You are responding inside a user-granted, device-local extension conversation. \
Use only the supplied object-scoped context and recent conversation. Do not inspect files, \
call tools, contact networks, claim learning evidence, or broaden the scope. Give the smallest \
useful reply and return the learner to a concrete prediction, explanation, comparison, or action.\n\n\
OBJECT-SCOPED CONTEXT (JSON):\n{context}\n\nRECENT CONVERSATION:\n{history}\nLEARNER: {message}"
    );
    if prompt.len() > MAX_PROMPT_BYTES {
        return Err(BridgeReply::err(
            code::INVALID_PARAMS,
            "conversation prompt exceeds the host limit",
        ));
    }
    Ok(prompt)
}

pub(crate) async fn converse<R: tauri::Runtime>(
    app: &AppHandle<R>,
    owner: &LeaseAuthority,
    lease: &str,
    params: Option<Value>,
) -> BridgeReply {
    if !selection_has_converse(app, owner) {
        return BridgeReply::err(code::DENIED, "agent conversation grant is missing");
    }
    let prompt = match parse_prompt(params) {
        Ok(prompt) => prompt,
        Err(reply) => return reply,
    };
    let (_guard, cancel) = match admit(lease) {
        Ok(admission) => admission,
        Err(reply) => return reply,
    };
    let reply = crate::managed_agents::converse_with_configured_agent(
        app.clone(),
        prompt,
        cancel,
    )
    .await;

    // The request may have outlived a disable, grant change, identity change,
    // removal, reinstall, package replacement, or frame close. Never return
    // useful bytes after any of those authority transitions.
    if !super::management::lease_authority_current_for_app(app, owner)
        || super::frame_host::lease_authority_snapshot(lease).as_ref() != Some(owner)
        || !selection_has_converse(app, owner)
    {
        return BridgeReply::err(code::DENIED, "conversation authority was revoked");
    }

    match reply {
        Ok(message) => conversation_reply(message),
        Err(error) => match error.as_str() {
            "agent_cancelled" => BridgeReply::err(code::DENIED, "conversation authority was revoked"),
            "agent_timeout" => BridgeReply::err(code::AGENT_TIMEOUT, "configured agent timed out"),
            "agent_not_configured" | "agent_unavailable" => {
                BridgeReply::err(code::AGENT_UNAVAILABLE, "configured local agent is unavailable")
            }
            "agent_reply_too_large" => {
                BridgeReply::err(code::QUOTA_EXCEEDED, "configured agent reply exceeded the host limit")
            }
            _ => BridgeReply::err(code::AGENT_FAILED, "configured local agent could not complete the turn"),
        },
    }
}

fn conversation_reply(message: String) -> BridgeReply {
    BridgeReply::ok(json!({"message": message, "evidence": false}))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn valid_params() -> Value {
        json!({
            "context": {
                "schemaVersion": 1,
                "challengeId": "challenge-1",
                "registryEntryId": "mean-squared-error",
                "object": {"id":"square", "kind":"node", "label":"Square", "status":"exploring"},
                "parent": {"id":"residual", "label":"Residual"},
                "children": [],
                "confusion": "Why square?",
                "learnerHow": "",
                "learnerWhy": "",
                "instruction": "Stay on this object and its immediate relationship."
            },
            "message": "Why?",
            "history": [{"role":"agent", "content":"What changes?", "at":"2026-09-01T00:00:00Z", "evidence":false}]
        })
    }

    #[test]
    fn exact_context_schema_is_accepted_and_nested_unknowns_are_rejected() {
        assert!(parse_prompt(Some(valid_params())).is_ok());
        let mut invalid = valid_params();
        invalid["context"]["object"]["canonicalSolution"] = json!("hidden");
        assert_eq!(
            parse_prompt(Some(invalid)).unwrap_err().error_code(),
            Some(code::INVALID_PARAMS)
        );
    }

    #[test]
    fn prompt_rejects_unknown_fields_and_unscoped_context() {
        assert_eq!(
            parse_prompt(Some(json!({"context": [], "message": "why?"})))
                .unwrap_err()
                .error_code(),
            Some(code::INVALID_PARAMS)
        );
        assert_eq!(
            parse_prompt(Some(json!({"context": {}, "message": "why?", "extra": true})))
                .unwrap_err()
                .error_code(),
            Some(code::INVALID_PARAMS)
        );
    }

    #[test]
    fn admission_is_single_flight_and_bounded() {
        cancel_lease("test-lease");
        let (guard, _) = admit("test-lease").expect("first request");
        assert_eq!(
            admit("test-lease").unwrap_err().error_code(),
            Some(code::RATE_LIMITED)
        );
        drop(guard);
        cancel_lease("test-lease");
    }

    #[test]
    fn successful_reply_has_the_exact_non_evidence_schema() {
        let reply = conversation_reply("Consider the selected node.".to_string());
        assert_eq!(
            reply.result,
            Some(json!({
                "message": "Consider the selected node.",
                "evidence": false
            }))
        );
        assert!(reply.ok);
        assert!(reply.error.is_none());
    }
}
