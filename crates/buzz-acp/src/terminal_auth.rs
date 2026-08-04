//! Provider-neutral terminal-authentication classification at the ACP seam.
//!
//! A *terminal* authentication failure is one that will not repair itself
//! between retries: the credential the adapter holds is expired, absent, or
//! rejected, and only an interactive login can change that. Retrying such a
//! failure burns attempt slots, delays the user-visible notice, and — worse —
//! keeps a batch alive that can be redispatched after a restart.
//!
//! The taxonomy here is deliberately provider-neutral. Structured ACP
//! authentication errors are recognised for every adapter. Prose recognition
//! is a compatibility shim, scoped to a single adapter family (Claude) whose
//! CLI has been observed emitting authentication failures as ordinary
//! JSON-RPC error text. The *same* prose from any other adapter stays an
//! ordinary retryable provider error, because we have no evidence it means
//! the same thing there.
//!
//! Nothing in this module retains raw upstream text. [`TerminalAuth`] carries
//! only categorical values — adapter family, ACP stage, and which evidence
//! fired — so it is safe to log, display, and serialise anywhere.

use std::fmt;

/// The adapter family a spawn belongs to.
///
/// Derived from the configured command and, when available, the adapter's own
/// reported `agentInfo`/`serverInfo` name. Used only to scope legacy prose
/// compatibility; the structured path ignores it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AdapterFamily {
    /// The Claude Code ACP adapter (`claude-agent-acp`, `claude-code-acp`, …).
    Claude,
    /// The Codex ACP adapter.
    Codex,
    /// Goose.
    Goose,
    /// The in-repo minimal ACP agent.
    BuzzAgent,
    /// Anything else, including custom and unknown harnesses.
    Other,
}

impl AdapterFamily {
    /// A stable, safe, categorical identifier for this family.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Claude => "claude",
            Self::Codex => "codex",
            Self::Goose => "goose",
            Self::BuzzAgent => "buzz-agent",
            Self::Other => "other",
        }
    }

    /// Classify a configured adapter command into a family.
    ///
    /// Matching is on the command's file stem so an absolute path, a `.exe`
    /// suffix, or an npm shim all resolve identically. Nothing but the stem is
    /// retained — the path itself never leaves this function.
    pub fn from_command(command: &str) -> Self {
        let stem = command_stem(command);
        match stem.as_str() {
            "claude-agent-acp" | "claude-code-acp" | "claude-code" | "claudecode" => Self::Claude,
            "codex-acp" | "codex" => Self::Codex,
            "goose" => Self::Goose,
            "buzz-agent" => Self::BuzzAgent,
            _ => Self::Other,
        }
    }
}

impl fmt::Display for AdapterFamily {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Lowercased file stem of a command string, with any directory component and
/// executable extension removed.
///
/// Splits on both `/` and `\` regardless of host platform. A Windows-style
/// command recorded on one machine can be classified on another (tests, shared
/// fixtures, a config synced between hosts), and `Path::file_stem` would treat
/// the backslashes as ordinary characters on Unix and hand back the whole
/// string.
pub(crate) fn command_stem(command: &str) -> String {
    let file = command
        .trim()
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or_default();
    let stem = match file.rsplit_once('.') {
        Some((base, _ext)) if !base.is_empty() => base,
        _ => file,
    };
    stem.to_ascii_lowercase()
}

/// The ACP stage at which a terminal authentication failure was observed.
///
/// Categorical only: it names the protocol call, never its arguments.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthStage {
    /// `initialize`.
    Initialize,
    /// `session/new`.
    SessionNew,
    /// `session/prompt` — covers both the initial message and the final prompt.
    Prompt,
    /// Any other ACP method.
    Other,
}

impl AuthStage {
    /// A stable, safe, categorical identifier for this stage.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Initialize => "initialize",
            Self::SessionNew => "session_new",
            Self::Prompt => "prompt",
            Self::Other => "other",
        }
    }

    /// Map an ACP JSON-RPC method name onto a stage.
    pub fn from_method(method: &str) -> Self {
        match method {
            "initialize" => Self::Initialize,
            "session/new" => Self::SessionNew,
            "session/prompt" => Self::Prompt,
            _ => Self::Other,
        }
    }
}

impl fmt::Display for AuthStage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Which evidence produced a terminal-auth classification.
///
/// Recorded so operators can tell a spec-compliant adapter signal apart from a
/// legacy prose shim without the raw message ever being retained.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthSignal {
    /// The adapter reported a structured ACP authentication-required error.
    Structured,
    /// Claude legacy prose: an OAuth session that could not be refreshed.
    ClaudeOauthUnrefreshable,
    /// Claude legacy prose: an expired session asking for re-authentication.
    ClaudeReauthenticate,
    /// Claude legacy prose: an HTTP 401 from the provider API.
    ClaudeApiUnauthorized,
}

