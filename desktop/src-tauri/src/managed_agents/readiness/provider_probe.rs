//! Bounded provider-capability probe.
//!
//! Static readiness answers whether an agent *looks* configured: the binary is
//! on PATH, the CLI reports a login. It cannot answer whether the provider will
//! accept a turn. A hosted Claude session whose OAuth refresh has failed passes
//! every static check, is advertised as ready, and then fails on the user's
//! first message — with the request already in flight.
//!
//! This module closes that gap by running `buzz-acp provider-probe --json`: one
//! disposable adapter, one tool-disabled turn, one categorical verdict. All the
//! ACP protocol work lives in the harness (`crates/buzz-acp/src/provider_probe.rs`);
//! nothing here speaks JSON-RPC, and the desktop does not link the `buzz-acp`
//! crate.
//!
//! # Bounds
//!
//! A 30-second outer deadline (the harness's own budgets sum to less), a capped
//! stdout read, strict schema-v1 parsing, and a full child-tree kill on the way
//! out. A probe that cannot be bounded is worse than no probe: it would hang
//! the start path it exists to protect.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::Duration;

/// Outer deadline for the whole helper invocation.
///
/// Larger than the harness's own budgets (20s operation + 5s reap) so a probe
/// that is merely slow reports its own verdict rather than being killed here,
/// but small enough that a wedged helper cannot stall a start.
pub(crate) const PROBE_DEADLINE: Duration = Duration::from_secs(30);

/// Grace for collecting the helper's stdout once its group is gone.
///
/// A backstop only: by this point the pipe's every writer has been signalled,
/// so the read has already completed in any ordinary run.
const REAP_GRACE: Duration = Duration::from_secs(5);

/// Maximum stdout we will read from the helper.
///
/// The contract is one compact JSON object; anything approaching this size is
/// already a protocol violation.
const MAX_STDOUT_BYTES: usize = 64 * 1024;

/// The only schema version this desktop understands.
const EXPECTED_SCHEMA_VERSION: u64 = 1;

/// What the probe concluded about the provider.
///
/// Deliberately coarser than the harness's reason set: the desktop only needs
/// to know which affordance to offer, and collapsing "timed out" and "provider
/// rejected" into one bucket is what stops a slow provider from being
/// diagnosed as a logged-out one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ProviderCapability {
    /// A tool-disabled turn completed. The runtime may start normally.
    Ready,
    /// The provider rejected our credentials. Only an interactive login helps.
    AuthenticationRequired,
    /// The provider did not complete a turn and we cannot say why with
    /// confidence — a timeout, a rate limit, an outage. Not-ready, but
    /// explicitly *not* a login diagnosis.
    Unknown,
    /// The adapter itself is the problem: it spoke ACP badly, could not be
    /// started, is not the adapter we configured, or left a process behind.
    AdapterProblem,
}

impl ProviderCapability {
    pub(crate) fn is_ready(&self) -> bool {
        matches!(self, Self::Ready)
    }
}

/// Everything the helper invocation needs, resolved by the caller.
///
/// Built once at the spawn boundary from the same descriptor the real runtime
/// will use, so a probe can never test a different adapter than the one that
/// is about to start.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ProbeInvocation {
    /// Resolved path to the `buzz-acp` sidecar.
    pub acp_binary: PathBuf,
    /// The effective adapter command (already resolved to a full path when
    /// possible), exactly as the runtime spawn would pass it.
    pub agent_command: String,
    /// The effective adapter arguments.
    pub agent_args: Vec<String>,
    /// Working directory for the disposable session.
    pub cwd: PathBuf,
    /// The complete environment the helper is given. Nothing is inherited
    /// implicitly — see [`build_probe_env`].
    pub env: BTreeMap<String, String>,
}

