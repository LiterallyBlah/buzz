//! `check-trace` — replay a captured runtime trace against
//! `docs/spec/MultiTenantRelay.tla`.
//!
//! This is the shell-invokable half of the crate. The library answers "does
//! this `Vec<TraceStep>` conform?"; this binary answers "does the JSONL file
//! the relay just wrote conform?", which is the question a gate can actually
//! ask. Its producer is [`buzz-relay`'s `JsonlTracer`], switched on with
//! `BUZZ_CONFORMANCE_TRACE_PATH`; its consumer is
//! `scripts/selfhost/gates/gate-conformance.sh` phase B.
//!
//! # Contract
//!
//! ```text
//! check-trace [OPTIONS] <TRACE.jsonl|->
//!
//!   exit 0   the trace conforms
//!   exit 1   the trace does NOT conform (verdict on stdout, detail on stderr)
//!   exit 2   the CLI could not form an opinion (bad flags, unreadable file)
//! ```
//!
//! Exit 1 and exit 2 are deliberately different. "The relay violated the spec"
//! and "I could not read the file" must never collapse into one status, or a
//! typo'd path in a gate script reads as a conformance failure — or worse, a
//! gate that only checks `!= 0` treats an unreadable trace as proof of a bug
//! it never saw.
//!
//! # The three failure modes, plus one
//!
//! The crate documents three (`src/lib.rs` § Failure modes) and this binary
//! reports each under a stable machine-readable name:
//!
//! | mode | source |
//! |---|---|
//! | `illegal_transition` | [`TransitionError::IllegalTransition`] |
//! | `state_mismatch` | [`TransitionError::StateMismatch`] |
//! | `non_interference` | [`TransitionError::NonInterference`] |
//! | `coverage_breach` | [`TransitionError::CoverageBreach`] |
//! | `malformed_step` | a line the schema cannot decode — CLI-level |
//!
//! `non_interference` is listed separately from `state_mismatch` because the
//! crate models it as its own variant; the lib docs fold it into "state
//! mismatch" prose. `malformed_step` is the CLI's own: a JSONL line the schema
//! cannot decode means emitter and checker disagree about the wire format,
//! which is a conformance finding (exit 1), not an I/O accident (exit 2). It
//! is not a truncation artefact either — `JsonlTracer::record` flushes after
//! every line, so a half-written line cannot survive a clean stop.
//!
//! # Why grouping exists
//!
//! [`check_trace`] bootstraps ONE model state from the first step and requires
//! every later step to agree on `(resolved_community, bound_host, actor)` —
//! because a trace, in the library's model, covers one worker handling one
//! request (`src/transitions.rs` § "What an abstract state means here").
//!
//! A live relay's file is not one request. It is every request the process
//! served, interleaved across workers and actors. Replaying it as a single
//! scenario would report `state_mismatch` the instant a second actor appeared
//! — a finding about the *file*, not about the relay.
//!
//! So `--group-by state` (the default) partitions the file by `state_after`
//! and replays each partition as its own scenario, preserving file order
//! within a partition. Consequences, stated plainly because one of them is a
//! real weakening:
//!
//! * **`non_interference` stays fully live.** Row-label confinement is judged
//!   against the partition's own resolved community, so a foreign row still
//!   bites. This is the mode that catches the tenant-fence bugs.
//! * **`illegal_transition` stays fully live** (e.g. an `AuthCheck` Allow with
//!   a foreign claim), as does `coverage_breach` via `ImplBug`.
//! * **`state_mismatch` becomes unreachable *within* a partition**, since the
//!   partition key is exactly the tuple that check compares. Under
//!   `--group-by state` that mode is exercised only by the crate's own fixture
//!   suite, not by the replayed run. Pass `--group-by none` to replay the file
//!   as a single scenario and get the mode back — correct for a trace known to
//!   cover exactly one request.
//!
//! # Why coverage is checked over the whole file
//!
//! `--require` names actions the *run* must exercise. Handing that set to each
//! partition would demand every request emit every action, which no real
//! workload does. So partitions are replayed with an empty requirement and the
//! requirement is settled once, against the union of kinds seen anywhere in
//! the file. With a single partition the two are identical, and
//! `global_coverage_matches_check_trace_on_a_single_group` pins that.
//!
//! # Why no `clap`
//!
//! This crate's `Cargo.toml` argues for every dependency it has, on the
//! grounds that the checker's whole value is being an independent judge with
//! nothing to inherit a bug from. Four flags do not justify pulling a
//! derive-macro argument parser and its tree into that. The parser below is
//! ~60 lines and is unit-tested.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::io::Read;
use std::process::ExitCode;

use buzz_conformance::checker::{check_trace, Scenario};
use buzz_conformance::transitions::TransitionError;
use buzz_conformance::{TraceStep, SCHEMA_VERSION};

