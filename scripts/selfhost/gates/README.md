# Staging promotion gates

The pipeline between "a commit exists" and "the deployer may ship it".

It takes candidate artifacts, stands them up against an **isolated** harness
stack, runs four gates in order of cheapness, and writes a hash-bound verdict to
`promote-stamp.json`. That file is the deployer's input; nothing else here is.

```
./scripts/selfhost/gates/run-gates.sh all                  # dry run (default)
./scripts/selfhost/gates/run-gates.sh all --execute
./scripts/selfhost/gates/run-gates.sh tests --execute      # one gate
./scripts/selfhost/gates/run-gates.sh soak --execute --soak-duration 14400
./scripts/selfhost/gates/run-gates.sh teardown --execute   # panic button
```

`--dry-run` is the default and prints the complete plan. Gates 3 and 4 build
binaries and start containers; a runner that did that on a bare invocation would
be a footgun on a box that also hosts live stacks.

---

## The gates

Ordered by cheapness, so a compile error costs seconds rather than a container
bring-up and a five-minute soak.

### 1. `tests` — the suites are green

**Proves.** The candidate's own suites pass on this host:

| step | command |
|---|---|
| A | `cargo check -p buzz-core -p buzz-sdk -p buzz-cli -p buzz-acp --all-targets` |
| B | `cargo test` on the same four, `--no-fail-fast` (doctests included) |
| C | `cargo check -p buzz-relay --lib` |
| D | `pnpm typecheck` (desktop) |
| E | `pnpm test` (desktop) |

**Does not prove.** *Not a full-workspace green.* `buzz-relay`'s **test targets**
pull OpenSSL through dev-dependencies and this host has no OpenSSL development
headers (`/usr/include/openssl/ssl.h` absent, `pkg-config --exists openssl`
fails), so `cargo test --workspace` cannot run here at all. Step C type-checks
the relay library and binaries so a candidate that does not *compile* is still
caught — but **zero relay unit tests execute in this pipeline**. That coverage
has to come from CI on a host with the headers, and until it does, gate 1's
signal about the relay is "it compiles", nothing more. Also out of scope: the
desktop Playwright/e2e suites, and anything requiring a live relay (gates 2–4).

**Waivers.** See below.

### 2. `conformance` — TLA+ trace replay

The north star, from `crates/buzz-conformance/src/lib.rs:6`: *don't ask "did the
model pass"; ask "did the running code emit a trace the model accepts."*

**Phase A — checker integrity. REAL, runs today.**
`cargo test -p buzz-conformance --all-targets`. Not decorative: the crate ships
adversarial fixtures that assert `check_trace` **fails** on a host/channel fence
skip (`bad_host_channel_mismatch.jsonl` → `IllegalTransition`), a foreign-row
leak (`bad_foreign_row_leak.jsonl` → `NonInterference`) and a coverage breach
(`bad_coverage_breach.jsonl` → `CoverageBreach`), plus proptests over the
checker. Phase A is what catches a checker that has been quietly reduced to
`Ok(())`. `buzz-conformance` depends on no production buzz crate (its
`Cargo.toml` "Independence rule"), so the OpenSSL gap does not touch it.

**Phase B — live trace replay. REAL.**
Stands up the isolated harness, runs the **candidate relay from source** with
`BUZZ_CONFORMANCE_TRACE_PATH` pointed at a file in the run's evidence directory,
drives the shared workload's write/read half against it, stops the relay with
`SIGTERM`, and replays the JSONL the relay actually wrote through `check-trace`.

| step | what it does |
|---|---|
| bring-up | `buzz-gates` compose project, schema reset, community seeded at `localhost:3031` |
| trace on | relay started with `BUZZ_CONFORMANCE_TRACE_PATH=<evidence>/relay-trace.jsonl` |
| workload | `workload_announce_repo` (kind:30617 write) → `workload_open_issue` (kind:1621 write) → `workload_read_history` ×2 identities (REQ reads) — all from `lib/workload.sh`, the same functions gates 3 and 4 use |
| stop | `harness_relay_stop` (SIGTERM); `JsonlTracer` flushes per line so a clean stop leaves no partial line |
| replay | `check-trace --group-by state --require write_insert_global,read_message_rows <trace>` |