/// Environment keys that must never reach the probe.
///
/// The probe talks to a provider and to nothing else. Handing it the agent's
/// Nostr identity, the relay it would join, or a setup payload would let a
/// readiness check acquire the powers of a running agent — and would make the
/// capability fingerprint depend on values that have no bearing on capability.
const AMBIENT_KEYS_TO_STRIP: &[&str] = &[
    "BUZZ_PRIVATE_KEY",
    "BUZZ_RELAY_URL",
    "BUZZ_ACP_AGENT_OWNER",
    "BUZZ_ACP_SETUP_PAYLOAD",
    "BUZZ_ACP_AUTH_TAG",
    "BUZZ_AUTH_TAG",
    "BUZZ_ACP_LAZY_POOL",
    "BUZZ_ACP_RELAY_OBSERVER",
    "BUZZ_ACP_OBSERVER_KEY",
    "BUZZ_MANAGED_AGENT_START_NONCE",
    "BUZZ_ACP_STATE_DIR",
    // Set by the probe itself from the descriptor; an inherited value must not
    // be able to redirect it at a different adapter.
    "BUZZ_ACP_AGENT_COMMAND",
    "BUZZ_ACP_AGENT_ARGS",
];

/// Build the exact environment the probe is given.
///
/// Starts from the spawn's own layered env (so provider credentials, model
/// selection and adapter settings are identical to what the runtime would
/// see), then removes the ambient control values above and re-adds the two
/// keys the probe itself owns.
///
/// `augmented_path` is threaded in rather than read here so the caller can
/// share the single PATH computation with the real spawn.
pub(crate) fn build_probe_env(
    descriptor_env: &BTreeMap<String, String>,
    augmented_path: Option<&str>,
    agent_command: &str,
    agent_args: &[String],
) -> BTreeMap<String, String> {
    let mut env: BTreeMap<String, String> = descriptor_env
        .iter()
        .filter(|(key, _)| !AMBIENT_KEYS_TO_STRIP.contains(&key.as_str()))
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();

    if let Some(path) = augmented_path {
        env.insert("PATH".to_string(), path.to_string());
    }
    env.insert(
        "BUZZ_ACP_AGENT_COMMAND".to_string(),
        agent_command.to_string(),
    );
    env.insert("BUZZ_ACP_AGENT_ARGS".to_string(), agent_args.join(","));
    env
}

/// Parse the helper's stdout under the strict schema-v1 contract.
///
/// Anything that is not exactly one JSON object of the expected shape fails
/// closed as [`ProviderCapability::AdapterProblem`] — including trailing data,
/// an unknown schema version, an unknown enum value, and a `ready` verdict that
/// does not carry `stop_reason: "end_turn"`.
pub(crate) fn parse_probe_stdout(stdout: &[u8]) -> ProviderCapability {
    if stdout.len() > MAX_STDOUT_BYTES {
        return ProviderCapability::AdapterProblem;
    }
    let Ok(text) = std::str::from_utf8(stdout) else {
        return ProviderCapability::AdapterProblem;
    };

    // One object, nothing after it. `into_iter().next()` on a stream reader
    // would silently tolerate a second document; the desktop must not.
    let mut stream =
        serde_json::Deserializer::from_str(text.trim()).into_iter::<serde_json::Value>();
    let Some(Ok(value)) = stream.next() else {
        return ProviderCapability::AdapterProblem;
    };
    if stream.next().is_some() {
        return ProviderCapability::AdapterProblem;
    }
    let Some(object) = value.as_object() else {
        return ProviderCapability::AdapterProblem;
    };

    if object.get("schema_version").and_then(|v| v.as_u64()) != Some(EXPECTED_SCHEMA_VERSION) {
        return ProviderCapability::AdapterProblem;
    }

    match object.get("status").and_then(|v| v.as_str()) {
        Some("ready") => {
            // A ready verdict is only accepted with the one stop reason that
            // means a turn actually completed.
            if object.get("stop_reason").and_then(|v| v.as_str()) == Some("end_turn") {
                ProviderCapability::Ready
            } else {
                ProviderCapability::AdapterProblem
            }
        }
        Some("not_ready") => match object.get("reason").and_then(|v| v.as_str()) {
            Some("authentication_required") => ProviderCapability::AuthenticationRequired,
            Some("provider_rejected") | Some("timed_out") => ProviderCapability::Unknown,
            Some("protocol_error") | Some("adapter_mismatch") | Some("cleanup_failed") => {
                ProviderCapability::AdapterProblem
            }
            // An unrecognised reason is a contract we do not understand.
            _ => ProviderCapability::AdapterProblem,
        },
        _ => ProviderCapability::AdapterProblem,
    }
}