const HELP: &str = "\
check-trace — replay a relay conformance trace against MultiTenantRelay.tla

USAGE
  check-trace [OPTIONS] <TRACE.jsonl>
  check-trace [OPTIONS] -            read the trace from stdin

OPTIONS
  --require <KINDS>   Critical action kinds the run MUST exercise at least
                      once, comma-separated. Repeatable; sets union.
                      Missing kinds are a coverage breach (exit 1).
                      DEFAULT: empty — see the note below.
  --group-by <MODE>   state | none                       [default: state]
                      state — partition the file by state_after
                              (resolved_community, bound_host, actor) and
                              replay each partition as its own scenario.
                              Correct for a live multi-request trace.
                      none  — replay the whole file as ONE scenario. Stricter
                              (a state_after change is then a state_mismatch),
                              correct only when the trace covers one request.
  --json              Emit the summary as one JSON object on stdout instead of
                      the human table. Shape is stable; see 'schema' field.
  -h, --help          This text.

ACTION KINDS (for --require)
  write_insert  write_insert_global  write_duplicate  sanitized_error
  auth_check    read_message_rows    read_by_id_rows  read_host_feed_rows
  (impl_bug exists in the schema but is a breach marker, not a requirement,
   and is rejected here.)

ON THE EMPTY DEFAULT
  With no --require, the run is only checked for well-formedness: illegal
  transitions, non-interference, and the ImplBug / empty-trace coverage
  breaches still bite, but 'the workload silently stopped exercising the write
  path' does not. The library is blunt about this — 'Coverage breach is
  load-bearing. Without it, trace conformance is decorative logging.'
  There is no honest default here because only the caller knows which actions
  its scenario drives, so the CLI defaults to empty AND says so loudly in the
  summary. Callers that are gates should always pass --require.

EXIT
  0  conforms
  1  does not conform (illegal_transition, state_mismatch, non_interference,
     coverage_breach, malformed_step)
  2  could not form an opinion (bad usage, unreadable trace)
";

/// The requirable action-kind vocabulary — the strings
/// `buzz_conformance::TraceAction::kind` returns, minus `impl_bug`.
///
/// Validating `--require` against this list is not pedantry: an unvalidated
/// typo (`--require auth-check`) would be a requirement nothing can satisfy,
/// and the gate would report a coverage breach that says more about the flag
/// than about the relay. A typo must be exit 2, never exit 1.
const REQUIRABLE_ACTION_KINDS: &[&str] = &[
    "write_insert",
    "write_insert_global",
    "write_duplicate",
    "sanitized_error",
    "auth_check",
    "read_message_rows",
    "read_by_id_rows",
    "read_host_feed_rows",
];

/// The breach marker. Never a coverage *requirement*.
const IMPL_BUG_KIND: &str = "impl_bug";

// ---- arguments --------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Grouping {
    /// Partition by `state_after`; replay each partition separately.
    PerRequestState,
    /// One scenario for the whole file.
    WholeFile,
}

impl Grouping {
    fn as_str(self) -> &'static str {
        match self {
            Grouping::PerRequestState => "state",
            Grouping::WholeFile => "none",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Args {
    trace_path: String,
    required: BTreeSet<String>,
    grouping: Grouping,
    json: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum ArgError {
    Help,
    Message(String),
}

impl std::fmt::Display for ArgError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ArgError::Help => f.write_str("help requested"),
            ArgError::Message(m) => f.write_str(m),
        }
    }
}

fn parse_args(argv: &[String]) -> Result<Args, ArgError> {
    let mut trace_path: Option<String> = None;
    let mut required: BTreeSet<String> = BTreeSet::new();
    let mut grouping = Grouping::PerRequestState;
    let mut json = false;

    let mut i = 0;
    while i < argv.len() {
        let arg = argv[i].as_str();
        match arg {
            "-h" | "--help" => return Err(ArgError::Help),
            "--json" => {
                json = true;
                i += 1;
            }
            "--require" | "--group-by" => {
                let value = argv.get(i + 1).ok_or_else(|| {
                    ArgError::Message(format!("{arg} needs a value (see --help)"))
                })?;
                if arg == "--require" {
                    for kind in value.split(',').map(str::trim).filter(|k| !k.is_empty()) {
                        if kind == IMPL_BUG_KIND {
                            return Err(ArgError::Message(format!(
                                "--require {IMPL_BUG_KIND}: {IMPL_BUG_KIND} is the coverage-breach \
                                 marker the relay emits when a seam exits without tracing. Any \
                                 trace containing it already fails; requiring it would be asking \
                                 for a bug"
                            )));
                        }
                        if !REQUIRABLE_ACTION_KINDS.contains(&kind) {
                            return Err(ArgError::Message(format!(
                                "--require {kind}: unknown action kind. Known: {}",
                                REQUIRABLE_ACTION_KINDS.join(", ")
                            )));
                        }
                        required.insert(kind.to_string());
                    }
                } else {
                    grouping = match value.as_str() {
                        "state" => Grouping::PerRequestState,
                        "none" => Grouping::WholeFile,
                        other => {
                            return Err(ArgError::Message(format!(
                                "--group-by {other}: expected 'state' or 'none'"
                            )))
                        }
                    };
                }
                i += 2;
            }
            other if other.starts_with("--") => {
                return Err(ArgError::Message(format!("unknown option: {other}")));
            }
            other => {
                if trace_path.is_some() {
                    return Err(ArgError::Message(format!(
                        "unexpected extra argument: {other} (exactly one trace path)"
                    )));
                }
                trace_path = Some(other.to_string());
                i += 1;
            }
        }
    }

    let trace_path = trace_path
        .ok_or_else(|| ArgError::Message("a trace path is required (use '-' for stdin)".into()))?;

    Ok(Args {
        trace_path,
        required,
        grouping,
        json,
    })
}

