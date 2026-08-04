//! Bounded, app-scoped cache for provider-capability probes.
//!
//! A probe costs a subprocess, an adapter spawn, and a provider round-trip.
//! Running one per start, per restart, per Doctor pass and per restored agent
//! would make the readiness gate the slowest thing in the app. Running one
//! *ever* would make it a lie the first time a credential expires.
//!
//! So: cache the verdict against a fingerprint of everything that could change
//! it, with a short TTL and a shorter one for failures, share one in-flight
//! probe between equal concurrent requests, and start cold on every app launch.
//!
//! # What the fingerprint covers
//!
//! Exactly the inputs that decide whether a turn will complete: the adapter
//! binary (path, size, mtime), the configured command identity, the ordered
//! arguments, the working directory, and the effective environment actually
//! passed to the probe. Deliberately *not*: pubkeys, relay URL, Nostr keys,
//! display name, or setup payload — none of them change what the provider will
//! answer, and including them would fragment the cache per agent for no
//! benefit while putting identity material into a hashed key.
//!
//! The fingerprint is never persisted and never displayed.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::{Condvar, Mutex};
use std::time::{Duration, Instant};

use sha2::{Digest, Sha256};

use super::provider_probe::{ProbeInvocation, ProviderCapability};

/// Fingerprint schema version. Bumping it invalidates every cached verdict,
/// which is the correct response to changing what the inputs mean.
const FINGERPRINT_SCHEMA_VERSION: &str = "provider-capability-v1";

/// How long a successful probe is trusted.
const SUCCESS_TTL: Duration = Duration::from_secs(300);

/// How long a failed probe is trusted.
///
/// Much shorter than success: a user who has just run `claude /login` expects
/// the app to notice, and re-probing a broken provider is cheap because it
/// usually fails fast.
const FAILURE_TTL: Duration = Duration::from_secs(30);

/// An opaque capability fingerprint.
///
/// Only ever compared, never rendered. `Debug` prints a fixed placeholder so a
/// stray log line cannot put it on disk.
#[derive(Clone, PartialEq, Eq, Hash)]
pub(crate) struct CapabilityFingerprint(String);

impl std::fmt::Debug for CapabilityFingerprint {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("CapabilityFingerprint(<redacted>)")
    }
}

/// Hash one field, length-delimited.
///
/// Without the length prefix, `("ab", "c")` and `("a", "bc")` would hash
/// identically — an adapter argument could then be forged from a neighbouring
/// field and two genuinely different descriptors would share a verdict.
fn absorb(hasher: &mut Sha256, field: &[u8]) {
    hasher.update((field.len() as u64).to_le_bytes());
    hasher.update(field);
}

/// Compute the capability fingerprint for a probe invocation.
///
/// Reads the adapter binary's size and modification time so an in-place
/// upgrade (`npm install -g …`) invalidates the cache without any explicit
/// signal. A binary we cannot stat contributes a fixed "unknown" marker rather
/// than being skipped, so the failure is visible in the fingerprint instead of
/// silently collapsing two different states together.
pub(crate) fn fingerprint(invocation: &ProbeInvocation) -> CapabilityFingerprint {
    let mut hasher = Sha256::new();
    absorb(&mut hasher, FINGERPRINT_SCHEMA_VERSION.as_bytes());

    let adapter_path = resolved_adapter_path(&invocation.agent_command);
    absorb(&mut hasher, adapter_path.as_bytes());

    let (size, mtime) = adapter_stat(Path::new(&adapter_path));
    absorb(&mut hasher, &size.to_le_bytes());
    absorb(&mut hasher, mtime.as_bytes());

    absorb(&mut hasher, invocation.agent_command.as_bytes());

    absorb(
        &mut hasher,
        &(invocation.agent_args.len() as u64).to_le_bytes(),
    );
    for arg in &invocation.agent_args {
        absorb(&mut hasher, arg.as_bytes());
    }

    absorb(&mut hasher, invocation.cwd.as_os_str().as_encoded_bytes());

    // `BTreeMap` iterates in sorted key order, so map insertion order cannot
    // change the fingerprint. Keys and values are absorbed separately so a key
    // ending where a value begins cannot be confused for the other.
    absorb(&mut hasher, &(invocation.env.len() as u64).to_le_bytes());
    for (key, value) in &invocation.env {
        absorb(&mut hasher, key.as_bytes());
        absorb(&mut hasher, value.as_bytes());
    }

    CapabilityFingerprint(hex::encode(hasher.finalize()))
}

