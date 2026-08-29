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
use std::path::{Path, PathBuf};
use std::sync::{Condvar, Mutex};
use std::time::{Duration, Instant};

use sha2::{Digest, Sha256};

use super::provider_probe::{
    run_provider_probe, ProbeInvocation, ProviderCapability, ProviderGate,
};

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
            // Set once this call has actually waited on an in-flight probe.
            // That probe is running concurrently with us, so its verdict is
            // not a verdict from *before* the user acted — which is the only
            // thing a forced refresh is entitled to reject.
            let mut joined_in_flight = false;
            loop {
                let slot = map.entry(key.clone()).or_default();

                if let Some(cached) = slot.verdict.clone() {
                    let usable = Instant::now() < cached.expires_at
                        && match freshness {
                            ProbeFreshness::Cached => true,
                            ProbeFreshness::ForceRefresh => joined_in_flight,
                        };
                    if usable {
                        return cached.capability;
                    }
                }

                if slot.probing {
                    // Someone is already contacting this exact provider with
                    // this exact descriptor. Both kinds of caller share it: a
                    // forced refresh exists to bypass an *old* verdict, not to
                    // queue behind the live one and then ask again. Claiming a
                    // fresh probe here is what turned eight concurrent Retry
                    // clicks into eight serial provider calls.
                    joined_in_flight = true;
                    map = self.finished.wait(map).unwrap_or_else(|e| e.into_inner());
                    continue;
                }

                // A forced refresh drops the stale verdict now, so a
                // concurrent cached reader cannot pick it up while we
                // re-probe.
                if freshness == ProbeFreshness::ForceRefresh {
                    slot.verdict = None;
                }
                slot.probing = true;
                break;
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

// ── Prepare / verify ─────────────────────────────────────────────────────────
//
// Provider I/O is the slowest thing a start path does — up to the probe's
// 30-second deadline — and every desktop lifecycle lock is app-global. So the
// gate is split in two: `prepare` does all of the waiting and holds nothing,
// `verify` does all of the deciding and holds no I/O. A caller runs `prepare`
// before it takes the transition, store and runtime-map locks, and `verify`
// after, which is what keeps status polling and every other agent responsive
// while one agent is starting.

/// The result of a provider preflight, ready to be stamped on the generation
/// that the caller is about to start.
///
/// Carrying the fingerprint alongside the verdict is what makes the stamp
/// meaningful: status reads the *generation's* answer, so a later cache result
/// for a since-changed descriptor cannot retroactively describe a process that
/// is already running — and `verify` can tell that the descriptor moved while
/// the probe was in flight.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ProviderPreflight {
    /// The verdict for the descriptor that was actually probed.
    pub capability: ProviderCapability,
    /// The fingerprint the verdict belongs to.
    pub fingerprint: CapabilityFingerprint,
}

/// The exact sidecar a prepared verdict was produced through.
///
/// Deliberately *not* the capability fingerprint. The fingerprint answers "will
/// the provider accept a turn for this descriptor", and which copy of our own
/// sidecar asked has no bearing on that — so it is excluded there on purpose,
/// otherwise a dev build path would churn the cache for every developer. But
/// spawn *authorisation* is a different question: the verdict may only
/// authorise the runtime it was actually taken through. This token carries that
/// identity, is closed, and is compared exactly.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum SidecarIdentity {
    /// The gate did not apply, so no sidecar was involved.
    NotApplicable,
    /// The gate applied but no sidecar could be resolved.
    Unavailable,
    /// The gate applied through exactly this resolved sidecar.
    Probe { acp_binary: PathBuf },
}

impl SidecarIdentity {
    /// The identity of the gate a verdict was prepared from — or re-derived
    /// from the authoritative record under the caller's locks.
    pub(crate) fn of(gate: &ProviderGate) -> Self {
        match gate {
            ProviderGate::NotApplicable => Self::NotApplicable,
            ProviderGate::Unavailable => Self::Unavailable,
            ProviderGate::Probe { acp_binary } => Self::Probe {
                acp_binary: acp_binary.clone(),
            },
        }
    }
}