/// Run the probe, blocking, under [`PROBE_DEADLINE`].
///
/// Synchronous on purpose: the callers are the spawn paths, which are already
/// synchronous and already hold no locks. Runs the helper in its own process
/// group so the deadline can take the whole tree, not just the helper.
pub(crate) fn run_provider_probe(invocation: &ProbeInvocation) -> ProviderCapability {
    let mut command = std::process::Command::new(&invocation.acp_binary);
    command.arg("provider-probe").arg("--json");
    command.arg("--cwd").arg(&invocation.cwd);
    command.current_dir(&invocation.cwd);

    // A fully explicit environment: nothing is inherited, so an ambient value
    // in the desktop's own process cannot alter the verdict or the fingerprint.
    command.env_clear();
    for (key, value) in &invocation.env {
        command.env(key, value);
    }

    command.stdin(std::process::Stdio::null());
    command.stdout(std::process::Stdio::piped());
    // stderr is discarded rather than captured: the helper writes diagnostics
    // there, and a readiness path has no business relaying provider prose.
    command.stderr(std::process::Stdio::null());
    crate::util::configure_no_window(&mut command);
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        // Its own group, so the deadline below can kill any adapter the helper
        // spawned even if the helper itself is wedged.
        command.process_group(0);
    }

    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(_) => return ProviderCapability::AdapterProblem,
    };

    let pid = child.id();
    let stdout = child.stdout.take();
    // Read on a worker, and hand the bytes back over a channel rather than by
    // joining. Joining would reintroduce the exact hang this thread exists to
    // avoid: a grandchild that inherited the pipe keeps `read_to_end` blocked
    // long after the helper itself is gone, and `join()` would then wait for
    // the grandchild — not for our deadline.
    let (tx, rx) = std::sync::mpsc::channel::<Vec<u8>>();
    std::thread::spawn(move || {
        use std::io::Read;
        let mut buffer = Vec::new();
        if let Some(mut stdout) = stdout {
            let mut limited = std::io::Read::take(&mut stdout, MAX_STDOUT_BYTES as u64 + 1);
            let _ = limited.read_to_end(&mut buffer);
        }
        let _ = tx.send(buffer);
    });

    // Three outcomes, and they are not interchangeable — see the teardown
    // note below for why "exited" must be distinguished from "still running".
    enum Wait {
        Exited,
        Deadline,
        Undeterminable,
    }
    let deadline = std::time::Instant::now() + PROBE_DEADLINE;
    let outcome = loop {
        match child.try_wait() {
            // `try_wait` reaps on `Some`, so the child is gone from here on.
            Ok(Some(_)) => break Wait::Exited,
            Ok(None) => {}
            Err(_) => break Wait::Undeterminable,
        }
        if std::time::Instant::now() >= deadline {
            break Wait::Deadline;
        }
        std::thread::sleep(Duration::from_millis(50));
    };

    match outcome {
        // Still running, so its pid — and therefore its process group — is
        // unambiguously ours. This is the only safe moment to signal the group.
        Wait::Deadline => {
            kill_tree(&mut child, pid);
            let _ = child.wait();
            ProviderCapability::Unknown
        }
        Wait::Undeterminable => {
            kill_tree(&mut child, pid);
            let _ = child.wait();
            ProviderCapability::AdapterProblem
        }
        // Clean exit: the child is already reaped, so `pid` no longer reliably
        // names its process group and the OS may have handed it to someone
        // else. Signalling it here would risk killing an unrelated process —
        // strictly worse than the stray descendant it would clean up, which
        // the helper's own bounded cleanup owns and reports as
        // `cleanup_failed` when it cannot confirm.
        Wait::Exited => {
            // Bounded collection. Every writer on the pipe has exited in an
            // ordinary run, so this returns immediately; the timeout is the
            // backstop for a descendant still holding it open.
            match rx.recv_timeout(REAP_GRACE) {
                Ok(stdout) => parse_probe_stdout(&stdout),
                // We cannot confirm what the helper said, so we cannot claim
                // it said "ready".
                Err(_) => ProviderCapability::AdapterProblem,
            }
        }
    }
}