The two blockers this gate used to report are closed:

> **B1 — runtime tracer binding.** `Config::from_env` parses
> `BUZZ_CONFORMANCE_TRACE_PATH` (`crates/buzz-relay/src/config.rs`) and
> `AppState::new` binds `JsonlTracer` through
> `crate::conformance::tracer_for_trace_path`
> (`crates/buzz-relay/src/conformance/tracers.rs`). **Unset — the production
> default — still binds `NoopTracer` and opens no file.** A path that cannot be
> opened aborts startup rather than downgrading to the no-op: a relay that was
> asked to trace and silently did not would hand this gate an empty file.
>
> **B2 — replay entrypoint.** `crates/buzz-conformance/src/bin/check-trace.rs`.
> Reads a JSONL trace (or `-` for stdin), replays it through `check_trace`,
> prints a structured summary (`--json` for the machine-readable form), and
> exits **0** conform / **1** non-conformant / **2** could-not-read. Exit 1 and
> 2 are never conflated — a typo'd path must not read as a spec violation.

`probe_emission_wiring` is still there and still runs first, but **inverted**: it
now asserts the wiring exists and fails the gate, with a full-width banner naming
the missing file, if any of it has gone away. It also refuses the *half*-wired
state — a tree that parses the variable but still hardcodes `NoopTracer` in
`state.rs` is called out by name. The runtime counterpart is the trace file's own
existence: a relay that ignored the variable leaves no file, and phase B fails
`no-trace-file` rather than replaying nothing and calling it clean.

**What phase B does not do.** It does **not** start `buzz-acp`. The seams the
trace schema covers are the relay's ingest and read paths (`handlers/ingest.rs`,
`handlers/req.rs` are the only emit sites), and booting the ACP stub would give
this gate a second, unrelated way to go red while proving nothing about
`MultiTenantRelay.tla`. The agent lifecycle is gates 3 and 4. It is also **not a
proof**: trace conformance judges only the executions actually run, and this is
one repo announce, one issue and two history reads. Widening it means widening
`lib/workload.sh`, which is shared on purpose.

**One mode is weaker than it looks.** Under the default `--group-by state`,
`check-trace` partitions the file by `state_after` — because a live trace is many
requests from many actors, and `check_trace` bootstraps one model state per
scenario. That keeps `non_interference`, `illegal_transition` and
`coverage_breach` fully live, but makes `state_mismatch` unreachable *within* a
partition (the partition key is the tuple that check compares). That mode is
carried by phase A's fixtures. `--group-by none` replays the whole file as one
scenario and restores it, and is correct for a single-request trace.

**So, plainly:** a `pass` now means both halves held — the checker still bites,
**and** this candidate relay emitted a trace it accepts. `gates[].details.phase_b`
carries the trace's `sha256` and byte count plus the full replay report, so the
claim is re-checkable against the file in the evidence directory.

### 3. `skew` — version-skew matrix

A rolling deploy is never atomic. For some window, a candidate relay talks to
deployed agents and vice versa.

| pairing | relay | acp |
|---|---|---|
| A | candidate — staged `buzz-relay` (built from source) | deployed — `/opt/buzz/bin/buzz-acp` |
| B | deployed — `buzz-local:unified-13acbaf2` | candidate — staged `buzz-acp` |

**Proves,** per pairing: boots → connects → discovers → enrols on a **fresh**
root → answers one comment → clean shutdown, with `dropped=0` and no `ERROR`
lines. Asserted on log markers, never on a human reading output:

| assertion | marker | source |
|---|---|---|
| connects | `connected to relay at` | `crates/buzz-acp/src/lib.rs:1925` |
| discovers channels | `discovered N channel(s)` | `lib.rs:2053` |
| discovers repo | `discovered repository` | `lib.rs:4695` |
| enrolment backfill | `enrolment history reconstruction complete` | `relay.rs:6439` |
| fresh-root enrol | `root history reconstruction complete` | `relay.rs:6441` |
| clean shutdown | `buzz-acp stopped` | `lib.rs:3641` |
| no dropped work | every `dropped=`/`dropped_total=` is 0 | `lib.rs:887,:985`, `relay.rs:1611,:1645` |