// ---- loading ----------------------------------------------------------------

/// One decoded line, carrying its 1-based file line number so a verdict can
/// point at the offending line rather than at an index inside a partition
/// nobody can see.
#[derive(Debug, Clone)]
struct Loaded {
    line_no: usize,
    step: TraceStep,
}

/// Decode JSONL. Blank lines are skipped (a trailing newline is normal);
/// anything else that fails to decode is a `malformed_step` finding, not a
/// skip — silently dropping a line the schema cannot read would delete the
/// evidence the checker exists to weigh.
fn load_trace(text: &str) -> Result<Vec<Loaded>, Failure> {
    let mut out = Vec::new();
    for (idx, line) in text.lines().enumerate() {
        let line_no = idx + 1;
        if line.trim().is_empty() {
            continue;
        }
        match serde_json::from_str::<TraceStep>(line) {
            Ok(step) => out.push(Loaded { line_no, step }),
            Err(e) => {
                return Err(Failure {
                    mode: "malformed_step",
                    detail: format!(
                        "trace line {line_no} does not decode as a TraceStep at \
                         schema_version={SCHEMA_VERSION}: {e}"
                    ),
                    group_index: None,
                    trace_line: Some(line_no),
                })
            }
        }
    }
    Ok(out)
}

// ---- checking ---------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
struct Failure {
    mode: &'static str,
    detail: String,
    group_index: Option<usize>,
    trace_line: Option<usize>,
}

fn mode_of(err: &TransitionError) -> &'static str {
    match err {
        TransitionError::IllegalTransition { .. } => "illegal_transition",
        TransitionError::StateMismatch { .. } => "state_mismatch",
        TransitionError::NonInterference { .. } => "non_interference",
        TransitionError::CoverageBreach { .. } => "coverage_breach",
    }
}

/// The step index a transition error points at, within its scenario.
/// `CoverageBreach` carries none — it is a property of the whole scenario.
fn step_index_of(err: &TransitionError) -> Option<usize> {
    match err {
        TransitionError::IllegalTransition { step_index, .. }
        | TransitionError::StateMismatch { step_index, .. }
        | TransitionError::NonInterference { step_index, .. } => Some(*step_index),
        TransitionError::CoverageBreach { .. } => None,
    }
}

/// Partition key: the tuple `check_step` compares every step against. Rendered
/// as a string so it can index a map without asking `AbstractState` for `Hash`
/// (the schema types are deliberately plain value types).
fn state_key(step: &TraceStep) -> String {
    format!(
        "{}|{}|{}",
        step.state_after.resolved_community,
        step.state_after.bound_host.0,
        step.state_after.actor.0
    )
}

#[derive(Debug)]
struct Report {
    trace_path: String,
    steps: usize,
    groups: usize,
    grouping: Grouping,
    actions_seen: BTreeMap<String, usize>,
    required: BTreeSet<String>,
    failure: Option<Failure>,
}

impl Report {
    fn conforms(&self) -> bool {
        self.failure.is_none()
    }
}

/// Split the loaded steps into scenarios according to `grouping`, preserving
/// first-seen partition order and file order within each partition.
fn partition(loaded: &[Loaded], grouping: Grouping) -> Vec<Vec<Loaded>> {
    match grouping {
        Grouping::WholeFile => {
            if loaded.is_empty() {
                Vec::new()
            } else {
                vec![loaded.to_vec()]
            }
        }
        Grouping::PerRequestState => {
            let mut order: Vec<Vec<Loaded>> = Vec::new();
            let mut index: HashMap<String, usize> = HashMap::new();
            for item in loaded {
                let key = state_key(&item.step);
                match index.get(&key) {
                    Some(at) => order[*at].push(item.clone()),
                    None => {
                        index.insert(key, order.len());
                        order.push(vec![item.clone()]);
                    }
                }
            }
            order
        }
    }
}

