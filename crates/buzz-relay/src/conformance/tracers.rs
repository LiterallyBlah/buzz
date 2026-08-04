//! Concrete [`Tracer`] implementations. Production uses [`NoopTracer`];
//! conformance tests + the CI replay job use [`JsonlTracer`].

use std::fs::{File, OpenOptions};
use std::io::{BufWriter, Write};
use std::path::Path;
use std::sync::{Arc, Mutex};

use buzz_conformance::{TraceStep, Tracer};

/// Zero-cost tracer used in production builds. Records nothing — the
/// emitter call still constructs the action arguments, but the build can
/// have the compiler eliminate them entirely behind a feature flag if
/// the cost ever shows up in benches.
#[derive(Debug, Default, Clone, Copy)]
pub struct NoopTracer;

impl Tracer for NoopTracer {
    fn record(&self, _step: TraceStep) {}
}

/// JSONL-to-file tracer for tests + the CI replay job. Each `record` call
/// serializes the step as one line of JSON and appends it. The file is
/// opened in append mode so multiple test runs accumulate; consumers are
/// expected to truncate between runs.
///
/// The internal `Mutex<BufWriter<File>>` serializes writes — concurrent
/// requests producing interleaved JSONL is fine on the read side because
/// the spec doesn't model emission order, only set membership.
pub struct JsonlTracer {
    out: Mutex<BufWriter<File>>,
}

impl JsonlTracer {
    /// Open a new JSONL tracer writing to `path`. Truncates any existing
    /// file at that path so a fresh test run starts clean.
    pub fn create<P: AsRef<Path>>(path: P) -> std::io::Result<Self> {
        let file = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(path)?;
        Ok(Self {
            out: Mutex::new(BufWriter::new(file)),
        })
    }
}

impl std::fmt::Debug for JsonlTracer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("JsonlTracer").finish_non_exhaustive()
    }
}

/// The one-line switch behind `BUZZ_CONFORMANCE_TRACE_PATH`
/// ([`crate::config::Config::conformance_trace_path`]).
///
/// * `None` — the production default — returns `Arc::new(NoopTracer)`. That is
///   *literally* the expression `AppState::new` used before this switch
///   existed: same allocation, same trait object, same `record` that compiles
///   to nothing. No file is touched, no error is possible, and nothing on the
///   request path branches — the choice is made once, at construction.
/// * `Some(path)` — returns a [`JsonlTracer`] appending one JSON step per line
///   to `path`, truncating whatever was there so a run's trace is only that
///   run's.
///
/// Returning `io::Result` rather than falling back to the no-op is deliberate
/// and the caller is expected to treat `Err` as fatal: see the binding site in
/// `crates/buzz-relay/src/state.rs`. A relay that was *asked* to trace and
/// quietly did not would hand its conformance gate an empty file, and an empty
/// file replays as a pass in any checker that does not fail closed. The
/// checker does fail closed (`buzz_conformance::checker::check_trace` calls an
/// empty trace a coverage breach) — but only if it is handed the file at all,
/// and the gate cannot distinguish "the relay refused to open it" from "the
/// workload produced nothing" unless the relay says so at boot.
pub fn tracer_for_trace_path(path: Option<&Path>) -> std::io::Result<Arc<dyn Tracer>> {
    match path {
        None => Ok(Arc::new(NoopTracer)),
        Some(path) => Ok(Arc::new(JsonlTracer::create(path)?)),
    }
}

impl Tracer for JsonlTracer {
    fn record(&self, step: TraceStep) {
        // Acquire-and-write. If the lock is poisoned we accept the panic
        // — this is observability code and a poisoned lock means a worse
        // bug landed elsewhere.
        let mut guard = match self.out.lock() {
            Ok(g) => g,
            Err(e) => e.into_inner(),
        };
        // Best-effort: a write failure here loses one trace step but
        // must NOT take down the request path. The Drop guard's
        // coverage-breach action is the safety net for systemic loss.
        if let Ok(line) = serde_json::to_string(&step) {
            let _ = guard.write_all(line.as_bytes());
            let _ = guard.write_all(b"\n");
            let _ = guard.flush();
        }
    }
}