impl AuthSignal {
    /// A stable, safe, categorical identifier for this signal.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Structured => "structured",
            Self::ClaudeOauthUnrefreshable => "claude_oauth_unrefreshable",
            Self::ClaudeReauthenticate => "claude_reauthenticate",
            Self::ClaudeApiUnauthorized => "claude_api_unauthorized",
        }
    }
}

impl fmt::Display for AuthSignal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A fully redacted terminal-authentication disposition.
///
/// Every field is a closed enum. There is no constructor that accepts free
/// text, so no upstream message, header, token, or credential document can be
/// smuggled into it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TerminalAuth {
    /// The adapter family that reported the failure.
    pub adapter: AdapterFamily,
    /// The ACP stage at which it was reported.
    pub stage: AuthStage,
    /// Which evidence fired.
    pub signal: AuthSignal,
}

impl fmt::Display for TerminalAuth {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "adapter={} stage={} signal={}",
            self.adapter, self.stage, self.signal
        )
    }
}

/// Who we are talking to, as far as compatibility matching is concerned.
///
/// `configured` comes from the harness configuration and is known before the
/// process starts. `reported` is filled in from the adapter's own
/// `agentInfo`/`serverInfo` at `initialize` and is used only to *widen*
/// recognition when the configured command is an opaque wrapper (a shim, a
/// `npx` invocation, a renamed binary).
#[derive(Debug, Clone, Default)]
pub struct AdapterIdentity {
    /// Family derived from the configured command.
    configured: Option<AdapterFamily>,
    /// Family derived from the adapter's reported name, when it reports one.
    reported: Option<AdapterFamily>,
}

impl AdapterIdentity {
    /// Build an identity from the configured adapter command alone.
    pub fn from_command(command: &str) -> Self {
        Self {
            configured: Some(AdapterFamily::from_command(command)),
            reported: None,
        }
    }

    /// Record the adapter's self-reported identity from an `initialize` result.
    ///
    /// Reads `serverInfo` (the ACP spelling) then `agentInfo` (the MCP-heritage
    /// spelling), matching every other identity reader in this crate. Only the
    /// derived family is retained; the raw name and version are dropped.
    pub fn observe_initialize(&mut self, init_result: &serde_json::Value) {
        let name = init_result
            .get("serverInfo")
            .or_else(|| init_result.get("agentInfo"))
            .and_then(|info| info.get("name"))
            .and_then(|v| v.as_str());
        let Some(name) = name else { return };
        self.reported = Some(family_from_reported_name(name));
    }

    /// The effective family for compatibility matching.
    ///
    /// A configured command that already names a known family wins: it is the
    /// operator's own statement of what they are running. Otherwise the
    /// adapter's reported name is consulted, which is what lets a wrapper
    /// script still be recognised.
    pub fn family(&self) -> AdapterFamily {
        match self.configured {
            Some(AdapterFamily::Other) | None => self.reported.unwrap_or(AdapterFamily::Other),
            Some(family) => family,
        }
    }

    /// The family the adapter reported for *itself*, independent of what we
    /// configured.
    ///
    /// Distinct from [`family`](Self::family) on purpose: compatibility
    /// matching wants the effective answer, while a mismatch check needs to
    /// see the two sides disagree. Collapsing them would make an adapter that
    /// is demonstrably the wrong binary indistinguishable from the right one.
    pub fn reported_family(&self) -> AdapterFamily {
        self.reported.unwrap_or(AdapterFamily::Other)
    }
}

/// Derive a family from an adapter's self-reported name.
///
/// Deliberately narrow: an adapter reporting a name that *contains* `claude`
/// is treated as the Claude family, because the observed adapters report
/// `claude-code-acp`, `Claude Code`, and `claude-agent-acp`. Names are
/// otherwise mapped exactly.
fn family_from_reported_name(name: &str) -> AdapterFamily {
    let lower = name.trim().to_ascii_lowercase();
    if lower.contains("claude") {
        return AdapterFamily::Claude;
    }
    match lower.as_str() {
        "codex" | "codex-acp" => AdapterFamily::Codex,
        "goose" => AdapterFamily::Goose,
        "buzz-agent" => AdapterFamily::BuzzAgent,
        _ => AdapterFamily::Other,
    }
}

// ── Structured classification ────────────────────────────────────────────────