/// A verdict computed with no lifecycle lock held.
#[derive(Debug, Clone)]
pub(crate) enum PreparedPreflight {
    /// The gate did not apply to the descriptor the caller snapshotted.
    NotApplicable,
    /// A verdict, valid only for the fingerprint *and* the sidecar it carries.
    Verdict {
        /// The verdict itself, as it would be stamped on the generation.
        preflight: ProviderPreflight,
        /// The exact sidecar the gate resolved to when this was prepared.
        sidecar: SidecarIdentity,
    },
}

/// Returned by [`verify`] when the descriptor resolved under the caller's locks
/// is not the one that was probed.
///
/// User-facing on purpose: the caller either retries once with a fresh probe or
/// refuses the start. It must never spawn.
pub(crate) const STALE_PROVIDER_PREFLIGHT: &str =
    "provider readiness changed while the agent was starting";

/// Resolve the provider verdict for `invocation`. **Hold no locks.**
///
/// Blocking: this is where the subprocess, the adapter spawn and the provider
/// round-trip happen.
pub(crate) fn prepare(
    cache: &ProviderReadinessCache,
    gate: &ProviderGate,
    invocation: &ProbeInvocation,
    freshness: ProbeFreshness,
) -> PreparedPreflight {
    match gate {
        ProviderGate::NotApplicable => PreparedPreflight::NotApplicable,
        // The gate applies but cannot be run. Fail closed with a verdict, not
        // by disappearing: an adapter problem is exactly what an unresolvable
        // sidecar is, and it routes the user to the same repair affordance.
        ProviderGate::Unavailable => PreparedPreflight::Verdict {
            preflight: ProviderPreflight {
                capability: ProviderCapability::AdapterProblem,
                fingerprint: fingerprint(invocation),
            },
            sidecar: SidecarIdentity::of(gate),
        },
        ProviderGate::Probe { .. } => {
            let fingerprint = fingerprint(invocation);
            let capability =
                cache.resolve(&fingerprint, freshness, || run_provider_probe(invocation));
            PreparedPreflight::Verdict {
                preflight: ProviderPreflight {
                    capability,
                    fingerprint,
                },
                sidecar: SidecarIdentity::of(gate),
            }
        }
    }
}

/// Check a prepared verdict against the descriptor resolved under the caller's
/// locks. **Performs no I/O**, so it is safe to call with every lock held.
///
/// `Ok(None)` means the gate does not apply and the caller proceeds exactly as
/// it did before this phase existed. `Err` means the descriptor moved while we
/// were probing — a persona edit, a credential change, an adapter upgrade, a
/// sidecar that now resolves somewhere else — and the old answer says nothing
/// about the new configuration.
///
/// Two independent identities have to agree, because they answer different
/// questions. The fingerprint says the verdict is about *this* adapter
/// descriptor; the sidecar token says the verdict was taken through *this*
/// runtime. A verdict that satisfies only the first would let a probe through
/// sidecar A authorise a spawn through sidecar B.
pub(crate) fn verify(
    prepared: &PreparedPreflight,
    gate: &ProviderGate,
    invocation: &ProbeInvocation,
) -> Result<Option<ProviderPreflight>, &'static str> {
    if matches!(gate, ProviderGate::NotApplicable) {
        return Ok(None);
    }
    let authoritative_sidecar = SidecarIdentity::of(gate);
    match prepared {
        // The caller's snapshot said the gate did not apply, but the
        // authoritative descriptor says it does. That disagreement is itself
        // the staleness.
        PreparedPreflight::NotApplicable => Err(STALE_PROVIDER_PREFLIGHT),
        PreparedPreflight::Verdict { preflight, sidecar }
            if *sidecar == authoritative_sidecar
                && preflight.fingerprint == fingerprint(invocation) =>
        {
            Ok(Some(preflight.clone()))
        }
        PreparedPreflight::Verdict { .. } => Err(STALE_PROVIDER_PREFLIGHT),
    }
}

#[cfg(test)]
mod tests;
