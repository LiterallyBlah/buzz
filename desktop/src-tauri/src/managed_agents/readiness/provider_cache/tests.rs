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
    let cache = ProviderReadinessCache::default();
    let fp = fingerprint(&invocation());
    let calls = AtomicUsize::new(0);

    let probe = || {
        calls.fetch_add(1, Ordering::SeqCst);
        ProviderCapability::Ready
    };

    assert_eq!(
        cache.resolve(&fp, ProbeFreshness::Cached, probe),
        ProviderCapability::Ready
    );
    assert_eq!(
        cache.resolve(&fp, ProbeFreshness::Cached, probe),
        ProviderCapability::Ready
    );
    assert_eq!(calls.load(Ordering::SeqCst), 1, "the second read is cached");

    assert_eq!(
        cache.resolve(&fp, ProbeFreshness::ForceRefresh, probe),
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
    let cache = ProviderReadinessCache::default();
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
    let cache = ProviderReadinessCache::default();
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
    let cache = StdArc::new(ProviderReadinessCache::default());
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
fn concurrent_forced_refreshes_share_one_probe() {
    // The candidate-1 defect: eight users' worth of Retry clicks (setup
    // card, manual restart, Doctor) arriving together each waited for the
    // in-flight probe and then claimed another one — eight serial provider
    // calls for one question.
    let cache = StdArc::new(ProviderReadinessCache::default());
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
                cache.resolve(&fp, ProbeFreshness::ForceRefresh, || {
                    calls.fetch_add(1, Ordering::SeqCst);
                    // Hold the slot long enough that every other thread is
                    // definitely inside the wait rather than racing ahead.
                    std::thread::sleep(Duration::from_millis(200));
                    ProviderCapability::AuthenticationRequired
                })
            })
        })
        .collect();

    for handle in handles {
        assert_eq!(
            handle.join().expect("thread"),
            ProviderCapability::AuthenticationRequired,
            "every forced caller must get the shared verdict"
        );
    }
    assert_eq!(
        calls.load(Ordering::SeqCst),
        1,
        "eight concurrent forced refreshes must produce exactly one probe"
    );
}

#[test]
fn a_forced_refresh_still_bypasses_a_verdict_that_predates_it() {
    // Sharing an in-flight probe must not become "reuse whatever is
    // cached": a forced refresh that arrives when nothing is running still
    // has to go and ask.
    let cache = ProviderReadinessCache::default();
    let fp = fingerprint(&invocation());
    let calls = AtomicUsize::new(0);
    let probe = || {
        calls.fetch_add(1, Ordering::SeqCst);
        ProviderCapability::Ready
    };

    cache.resolve(&fp, ProbeFreshness::Cached, probe);
    cache.resolve(&fp, ProbeFreshness::ForceRefresh, probe);
    cache.resolve(&fp, ProbeFreshness::ForceRefresh, probe);
    assert_eq!(
        calls.load(Ordering::SeqCst),
        3,
        "each sequential forced refresh re-probes"
    );
}

#[test]
fn forced_refreshes_of_different_fingerprints_stay_independent() {
    let cache = StdArc::new(ProviderReadinessCache::default());
    let calls = StdArc::new(AtomicUsize::new(0));

    let handles: Vec<_> = (0..4)
        .map(|i| {
            let cache = StdArc::clone(&cache);
            let calls = StdArc::clone(&calls);
            let mut inv = invocation();
            inv.agent_args = vec![format!("acp-{i}")];
            let fp = fingerprint(&inv);
            std::thread::spawn(move || {
                cache.resolve(&fp, ProbeFreshness::ForceRefresh, || {
                    calls.fetch_add(1, Ordering::SeqCst);
                    std::thread::sleep(Duration::from_millis(50));
                    ProviderCapability::Ready
                })
            })
        })
        .collect();
    for handle in handles {
        handle.join().expect("thread");
    }
    assert_eq!(calls.load(Ordering::SeqCst), 4);
    assert_eq!(cache.len(), 4);
}