The workload uses an **explicit `@mention`** plus the `p`-tag, because an unknown
root only enrols on `Addressing::ExplicitMention`
(`crates/buzz-acp/src/project.rs:7892` `wake_or_enrol`; token matcher
`project.rs:7134`). A structural `p`-tag alone is deliberately ignored by the
harness — a workload that only `p`-tagged would prove nothing while appearing to
pass.

**Does not prove.** *Not agent quality*: the turn is answered by
`acp-stub-agent.mjs`, a deterministic stub (see below). *Not the full protocol
surface*: one repo, one issue, one turn — a skew bug in media upload or voice
sails straight through. *Not candidate×candidate* (that is gates 1/2/4). *Not
rollback*: nothing here shows the deployed version can read what the candidate
wrote and then be rolled back.

### 4. `soak` — sustained load

The same workload looped for a wall-clock duration, candidate × candidate.
Asserts every iteration completes, no `ERROR` lines accumulate, every `dropped=`
counter stays 0, and samples RSS per iteration into `rss.csv`.

**The default is 300s and that is a smoke soak, not a release soak.** It catches
fast leaks, immediate handle exhaustion and reconnect storms. It does *not* catch
slow leaks, daily-cycle effects, log rotation or disk fill. A real pre-release
soak runs for hours: `--soak-duration 14400`. The stamp records the duration
actually used and classifies it (`smoke` / `extended` / `release-grade`), so a
five-minute run can never be mistaken for an overnight one — read
`gates[].details.duration_s` before trusting a soak result.

**Does not prove** anything about concurrency: this is one agent taking one turn
at a time. It is a duration test, not a load test. No RSS threshold is enforced —
a threshold picked without a baseline is a flaky test waiting to happen, so the
numbers are recorded for a human instead.

---

## Waivers — `waivers.txt`

**Green means green.** The branch may carry known-red tests; the runner will not
special-case them silently. `waivers.txt` is the only way a red test does not
block, and every waiver is:

* printed in a full-width banner naming the test, the reason and the date, on
  every run;
* recorded in the stamp under `gates[].details.waivers.applied`;
* reflected in the verdict, which downgrades from `promotable` to
  **`promotable_with_waivers`** — a distinct string the deployer must have a
  policy for, never treated as equal to `promotable`.

Stale waivers (declared but matching no failure this run) are reported so dead
lines get deleted. **The goal state of the file is empty**; it is seeded with the
two in-flight `buzz-acp` mention-tokenizer tests and nothing else.

Compile and type errors are **never** waivable. A waiver for "it does not build"
is not a waiver.

Format: `<test-id> | <reason> | <YYYY-MM-DD>`. See the file's own header.

---

## The stamp — `promote-stamp.json`

```jsonc
{
  "schema": "buzz.staging.promote-stamp/v1",
  "run_id": "20260804T170000Z-0fa54b3c",
  "candidate": {
    "source_commit": "…", "source_branch": "…",
    "worktree_dirty": false,
    "build_input_digest": "…",          // sha256 over build-relevant tracked files
    "cargo_profile": "ci",
    "taken_at": "…",
    "artifacts": [                       // STAGED copies, not target/ — see below
      { "role": "relay", "path": "/tmp/buzz-gates/<run_id>/artifacts/buzz-relay",
        "sha256": "…", "bytes": 0, "present": true },
      { "role": "acp",   "path": "…/artifacts/buzz-acp", "sha256": "…", … },
      { "role": "cli",   "path": "…/artifacts/buzz",     "sha256": "…", … }
    ]
  },
  "baseline": {                          // the "deployed" side of the skew matrix
    "artifacts": [
      { "role": "acp",   "kind": "file",  "ref": "/opt/buzz/bin/buzz-acp", "sha256": "…" },
      { "role": "relay", "kind": "image", "ref": "buzz-local:unified-13acbaf2",
        "image_id": "sha256:…" }
    ]
  },
  "binding": {                           // lock-vs-verify drift report
    "bound": true,
    "commit_changed": false, "commit_before": "…", "commit_after": "…",
    "tree_digest_changed": false,
    "artifact_drift": []
  },
  "gates": [
    { "name": "tests", "result": "pass", "duration_s": 126,
      "started_at": "…", "evidence": "/tmp/buzz-gates/<run_id>/tests",
      "details": { … } }
    // … conformance, skew, soak
  ],
  "stamped_at": "…",
  "verdict": "promotable",
  "verdict_reason": "…",
  "promoted_by": { "runner": "scripts/selfhost/gates/run-gates.sh",
                   "host": "…", "run_dir": "…" }
}
```

