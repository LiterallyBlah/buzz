//! One-shot provider capability probe.
//!
//! Static checks — is the binary on PATH, does the CLI report a login — answer
//! whether an agent *looks* configured. They cannot answer whether the
//! provider will actually accept a turn: a hosted Claude session whose OAuth
//! refresh has failed passes every static check and then fails on first use,
//! which is exactly the failure this probe exists to catch before a runtime is
//! advertised as usable.
//!
//! So the probe does the only thing that settles the question: it spawns a
//! disposable adapter, completes `initialize` → `session/new` → one fixed
//! tool-disabled `session/prompt`, and requires the turn to finish with
//! `end_turn`. Nothing about the model's answer matters and none of it is
//! printed — only that a turn completed.
//!
//! # Fail-closed
//!
//! Every uncertainty is not-ready: a refusal, a token or request limit, a
//! timeout, malformed output, an adapter that is not the one we configured, or
//! a cleanup we could not confirm. "Ready" has exactly one shape.
//!
//! # Output safety
//!
//! stdout carries exactly one compact JSON object of closed categorical
//! values, plus the adapter's own reported name and version (length-capped and
//! stripped of control characters). No path, argument, environment value,
//! prompt text, model output, raw provider error, or credential can reach it.

use std::path::Path;
use std::time::Duration;

use crate::acp::{AcpClient, ChildReap, StopReason};
use crate::terminal_auth::AdapterFamily;

/// Output schema version. Consumers parse strictly and reject anything else.
pub const SCHEMA_VERSION: u32 = 1;

/// The fixed probe prompt. Deliberately trivial and tool-free: we are testing
/// the provider round-trip, not the model.
pub const PROBE_PROMPT: &str = "Reply with exactly OK. Do not use tools.";

/// Budget for the whole non-prompt protocol sequence (spawn is excluded — the
/// client is owned before the clock starts so cleanup always runs).
pub const OPERATION_BUDGET: Duration = Duration::from_secs(20);

/// Idle budget for the probe turn: reset by any adapter activity.
pub const PROMPT_IDLE_BUDGET: Duration = Duration::from_secs(10);

/// Absolute wall-clock cap for the probe turn.
pub const PROMPT_HARD_BUDGET: Duration = Duration::from_secs(15);

/// Grace for reaping the adapter process group after cleanup starts.
pub const REAP_GRACE: Duration = Duration::from_secs(5);

/// Longest adapter-reported name/version we will echo.
const MAX_REPORTED_LEN: usize = 64;

/// How far the probe got.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ProbeStage {
    Spawn,
    Initialize,
    SessionNew,
    Prompt,
    Cleanup,
}

/// Closed set of not-ready reasons.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ProbeReason {
    /// The provider rejected our credentials — only an interactive login fixes it.
    AuthenticationRequired,
    /// The provider answered, but not with a completed turn: a refusal, a
    /// token/request limit, or an ordinary provider error.
    ProviderRejected,
    /// A budget expired.
    TimedOut,
    /// The adapter spoke ACP badly, or could not be started at all.
    ProtocolError,
    /// The adapter is not the one we configured.
    AdapterMismatch,
    /// The disposable process could not be confirmed dead.
    CleanupFailed,
}

/// The probe's entire public output.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct ProbeReport {
    pub schema_version: u32,
    pub status: &'static str,
    pub stage: ProbeStage,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<ProbeReason>,
    /// The configured adapter's categorical id (`claude`, `codex`, …).
    pub adapter_id: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub adapter_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub adapter_version: Option<String>,
    /// Present only when ready, and only ever `"end_turn"`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stop_reason: Option<&'static str>,
}

impl ProbeReport {
    fn not_ready(
        adapter_id: &'static str,
        stage: ProbeStage,
        reason: ProbeReason,
        identity: ReportedIdentity,
    ) -> Self {
        Self {
            schema_version: SCHEMA_VERSION,
            status: "not_ready",
            stage,
            reason: Some(reason),
            adapter_id,
            adapter_name: identity.name,
            adapter_version: identity.version,
            stop_reason: None,
        }
    }

    fn ready(adapter_id: &'static str, identity: ReportedIdentity) -> Self {
        Self {
            schema_version: SCHEMA_VERSION,
            status: "ready",
            stage: ProbeStage::Cleanup,
            reason: None,
            adapter_id,
            adapter_name: identity.name,
            adapter_version: identity.version,
            stop_reason: Some("end_turn"),
        }
    }

    /// Whether the probe proved the provider usable.
    pub fn is_ready(&self) -> bool {
        self.status == "ready"
    }
}