/// JSON-RPC error code ACP adapters use for "authentication required".
///
/// This is also the generic implementation-defined fallback code, so the code
/// alone is never sufficient — it must be accompanied by an explicit
/// authentication marker (see [`structured_auth_required`]).
const ACP_AUTH_REQUIRED_CODE: i64 = -32000;

/// Values an adapter may put in the error `data` payload to say
/// "authentication required" in a structured, machine-readable way.
const STRUCTURED_AUTH_TOKENS: &[&str] = &[
    "auth_required",
    "authrequired",
    "authentication_required",
    "authenticationrequired",
];

/// Returns `true` when a JSON-RPC error object carries a **structured**
/// authentication-required signal.
///
/// Two accepted shapes:
///
/// 1. A recognised token in the error `data` payload — as a bare string, as
///    the value of a `type`/`code`/`error`/`reason`/`kind` field, or as
///    `authRequired: true`. This is code-agnostic: an adapter that picks its
///    own numeric code is still recognised.
/// 2. The ACP `-32000` code paired with an exact `Authentication required`
///    message. `-32000` alone means nothing (it is this crate's own fallback
///    for a code-less error), so the message must corroborate it.
fn structured_auth_required(error: &serde_json::Value) -> bool {
    if let Some(data) = error.get("data") {
        if data_carries_auth_token(data) {
            return true;
        }
    }

    let code = error.get("code").and_then(|c| c.as_i64());
    if code == Some(ACP_AUTH_REQUIRED_CODE) {
        if let Some(message) = error.get("message").and_then(|m| m.as_str()) {
            if message
                .trim()
                .eq_ignore_ascii_case("authentication required")
            {
                return true;
            }
        }
    }

    false
}

/// Look for an authentication-required token inside an error `data` payload.
fn data_carries_auth_token(data: &serde_json::Value) -> bool {
    let matches_token = |value: &str| {
        let normalized = value.trim().to_ascii_lowercase().replace([' ', '-'], "_");
        STRUCTURED_AUTH_TOKENS.contains(&normalized.as_str())
            || STRUCTURED_AUTH_TOKENS.contains(&normalized.replace('_', "").as_str())
    };

    match data {
        serde_json::Value::String(s) => matches_token(s),
        serde_json::Value::Object(map) => {
            if map
                .get("authRequired")
                .or_else(|| map.get("auth_required"))
                .and_then(|v| v.as_bool())
                == Some(true)
            {
                return true;
            }
            for key in ["type", "code", "error", "reason", "kind"] {
                if let Some(value) = map.get(key).and_then(|v| v.as_str()) {
                    if matches_token(value) {
                        return true;
                    }
                }
            }
            false
        }
        _ => false,
    }
}

// ── Claude legacy prose compatibility ────────────────────────────────────────

/// Exact prose the Claude CLI emits when an OAuth session cannot be refreshed.
///
/// Observed verbatim in the field. Matched as a substring because the adapter
/// wraps it in its own framing (`API Error: …`, `Internal error: …`), but the
/// phrase itself is matched in full — no partial word ever fires this.
const CLAUDE_OAUTH_UNREFRESHABLE: &str = "OAuth session expired and could not be refreshed";

/// Exact prose the Claude CLI emits when its OAuth access token has expired.
///
/// Matched in full. A bare `Re-authenticate` substring is not enough and must
/// never be used: adapters relay *other* services' login prompts through the
/// same channel — `GitHub integration unavailable. Re-authenticate GitHub to
/// continue.` is an ordinary tool failure, and classifying it as terminal
/// would durably tombstone the user's request over a GitHub token.
const CLAUDE_ACCESS_TOKEN_EXPIRED: &str =
    "OAuth access token has expired. Re-authenticate to continue.";

/// The HTTP-401 form Claude surfaces through the adapter.
const CLAUDE_API_UNAUTHORIZED: &str = "API Error: 401";

/// Recognise the three legacy Claude authentication prose forms.
///
/// Scoped to [`AdapterFamily::Claude`] by the caller. Ordered most-specific
/// first so the recorded signal names the strongest evidence present.
///
/// Every form here is matched as a *complete observed phrase*. Each one is
/// permitted to be wrapped in the adapter's own framing (`API Error: …`,
/// `Internal error: …`), which is why these are substring tests rather than
/// equality — but no individual word of any phrase can fire on its own.
fn claude_legacy_signal(message: &str) -> Option<AuthSignal> {
    if message.contains(CLAUDE_OAUTH_UNREFRESHABLE) {
        return Some(AuthSignal::ClaudeOauthUnrefreshable);
    }
    if message.contains(CLAUDE_ACCESS_TOKEN_EXPIRED) {
        return Some(AuthSignal::ClaudeReauthenticate);
    }
    if message.contains(CLAUDE_API_UNAUTHORIZED) {
        return Some(AuthSignal::ClaudeApiUnauthorized);
    }
    None
}