/// Canonicalise the adapter command to an absolute path when we can resolve it.
///
/// A command found on PATH and the same binary named absolutely must produce
/// one fingerprint; a command that resolves to a *different* binary after a
/// PATH change must produce a different one.
fn resolved_adapter_path(command: &str) -> String {
    crate::managed_agents::resolve_command(command)
        .and_then(|path| std::fs::canonicalize(&path).ok().or(Some(path)))
        .map(|path| path.display().to_string())
        .unwrap_or_else(|| format!("<unresolved>:{command}"))
}

/// The adapter binary's size and modification time, as fingerprint inputs.
fn adapter_stat(path: &Path) -> (u64, String) {
    match std::fs::metadata(path) {
        Ok(meta) => {
            let mtime = meta
                .modified()
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| format!("{}.{:09}", d.as_secs(), d.subsec_nanos()))
                .unwrap_or_else(|| "<unknown-mtime>".to_string());
            (meta.len(), mtime)
        }
        Err(_) => (0, "<unstattable>".to_string()),
    }
}

/// A cached verdict and when it stops being trusted.
#[derive(Clone)]
struct CachedVerdict {
    capability: ProviderCapability,
    expires_at: Instant,
}

/// Per-fingerprint slot: either a live verdict, an in-flight probe, or both.
#[derive(Default)]
struct Slot {
    verdict: Option<CachedVerdict>,
    probing: bool,
}

/// The cache itself.
///
/// App-scoped, held in `AppState`: not a process-global (which would leak
/// between test binaries and outlive the config it was computed from) and not
/// persisted (a verdict older than the app is a verdict about a world that may
/// no longer exist).
#[derive(Default)]
pub(crate) struct ProviderReadinessCache {
    inner: Mutex<BTreeMap<String, Slot>>,
    /// Signalled whenever a probe finishes, so waiters on the same fingerprint
    /// wake without polling.
    finished: Condvar,
}

/// Why a lookup was made — controls whether a cached verdict may be used.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ProbeFreshness {
    /// An ordinary start. A live cached verdict is fine.
    Cached,
    /// A manual restart, setup Retry, auth completion, or Doctor pass. The user
    /// just did something they expect us to notice, so the cache is bypassed.
    ForceRefresh,
}

impl ProviderReadinessCache {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Resolve a capability verdict, running `probe` at most once per
    /// fingerprint across all concurrent callers.
    ///
    /// Blocking: callers are the (already synchronous, already lock-free)
    /// spawn paths.
    pub(crate) fn resolve<F>(
        &self,
        fingerprint: &CapabilityFingerprint,
        freshness: ProbeFreshness,
        probe: F,
    ) -> ProviderCapability
    where
        F: FnOnce() -> ProviderCapability,
    {
        let key = fingerprint.0.clone();

        // ── Claim the probe, or wait for the one already running ────────────
        {
            let mut map = self.inner.lock().unwrap_or_else(|e| e.into_inner());
            loop {
                let slot = map.entry(key.clone()).or_default();

                if freshness == ProbeFreshness::Cached {
                    if let Some(cached) = slot.verdict.clone() {
                        if Instant::now() < cached.expires_at {
                            return cached.capability;
                        }
                    }
                }

                if !slot.probing {
                    // A forced refresh drops the stale verdict now, so a
                    // concurrent cached reader cannot pick it up while we
                    // re-probe.
                    if freshness == ProbeFreshness::ForceRefresh {
                        slot.verdict = None;
                    }
                    slot.probing = true;
                    break;
                }

                // Someone else is probing this exact fingerprint. Wait for it
                // rather than spawning a second adapter against the same
                // provider.
                map = self.finished.wait(map).unwrap_or_else(|e| e.into_inner());

                // A forced refresh does not get to consume the verdict it was
                // waiting on — it asked for a fresh one. Loop again; the slot
                // is now free and this iteration will claim it.
                if freshness == ProbeFreshness::ForceRefresh {
                    continue;
                }
                if let Some(cached) = map.get(&key).and_then(|s| s.verdict.clone()) {
                    if Instant::now() < cached.expires_at {
                        return cached.capability;
                    }
                }
            }
        }

        // ── Probe outside the lock ──────────────────────────────────────────
        let capability = probe();

        let ttl = if capability.is_ready() {
            SUCCESS_TTL
        } else {
            FAILURE_TTL
        };
        let mut map = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let slot = map.entry(key).or_default();
        slot.verdict = Some(CachedVerdict {
            capability: capability.clone(),
            expires_at: Instant::now() + ttl,
        });
        slot.probing = false;
        drop(map);
        self.finished.notify_all();

        capability
    }

    /// Test-only: how many fingerprints the cache is tracking.
    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.inner.lock().unwrap().len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc as StdArc;

    fn invocation() -> ProbeInvocation {
        ProbeInvocation {
            acp_binary: std::path::PathBuf::from("/opt/buzz/buzz-acp"),
            agent_command: "claude-agent-acp".into(),
            agent_args: vec!["acp".into()],
            cwd: std::path::PathBuf::from("/home/user"),
            env: [("ANTHROPIC_API_KEY".to_string(), "sk-a".to_string())]
                .into_iter()
                .collect(),
        }
    }