/// Replay a loaded trace. Returns the report; `report.failure` is `None` iff
/// the trace conforms.
fn check_loaded(
    trace_path: &str,
    loaded: Vec<Loaded>,
    required: &BTreeSet<String>,
    grouping: Grouping,
) -> Report {
    let mut actions_seen: BTreeMap<String, usize> = BTreeMap::new();
    for item in &loaded {
        *actions_seen
            .entry(item.step.action.kind().to_string())
            .or_insert(0) += 1;
    }

    // Empty file: hand it to the library so the verdict and its wording come
    // from the checker, not from here. It calls this a coverage breach —
    // "the seam was reached and emitted nothing" — and that is exactly the
    // reading a gate needs when a relay was asked to trace and produced an
    // empty file.
    if loaded.is_empty() {
        let err = check_trace(&Scenario::unstructured(Vec::new()))
            .expect_err("the checker fails closed on an empty trace");
        return Report {
            trace_path: trace_path.to_string(),
            steps: 0,
            groups: 0,
            grouping,
            actions_seen,
            required: required.clone(),
            failure: Some(Failure {
                mode: mode_of(&err),
                detail: err.to_string(),
                group_index: None,
                trace_line: None,
            }),
        };
    }

    let groups = partition(&loaded, grouping);

    let mut failure = None;
    for (gi, group) in groups.iter().enumerate() {
        let scenario = Scenario {
            trace: group.iter().map(|l| l.step.clone()).collect(),
            // Coverage is settled once, over the whole file — see the module
            // docs. Per-partition requirements would demand every request
            // emit every action.
            required_critical_actions: HashSet::new(),
        };
        if let Err(err) = check_trace(&scenario) {
            let trace_line = step_index_of(&err).and_then(|i| group.get(i).map(|l| l.line_no));
            failure = Some(Failure {
                mode: mode_of(&err),
                detail: err.to_string(),
                group_index: Some(gi),
                trace_line,
            });
            break;
        }
    }

    // Whole-run coverage. Reported as the library's own `CoverageBreach` so
    // the mode name and the "coverage breach: " prefix a reader sees are the
    // crate's, not this binary's invention.
    if failure.is_none() {
        let missing: Vec<&str> = required
            .iter()
            .map(String::as_str)
            .filter(|k| !actions_seen.contains_key(*k))
            .collect();
        if !missing.is_empty() {
            let err = TransitionError::CoverageBreach {
                detail: format!(
                    "required actions never emitted anywhere in the trace: {missing:?}"
                ),
            };
            failure = Some(Failure {
                mode: mode_of(&err),
                detail: err.to_string(),
                group_index: None,
                trace_line: None,
            });
        }
    }

    Report {
        trace_path: trace_path.to_string(),
        steps: loaded.len(),
        groups: groups.len(),
        grouping,
        actions_seen,
        required: required.clone(),
        failure,
    }
}

// ---- output -----------------------------------------------------------------

fn print_human(report: &Report) {
    println!("check-trace {}", report.trace_path);
    println!("  schema_version   {SCHEMA_VERSION}");
    println!("  steps            {}", report.steps);
    println!(
        "  groups           {}  (--group-by {})",
        report.groups,
        report.grouping.as_str()
    );
    if report.actions_seen.is_empty() {
        println!("  actions seen     (none)");
    } else {
        let rendered: Vec<String> = report
            .actions_seen
            .iter()
            .map(|(k, n)| format!("{k}={n}"))
            .collect();
        println!("  actions seen     {}", rendered.join("  "));
    }
    if report.required.is_empty() {
        println!(
            "  required         (none) — coverage-breach mode limited to impl_bug and the \
             empty trace; pass --require to assert this scenario's coverage"
        );
    } else {
        println!(
            "  required         {}",
            report
                .required
                .iter()
                .map(String::as_str)
                .collect::<Vec<_>>()
                .join(", ")
        );
    }
    match &report.failure {
        None => println!("  VERDICT          CONFORM"),
        Some(f) => {
            println!("  VERDICT          NON-CONFORMANT ({})", f.mode);
            if let Some(gi) = f.group_index {
                println!("  group            {}/{}", gi + 1, report.groups);
            }
            if let Some(line) = f.trace_line {
                println!("  trace line       {line}");
            }
            println!("  detail           {}", f.detail);
        }
    }
}

fn print_json(report: &Report) {
    let failure = match &report.failure {
        None => serde_json::Value::Null,
        Some(f) => serde_json::json!({
            "mode": f.mode,
            "detail": f.detail,
            "group_index": f.group_index,
            "trace_line": f.trace_line,
        }),
    };
    let value = serde_json::json!({
        "schema": "buzz.conformance.check-trace/v1",
        "trace_path": report.trace_path,
        "schema_version": SCHEMA_VERSION,
        "steps": report.steps,
        "groups": report.groups,
        "group_by": report.grouping.as_str(),
        "actions_seen": report.actions_seen,
        "required_critical_actions": report.required.iter().collect::<Vec<_>>(),
        "verdict": if report.conforms() { "conform" } else { "non_conformant" },
        "failure": failure,
    });
    println!(
        "{}",
        serde_json::to_string_pretty(&value).unwrap_or_else(|_| value.to_string())
    );
}