#[test]
fn a_cached_caller_joining_an_in_flight_forced_refresh_does_not_add_a_probe() {
    // Mixed traffic: launch restore (Cached) and a Retry click
    // (ForceRefresh) landing together must still be one provider call.
    let cache = StdArc::new(ProviderReadinessCache::default());
    let fp = fingerprint(&invocation());
    let calls = StdArc::new(AtomicUsize::new(0));
    let started = StdArc::new(std::sync::Barrier::new(6));

    let handles: Vec<_> = (0..6)
        .map(|i| {
            let cache = StdArc::clone(&cache);
            let fp = fp.clone();
            let calls = StdArc::clone(&calls);
            let started = StdArc::clone(&started);
            let freshness = if i % 2 == 0 {
                ProbeFreshness::Cached
            } else {
                ProbeFreshness::ForceRefresh
            };
            std::thread::spawn(move || {
                started.wait();
                cache.resolve(&fp, freshness, || {
                    calls.fetch_add(1, Ordering::SeqCst);
                    std::thread::sleep(Duration::from_millis(200));
                    ProviderCapability::Ready
                })
            })
        })
        .collect();
    for handle in handles {
        assert_eq!(handle.join().expect("thread"), ProviderCapability::Ready);
    }
    assert_eq!(calls.load(Ordering::SeqCst), 1);
}

// ── prepare / verify ───────────────────────────────────────────────────

#[test]
fn a_not_applicable_gate_never_reaches_the_provider() {
    let cache = ProviderReadinessCache::default();
    let prepared = prepare(
        &cache,
        &ProviderGate::NotApplicable,
        &invocation(),
        ProbeFreshness::ForceRefresh,
    );
    assert!(matches!(prepared, PreparedPreflight::NotApplicable));
    assert_eq!(cache.len(), 0, "no fingerprint was ever probed");
    assert_eq!(
        verify(&prepared, &ProviderGate::NotApplicable, &invocation()),
        Ok(None)
    );
}

#[test]
fn an_unavailable_gate_prepares_a_fail_closed_verdict_without_probing() {
    let cache = ProviderReadinessCache::default();
    let prepared = prepare(
        &cache,
        &ProviderGate::Unavailable,
        &invocation(),
        ProbeFreshness::ForceRefresh,
    );
    let PreparedPreflight::Verdict { preflight, sidecar } = &prepared else {
        panic!("an unavailable gate must still produce a verdict");
    };
    assert_eq!(preflight.capability, ProviderCapability::AdapterProblem);
    assert_eq!(*sidecar, SidecarIdentity::Unavailable);
    assert_eq!(cache.len(), 0, "nothing was probed");
    // And it survives verification, so the caller enters setup mode rather
    // than starting unproven.
    let checked = verify(&prepared, &ProviderGate::Unavailable, &invocation())
        .expect("an unavailable gate is not stale");
    assert_eq!(
        checked.expect("a verdict").capability,
        ProviderCapability::AdapterProblem
    );
}

#[test]
fn verify_accepts_only_the_descriptor_that_was_probed() {
    let cache = ProviderReadinessCache::default();
    let gate = ProviderGate::Probe {
        acp_binary: std::path::PathBuf::from("/opt/buzz/buzz-acp"),
    };
    let prepared = prepare(&cache, &gate, &invocation(), ProbeFreshness::Cached);

    assert!(verify(&prepared, &gate, &invocation()).is_ok());

    // A persona edit lands between the probe and the spawn.
    let mut moved = invocation();
    moved.agent_args = vec!["acp".into(), "--dangerously-skip-permissions".into()];
    assert_eq!(
        verify(&prepared, &gate, &moved),
        Err(STALE_PROVIDER_PREFLIGHT),
        "a descriptor that moved must not be gated on the old answer"
    );

    // And a snapshot that thought the gate did not apply cannot authorise
    // a spawn that finds it does.
    assert_eq!(
        verify(&PreparedPreflight::NotApplicable, &gate, &invocation()),
        Err(STALE_PROVIDER_PREFLIGHT)
    );
}