### Verdicts

| verdict | meaning |
|---|---|
| `promotable` | every gate passed, no waivers, artifacts bound |
| `promotable_with_waivers` | as above, but waivers were applied — needs a deployer policy |
| `blocked` | at least one gate failed |
| `refused` | artifacts or `HEAD` moved mid-run — **never deploy**, regardless of gate results |
| `incomplete` | a gate produced no result, was dry-run only, or reported `blocked` (it could not run for an environmental reason) |

A gate result that is neither `pass` nor one of the states named above also lands
on `incomplete` rather than falling through to `promotable`. "We could not check"
must never be readable as "we checked".

### Hash binding

`run-gates.sh` writes `candidate-lock.json` **before** the first gate.
`stamp.sh` recomputes the same description afterwards and compares.

* **Hard (refuses the stamp):** any artifact `sha256` changed, any artifact
  disappeared, or `HEAD` moved. These mean the bytes the gates exercised are not
  the bytes the deployer would ship.

### Staged artifacts — why the candidate is copied out of `target/`

`run-gates.sh --execute` builds the candidate, then **copies** the binaries to
`<run_dir>/artifacts/` and makes them read-only. The lock hashes those copies
and the gates *execute* those copies (`--no-build` reuses whatever is staged).

This is not ceremony. Hashing `target/` directly does not work, and the reason is
worth knowing:

> Cargo computes **feature unification over the selected package set**. So
> `cargo build -p buzz-relay -p buzz-acp -p buzz-cli` (the pre-lock build) and
> `cargo test -p buzz-core -p buzz-sdk -p buzz-cli -p buzz-acp` (gate 1) each
> rewrite `target/<profile>/buzz-acp` with **different bytes** — because
> `buzz-relay` being in one selection and not the other changes which features
> shared dependencies get built with. Observed live on this branch: `79dadf79…`
> immediately after the build, `a93fc22c…` after gate 1, with no source change
> and `HEAD` unmoved.

Hashing `target/` therefore measures *what cargo most recently felt like
emitting*, not *what was tested* — and the stamp refused two consecutive valid
runs before this was understood. Staging freezes the candidate: nothing cargo
does afterwards can touch it, so any later hash change is genuine drift, which is
what the refusal is actually for.
* **Advisory (recorded + warned):** worktree dirtiness and `build_input_digest`
  drift. A source edit that never made it into a rebuild cannot invalidate a test
  run of the old binary — but the deployer deserves to see it happened. This tier
  exists because this repo is a *shared worktree* with several agents editing
  concurrently; making source churn hard-refuse would make every stamp refused
  and the tool useless.

---

## Handoff to the deployer

Checked against the deployer agent's actual files as of `scripts/selfhost/`
(`selfhost-release.schema.json`, `mint-manifest.py`, `deploy.sh`).

### `promoted_by` is NOT the seam — do not wire the stamp into it

The manifest's `promoted_by` is a **reserved Phase-2 owner-approval slot**. The
schema requires it, requires it to be `null` in Phase 1, and `mint-manifest.py`
**rejects a non-null value** (`mint-manifest.py:292`) rather than ignoring it —
deliberately, because "a manifest that claims an approval nothing verifies is
strictly worse than a manifest that claims nothing." Its anticipated Phase-2
shape is `{kind: "owner-approval", pubkey, event, sig}`: a signature, not a gate
result.

So: **a gate stamp must not be stuffed into `promoted_by`.** Doing so would make
every minted manifest fail validation.