// ---- entrypoint -------------------------------------------------------------

fn read_input(path: &str) -> Result<String, String> {
    if path == "-" {
        let mut buf = String::new();
        std::io::stdin()
            .read_to_string(&mut buf)
            .map_err(|e| format!("cannot read trace from stdin: {e}"))?;
        Ok(buf)
    } else {
        std::fs::read_to_string(path).map_err(|e| format!("cannot read trace {path}: {e}"))
    }
}

fn main() -> ExitCode {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    let args = match parse_args(&argv) {
        Ok(args) => args,
        Err(ArgError::Help) => {
            print!("{HELP}");
            return ExitCode::SUCCESS;
        }
        Err(ArgError::Message(msg)) => {
            eprintln!("check-trace: {msg}");
            eprintln!();
            eprint!("{HELP}");
            return ExitCode::from(2);
        }
    };

    let text = match read_input(&args.trace_path) {
        Ok(text) => text,
        Err(msg) => {
            // Exit 2, NOT 1: an unreadable file is the absence of evidence,
            // never evidence of a violation.
            eprintln!("check-trace: {msg}");
            return ExitCode::from(2);
        }
    };

    let report = match load_trace(&text) {
        Ok(loaded) => check_loaded(&args.trace_path, loaded, &args.required, args.grouping),
        Err(failure) => Report {
            trace_path: args.trace_path.clone(),
            steps: 0,
            groups: 0,
            grouping: args.grouping,
            actions_seen: BTreeMap::new(),
            required: args.required.clone(),
            failure: Some(failure),
        },
    };

    if args.json {
        print_json(&report);
    } else {
        print_human(&report);
    }

    if report.conforms() {
        ExitCode::SUCCESS
    } else {
        if let Some(f) = &report.failure {
            eprintln!("check-trace: NON-CONFORMANT ({}): {}", f.mode, f.detail);
        }
        ExitCode::from(1)
    }
}

