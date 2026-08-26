use super::*;

pub(super) const CHANNEL: &str = "11111111-2222-3333-4444-555555555555";
/// A host-minted lease, registered in the real frame-host map by the tests
/// that need the production lease check to resolve.
pub(super) const LEASE: &str = "lease-for-publish-tests";
pub(super) const OTHER_CHANNEL: &str = "99999999-8888-7777-6666-555555555555";

/// A grant of kind 9 in `CHANNEL`, and nothing else.
pub(super) fn granted_kind9_in_channel(kind_value: u32, channel: &str) -> bool {
    kind_value == kind::KIND_STREAM_MESSAGE && channel == CHANNEL
}

pub(super) fn tag(parts: &[&str]) -> Vec<String> {
    parts.iter().map(|p| (*p).to_string()).collect()
}

/// A well-formed kind-9 message in the granted channel.
pub(super) fn message(tags: Vec<Vec<String>>, content: &str) -> CanonicalEvent {
    CanonicalEvent {
        kind: kind::KIND_STREAM_MESSAGE,
        content: content.to_string(),
        tags,
        created_at: 1_700_000_000,
    }
}

/// Refuse nothing. Used to isolate a single gate: with every `(kind, channel)`
/// granted, the only thing left that can refuse is the check under test.
///
/// Without this, an earlier gate is untestable — the checks are ordered so
/// later ones catch the same cases, so deleting the denylist left every
/// "is it denied?" test green. Asserting *which* gate refused, with the others
/// unable to fire, is what makes each one independently defended.
pub(super) fn grants_everything(_kind: u32, _channel: &str) -> bool {
    true
}

pub(super) fn refusal(event: &CanonicalEvent) -> Option<Refusal> {
    authorise(event, granted_kind9_in_channel).err()
}

pub(super) fn refusal_with_everything_granted(event: &CanonicalEvent) -> Option<Refusal> {
    authorise(event, grants_everything).err()
}

/// `parse_template` over a minimal valid publish body.
pub(super) fn template_params(created_at: Option<i64>) -> Value {
    let mut map = serde_json::Map::new();
    map.insert("kind".into(), serde_json::json!(9));
    map.insert("content".into(), serde_json::json!("hello"));
    map.insert("tags".into(), serde_json::json!([["h", CHANNEL]]));
    if let Some(value) = created_at {
        map.insert("created_at".into(), serde_json::json!(value));
    }
    Value::Object(map)
}

pub(super) fn parse_code(params: Value, now: i64) -> Result<EventTemplate, String> {
    parse_template(Some(params), now).map_err(|reply| {
        reply
            .error
            .map(|e| e.code)
            .unwrap_or_else(|| "(none)".to_string())
    })
}