### The seam that does work today: `source_commit`, out of band

`source_commit` is required on the manifest and the deployer already **refuses
to run unless the build repo's HEAD equals it**. That is the join key. No schema
change is needed for the deployer to consume this stamp:

1. Read `scripts/selfhost/gates/promote-stamp.json`.
2. Refuse unless `verdict == "promotable"` (or `promotable_with_waivers` under an
   explicit policy). Treat `verdict` as a **closed enum** — never
   `startswith("promotable")`, which would also accept a future
   `promotable_but_expired`.
3. Refuse unless `stamp.candidate.source_commit == manifest.source_commit`. This
   is what stops a stamp for one commit authorising a manifest for another.
4. Refuse unless `stamp.binding.bound == true`.
5. Re-verify the hashes it can:

   | manifest | stamp | join |
   |---|---|---|
   | `components.acp.sha256` | `candidate.artifacts[] role=="acp" .sha256` | **exact sha256** |
   | `components.cli.sha256` | `candidate.artifacts[] role=="cli" .sha256` | **exact sha256** |
   | `components.relay-image.image_id` | `candidate.artifacts[] role=="relay"` | **no hash join — see below** |

`components.acp` / `components.cli` are `{artifact, sha256, bytes}`, the same
shape this stamp already emits, so those two join exactly.

### Known gap: the relay is gated as a binary, shipped as an image

`components.relay-image` carries `{image, image_id}` — a Docker image. This
pipeline builds and gates a **from-source relay binary**
(`target/<profile>/buzz-relay`, staged). Those are different bytes, so
`candidate.artifacts[role=="relay"].sha256` **cannot** be checked against
`image_id`. For the relay, the two sides are joined only by `source_commit`.

That is a real weakening of the guarantee and should be closed one of two ways:

* **preferred** — have the gates build and exercise the relay *image*
  (`buzz-local:<tag>` via the repo `Dockerfile`), so the artifact gated is the
  artifact shipped; the stamp then carries `image_id` and joins exactly; or
* have the deployer accept `source_commit` as the relay's only binding and say so
  explicitly in its journal, rather than implying image-level verification.

Until then, do not describe the relay half of this stamp as hash-bound to the
deployed image. It is bound to a binary built from the same commit.

### If a first-class field is wanted

The cleanest schema addition would be an optional
`staging_stamp: {schema, run_id, verdict, stamped_at, source_commit}` sibling of
`promoted_by` — distinct from it, because *"the gates passed"* and *"a human with
a key approved this"* are different claims and collapsing them is how an audit
log starts lying. That is the deployer agent's call; this runner does not own
`scripts/selfhost/` outside `gates/`.

`promote-stamp.json` is written into this directory and is **untracked**. Adding
it to `.gitignore` is the orchestrator's call — this runner does not own files
outside `scripts/selfhost/gates/**`.

---

## Isolation and teardown guarantees