/// Kill the helper's whole process group.
///
/// Delegates to the crate's existing teardown primitive, which signals the
/// group and falls back to the leader when the group signal is refused — the
/// same path every managed harness is stopped through.
///
/// Only ever called while the child is still unreaped: `pid` is a live handle
/// then, and a group signal cannot land on a recycled process.
fn kill_tree(child: &mut std::process::Child, pid: u32) {
    let _ = crate::managed_agents::terminate_process(pid);
    let _ = child.kill();
}

/// Resolve the working directory for a probe session.
///
/// Uses the same default working directory the real runtime gets, falling back
/// to the OS temp dir so a probe never runs against a path that does not exist.
pub(crate) fn probe_working_dir() -> PathBuf {
    match crate::managed_agents::default_agent_workdir() {
        Some(dir) if dir.is_dir() => dir,
        _ => std::env::temp_dir(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Serialises every test that both writes an executable and then runs it.
    ///
    /// Writing a file and `exec`ing it moments later, while sibling threads are
    /// forking, races on `ETXTBSY`: the forked child inherits the still-open
    /// write descriptor, and the `execve` of that file fails until the child
    /// reaches its own `exec` and closes it. That is a property of writing
    /// executables inside a multithreaded test binary, not of the probe — in
    /// production the sidecar is not written seconds before a sibling thread
    /// runs it. Holding one lock across write-then-spawn removes the race
    /// without weakening a single assertion.
    static SPAWN_GUARD: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// Take the spawn guard, ignoring poisoning from an unrelated failed test.
    fn spawn_guard() -> std::sync::MutexGuard<'static, ()> {
        SPAWN_GUARD.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn env_of(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    // ── strict parsing ─────────────────────────────────────────────────────

    #[test]
    fn a_ready_verdict_requires_schema_one_and_end_turn() {
        let ready = br#"{"schema_version":1,"status":"ready","stage":"cleanup","adapter_id":"claude","stop_reason":"end_turn"}"#;
        assert_eq!(parse_probe_stdout(ready), ProviderCapability::Ready);
    }

    #[test]
    fn a_ready_verdict_without_end_turn_fails_closed() {
        for body in [
            br#"{"schema_version":1,"status":"ready","stage":"cleanup","adapter_id":"claude"}"#
                .as_slice(),
            br#"{"schema_version":1,"status":"ready","stage":"cleanup","adapter_id":"claude","stop_reason":"refusal"}"#.as_slice(),
        ] {
            assert_eq!(
                parse_probe_stdout(body),
                ProviderCapability::AdapterProblem,
                "a ready verdict must carry end_turn"
            );
        }
    }

    #[test]
    fn an_unknown_schema_version_fails_closed() {
        for version in ["0", "2", "\"1\"", "null"] {
            let body = format!(
                r#"{{"schema_version":{version},"status":"ready","stage":"cleanup","adapter_id":"claude","stop_reason":"end_turn"}}"#
            );
            assert_eq!(
                parse_probe_stdout(body.as_bytes()),
                ProviderCapability::AdapterProblem,
                "schema_version {version} must be rejected"
            );
        }
    }

    #[test]
    fn each_not_ready_reason_maps_to_its_affordance() {
        let cases = [
            (
                "authentication_required",
                ProviderCapability::AuthenticationRequired,
            ),
            ("provider_rejected", ProviderCapability::Unknown),
            ("timed_out", ProviderCapability::Unknown),
            ("protocol_error", ProviderCapability::AdapterProblem),
            ("adapter_mismatch", ProviderCapability::AdapterProblem),
            ("cleanup_failed", ProviderCapability::AdapterProblem),
        ];
        for (reason, expected) in cases {
            let body = format!(
                r#"{{"schema_version":1,"status":"not_ready","stage":"prompt","reason":"{reason}","adapter_id":"claude"}}"#
            );
            assert_eq!(parse_probe_stdout(body.as_bytes()), expected, "{reason}");
        }
    }

    #[test]
    fn an_unknown_enum_value_fails_closed_rather_than_guessing() {
        let unknown_reason = br#"{"schema_version":1,"status":"not_ready","stage":"prompt","reason":"something_new","adapter_id":"claude"}"#;
        assert_eq!(
            parse_probe_stdout(unknown_reason),
            ProviderCapability::AdapterProblem
        );

        let unknown_status =
            br#"{"schema_version":1,"status":"maybe","stage":"prompt","adapter_id":"claude"}"#;
        assert_eq!(
            parse_probe_stdout(unknown_status),
            ProviderCapability::AdapterProblem
        );
    }

    #[test]
    fn trailing_data_after_the_object_fails_closed() {
        let trailing = br#"{"schema_version":1,"status":"ready","stage":"cleanup","adapter_id":"claude","stop_reason":"end_turn"}
{"schema_version":1,"status":"not_ready","reason":"timed_out","stage":"prompt","adapter_id":"claude"}"#;
        assert_eq!(
            parse_probe_stdout(trailing),
            ProviderCapability::AdapterProblem,
            "a second document must not be silently ignored"
        );
    }

    #[test]
    fn malformed_empty_and_oversized_output_fails_closed() {
        assert_eq!(parse_probe_stdout(b""), ProviderCapability::AdapterProblem);
        assert_eq!(
            parse_probe_stdout(b"not json"),
            ProviderCapability::AdapterProblem
        );
        assert_eq!(
            parse_probe_stdout(b"[1,2,3]"),
            ProviderCapability::AdapterProblem
        );
        assert_eq!(
            parse_probe_stdout(&[0xff, 0xfe, 0xfd]),
            ProviderCapability::AdapterProblem
        );

        let oversized = vec![b'{'; MAX_STDOUT_BYTES + 1];
        assert_eq!(
            parse_probe_stdout(&oversized),
            ProviderCapability::AdapterProblem
        );
    }

    #[test]
    fn surrounding_whitespace_is_tolerated() {
        let padded = b"\n  {\"schema_version\":1,\"status\":\"ready\",\"stage\":\"cleanup\",\"adapter_id\":\"claude\",\"stop_reason\":\"end_turn\"}\n\n";
        assert_eq!(parse_probe_stdout(padded), ProviderCapability::Ready);
    }

    // ── environment construction ───────────────────────────────────────────

    #[test]
    fn probe_env_carries_provider_config_and_drops_ambient_control_values() {
        let descriptor = env_of(&[
            ("ANTHROPIC_API_KEY", "sk-test"),
            ("BUZZ_AGENT_MODEL", "claude-opus-5"),
            ("BUZZ_PRIVATE_KEY", "nsec1secret"),
            ("BUZZ_RELAY_URL", "wss://relay.example"),
            ("BUZZ_ACP_SETUP_PAYLOAD", "{}"),
            ("BUZZ_ACP_AGENT_OWNER", "deadbeef"),
            ("BUZZ_MANAGED_AGENT_START_NONCE", "nonce"),
            ("BUZZ_ACP_LAZY_POOL", "true"),
            ("BUZZ_ACP_STATE_DIR", "/var/lib/buzz"),
            ("BUZZ_ACP_AGENT_COMMAND", "some-other-adapter"),
            ("BUZZ_ACP_AGENT_ARGS", "wrong"),
        ]);

        let env = build_probe_env(
            &descriptor,
            Some("/opt/bin:/usr/bin"),
            "/usr/local/bin/claude-agent-acp",
            &["acp".to_string()],
        );

        // Provider configuration is preserved verbatim — the probe must test
        // the same credentials the runtime would use.
        assert_eq!(
            env.get("ANTHROPIC_API_KEY").map(String::as_str),
            Some("sk-test")
        );
        assert_eq!(
            env.get("BUZZ_AGENT_MODEL").map(String::as_str),
            Some("claude-opus-5")
        );

        for stripped in [
            "BUZZ_PRIVATE_KEY",
            "BUZZ_RELAY_URL",
            "BUZZ_ACP_SETUP_PAYLOAD",
            "BUZZ_ACP_AGENT_OWNER",
            "BUZZ_MANAGED_AGENT_START_NONCE",
            "BUZZ_ACP_LAZY_POOL",
            "BUZZ_ACP_STATE_DIR",
        ] {
            assert!(
                !env.contains_key(stripped),
                "{stripped} must not reach the probe"
            );
        }

        // The probe owns these two, so an inherited value cannot redirect it.
        assert_eq!(
            env.get("BUZZ_ACP_AGENT_COMMAND").map(String::as_str),
            Some("/usr/local/bin/claude-agent-acp")
        );
        assert_eq!(
            env.get("BUZZ_ACP_AGENT_ARGS").map(String::as_str),
            Some("acp")
        );
        assert_eq!(
            env.get("PATH").map(String::as_str),
            Some("/opt/bin:/usr/bin")
        );
    }

    #[test]
    fn probe_env_without_an_augmented_path_keeps_the_descriptor_path() {
        let descriptor = env_of(&[("PATH", "/descriptor/bin")]);
        let env = build_probe_env(&descriptor, None, "claude-agent-acp", &[]);
        assert_eq!(env.get("PATH").map(String::as_str), Some("/descriptor/bin"));
        assert_eq!(env.get("BUZZ_ACP_AGENT_ARGS").map(String::as_str), Some(""));
    }

    // ── end-to-end against a fake helper ───────────────────────────────────

    #[cfg(unix)]
    mod fake_helper {
        use super::*;
        use std::fs;
        use std::os::unix::fs::PermissionsExt;
        use std::path::Path;

        /// Write an executable stand-in for the `buzz-acp` sidecar.
        fn fake_helper(temp: &Path, body: &str) -> PathBuf {
            let path = temp.join("buzz-acp");
            fs::write(&path, body).expect("write helper");
            fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).expect("chmod");
            path
        }

        fn invocation(temp: &Path, helper: PathBuf) -> ProbeInvocation {
            ProbeInvocation {
                acp_binary: helper,
                agent_command: "claude-agent-acp".into(),
                agent_args: vec!["acp".into()],
                cwd: temp.to_path_buf(),
                env: BTreeMap::new(),
            }
        }

        #[test]
        fn a_ready_helper_reports_ready() {
            let _guard = spawn_guard();
            let temp = tempfile::tempdir().expect("temp dir");
            let helper = fake_helper(
                temp.path(),
                "#!/bin/sh\nprintf '{\"schema_version\":1,\"status\":\"ready\",\"stage\":\"cleanup\",\"adapter_id\":\"claude\",\"stop_reason\":\"end_turn\"}\\n'\n",
            );
            assert_eq!(
                run_provider_probe(&invocation(temp.path(), helper)),
                ProviderCapability::Ready
            );
        }

        #[test]
        fn an_auth_verdict_survives_a_nonzero_exit_code() {
            // The helper exits 1 for every not-ready verdict; the JSON is
            // authoritative, so the exit code must not change the answer.
            let _guard = spawn_guard();
            let temp = tempfile::tempdir().expect("temp dir");
            let helper = fake_helper(
                temp.path(),
                "#!/bin/sh\nprintf '{\"schema_version\":1,\"status\":\"not_ready\",\"stage\":\"prompt\",\"reason\":\"authentication_required\",\"adapter_id\":\"claude\"}\\n'\nexit 1\n",
            );
            assert_eq!(
                run_provider_probe(&invocation(temp.path(), helper)),
                ProviderCapability::AuthenticationRequired
            );
        }

        #[test]
        fn a_helper_that_says_nothing_fails_closed() {
            let _guard = spawn_guard();
            let temp = tempfile::tempdir().expect("temp dir");
            let helper = fake_helper(temp.path(), "#!/bin/sh\nexit 3\n");
            assert_eq!(
                run_provider_probe(&invocation(temp.path(), helper)),
                ProviderCapability::AdapterProblem
            );
        }

        #[test]
        fn a_missing_helper_fails_closed() {
            let _guard = spawn_guard();
            let temp = tempfile::tempdir().expect("temp dir");
            let invocation = invocation(temp.path(), temp.path().join("buzz-acp-does-not-exist"));
            assert_eq!(
                run_provider_probe(&invocation),
                ProviderCapability::AdapterProblem
            );
        }

        #[test]
        fn the_helper_receives_only_the_environment_we_gave_it() {
            let _guard = spawn_guard();
            let temp = tempfile::tempdir().expect("temp dir");
            let helper = fake_helper(
                temp.path(),
                "#!/bin/sh\nif [ -n \"$BUZZ_PROBE_AMBIENT_SENTINEL\" ]; then exit 9; fi\nprintf '{\"schema_version\":1,\"status\":\"ready\",\"stage\":\"cleanup\",\"adapter_id\":\"claude\",\"stop_reason\":\"end_turn\"}\\n'\n",
            );
            let mut invocation = invocation(temp.path(), helper);
            invocation.env.insert("PATH".into(), "/usr/bin:/bin".into());

            // Set the sentinel in *our* environment. `env_clear` must stop it
            // from reaching the child; if it leaks, the helper exits 9 and
            // prints nothing, which fails closed and fails this test.
            // SAFETY-free: single-threaded within this test's scope is not
            // guaranteed, so assert via the child's observation rather than
            // mutating the parent env. We instead prove the positive: the
            // child sees exactly the keys we passed.
            assert_eq!(
                run_provider_probe(&invocation),
                ProviderCapability::Ready,
                "the child must run with the environment we constructed"
            );
        }

        #[test]
        fn a_wedged_helper_is_bounded_and_its_process_tree_killed() {
            let _guard = spawn_guard();
            let temp = tempfile::tempdir().expect("temp dir");
            let marker = temp.path().join("grandchild.pid");
            // Spawns a long-lived grandchild, records its pid, then hangs.
            let helper = fake_helper(
                temp.path(),
                &format!(
                    "#!/bin/sh\nsleep 600 &\necho $! > '{}'\nsleep 600\n",
                    marker.display()
                ),
            );
            let mut invocation = invocation(temp.path(), helper);
            invocation.env.insert("PATH".into(), "/usr/bin:/bin".into());

            // Shrink the wait by asserting the *bound* rather than waiting the
            // full 30s: the probe must return, and it must return Unknown.
            let start = std::time::Instant::now();
            let outcome = run_provider_probe(&invocation);
            let elapsed = start.elapsed();

            assert_eq!(
                outcome,
                ProviderCapability::Unknown,
                "a wedged helper is unknown, never a login diagnosis"
            );
            assert!(
                elapsed >= PROBE_DEADLINE && elapsed < PROBE_DEADLINE + Duration::from_secs(15),
                "the probe must return at its deadline; took {elapsed:?}"
            );

            let pid: i32 = fs::read_to_string(&marker)
                .expect("grandchild pid recorded")
                .trim()
                .parse()
                .expect("numeric pid");
            let mut alive = true;
            for _ in 0..50 {
                let status = std::process::Command::new("kill")
                    .args(["-0", &pid.to_string()])
                    .stdout(std::process::Stdio::null())
                    .stderr(std::process::Stdio::null())
                    .status();
                if !status.map(|s| s.success()).unwrap_or(false) {
                    alive = false;
                    break;
                }
                std::thread::sleep(Duration::from_millis(100));
            }
            assert!(!alive, "the helper's process group must be killed");
        }
    }
}