    // ── fingerprint ────────────────────────────────────────────────────────

    #[test]
    fn an_unchanged_descriptor_fingerprints_identically() {
        assert_eq!(fingerprint(&invocation()), fingerprint(&invocation()));
    }

    #[test]
    fn env_map_order_does_not_affect_the_fingerprint() {
        let mut forward = invocation();
        forward.env.insert("A".into(), "1".into());
        forward.env.insert("Z".into(), "26".into());

        let mut reverse = invocation();
        reverse.env.insert("Z".into(), "26".into());
        reverse.env.insert("A".into(), "1".into());

        assert_eq!(fingerprint(&forward), fingerprint(&reverse));
    }

    #[test]
    fn every_capability_affecting_input_changes_the_fingerprint() {
        let base = fingerprint(&invocation());

        let mut command = invocation();
        command.agent_command = "claude-code-acp".into();
        assert_ne!(fingerprint(&command), base, "command identity");

        let mut args = invocation();
        args.agent_args = vec!["acp".into(), "--verbose".into()];
        assert_ne!(fingerprint(&args), base, "argument list");

        let mut arg_order = invocation();
        arg_order.agent_args = vec!["b".into(), "a".into()];
        let mut arg_order_swapped = invocation();
        arg_order_swapped.agent_args = vec!["a".into(), "b".into()];
        assert_ne!(
            fingerprint(&arg_order),
            fingerprint(&arg_order_swapped),
            "argument order"
        );

        let mut cwd = invocation();
        cwd.cwd = std::path::PathBuf::from("/tmp/other");
        assert_ne!(fingerprint(&cwd), base, "working directory");

        let mut env_value = invocation();
        env_value
            .env
            .insert("ANTHROPIC_API_KEY".into(), "sk-b".into());
        assert_ne!(fingerprint(&env_value), base, "env value");

        let mut env_key = invocation();
        env_key.env.insert("ANTHROPIC_BASE_URL".into(), "x".into());
        assert_ne!(fingerprint(&env_key), base, "env key");
    }

    #[test]
    fn concatenation_of_adjacent_fields_cannot_be_forged() {
        // Without length-delimited absorption these two would hash the same.
        let mut left = invocation();
        left.env = [("AB".to_string(), "C".to_string())].into_iter().collect();
        let mut right = invocation();
        right.env = [("A".to_string(), "BC".to_string())].into_iter().collect();
        assert_ne!(fingerprint(&left), fingerprint(&right));
    }

    #[test]
    fn the_acp_sidecar_path_is_not_a_capability_input() {
        // The sidecar is our own binary. Which copy of it runs the probe has
        // no bearing on what the provider will answer, and letting a dev-build
        // path churn the fingerprint would defeat the cache.
        let mut other = invocation();
        other.acp_binary = std::path::PathBuf::from("/elsewhere/buzz-acp");
        assert_eq!(fingerprint(&other), fingerprint(&invocation()));
    }

    #[test]
    fn a_fingerprint_never_renders_its_value() {
        let printed = format!("{:?}", fingerprint(&invocation()));
        assert_eq!(printed, "CapabilityFingerprint(<redacted>)");
    }

    #[cfg(unix)]
    #[test]
    fn an_in_place_adapter_upgrade_changes_the_fingerprint() {
        use std::fs;
        use std::os::unix::fs::PermissionsExt;

        let temp = tempfile::tempdir().expect("temp dir");
        let adapter = temp.path().join("claude-agent-acp");
        fs::write(&adapter, "#!/bin/sh\nexit 0\n").expect("write v1");
        fs::set_permissions(&adapter, fs::Permissions::from_mode(0o755)).expect("chmod");

        let mut inv = invocation();
        inv.agent_command = adapter.display().to_string();
        let before = fingerprint(&inv);

        // A larger binary with a later mtime — the shape of an npm upgrade.
        std::thread::sleep(Duration::from_millis(20));
        fs::write(&adapter, "#!/bin/sh\n# version two\nexit 0\n").expect("write v2");
        let after = fingerprint(&inv);

        assert_ne!(
            before, after,
            "an upgraded adapter must not reuse the old verdict"
        );
    }

    // ── cache behaviour ────────────────────────────────────────────────────