// ── Entry point ──────────────────────────────────────────────────────────────

/// Classify a JSON-RPC error object from an ACP adapter.
///
/// Returns `Some(TerminalAuth)` only when the failure is non-retryable
/// authentication. Everything else — transport blips, provider rate limits,
/// tool errors, unrelated Claude errors — returns `None` and keeps its
/// existing retryable treatment.
///
/// Structured evidence is checked first and applies to every adapter. Legacy
/// prose is checked second and only for the Claude family.
pub fn classify_jsonrpc_error(
    error: &serde_json::Value,
    identity: &AdapterIdentity,
    stage: AuthStage,
) -> Option<TerminalAuth> {
    let adapter = identity.family();

    if structured_auth_required(error) {
        return Some(TerminalAuth {
            adapter,
            stage,
            signal: AuthSignal::Structured,
        });
    }

    if adapter != AdapterFamily::Claude {
        return None;
    }

    // The message field is the only place the legacy forms appear. When it is
    // absent or non-string there is no prose to match and we fail open to
    // "retryable", which is the safe direction: an extra retry costs an
    // attempt slot, a false terminal disposition destroys a user's request.
    let message = error.get("message").and_then(|m| m.as_str())?;
    claude_legacy_signal(message).map(|signal| TerminalAuth {
        adapter,
        stage,
        signal,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn claude() -> AdapterIdentity {
        AdapterIdentity::from_command("claude-agent-acp")
    }

    fn goose() -> AdapterIdentity {
        AdapterIdentity::from_command("goose")
    }

    #[test]
    fn command_families_resolve_from_path_and_suffix() {
        assert_eq!(
            AdapterFamily::from_command("/usr/local/bin/claude-agent-acp"),
            AdapterFamily::Claude
        );
        assert_eq!(
            AdapterFamily::from_command(r"C:\Tools\claude-code-acp.exe"),
            AdapterFamily::Claude
        );
        assert_eq!(
            AdapterFamily::from_command("codex-acp"),
            AdapterFamily::Codex
        );
        assert_eq!(AdapterFamily::from_command("goose"), AdapterFamily::Goose);
        assert_eq!(
            AdapterFamily::from_command("my-acp-agent"),
            AdapterFamily::Other
        );
    }

    #[test]
    fn reported_identity_widens_only_an_unknown_configured_command() {
        let mut wrapper = AdapterIdentity::from_command("run-my-adapter.sh");
        assert_eq!(wrapper.family(), AdapterFamily::Other);
        wrapper.observe_initialize(&json!({"serverInfo": {"name": "claude-code-acp"}}));
        assert_eq!(wrapper.family(), AdapterFamily::Claude);

        // A configured Goose command is not reclassified by a reported name.
        let mut configured = AdapterIdentity::from_command("goose");
        configured.observe_initialize(&json!({"agentInfo": {"name": "Claude Code"}}));
        assert_eq!(configured.family(), AdapterFamily::Goose);
    }

    #[test]
    fn structured_auth_is_terminal_for_every_adapter() {
        for identity in [claude(), goose(), AdapterIdentity::from_command("weird")] {
            let error = json!({"code": -32000, "message": "Authentication required"});
            let classified = classify_jsonrpc_error(&error, &identity, AuthStage::SessionNew)
                .expect("structured auth must classify for every adapter");
            assert_eq!(classified.signal, AuthSignal::Structured);
            assert_eq!(classified.stage, AuthStage::SessionNew);
        }
    }

    #[test]
    fn structured_auth_data_shapes_are_recognised() {
        let shapes = [
            json!({"code": -32603, "message": "nope", "data": "auth_required"}),
            json!({"code": 42, "message": "nope", "data": {"type": "authRequired"}}),
            json!({"code": 42, "message": "nope", "data": {"error": "authentication required"}}),
            json!({"code": 42, "message": "nope", "data": {"authRequired": true}}),
        ];
        for error in shapes {
            assert!(
                classify_jsonrpc_error(&error, &goose(), AuthStage::Prompt).is_some(),
                "structured shape must classify: {error}"
            );
        }
    }

    #[test]
    fn bare_minus_32000_without_corroboration_is_not_terminal() {
        // -32000 is this crate's own fallback code for a code-less error, so it
        // must never be sufficient on its own.
        let error = json!({"code": -32000, "message": "quota exceeded"});
        assert!(classify_jsonrpc_error(&error, &goose(), AuthStage::Prompt).is_none());
        assert!(classify_jsonrpc_error(&error, &claude(), AuthStage::Prompt).is_none());
    }

    #[test]
    fn claude_legacy_forms_classify_with_their_own_signal() {
        let cases = [
            (
                "API Error: OAuth session expired and could not be refreshed",
                AuthSignal::ClaudeOauthUnrefreshable,
            ),
            (
                "API Error: OAuth access token has expired. Re-authenticate to continue.",
                AuthSignal::ClaudeReauthenticate,
            ),
            (
                "Internal error: API Error: 401 unauthorized",
                AuthSignal::ClaudeApiUnauthorized,
            ),
        ];
        for (message, expected) in cases {
            let error = json!({"code": -32000, "message": message});
            let classified = classify_jsonrpc_error(&error, &claude(), AuthStage::Prompt)
                .unwrap_or_else(|| panic!("must classify: {message}"));
            assert_eq!(classified.signal, expected);
            assert_eq!(classified.adapter, AdapterFamily::Claude);
        }
    }

    #[test]
    fn identical_prose_from_a_non_claude_adapter_stays_retryable() {
        let messages = [
            "API Error: OAuth session expired and could not be refreshed",
            "API Error: OAuth access token has expired. Re-authenticate to continue.",
            "Internal error: API Error: 401 unauthorized",
        ];
        for message in messages {
            let error = json!({"code": -32000, "message": message});
            for identity in [goose(), AdapterIdentity::from_command("codex-acp")] {
                assert!(
                    classify_jsonrpc_error(&error, &identity, AuthStage::Prompt).is_none(),
                    "non-Claude adapter must stay retryable: {message}"
                );
            }
        }
    }

    #[test]
    fn another_services_login_prompt_is_not_a_claude_auth_failure() {
        // A Claude session relays *other* services' authentication prose. A
        // bare `Re-authenticate` substring classified all of it as terminal,
        // which would durably tombstone the user's request over someone
        // else's expired token.
        let messages = [
            "GitHub integration unavailable. Re-authenticate GitHub to continue.",
            "Re-authenticate with Jira to continue.",
            "mcp server 'linear' rejected the request: re-authenticate and retry",
            "Slack token rejected — please Re-authenticate.",
            // The right words, but not the observed Claude sentence.
            "OAuth access token has expired.",
            "Re-authenticate to continue.",
        ];
        for message in messages {
            let error = json!({"code": -32000, "message": message});
            assert!(
                classify_jsonrpc_error(&error, &claude(), AuthStage::Prompt).is_none(),
                "an unrelated re-auth instruction must stay retryable: {message}"
            );
        }
    }

    #[test]
    fn unrelated_claude_errors_stay_retryable() {
        let messages = [
            "API Error: 500 internal server error",
            "API Error: 429 rate limit exceeded",
            "tool execution failed: file not found",
            "authenticated successfully",
            "",
        ];
        for message in messages {
            let error = json!({"code": -32000, "message": message});
            assert!(
                classify_jsonrpc_error(&error, &claude(), AuthStage::Prompt).is_none(),
                "unrelated Claude error must stay retryable: {message}"
            );
        }
    }

    #[test]
    fn malformed_error_objects_do_not_panic_and_stay_retryable() {
        let malformed = [
            json!({}),
            json!({"code": "not-a-number"}),
            json!({"message": 7}),
            json!({"data": []}),
            json!({"data": null}),
            json!(null),
            json!("just a string"),
        ];
        for error in malformed {
            assert!(classify_jsonrpc_error(&error, &claude(), AuthStage::Other).is_none());
            assert!(classify_jsonrpc_error(&error, &goose(), AuthStage::Other).is_none());
        }
    }

    #[test]
    fn stage_maps_from_acp_method_names() {
        assert_eq!(AuthStage::from_method("initialize"), AuthStage::Initialize);
        assert_eq!(AuthStage::from_method("session/new"), AuthStage::SessionNew);
        assert_eq!(AuthStage::from_method("session/prompt"), AuthStage::Prompt);
        assert_eq!(AuthStage::from_method("session/cancel"), AuthStage::Other);
    }

    #[test]
    fn display_carries_only_categorical_values() {
        let terminal = TerminalAuth {
            adapter: AdapterFamily::Claude,
            stage: AuthStage::Prompt,
            signal: AuthSignal::ClaudeApiUnauthorized,
        };
        assert_eq!(
            terminal.to_string(),
            "adapter=claude stage=prompt signal=claude_api_unauthorized"
        );
    }
}