/// The adapter's self-reported name and version, sanitised for output.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct ReportedIdentity {
    name: Option<String>,
    version: Option<String>,
}

impl ReportedIdentity {
    fn from_initialize(init: &serde_json::Value) -> Self {
        let info = init.get("serverInfo").or_else(|| init.get("agentInfo"));
        Self {
            name: info
                .and_then(|i| i.get("name"))
                .and_then(|v| v.as_str())
                .and_then(sanitize_reported),
            version: info
                .and_then(|i| i.get("version"))
                .and_then(|v| v.as_str())
                .and_then(sanitize_reported),
        }
    }
}

/// Trim, strip control characters, and length-cap an adapter-reported string.
///
/// The adapter chooses these values, so they are untrusted input on our stdout.
/// Control characters could break the single-line JSON contract; an unbounded
/// length could turn a readiness check into a memory problem.
fn sanitize_reported(raw: &str) -> Option<String> {
    let cleaned: String = raw
        .trim()
        .chars()
        .filter(|c| !c.is_control())
        .take(MAX_REPORTED_LEN)
        .collect();
    let cleaned = cleaned.trim().to_string();
    (!cleaned.is_empty()).then_some(cleaned)
}

/// Classify an [`AcpError`] into a probe reason.
///
/// [`AcpError`]: crate::acp::AcpError
fn reason_for_error(error: &crate::acp::AcpError) -> ProbeReason {
    use crate::acp::AcpError;
    match error {
        AcpError::TerminalAuth(_) => ProbeReason::AuthenticationRequired,
        AcpError::IdleTimeout(_)
        | AcpError::HardTimeout { .. }
        | AcpError::Timeout(_)
        | AcpError::WriteTimeout(_)
        | AcpError::CancelDrainTimeout(_) => ProbeReason::TimedOut,
        AcpError::AgentError { .. } => ProbeReason::ProviderRejected,
        AcpError::Io(_) | AcpError::Json(_) | AcpError::AgentExited | AcpError::Protocol(_) => {
            ProbeReason::ProtocolError
        }
    }
}

/// Classify a completed turn's stop reason.
///
/// `end_turn` is the only ready result. Everything else — a refusal, a token
/// limit, a request limit, a cancellation — is not-ready, because none of them
/// prove the provider will complete an ordinary turn.
fn reason_for_stop(stop: &StopReason) -> Option<ProbeReason> {
    match stop {
        StopReason::EndTurn => None,
        StopReason::Refusal
        | StopReason::MaxTokens
        | StopReason::MaxTurnRequests
        | StopReason::Cancelled => Some(ProbeReason::ProviderRejected),
    }
}

/// Whether the adapter that answered is the one we configured.
///
/// Only a *contradiction* fails: a configured family of `Other` (a custom or
/// wrapper command) matches anything, and an adapter that reports no
/// recognisable name is not treated as a mismatch. What must not happen is
/// running a Claude readiness gate against something that is demonstrably not
/// Claude.
fn identity_matches(configured: AdapterFamily, reported: AdapterFamily) -> bool {
    configured == AdapterFamily::Other || reported == AdapterFamily::Other || configured == reported
}

/// Run the probe and return its report.
///
/// The client is owned before any budget starts, so every exit path — success,
/// error, timeout — reaches the cleanup below and reaps the child.
pub async fn run_probe(command: &str, args: &[String], cwd: &Path) -> ProbeReport {
    let adapter_family = AdapterFamily::from_command(command);
    let adapter_id = adapter_family.as_str();

    // A working directory that does not exist is our own configuration
    // problem, not the provider's. Fail closed before spawning anything.
    if !cwd.is_dir() {
        return ProbeReport::not_ready(
            adapter_id,
            ProbeStage::Spawn,
            ProbeReason::ProtocolError,
            ReportedIdentity::default(),
        );
    }
    let Some(cwd_str) = cwd.to_str() else {
        return ProbeReport::not_ready(
            adapter_id,
            ProbeStage::Spawn,
            ProbeReason::ProtocolError,
            ReportedIdentity::default(),
        );
    };

    let mut client = match AcpClient::spawn(command, args, &[], false).await {
        Ok(client) => client,
        Err(_) => {
            return ProbeReport::not_ready(
                adapter_id,
                ProbeStage::Spawn,
                ProbeReason::ProtocolError,
                ReportedIdentity::default(),
            );
        }
    };

    let (report, session_id) =
        probe_protocol(&mut client, adapter_family, adapter_id, cwd_str).await;

    // Cleanup always runs. A graceful ACP cancel first (best effort, and only
    // when a session exists), then the process-group kill and bounded reap.
    if let Some(session_id) = session_id {
        let _ = tokio::time::timeout(REAP_GRACE, client.session_cancel(&session_id)).await;
    }
    let reap = client.shutdown().await;
    apply_cleanup(report, &reap, adapter_id)
}