    #[test]
    fn a_live_verdict_is_reused_and_a_forced_refresh_is_not() {
        let cache = ProviderReadinessCache::new();
        let fp = fingerprint(&invocation());
        let calls = AtomicUsize::new(0);

        let mut probe = || {
            calls.fetch_add(1, Ordering::SeqCst);
            ProviderCapability::Ready
        };

        assert_eq!(
            cache.resolve(&fp, ProbeFreshness::Cached, &mut probe),
            ProviderCapability::Ready
        );
        assert_eq!(
            cache.resolve(&fp, ProbeFreshness::Cached, &mut probe),
            ProviderCapability::Ready
        );
        assert_eq!(calls.load(Ordering::SeqCst), 1, "the second read is cached");

        assert_eq!(
            cache.resolve(&fp, ProbeFreshness::ForceRefresh, &mut probe),
            ProviderCapability::Ready
        );
        assert_eq!(
            calls.load(Ordering::SeqCst),
            2,
            "a forced refresh must re-probe"
        );
    }

    #[test]
    fn different_fingerprints_do_not_share_a_verdict() {
        let cache = ProviderReadinessCache::new();
        let ready_fp = fingerprint(&invocation());
        let mut other = invocation();
        other.agent_command = "codex-acp".into();
        let other_fp = fingerprint(&other);

        assert_eq!(
            cache.resolve(&ready_fp, ProbeFreshness::Cached, || {
                ProviderCapability::Ready
            }),
            ProviderCapability::Ready
        );
        assert_eq!(
            cache.resolve(&other_fp, ProbeFreshness::Cached, || {
                ProviderCapability::AuthenticationRequired
            }),
            ProviderCapability::AuthenticationRequired
        );
        assert_eq!(cache.len(), 2);
    }

    #[test]
    fn a_success_and_a_failure_get_different_lifetimes() {
        // The TTLs are the contract; assert them directly rather than sleeping
        // for five minutes.
        assert_eq!(SUCCESS_TTL, Duration::from_secs(300));
        assert_eq!(FAILURE_TTL, Duration::from_secs(30));
        assert!(FAILURE_TTL < SUCCESS_TTL);
    }

    #[test]
    fn an_expired_verdict_is_re_probed() {
        let cache = ProviderReadinessCache::new();
        let fp = fingerprint(&invocation());
        cache.resolve(&fp, ProbeFreshness::Cached, || ProviderCapability::Ready);

        // Force expiry by rewriting the stored deadline into the past.
        {
            let mut map = cache.inner.lock().unwrap();
            let slot = map.get_mut(&fp.0).expect("slot");
            slot.verdict.as_mut().expect("verdict").expires_at =
                Instant::now() - Duration::from_secs(1);
        }

        let calls = AtomicUsize::new(0);
        let outcome = cache.resolve(&fp, ProbeFreshness::Cached, || {
            calls.fetch_add(1, Ordering::SeqCst);
            ProviderCapability::AuthenticationRequired
        });
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert_eq!(outcome, ProviderCapability::AuthenticationRequired);
    }

    #[test]
    fn equal_concurrent_fingerprints_share_one_probe() {
        let cache = StdArc::new(ProviderReadinessCache::new());
        let fp = fingerprint(&invocation());
        let calls = StdArc::new(AtomicUsize::new(0));
        let started = StdArc::new(std::sync::Barrier::new(8));

        let handles: Vec<_> = (0..8)
            .map(|_| {
                let cache = StdArc::clone(&cache);
                let fp = fp.clone();
                let calls = StdArc::clone(&calls);
                let started = StdArc::clone(&started);
                std::thread::spawn(move || {
                    started.wait();
                    cache.resolve(&fp, ProbeFreshness::Cached, || {
                        calls.fetch_add(1, Ordering::SeqCst);
                        // Hold the slot long enough that every other thread is
                        // definitely waiting rather than racing ahead.
                        std::thread::sleep(Duration::from_millis(200));
                        ProviderCapability::Ready
                    })
                })
            })
            .collect();

        for handle in handles {
            assert_eq!(handle.join().expect("thread"), ProviderCapability::Ready);
        }
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "eight equal concurrent requests must produce exactly one probe"
        );
    }

    #[test]
    fn unequal_concurrent_fingerprints_probe_independently() {
        let cache = StdArc::new(ProviderReadinessCache::new());
        let calls = StdArc::new(AtomicUsize::new(0));

        let handles: Vec<_> = (0..4)
            .map(|i| {
                let cache = StdArc::clone(&cache);
                let calls = StdArc::clone(&calls);
                let mut inv = invocation();
                inv.agent_args = vec![format!("acp-{i}")];
                let fp = fingerprint(&inv);
                std::thread::spawn(move || {
                    cache.resolve(&fp, ProbeFreshness::Cached, || {
                        calls.fetch_add(1, Ordering::SeqCst);
                        ProviderCapability::Ready
                    })
                })
            })
            .collect();
        for handle in handles {
            handle.join().expect("thread");
        }
        assert_eq!(calls.load(Ordering::SeqCst), 4);
    }
}