* Compose project is **`buzz-gates`**, never `buzz-harness` (a sibling
  worktree's stack, routinely up for days), never `buzz-prod`, never `buzz`.
  The forbidden list is enforced by `harness_guard` (`lib/harness.sh`), which
  hard-refuses by name — crude, but the failure that actually happens is a
  `--project-name` typo, and its blast radius is a production outage.
* Ports shift off the harness block so both coexist: postgres `5473`, redis
  `6473`, minio `9473/9474`, relay `3031`, health `8089`, metrics `9203`
  (`docker-compose.gates.yml`, layered over the repo's
  `docker-compose.harness.yml` — same topology, only host ports differ).
* Named volumes need no overlay: Compose namespaces them per project.
* **Teardown is a trap, not a documented follow-up command.** `harness_arm_teardown`
  installs `EXIT INT TERM`; `harness_down` runs `down -v --remove-orphans`, kills
  the relay and every registered acp pid, and then **verifies** with
  `harness_assert_torn_down` (zero containers carrying our compose project
  label). A gate that dies mid-assertion still leaves nothing behind.
* `--no-teardown` exists for debugging and announces itself loudly.
* Nothing reads or writes `/opt/buzz/keys`, `/etc`, systemd, or the `buzz-prod`
  stack. The deployed artifacts are **read-only inputs**: hashed, executed inside
  the harness, never modified.
* Keys are minted per run via `buzz-admin generate-key`
  (`crates/buzz-admin/src/main.rs:132`) and never persisted.

### Two host-specific deviations

* **tmux is not installed here**, so the relay is daemonised with `setsid`
  rather than the tmux session `scripts/start-isolated-test-relay.sh:130-133`
  uses. Same problem solved (the ephemeral invoking shell's process group is
  reaped and would `SIGTERM` a foreground relay), different tool.
* **`psql` is not on the host PATH**, so schema/seed work goes through
  `docker compose exec -T postgres psql`. `setup-desktop-test-data.sh` already
  supports this via `BUZZ_DB_DOCKER_CONTAINER`.

---

## The stub agent — `acp-stub-agent.mjs`

`buzz-acp` runs the ACP `initialize` handshake with an agent subprocess before it
will connect to the relay at all (`crates/buzz-acp/src/lib.rs:8578`
`initialize_agent_pool`, called during boot at `lib.rs:1887`). Its default
`--agent-command` is `goose`, which is **not installed on this host** and is a
real LLM: non-deterministic, rate-limited, billable. A promotion gate that
depends on an LLM answering correctly is not a gate, it is a coin flip.

So gates 3 and 4 point `BUZZ_ACP_AGENT_COMMAND` at the stub, which speaks NDJSON
JSON-RPC 2.0 over stdio and implements exactly `initialize`, `session/new`,
`session/prompt` (streaming one `agent_message_chunk`, then
`stopReason: "end_turn"`) and `session/cancel`, answering any other request with
`{}`. **Agent intelligence is explicitly not under test**; the harness's
connect / discover / enrol / reply / shutdown path is.

---

## Layout

| path | role |
|---|---|
| `run-gates.sh` | orchestrator + subcommands |
| `gate-tests.sh` `gate-conformance.sh` `gate-skew.sh` `gate-soak.sh` | one gate each, individually runnable |
| `stamp.sh` | collects `result.json` files, re-hashes, writes the stamp. Runs no tests itself — a stamper that could also produce evidence could produce evidence for a stamp nobody ran |
| `waivers.txt` | the debt ledger |
| `acp-stub-agent.mjs` | deterministic ACP agent |
| `docker-compose.gates.yml` | port overlay + the `deployed-relay` profile |
| `lib/common.sh` | logging, dry-run plumbing, `record_result` |
| `lib/candidate.sh` | artifact identification, staging, and hash binding |
| `lib/harness.sh` | isolated stack lifecycle + marker assertions |
| `lib/waivers.sh` | waiver parsing/classification, failure extraction |
| `lib/workload.sh` | the one synthetic scenario, shared by conformance, skew and soak |

Gate 2 also depends on two things outside this directory, both of which its
probe asserts before running:

| path | role |
|---|---|
| `crates/buzz-relay/src/conformance/tracers.rs` | `tracer_for_trace_path` — the `BUZZ_CONFORMANCE_TRACE_PATH` switch. `None` ⇒ `NoopTracer`, unchanged production behaviour |
| `crates/buzz-conformance/src/bin/check-trace.rs` | the replay entrypoint phase B invokes |

Each gate writes exactly one `result.json` into its evidence directory. That file
is the **only** contract between a gate and `stamp.sh`; no verdict is ever
derived by scraping pretty console output.

Evidence defaults to `/tmp/buzz-gates/<run_id>/` (override with
`--evidence-root`), deliberately outside the repo tree. The staged candidate
lives at `<run_id>/artifacts/` — keep it as long as you want the stamp to be
re-verifiable.

---

## What has actually been executed

Honesty about this runner's own verification, as of the commit that introduced
it:

* **Gate 1 — run for real, green.** Desktop `pnpm test`: **4288/4288 pass**, 0
  fail, ~89s. `pnpm typecheck`: clean. Rust subset: `cargo check --all-targets`
  and `cargo check -p buzz-relay --lib` clean; `cargo test` red on exactly the
  two seeded waivers and nothing else, so the gate reports **PASS with both
  waivers applied and zero stale**.
* **Gate 1 also caught two real things**, which is the best evidence it works:
  1. A **mid-edit compile break** in `crates/buzz-acp/src/lib.rs` from a sibling
     agent sharing this worktree — with the reported line number *moving between
     two runs seconds apart*. The gate refused to call it green and correctly
     marked the waivers stale (compilation never got far enough to run tests, so
     no failure matched them).
  2. The **hash-binding refusal firing for real, twice**: gate 1's `cargo test`
     rewrote `target/debug/buzz-acp` with different bytes than the pre-lock
     `cargo build` produced, so the stamp came out `refused`. Root cause was
     cargo feature unification across differing package selections, not a source
     change — which is why the candidate is now **staged out of `target/`**
     before locking. After that fix the same run stamps
     `promotable_with_waivers` with `binding.bound: true` and zero drift.
* **Gate 2 — both phases run for real, green.** Phase A: 41 tests across the
  checker's unit suite, the `check-trace` binary's own suite, the proptests and
  the replay fixtures — including `bad_host_channel_mismatch_is_illegal_transition`,
  `foreign_row_leak_is_non_interference` and `coverage_breach_is_caught`.
  Phase B, live on this host: harness up → candidate relay started with
  `BUZZ_CONFORMANCE_TRACE_PATH` → workload → clean stop → replay.
  **5 trace steps, 2 partitions, `write_insert_global=3 read_message_rows=2`,
  verdict CONFORM, 81s**, zero relay `ERROR` lines, teardown verified at zero
  containers with `buzz-harness` (up 44h) and `buzz-prod` untouched throughout.
* **Gate 2's coverage requirement caught a real gap on its first live run**,
  which is the best evidence it is not decorative. The first run captured
  **two writes and zero reads** and went red on
  `coverage breach: required actions never emitted anywhere in the trace:
  ["read_message_rows"]`. Cause: the `buzz` CLI reads through the relay's HTTP
  bridge (`POST /query`), which is a *different* read path from the WebSocket
  `handle_req` where the trace's read emit sites live. Without the requirement
  the run would have passed on a trace with no read observations at all — which
  would have meant `Inv_NonInterference`, the invariant this whole gate exists
  for, was never once evaluated. Fixed by driving the traced path with
  `buzz-test-cli` (`workload_read_subscription`). A follow-up refinement for the
  same reason: the workload now also posts a kind:1 comment on the root, so the
  read observations come back with **rows to confine** instead of
  `row_communities: []`, which would have satisfied the invariant vacuously.
* **The relay's trace switch was exercised both ways on a live relay**, against
  this same isolated harness. Unset: relay boots, serves a write, logs no
  `ERROR`, and **no `.jsonl` is created anywhere** — the no-op tracer opens
  nothing, which is the production default. Pointed at an unopenable path: the
  relay **refuses to start**, panicking in `AppState::new` after the DB and
  Redis connect but before it serves anything, with
  `BUZZ_CONFORMANCE_TRACE_PATH=…: cannot open the conformance trace file for
  writing (…). Refusing to start: a relay asked to emit a conformance trace that
  cannot write one would run untraced and hand its gate an empty file.`
* **Gates 3 and 4 — implemented, NOT yet run green end to end on this host.**
  The marker tables above are derived from source, not from an observed
  transcript. Treat the first live run as a debugging session, not as a verdict.
  The `deployed-relay` compose service and the ACP stub are likewise unexercised
  against a live relay.
* **Isolation and teardown — verified.** `buzz-gates` brought up (postgres 5473,
  redis 6473, minio 9473/9474), then torn down via `run-gates.sh teardown
  --execute`, leaving **zero containers**, with `buzz-harness` (up 43h) and the
  `buzz-prod` stack untouched throughout. `harness_guard` was confirmed to exit
  2 rather than act when handed `--project-name buzz-harness` or `buzz-prod`.
* **`bash -n`** clean on all 10 shell files. **`shellcheck` is not installed on
  this host** (no system package, no hermit package, no `node_modules` copy), so
  the scripts have **not** been shellcheck-clean-verified — worth running in CI
  where it is available.