#[cfg(test)]
mod tests {
    //! Unit tests for the `BUZZ_CONFORMANCE_TRACE_PATH` switch.
    //!
    //! HOST NOTE: `buzz-relay`'s **test** targets do not build on the
    //! self-hosted staging box — its dev-dependencies pull OpenSSL and that
    //! host has no OpenSSL development headers (see
    //! `scripts/selfhost/gates/README.md` § "Does not prove"). These tests are
    //! therefore written for CI, and the switch's behaviour on that box is
    //! proven end to end instead by `gate-conformance.sh` phase B, which runs a
    //! real relay with the variable set and replays the file it produced.
    //!
    //! std-only on purpose: no `tempfile`, no new dev-dependency, so nothing
    //! about this file changes which crates the relay's test target has to
    //! resolve.

    use super::*;
    use buzz_conformance::{
        AbstractState, ActorLabel, CommunityLabel, HostLabel, SanitizedReason, TraceAction,
    };

    /// A per-test scratch directory under the system temp dir. Named by pid +
    /// tag so parallel test threads never share one.
    fn scratch_dir(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "buzz-relay-tracer-switch-{}-{}",
            std::process::id(),
            tag
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("scratch dir");
        dir
    }

    fn a_step() -> TraceStep {
        TraceStep::new(
            TraceAction::SanitizedError {
                reason: SanitizedReason::Invalid,
            },
            AbstractState {
                resolved_community: CommunityLabel::from_uuid(uuid::Uuid::from_u128(0xA)),
                bound_host: HostLabel("relay.test".to_string()),
                actor: ActorLabel("0123456789abcdef".to_string()),
            },
        )
    }

    /// The production default. `None` must produce a tracer that writes
    /// nothing anywhere — the assertion is on the filesystem, not on the type,
    /// because "no file appeared" is the property that actually matters when
    /// this ships.
    #[test]
    fn unset_trace_path_writes_nothing() {
        let dir = scratch_dir("unset");
        let tracer = tracer_for_trace_path(None).expect("None never fails");
        tracer.record(a_step());
        tracer.record(a_step());
        let entries: Vec<_> = std::fs::read_dir(&dir)
            .expect("scratch dir readable")
            .collect();
        assert!(
            entries.is_empty(),
            "the no-op tracer must not create files; found {} entries",
            entries.len()
        );
    }

    /// Set path ⇒ one JSON object per line, each round-tripping back to the
    /// exact step recorded. The line-per-step shape is the contract
    /// `check-trace` parses, so this test is what stops the emitter and the
    /// replay CLI drifting apart.
    #[test]
    fn set_trace_path_writes_one_round_trippable_line_per_step() {
        let path = scratch_dir("set").join("trace.jsonl");
        let tracer = tracer_for_trace_path(Some(&path)).expect("creatable path");
        tracer.record(a_step());
        tracer.record(a_step());

        let text = std::fs::read_to_string(&path).expect("trace file readable");
        let lines: Vec<&str> = text.lines().filter(|l| !l.trim().is_empty()).collect();
        assert_eq!(lines.len(), 2, "one line per recorded step");
        for line in lines {
            let parsed: TraceStep = serde_json::from_str(line).expect("line parses as a TraceStep");
            assert_eq!(parsed, a_step(), "line round-trips to the recorded step");
        }
    }

    /// `JsonlTracer::create` truncates. A gate that replayed a file still
    /// holding the *previous* run's steps would be scoring the wrong relay.
    #[test]
    fn set_trace_path_truncates_a_previous_run() {
        let path = scratch_dir("truncate").join("trace.jsonl");
        std::fs::write(&path, "{\"stale\":\"from an earlier run\"}\n").expect("seed stale file");

        let tracer = tracer_for_trace_path(Some(&path)).expect("creatable path");
        tracer.record(a_step());

        let text = std::fs::read_to_string(&path).expect("trace file readable");
        assert!(
            !text.contains("stale"),
            "the previous run's trace must be truncated, got: {text}"
        );
        assert_eq!(text.lines().count(), 1, "exactly this run's single step");
    }

    /// The load-bearing negative: an unopenable path is an `Err`, never a
    /// quiet fallback to `NoopTracer`. `state.rs` turns this `Err` into a
    /// refusal to start; if this returned `Ok(NoopTracer)` instead, a relay
    /// asked to trace would run untraced and the gate would replay an empty
    /// file it could not distinguish from an idle workload.
    #[test]
    fn unopenable_trace_path_is_an_error_not_a_silent_noop() {
        let path = scratch_dir("unopenable")
            .join("no-such-directory")
            .join("trace.jsonl");
        let err = tracer_for_trace_path(Some(&path))
            .err()
            .expect("a path under a missing directory must fail");
        assert_eq!(
            err.kind(),
            std::io::ErrorKind::NotFound,
            "expected NotFound for a missing parent directory, got {err:?}"
        );
    }
}