/// Fold the cleanup outcome into the report.
///
/// A child we could not confirm dead invalidates the whole answer, even a
/// successful turn: advertising a runtime as ready while an unaccounted-for
/// adapter process may still be holding the provider session is exactly the
/// kind of half-truth this probe exists to eliminate.
fn apply_cleanup(
    mut report: ProbeReport,
    reap: &ChildReap,
    adapter_id: &'static str,
) -> ProbeReport {
    match reap {
        ChildReap::Reaped(_) => report,
        ChildReap::TimedOut | ChildReap::WaitError(_) => {
            let identity = ReportedIdentity {
                name: report.adapter_name.take(),
                version: report.adapter_version.take(),
            };
            ProbeReport::not_ready(
                adapter_id,
                ProbeStage::Cleanup,
                ProbeReason::CleanupFailed,
                identity,
            )
        }
    }
}

/// The protocol half of the probe, bounded by [`OPERATION_BUDGET`].
///
/// Returns the report plus the session id, so the caller can cancel it during
/// cleanup even when the probe failed partway through.
async fn probe_protocol(
    client: &mut AcpClient,
    adapter_family: AdapterFamily,
    adapter_id: &'static str,
    cwd: &str,
) -> (ProbeReport, Option<String>) {
    let deadline = tokio::time::Instant::now() + OPERATION_BUDGET;

    // ── initialize ───────────────────────────────────────────────────────────
    let init = match tokio::time::timeout_at(deadline, client.initialize()).await {
        Ok(Ok(init)) => init,
        Ok(Err(e)) => {
            return (
                ProbeReport::not_ready(
                    adapter_id,
                    ProbeStage::Initialize,
                    reason_for_error(&e),
                    ReportedIdentity::default(),
                ),
                None,
            )
        }
        Err(_) => {
            return (
                ProbeReport::not_ready(
                    adapter_id,
                    ProbeStage::Initialize,
                    ProbeReason::TimedOut,
                    ReportedIdentity::default(),
                ),
                None,
            )
        }
    };
    let identity = ReportedIdentity::from_initialize(&init);

    if !identity_matches(adapter_family, client.reported_adapter_family()) {
        return (
            ProbeReport::not_ready(
                adapter_id,
                ProbeStage::Initialize,
                ProbeReason::AdapterMismatch,
                identity,
            ),
            None,
        );
    }

    // ── session/new ──────────────────────────────────────────────────────────
    let session =
        match tokio::time::timeout_at(deadline, client.session_new_full(cwd, vec![], None, None))
            .await
        {
            Ok(Ok(session)) => session,
            Ok(Err(e)) => {
                return (
                    ProbeReport::not_ready(
                        adapter_id,
                        ProbeStage::SessionNew,
                        reason_for_error(&e),
                        identity,
                    ),
                    None,
                )
            }
            Err(_) => {
                return (
                    ProbeReport::not_ready(
                        adapter_id,
                        ProbeStage::SessionNew,
                        ProbeReason::TimedOut,
                        identity,
                    ),
                    None,
                )
            }
        };
    let session_id = session.session_id;

    // ── one tool-disabled turn ───────────────────────────────────────────────
    //
    // The prompt gets its own idle and hard budgets rather than the operation
    // deadline: a provider that is merely slow to first token should not be
    // failed by time already spent on session setup.
    let outcome = client
        .session_prompt_with_idle_timeout(
            &session_id,
            PROBE_PROMPT,
            PROMPT_IDLE_BUDGET,
            PROMPT_HARD_BUDGET,
        )
        .await;

    let report = match outcome {
        Ok(stop) => match reason_for_stop(&stop) {
            None => ProbeReport::ready(adapter_id, identity),
            Some(reason) => {
                ProbeReport::not_ready(adapter_id, ProbeStage::Prompt, reason, identity)
            }
        },
        Err(e) => ProbeReport::not_ready(
            adapter_id,
            ProbeStage::Prompt,
            reason_for_error(&e),
            identity,
        ),
    };
    (report, Some(session_id))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::acp::AcpError;

    fn identity(name: &str, version: &str) -> ReportedIdentity {
        ReportedIdentity {
            name: Some(name.to_string()),
            version: Some(version.to_string()),
        }
    }

    #[test]
    fn ready_report_serialises_to_one_compact_object() {
        let report = ProbeReport::ready("claude", identity("claude-code-acp", "1.2.3"));
        let json = serde_json::to_string(&report).expect("serialise");
        assert!(!json.contains('\n'), "output must be a single line: {json}");
        let parsed: serde_json::Value = serde_json::from_str(&json).expect("parse");
        assert_eq!(parsed["schema_version"], 1);
        assert_eq!(parsed["status"], "ready");
        assert_eq!(parsed["stage"], "cleanup");
        assert_eq!(parsed["adapter_id"], "claude");
        assert_eq!(parsed["adapter_name"], "claude-code-acp");
        assert_eq!(parsed["adapter_version"], "1.2.3");
        assert_eq!(parsed["stop_reason"], "end_turn");
        assert!(parsed.get("reason").is_none(), "ready carries no reason");
    }

    #[test]
    fn not_ready_report_omits_stop_reason() {
        let report = ProbeReport::not_ready(
            "claude",
            ProbeStage::Prompt,
            ProbeReason::AuthenticationRequired,
            ReportedIdentity::default(),
        );
        let parsed: serde_json::Value =
            serde_json::from_str(&serde_json::to_string(&report).expect("serialise"))
                .expect("parse");
        assert_eq!(parsed["status"], "not_ready");
        assert_eq!(parsed["reason"], "authentication_required");
        assert!(parsed.get("stop_reason").is_none());
        assert!(parsed.get("adapter_name").is_none());
    }

    #[test]
    fn every_report_field_is_categorical() {
        // The full field set is closed. A new field that could carry free text
        // has to be added here deliberately, which is the point.
        let report = ProbeReport::ready("codex", identity("codex-acp", "0.9"));
        let parsed: serde_json::Value =
            serde_json::from_str(&serde_json::to_string(&report).expect("serialise"))
                .expect("parse");
        let mut keys: Vec<&str> = parsed
            .as_object()
            .expect("object")
            .keys()
            .map(String::as_str)
            .collect();
        keys.sort_unstable();
        assert_eq!(
            keys,
            [
                "adapter_id",
                "adapter_name",
                "adapter_version",
                "schema_version",
                "stage",
                "status",
                "stop_reason",
            ]
        );
    }

    #[test]
    fn only_end_turn_is_ready() {
        assert_eq!(reason_for_stop(&StopReason::EndTurn), None);
        for stop in [
            StopReason::Refusal,
            StopReason::MaxTokens,
            StopReason::MaxTurnRequests,
            StopReason::Cancelled,
        ] {
            assert_eq!(
                reason_for_stop(&stop),
                Some(ProbeReason::ProviderRejected),
                "{stop:?} must fail closed"
            );
        }
    }

    #[test]
    fn errors_map_to_their_categorical_reasons() {
        use crate::terminal_auth::{AuthSignal, AuthStage, TerminalAuth};
        let terminal = AcpError::TerminalAuth(TerminalAuth {
            adapter: AdapterFamily::Claude,
            stage: AuthStage::Prompt,
            signal: AuthSignal::ClaudeReauthenticate,
        });
        assert_eq!(
            reason_for_error(&terminal),
            ProbeReason::AuthenticationRequired
        );
        assert_eq!(
            reason_for_error(&AcpError::IdleTimeout(Duration::from_secs(1))),
            ProbeReason::TimedOut
        );
        assert_eq!(
            reason_for_error(&AcpError::HardTimeout {
                silence: Duration::from_secs(1)
            }),
            ProbeReason::TimedOut
        );
        assert_eq!(
            reason_for_error(&AcpError::AgentError {
                code: -32000,
                message: "rate limited".into()
            }),
            ProbeReason::ProviderRejected
        );
        assert_eq!(
            reason_for_error(&AcpError::Protocol("garbage".into())),
            ProbeReason::ProtocolError
        );
        assert_eq!(
            reason_for_error(&AcpError::AgentExited),
            ProbeReason::ProtocolError
        );
    }

    #[test]
    fn identity_mismatch_only_fires_on_a_contradiction() {
        assert!(identity_matches(
            AdapterFamily::Claude,
            AdapterFamily::Claude
        ));
        // A custom wrapper command matches whatever answers.
        assert!(identity_matches(AdapterFamily::Other, AdapterFamily::Goose));
        // An adapter that reports nothing recognisable is not a contradiction.
        assert!(identity_matches(
            AdapterFamily::Claude,
            AdapterFamily::Other
        ));
        // A configured Claude runtime answered by Goose is.
        assert!(!identity_matches(
            AdapterFamily::Claude,
            AdapterFamily::Goose
        ));
    }

    #[test]
    fn reported_identity_is_sanitised() {
        assert_eq!(
            sanitize_reported("  claude-code-acp \n"),
            Some("claude-code-acp".to_string())
        );
        assert_eq!(sanitize_reported(""), None);
        assert_eq!(sanitize_reported("   "), None);
        assert_eq!(sanitize_reported("a\nb\tc"), Some("abc".to_string()));
        let long = "x".repeat(500);
        assert_eq!(
            sanitize_reported(&long).map(|s| s.len()),
            Some(MAX_REPORTED_LEN)
        );
    }

    #[test]
    fn reported_identity_reads_both_acp_and_mcp_spellings() {
        let acp = ReportedIdentity::from_initialize(
            &serde_json::json!({"serverInfo": {"name": "claude-code-acp", "version": "2.0"}}),
        );
        assert_eq!(acp, identity("claude-code-acp", "2.0"));

        let mcp = ReportedIdentity::from_initialize(
            &serde_json::json!({"agentInfo": {"name": "goose", "version": "1.0"}}),
        );
        assert_eq!(mcp, identity("goose", "1.0"));

        assert_eq!(
            ReportedIdentity::from_initialize(&serde_json::json!({})),
            ReportedIdentity::default()
        );
    }

    #[test]
    fn an_unconfirmed_cleanup_fails_a_would_be_ready_probe() {
        let ready = ProbeReport::ready("claude", identity("claude-code-acp", "1.0"));
        for reap in [
            ChildReap::TimedOut,
            ChildReap::WaitError("wait failed".into()),
        ] {
            let folded = apply_cleanup(ready.clone(), &reap, "claude");
            assert!(!folded.is_ready(), "{reap:?} must invalidate a ready probe");
            assert_eq!(folded.stage, ProbeStage::Cleanup);
            assert_eq!(folded.reason, Some(ProbeReason::CleanupFailed));
            // The adapter identity survives the downgrade so the desktop can
            // still say which adapter misbehaved.
            assert_eq!(folded.adapter_name.as_deref(), Some("claude-code-acp"));
            assert!(folded.stop_reason.is_none());
        }
    }

    #[test]
    fn a_confirmed_reap_leaves_the_report_alone() {
        let ready = ProbeReport::ready("claude", identity("claude-code-acp", "1.0"));
        let reaped = ChildReap::Reaped(exited_status());
        assert_eq!(apply_cleanup(ready.clone(), &reaped, "claude"), ready);

        let failed = ProbeReport::not_ready(
            "claude",
            ProbeStage::Prompt,
            ProbeReason::AuthenticationRequired,
            ReportedIdentity::default(),
        );
        assert_eq!(apply_cleanup(failed.clone(), &reaped, "claude"), failed);
    }

    /// An `ExitStatus` for a process that ran and exited, without assuming a
    /// platform-specific constructor.
    fn exited_status() -> std::process::ExitStatus {
        std::process::Command::new("true")
            .status()
            .or_else(|_| {
                std::process::Command::new("cmd")
                    .args(["/C", "exit 0"])
                    .status()
            })
            .expect("a trivially successful process")
    }

    #[test]
    fn budgets_match_the_documented_contract() {
        assert_eq!(OPERATION_BUDGET, Duration::from_secs(20));
        assert_eq!(PROMPT_IDLE_BUDGET, Duration::from_secs(10));
        assert_eq!(PROMPT_HARD_BUDGET, Duration::from_secs(15));
        assert_eq!(REAP_GRACE, Duration::from_secs(5));
    }

    // ── end-to-end against a fake ACP adapter ──────────────────────────────
    //
    // These spawn a real subprocess speaking real NDJSON JSON-RPC, so the
    // probe's protocol ordering, classification, and cleanup are exercised
    // exactly as they run in production — with no live provider anywhere.

    #[cfg(unix)]
    mod fake_adapter {
        use super::*;
        use std::fs;
        use std::os::unix::fs::PermissionsExt;
        use std::path::PathBuf;

        /// `initialize` results for the two adapter families under test.
        const CLAUDE_INIT: &str =
            r#"{"protocolVersion":2,"serverInfo":{"name":"claude-code-acp","version":"1.4.2"}}"#;
        const GOOSE_INIT: &str =
            r#"{"protocolVersion":2,"serverInfo":{"name":"goose","version":"1.0"}}"#;

        /// A fake adapter's scripted answers.
        struct Script {
            /// The `initialize` result object.
            initialize: String,
            /// The shell line(s) answering `session/new`.
            session_new: String,
            /// The shell line(s) answering `session/prompt`.
            prompt: String,
        }

        impl Default for Script {
            fn default() -> Self {
                Self {
                    initialize: CLAUDE_INIT.into(),
                    session_new: reply(r#"{"sessionId":"probe-session"}"#),
                    prompt: reply(r#"{"stopReason":"end_turn"}"#),
                }
            }
        }

        /// A shell fragment writing a JSON-RPC success result for the current id.
        fn reply(result: &str) -> String {
            format!("printf '{{\"jsonrpc\":\"2.0\",\"id\":%s,\"result\":{result}}}\\n' \"$id\"")
        }

        /// A shell fragment writing a JSON-RPC error for the current id.
        fn error_reply(code: i64, message: &str) -> String {
            format!(
                "printf '{{\"jsonrpc\":\"2.0\",\"id\":%s,\"error\":{{\"code\":{code},\"message\":\"{message}\"}}}}\\n' \"$id\""
            )
        }

        struct FakeAdapter {
            _temp: tempfile::TempDir,
            path: PathBuf,
            log: PathBuf,
            cwd: PathBuf,
        }

        impl FakeAdapter {
            fn new(name: &str, script: Script) -> Self {
                let temp = tempfile::tempdir().expect("temp dir");
                let path = temp.path().join(name);
                let log = temp.path().join("methods.log");
                let cwd = temp.path().join("work");
                fs::create_dir_all(&cwd).expect("work dir");

                let body = format!(
                    r#"#!/bin/sh
LOG='{log}'
while IFS= read -r line; do
  id=`printf '%s' "$line" | sed -n 's/.*"id":\([0-9][0-9]*\).*/\1/p'`
  case "$line" in
    *'"method":"initialize"'*)
      echo initialize >> "$LOG"
      {initialize_reply}
      ;;
    *'"method":"session/new"'*)
      echo session/new >> "$LOG"
      {session_new}
      ;;
    *'"method":"session/prompt"'*)
      echo session/prompt >> "$LOG"
      {prompt}
      ;;
    *'"method":"session/cancel"'*)
      echo session/cancel >> "$LOG"
      ;;
  esac
done
"#,
                    log = log.display(),
                    initialize_reply = reply(&script.initialize),
                    session_new = script.session_new,
                    prompt = script.prompt,
                );
                fs::write(&path, body).expect("write adapter");
                fs::set_permissions(&path, fs::Permissions::from_mode(0o755))
                    .expect("chmod adapter");

                Self {
                    _temp: temp,
                    path,
                    log,
                    cwd,
                }
            }

            async fn probe(&self) -> ProbeReport {
                run_probe(
                    self.path.to_str().expect("utf8 path"),
                    &[],
                    self.cwd.as_path(),
                )
                .await
            }

            fn methods(&self) -> Vec<String> {
                fs::read_to_string(&self.log)
                    .unwrap_or_default()
                    .lines()
                    .map(str::to_string)
                    .collect()
            }
        }

        #[tokio::test]
        async fn a_healthy_adapter_is_probed_in_protocol_order_and_reports_ready() {
            let adapter = FakeAdapter::new("claude-agent-acp", Script::default());
            let report = adapter.probe().await;

            assert!(
                report.is_ready(),
                "healthy adapter must be ready: {report:?}"
            );
            assert_eq!(report.stop_reason, Some("end_turn"));
            assert_eq!(report.adapter_id, "claude");
            assert_eq!(report.adapter_name.as_deref(), Some("claude-code-acp"));
            assert_eq!(report.adapter_version.as_deref(), Some("1.4.2"));
            assert_eq!(report.reason, None);

            let methods = adapter.methods();
            assert_eq!(
                &methods[..3],
                &["initialize", "session/new", "session/prompt"],
                "the probe must follow ACP order: {methods:?}"
            );
        }

        #[tokio::test]
        async fn only_end_turn_is_accepted_from_a_completed_turn() {
            for (stop, expected) in [
                ("end_turn", None),
                ("refusal", Some(ProbeReason::ProviderRejected)),
                ("max_tokens", Some(ProbeReason::ProviderRejected)),
                ("max_turn_requests", Some(ProbeReason::ProviderRejected)),
                ("cancelled", Some(ProbeReason::ProviderRejected)),
            ] {
                let adapter = FakeAdapter::new(
                    "claude-agent-acp",
                    Script {
                        prompt: reply(&format!(r#"{{"stopReason":"{stop}"}}"#)),
                        ..Script::default()
                    },
                );
                let report = adapter.probe().await;
                assert_eq!(report.reason, expected, "stopReason {stop}: {report:?}");
                assert_eq!(report.is_ready(), expected.is_none());
            }
        }

        #[tokio::test]
        async fn terminal_auth_during_session_creation_is_reported_at_that_stage() {
            let adapter = FakeAdapter::new(
                "claude-agent-acp",
                Script {
                    session_new: error_reply(
                        -32000,
                        "API Error: OAuth session expired and could not be refreshed",
                    ),
                    ..Script::default()
                },
            );
            let report = adapter.probe().await;
            assert_eq!(report.reason, Some(ProbeReason::AuthenticationRequired));
            assert_eq!(report.stage, ProbeStage::SessionNew);
            assert!(!report.is_ready());
            assert_eq!(
                adapter.methods(),
                vec!["initialize", "session/new"],
                "the probe must not prompt after a failed session"
            );
        }

        #[tokio::test]
        async fn terminal_auth_during_the_prompt_is_reported_at_that_stage() {
            for message in [
                "API Error: OAuth session expired and could not be refreshed",
                "API Error: OAuth access token has expired. Re-authenticate to continue.",
                "Internal error: API Error: 401 unauthorized",
            ] {
                let adapter = FakeAdapter::new(
                    "claude-agent-acp",
                    Script {
                        prompt: error_reply(-32000, message),
                        ..Script::default()
                    },
                );
                let report = adapter.probe().await;
                assert_eq!(
                    report.reason,
                    Some(ProbeReason::AuthenticationRequired),
                    "{message}: {report:?}"
                );
                assert_eq!(report.stage, ProbeStage::Prompt);
            }
        }

        #[tokio::test]
        async fn structured_auth_is_recognised_without_any_prose() {
            let adapter = FakeAdapter::new(
                "goose",
                Script {
                    initialize: GOOSE_INIT.into(),
                    prompt: "printf '{\"jsonrpc\":\"2.0\",\"id\":%s,\"error\":{\"code\":-32603,\"message\":\"nope\",\"data\":{\"type\":\"auth_required\"}}}\\n' \"$id\"".into(),
                    ..Script::default()
                },
            );
            let report = adapter.probe().await;
            assert_eq!(report.reason, Some(ProbeReason::AuthenticationRequired));
            assert_eq!(report.adapter_id, "goose");
        }

        #[tokio::test]
        async fn the_same_prose_from_a_non_claude_adapter_is_only_a_provider_rejection() {
            let adapter = FakeAdapter::new(
                "goose",
                Script {
                    initialize: GOOSE_INIT.into(),
                    prompt: error_reply(
                        -32000,
                        "API Error: OAuth session expired and could not be refreshed",
                    ),
                    ..Script::default()
                },
            );
            let report = adapter.probe().await;
            assert_eq!(
                report.reason,
                Some(ProbeReason::ProviderRejected),
                "a non-Claude adapter's identical prose must not be diagnosed as a login problem"
            );
        }

        #[tokio::test]
        async fn an_unrelated_provider_error_is_a_provider_rejection() {
            let adapter = FakeAdapter::new(
                "claude-agent-acp",
                Script {
                    prompt: error_reply(-32000, "API Error: 429 rate limit exceeded"),
                    ..Script::default()
                },
            );
            let report = adapter.probe().await;
            assert_eq!(report.reason, Some(ProbeReason::ProviderRejected));
        }

        #[tokio::test]
        async fn a_malformed_response_fails_closed_without_panicking() {
            // `session/new` answers with a result that has no sessionId.
            let adapter = FakeAdapter::new(
                "claude-agent-acp",
                Script {
                    session_new: reply(r#"{"notASession":true}"#),
                    ..Script::default()
                },
            );
            let report = adapter.probe().await;
            assert_eq!(report.reason, Some(ProbeReason::ProtocolError));
            assert_eq!(report.stage, ProbeStage::SessionNew);
        }

        #[tokio::test]
        async fn an_adapter_that_exits_immediately_fails_closed() {
            let temp = tempfile::tempdir().expect("temp dir");
            let path = temp.path().join("claude-agent-acp");
            fs::write(&path, "#!/bin/sh\nexit 0\n").expect("write");
            fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).expect("chmod");

            let report = run_probe(path.to_str().expect("utf8"), &[], temp.path()).await;
            assert_eq!(report.reason, Some(ProbeReason::ProtocolError));
            assert_eq!(report.stage, ProbeStage::Initialize);
        }

        #[tokio::test]
        async fn a_missing_adapter_fails_closed_at_spawn() {
            let temp = tempfile::tempdir().expect("temp dir");
            let report = run_probe("buzz-provider-probe-no-such-binary", &[], temp.path()).await;
            assert_eq!(report.stage, ProbeStage::Spawn);
            assert_eq!(report.reason, Some(ProbeReason::ProtocolError));
        }

        #[tokio::test]
        async fn a_missing_working_directory_fails_closed_before_spawning() {
            let temp = tempfile::tempdir().expect("temp dir");
            let adapter = FakeAdapter::new("claude-agent-acp", Script::default());
            let report = run_probe(
                adapter.path.to_str().expect("utf8"),
                &[],
                &temp.path().join("does-not-exist"),
            )
            .await;
            assert_eq!(report.stage, ProbeStage::Spawn);
            assert!(
                adapter.methods().is_empty(),
                "nothing may be spawned for an invalid working directory"
            );
        }

        #[tokio::test]
        async fn an_adapter_that_is_not_the_one_we_configured_fails_closed() {
            let adapter = FakeAdapter::new(
                "claude-agent-acp",
                Script {
                    initialize: GOOSE_INIT.into(),
                    ..Script::default()
                },
            );
            let report = adapter.probe().await;
            assert_eq!(report.reason, Some(ProbeReason::AdapterMismatch));
            assert_eq!(report.stage, ProbeStage::Initialize);
            assert_eq!(
                adapter.methods(),
                vec!["initialize"],
                "a mismatched adapter must never be given a session"
            );
        }

        /// A silent adapter must be timed out, killed, and reaped — including
        /// the grandchild it spawned into its own process group.
        #[tokio::test]
        async fn a_silent_adapter_is_timed_out_and_its_process_tree_reaped() {
            let temp = tempfile::tempdir().expect("temp dir");
            let path = temp.path().join("claude-agent-acp");
            let marker = temp.path().join("grandchild.pid");
            // The adapter spawns a long-lived grandchild, records its pid, and
            // then never answers anything.
            fs::write(
                &path,
                format!(
                    "#!/bin/sh\nsleep 600 &\necho $! > '{}'\nsleep 600\n",
                    marker.display()
                ),
            )
            .expect("write");
            fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).expect("chmod");

            let start = std::time::Instant::now();
            let report = run_probe(path.to_str().expect("utf8"), &[], temp.path()).await;
            let elapsed = start.elapsed();

            assert_eq!(report.reason, Some(ProbeReason::TimedOut));
            assert_eq!(report.stage, ProbeStage::Initialize);
            assert!(
                elapsed < OPERATION_BUDGET + REAP_GRACE + Duration::from_secs(10),
                "the probe must stay inside its budgets; took {elapsed:?}"
            );

            let pid: i32 = fs::read_to_string(&marker)
                .expect("grandchild pid recorded")
                .trim()
                .parse()
                .expect("numeric pid");
            // Poll briefly: the group kill is delivered asynchronously.
            let mut alive = true;
            for _ in 0..50 {
                if !process_alive(pid) {
                    alive = false;
                    break;
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
            assert!(
                !alive,
                "the adapter's process group, including pid {pid}, must be killed"
            );
        }

        /// `kill -0` without sending a signal.
        fn process_alive(pid: i32) -> bool {
            std::process::Command::new("kill")
                .args(["-0", &pid.to_string()])
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status()
                .map(|s| s.success())
                .unwrap_or(false)
        }

        /// The report must never carry a secret, a path, an argument, the
        /// prompt, the model's answer, or a raw provider error — even when the
        /// adapter goes out of its way to put all of them in one message.
        #[tokio::test]
        async fn the_report_contains_no_secret_path_prompt_or_raw_error_sentinel() {
            const SECRET: &str = "sk-ant-SENTINEL-CREDENTIAL";
            const MODEL_TEXT: &str = "SENTINEL-MODEL-OUTPUT";
            let adapter = FakeAdapter::new(
                "claude-agent-acp",
                Script {
                    prompt: error_reply(
                        -32000,
                        &format!(
                            "API Error: 401 unauthorized token={SECRET} said {MODEL_TEXT} \
                             while running Reply with exactly OK"
                        ),
                    ),
                    ..Script::default()
                },
            );
            let report = adapter.probe().await;
            assert_eq!(report.reason, Some(ProbeReason::AuthenticationRequired));

            let json = serde_json::to_string(&report).expect("serialise");
            for sentinel in [
                SECRET,
                MODEL_TEXT,
                PROBE_PROMPT,
                "unauthorized",
                "401",
                adapter.path.to_str().expect("utf8"),
                adapter.cwd.to_str().expect("utf8"),
            ] {
                assert!(
                    !json.contains(sentinel),
                    "probe output leaked {sentinel:?}: {json}"
                );
            }
            assert!(!json.contains('\n'), "output must stay one line: {json}");
        }
    }
}