/// The bundled sidecar and a developer build of the same binary — the two
/// paths `record.acp_command` realistically moves between.
const SIDECAR_A: &str = "/opt/Buzz.app/Contents/MacOS/buzz-acp";
const SIDECAR_B: &str = "/home/user/buzz/target/debug/buzz-acp";

fn gate_through(sidecar: &str) -> ProviderGate {
    ProviderGate::Probe {
        acp_binary: PathBuf::from(sidecar),
    }
}

fn invocation_through(sidecar: &str) -> ProbeInvocation {
    let mut invocation = invocation();
    invocation.acp_binary = PathBuf::from(sidecar);
    invocation
}

#[test]
fn changed_runtime_sidecar_must_invalidate_prepared_verdict() {
    // The probe runs with no lifecycle lock held, so `record.acp_command`
    // can move under it. A verdict taken through sidecar A proves nothing
    // about a spawn through sidecar B, and the capability fingerprint
    // cannot notice: the sidecar is excluded from it on purpose.
    let cache = ProviderReadinessCache::default();
    assert_eq!(
        fingerprint(&invocation_through(SIDECAR_A)),
        fingerprint(&invocation_through(SIDECAR_B)),
        "the sidecar is not a capability input — the fingerprint alone cannot catch this"
    );

    let prepared = prepare(
        &cache,
        &gate_through(SIDECAR_A),
        &invocation_through(SIDECAR_A),
        ProbeFreshness::Cached,
    );

    assert_eq!(
        verify(
            &prepared,
            &gate_through(SIDECAR_B),
            &invocation_through(SIDECAR_B)
        ),
        Err(STALE_PROVIDER_PREFLIGHT),
        "a verdict from one sidecar must not authorise a spawn through another"
    );
    assert!(
        verify(
            &prepared,
            &gate_through(SIDECAR_A),
            &invocation_through(SIDECAR_A)
        )
        .is_ok(),
        "the sidecar that produced the verdict still authorises its own spawn"
    );
}

#[test]
fn a_resolved_and_an_unresolved_sidecar_never_authorise_each_other() {
    let cache = ProviderReadinessCache::default();

    // Unresolved while probing, resolved by the time we spawn.
    let unresolved = prepare(
        &cache,
        &ProviderGate::Unavailable,
        &invocation_through(SIDECAR_A),
        ProbeFreshness::Cached,
    );
    assert_eq!(
        verify(
            &unresolved,
            &gate_through(SIDECAR_A),
            &invocation_through(SIDECAR_A)
        ),
        Err(STALE_PROVIDER_PREFLIGHT),
        "a fail-closed verdict must not be reused once a sidecar appears"
    );

    // Resolved while probing, unresolvable by the time we spawn.
    let resolved = prepare(
        &cache,
        &gate_through(SIDECAR_A),
        &invocation_through(SIDECAR_A),
        ProbeFreshness::Cached,
    );
    assert_eq!(
        verify(
            &resolved,
            &ProviderGate::Unavailable,
            &invocation_through(SIDECAR_A)
        ),
        Err(STALE_PROVIDER_PREFLIGHT),
        "a probed verdict must not survive the sidecar disappearing"
    );
}

#[test]
fn binding_the_sidecar_does_not_fragment_the_capability_cache() {
    // The two identities stay separate: the verdict is bound to its exact
    // sidecar, while the capability slot — which is about the provider, not
    // about which copy of our own binary asked — is still shared.
    let cache = ProviderReadinessCache::default();
    let first = prepare(
        &cache,
        &gate_through(SIDECAR_A),
        &invocation_through(SIDECAR_A),
        ProbeFreshness::Cached,
    );
    let second = prepare(
        &cache,
        &gate_through(SIDECAR_B),
        &invocation_through(SIDECAR_B),
        ProbeFreshness::Cached,
    );
    assert_eq!(
        cache.len(),
        1,
        "one descriptor keeps one capability slot regardless of sidecar"
    );

    let PreparedPreflight::Verdict {
        preflight: first_preflight,
        sidecar: first_sidecar,
    } = &first
    else {
        panic!("a probe gate must produce a verdict");
    };
    let PreparedPreflight::Verdict {
        preflight: second_preflight,
        sidecar: second_sidecar,
    } = &second
    else {
        panic!("a probe gate must produce a verdict");
    };
    assert_eq!(
        first_preflight, second_preflight,
        "the cached capability answer is reused"
    );
    assert_ne!(
        first_sidecar, second_sidecar,
        "but each verdict is bound to the sidecar that produced it"
    );
}