// -----------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use buzz_conformance::{
        AbstractState, ActorLabel, ChannelLabel, CommunityLabel, HostLabel, OpaqueId,
        SanitizedReason, TraceAction, Verdict,
    };
    use uuid::Uuid;

    fn args(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    fn cid(n: u128) -> CommunityLabel {
        CommunityLabel::from_uuid(Uuid::from_u128(n))
    }

    fn ch(n: u128) -> ChannelLabel {
        ChannelLabel(Uuid::from_u128(n))
    }

    fn state(c: CommunityLabel, actor: &str) -> AbstractState {
        AbstractState {
            resolved_community: c,
            bound_host: HostLabel("localhost:3031".into()),
            actor: ActorLabel(actor.into()),
        }
    }

    fn step(action: TraceAction, c: CommunityLabel, actor: &str) -> TraceStep {
        TraceStep::new(action, state(c, actor))
    }

    fn jsonl(steps: &[TraceStep]) -> String {
        let mut out = String::new();
        for s in steps {
            out.push_str(&serde_json::to_string(s).expect("serialize"));
            out.push('\n');
        }
        out
    }

    fn check_text(text: &str, required: &[&str], grouping: Grouping) -> Report {
        let loaded = load_trace(text).expect("well-formed fixture");
        let req: BTreeSet<String> = required.iter().map(|s| s.to_string()).collect();
        check_loaded("<test>", loaded, &req, grouping)
    }

    // ---- vocabulary tripwire ------------------------------------------------

    /// Compile-time tripwire. Adding a variant to `TraceAction` makes this
    /// match non-exhaustive, so the CLI's `--require` vocabulary cannot silently
    /// fall behind the schema. (Named `_kind` because its only job is to fail
    /// to compile.)
    fn _kind(action: &TraceAction) -> &'static str {
        match action {
            TraceAction::WriteInsert { .. } => "write_insert",
            TraceAction::WriteInsertGlobal { .. } => "write_insert_global",
            TraceAction::WriteDuplicate { .. } => "write_duplicate",
            TraceAction::SanitizedError { .. } => "sanitized_error",
            TraceAction::AuthCheck { .. } => "auth_check",
            TraceAction::ReadMessageRows { .. } => "read_message_rows",
            TraceAction::ReadByIdRows { .. } => "read_by_id_rows",
            TraceAction::ReadHostFeedRows { .. } => "read_host_feed_rows",
            TraceAction::ImplBug { .. } => IMPL_BUG_KIND,
        }
    }

    /// Every requirable kind is a real `TraceAction::kind()` string, and the
    /// only kind excluded is the breach marker.
    #[test]
    fn requirable_vocabulary_matches_the_schema() {
        let all = [
            TraceAction::WriteInsert {
                msg_id: OpaqueId("m".into()),
                channel: ch(1),
                claimed_community: None,
            },
            TraceAction::WriteInsertGlobal {
                msg_id: OpaqueId("m".into()),
                claimed_community: None,
            },
            TraceAction::WriteDuplicate {
                msg_id: OpaqueId("m".into()),
                channel: ch(1),
                claimed_community: None,
            },
            TraceAction::SanitizedError {
                reason: SanitizedReason::Invalid,
            },
            TraceAction::AuthCheck {
                channel: ch(1),
                claimed_community: None,
                verdict: Verdict::Deny,
            },
            TraceAction::ReadMessageRows {
                channel: None,
                row_communities: vec![],
            },
            TraceAction::ReadByIdRows {
                channel: None,
                row_communities: vec![],
            },
            TraceAction::ReadHostFeedRows {
                row_communities: vec![],
            },
            TraceAction::ImplBug { kind: "x".into() },
        ];
        for action in &all {
            assert_eq!(_kind(action), action.kind(), "tripwire agrees with schema");
        }
        let schema: BTreeSet<&str> = all.iter().map(|a| a.kind()).collect();
        let requirable: BTreeSet<&str> = REQUIRABLE_ACTION_KINDS.iter().copied().collect();
        let excluded: Vec<&&str> = schema.difference(&requirable).collect();
        assert_eq!(
            excluded,
            vec![&IMPL_BUG_KIND],
            "the only non-requirable kind is the breach marker"
        );
    }

    // ---- argument parsing ---------------------------------------------------

    #[test]
    fn defaults_are_group_by_state_no_requirements_human_output() {
        let a = parse_args(&args(&["trace.jsonl"])).expect("parses");
        assert_eq!(a.trace_path, "trace.jsonl");
        assert_eq!(a.grouping, Grouping::PerRequestState);
        assert!(a.required.is_empty());
        assert!(!a.json);
    }

    #[test]
    fn require_accepts_comma_lists_and_repeats_and_unions_them() {
        let a = parse_args(&args(&[
            "--require",
            "auth_check,read_message_rows",
            "--require",
            "auth_check, write_insert_global",
            "t.jsonl",
        ]))
        .expect("parses");
        assert_eq!(
            a.required.iter().map(String::as_str).collect::<Vec<_>>(),
            vec!["auth_check", "read_message_rows", "write_insert_global"]
        );
    }

    /// A typo'd kind must be a usage error, not an unsatisfiable requirement
    /// that reads as a relay bug.
    #[test]
    fn unknown_require_kind_is_a_usage_error() {
        let err = parse_args(&args(&["--require", "auth-check", "t.jsonl"])).unwrap_err();
        assert!(
            matches!(&err, ArgError::Message(m) if m.contains("unknown action kind")),
            "got {err:?}"
        );
    }

    #[test]
    fn requiring_the_breach_marker_is_rejected() {
        let err = parse_args(&args(&["--require", "impl_bug", "t.jsonl"])).unwrap_err();
        assert!(
            matches!(&err, ArgError::Message(m) if m.contains("impl_bug")),
            "got {err:?}"
        );
    }

    #[test]
    fn group_by_none_and_json_and_stdin_parse() {
        let a = parse_args(&args(&["--group-by", "none", "--json", "-"])).expect("parses");
        assert_eq!(a.grouping, Grouping::WholeFile);
        assert!(a.json);
        assert_eq!(a.trace_path, "-");
    }

    #[test]
    fn bad_group_by_missing_path_extra_path_and_unknown_flag_are_usage_errors() {
        for argv in [
            args(&["--group-by", "request", "t.jsonl"]),
            args(&[]),
            args(&["a.jsonl", "b.jsonl"]),
            args(&["--nope", "t.jsonl"]),
            args(&["--require"]),
        ] {
            assert!(
                matches!(parse_args(&argv), Err(ArgError::Message(_))),
                "expected a usage error for {argv:?}"
            );
        }
    }

    #[test]
    fn help_is_its_own_outcome() {
        assert_eq!(parse_args(&args(&["--help"])), Err(ArgError::Help));
        assert_eq!(parse_args(&args(&["-h"])), Err(ArgError::Help));
    }

    // ---- loading ------------------------------------------------------------

    #[test]
    fn blank_lines_are_skipped_and_line_numbers_stay_true_to_the_file() {
        let a = step(
            TraceAction::SanitizedError {
                reason: SanitizedReason::Invalid,
            },
            cid(1),
            "alice",
        );
        let text = format!(
            "\n{}\n\n{}\n",
            jsonl(std::slice::from_ref(&a)).trim(),
            jsonl(&[a]).trim()
        );
        let loaded = load_trace(&text).expect("loads");
        assert_eq!(loaded.len(), 2);
        assert_eq!(loaded[0].line_no, 2);
        assert_eq!(loaded[1].line_no, 4);
    }

    #[test]
    fn an_undecodable_line_is_malformed_step_not_a_skip() {
        let good = jsonl(&[step(
            TraceAction::SanitizedError {
                reason: SanitizedReason::Invalid,
            },
            cid(1),
            "alice",
        )]);
        let text = format!("{good}{{\"schema_version\":1,\"action\":\"nope\"}}\n");
        let failure = load_trace(&text).expect_err("second line must not decode");
        assert_eq!(failure.mode, "malformed_step");
        assert_eq!(failure.trace_line, Some(2));
    }

    // ---- verdicts -----------------------------------------------------------

    #[test]
    fn an_empty_trace_is_the_checkers_own_coverage_breach() {
        let report = check_text("", &[], Grouping::PerRequestState);
        assert!(!report.conforms());
        let f = report.failure.expect("failure");
        assert_eq!(f.mode, "coverage_breach");
        assert!(f.detail.contains("empty"), "detail was: {}", f.detail);
    }

    /// The shape a live relay actually produces: several actors, several
    /// communities, interleaved. Grouped by state it conforms; replayed as one
    /// scenario it does not — and the mode that bites is `state_mismatch`,
    /// which is a fact about the FILE, not about the relay. This test is the
    /// justification for `--group-by state` being the default.
    #[test]
    fn interleaved_multi_actor_trace_conforms_grouped_but_not_whole_file() {
        let alice = cid(1);
        let bob = cid(2);
        let text = jsonl(&[
            step(
                TraceAction::AuthCheck {
                    channel: ch(10),
                    claimed_community: Some(alice),
                    verdict: Verdict::Allow,
                },
                alice,
                "alice",
            ),
            step(
                TraceAction::AuthCheck {
                    channel: ch(20),
                    claimed_community: Some(bob),
                    verdict: Verdict::Allow,
                },
                bob,
                "bob",
            ),
            step(
                TraceAction::ReadMessageRows {
                    channel: Some(ch(10)),
                    row_communities: vec![alice],
                },
                alice,
                "alice",
            ),
            step(
                TraceAction::ReadMessageRows {
                    channel: Some(ch(20)),
                    row_communities: vec![bob],
                },
                bob,
                "bob",
            ),
        ]);

        let grouped = check_text(&text, &[], Grouping::PerRequestState);
        assert!(grouped.conforms(), "grouped: {:?}", grouped.failure);
        assert_eq!(grouped.groups, 2);
        assert_eq!(grouped.steps, 4);

        let whole = check_text(&text, &[], Grouping::WholeFile);
        assert!(
            !whole.conforms(),
            "whole file must trip the model bootstrap"
        );
        assert_eq!(whole.failure.expect("failure").mode, "state_mismatch");
    }

    /// A foreign row in ONE partition must still bite, and the verdict must
    /// cite the file line — not the index inside a partition the reader cannot
    /// see. This is the mode grouping must never weaken.
    #[test]
    fn a_foreign_row_in_a_later_group_bites_and_cites_its_file_line() {
        let alice = cid(1);
        let bob = cid(2);
        let text = jsonl(&[
            step(
                TraceAction::ReadMessageRows {
                    channel: Some(ch(10)),
                    row_communities: vec![alice],
                },
                alice,
                "alice",
            ),
            step(
                TraceAction::AuthCheck {
                    channel: ch(20),
                    claimed_community: Some(bob),
                    verdict: Verdict::Allow,
                },
                bob,
                "bob",
            ),
            step(
                TraceAction::ReadMessageRows {
                    channel: Some(ch(20)),
                    // bob's request leaking one of alice's rows
                    row_communities: vec![bob, alice],
                },
                bob,
                "bob",
            ),
        ]);
        let report = check_text(&text, &[], Grouping::PerRequestState);
        let f = report.failure.expect("must not conform");
        assert_eq!(f.mode, "non_interference");
        assert_eq!(f.group_index, Some(1), "bob's partition is the second");
        assert_eq!(f.trace_line, Some(3), "the leaking line in the FILE");
    }

    #[test]
    fn an_impl_bug_step_is_a_coverage_breach() {
        let text = jsonl(&[step(
            TraceAction::ImplBug {
                kind: "ingest_event_exited_without_trace".into(),
            },
            cid(1),
            "alice",
        )]);
        let report = check_text(&text, &[], Grouping::PerRequestState);
        assert_eq!(report.failure.expect("failure").mode, "coverage_breach");
    }

    #[test]
    fn an_allow_with_a_foreign_claim_is_an_illegal_transition() {
        let text = jsonl(&[step(
            TraceAction::AuthCheck {
                channel: ch(10),
                claimed_community: Some(cid(999)),
                verdict: Verdict::Allow,
            },
            cid(1),
            "alice",
        )]);
        let report = check_text(&text, &[], Grouping::PerRequestState);
        assert_eq!(report.failure.expect("failure").mode, "illegal_transition");
    }

    /// Coverage is a property of the run, not of each partition: a required
    /// action satisfied only by a DIFFERENT actor's request still counts.
    #[test]
    fn coverage_is_settled_over_the_whole_run_not_per_partition() {
        let alice = cid(1);
        let bob = cid(2);
        let text = jsonl(&[
            step(
                TraceAction::AuthCheck {
                    channel: ch(10),
                    claimed_community: Some(alice),
                    verdict: Verdict::Allow,
                },
                alice,
                "alice",
            ),
            step(
                TraceAction::ReadMessageRows {
                    channel: None,
                    row_communities: vec![bob],
                },
                bob,
                "bob",
            ),
        ]);
        let ok = check_text(
            &text,
            &["auth_check", "read_message_rows"],
            Grouping::PerRequestState,
        );
        assert!(ok.conforms(), "{:?}", ok.failure);

        let missing = check_text(&text, &["write_insert"], Grouping::PerRequestState);
        let f = missing.failure.expect("must not conform");
        assert_eq!(f.mode, "coverage_breach");
        assert!(
            f.detail.contains("write_insert"),
            "the breach must name the missing action, got: {}",
            f.detail
        );
    }

    /// Equivalence pin: with a single partition, this binary's whole-run
    /// coverage check and `check_trace`'s own per-scenario one must agree —
    /// both on the pass and on the breach. If the library changes its coverage
    /// rule, this test is what notices.
    #[test]
    fn global_coverage_matches_check_trace_on_a_single_group() {
        let c = cid(7);
        let trace = vec![
            step(
                TraceAction::AuthCheck {
                    channel: ch(10),
                    claimed_community: Some(c),
                    verdict: Verdict::Allow,
                },
                c,
                "solo",
            ),
            step(
                TraceAction::WriteInsert {
                    msg_id: OpaqueId("abc".into()),
                    channel: ch(10),
                    claimed_community: Some(c),
                },
                c,
                "solo",
            ),
        ];
        let text = jsonl(&trace);

        for (required, expect_ok) in [
            (vec!["auth_check", "write_insert"], true),
            (vec!["read_message_rows"], false),
        ] {
            let via_cli = check_text(&text, &required, Grouping::PerRequestState);
            assert_eq!(via_cli.groups, 1, "fixture must be a single partition");

            let via_lib = check_trace(&Scenario {
                trace: trace.clone(),
                required_critical_actions: required.iter().map(|s| s.to_string()).collect(),
            });

            assert_eq!(
                via_cli.conforms(),
                via_lib.is_ok(),
                "CLI and library disagreed for required={required:?}"
            );
            assert_eq!(via_cli.conforms(), expect_ok);
            if let (Some(f), Err(e)) = (&via_cli.failure, &via_lib) {
                assert_eq!(f.mode, mode_of(e), "same failure mode");
            }
        }
    }

    /// The committed positive fixture must replay green through this binary
    /// too — the CLI and the library must not diverge on the crate's own
    /// golden trace.
    #[test]
    fn the_committed_good_fixture_replays_green() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests")
            .join("fixtures")
            .join("good.jsonl");
        let text = std::fs::read_to_string(&path).expect("good.jsonl readable");
        let report = check_text(
            &text,
            &["auth_check", "write_insert", "read_message_rows"],
            Grouping::PerRequestState,
        );
        assert!(report.conforms(), "{:?}", report.failure);
        assert_eq!(report.steps, 3);
        assert_eq!(report.groups, 1);
    }

    /// …and the committed adversarial fixtures must each replay red, with the
    /// mode the crate's own suite asserts. This is what keeps the shell
    /// entrypoint honest: `gate-conformance.sh` phase A proves the library
    /// bites, and this proves the binary the gate actually invokes bites the
    /// same way.
    #[test]
    fn the_committed_bad_fixtures_replay_red_with_the_expected_modes() {
        for (name, expected) in [
            ("bad_host_channel_mismatch.jsonl", "illegal_transition"),
            ("bad_foreign_row_leak.jsonl", "non_interference"),
            ("bad_coverage_breach.jsonl", "coverage_breach"),
        ] {
            let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("tests")
                .join("fixtures")
                .join(name);
            let text = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{name}: {e}"));
            let report = check_text(&text, &[], Grouping::PerRequestState);
            let f = report
                .failure
                .unwrap_or_else(|| panic!("{name} must not conform"));
            assert_eq!(f.mode, expected, "{name}");
        }
    }
}