#[test]
fn verify_does_no_provider_io() {
    // `verify` is the half that runs with every lifecycle lock held, so it
    // must never be able to reach the cache, let alone a subprocess.
    let cache = ProviderReadinessCache::default();
    let gate = ProviderGate::Probe {
        acp_binary: std::path::PathBuf::from("/opt/buzz/buzz-acp"),
    };
    let prepared = prepare(&cache, &gate, &invocation(), ProbeFreshness::Cached);
    let before = cache.len();
    for _ in 0..20 {
        let _ = verify(&prepared, &gate, &invocation());
    }
    assert_eq!(cache.len(), before, "verify must not touch the cache");
}

#[test]
fn lifecycle_locks_stay_free_while_provider_io_is_in_flight() {
    // The candidate-1 defect was structural: the start path held the
    // transition, store and runtime-map locks across the probe, so status
    // polling and every unrelated agent queued behind a provider call that
    // can take 30 seconds. This drives a probe that blocks until told to
    // finish, and proves the locked half of the gate — plus a stand-in for
    // status polling — runs to completion meanwhile.
    let transition = StdArc::new(std::sync::Mutex::new(0usize));
    let store = StdArc::new(std::sync::Mutex::new(0usize));
    let runtimes = StdArc::new(std::sync::Mutex::new(0usize));

    let cache = StdArc::new(ProviderReadinessCache::default());
    let gate = ProviderGate::Probe {
        acp_binary: std::path::PathBuf::from("/opt/buzz/buzz-acp"),
    };
    let fp = fingerprint(&invocation());
    // Rendezvous: the prober has entered provider I/O; and, later, it may
    // leave. Barriers rather than sleeps so the ordering is deterministic.
    let inside_probe = StdArc::new(std::sync::Barrier::new(2));
    let may_finish = StdArc::new(std::sync::Barrier::new(2));

    let prober = {
        let (cache, fp, inside_probe, may_finish) = (
            StdArc::clone(&cache),
            fp.clone(),
            StdArc::clone(&inside_probe),
            StdArc::clone(&may_finish),
        );
        std::thread::spawn(move || {
            // Exactly what the start path now does: no lifecycle lock is
            // even in scope here.
            cache.resolve(&fp, ProbeFreshness::ForceRefresh, || {
                inside_probe.wait();
                may_finish.wait();
                ProviderCapability::Ready
            })
        })
    };

    inside_probe.wait();

    // Provider I/O is in flight. Everything that needs the app-global
    // lifecycle locks must still work.
    {
        let mut transition = transition.lock().expect("transition lock");
        let mut store = store.lock().expect("store lock");
        let mut runtimes = runtimes.lock().expect("runtime map lock");
        *transition += 1;
        *store += 1;
        *runtimes += 1;

        // The locked half of the gate is pure comparison, so it is safe
        // here — and it must not block.
        let prepared = PreparedPreflight::Verdict {
            preflight: ProviderPreflight {
                capability: ProviderCapability::Ready,
                fingerprint: fp.clone(),
            },
            sidecar: SidecarIdentity::of(&gate),
        };
        assert!(verify(&prepared, &gate, &invocation()).is_ok());
    }

    // Only now let the provider call complete.
    may_finish.wait();
    assert_eq!(prober.join().expect("prober"), ProviderCapability::Ready);
    assert_eq!(*transition.lock().expect("transition lock"), 1);
    assert_eq!(*store.lock().expect("store lock"), 1);
    assert_eq!(*runtimes.lock().expect("runtime map lock"), 1);
}

#[test]
fn unequal_concurrent_fingerprints_probe_independently() {
    let cache = StdArc::new(ProviderReadinessCache::default());
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
