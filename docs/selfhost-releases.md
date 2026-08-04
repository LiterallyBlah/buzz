# Self-hosted Buzz releases

How a change gets from an agent's worktree onto the private Buzz box, and why
the pipeline is shaped the way it is.

The situation is unusual and the whole design follows from it: **Buzz's agents
develop Buzz, on the machine that runs Buzz.** An agent editing `buzz-acp` is
editing the binary it is currently executing. Without a deliberate separation,
the reasonable-sounding act of "restart to pick up my fix" is an agent choosing
to replace itself, mid-thought, with something nothing has tested.

So the deployer is out of band. It is the one component that swaps binaries and
images, it is never the thing being replaced, and agents never touch live state.

* Manifest format: [`scripts/selfhost/release-manifest.md`](../scripts/selfhost/release-manifest.md)
* Executor: [`scripts/selfhost/deploy.sh`](../scripts/selfhost/deploy.sh)
* Minter: [`scripts/selfhost/mint-manifest.py`](../scripts/selfhost/mint-manifest.py)
* Units and install steps: [`scripts/selfhost/units/README.md`](../scripts/selfhost/units/README.md)

## Separation of powers

Four roles. No principal holds two of them, and each hand-off is a document
rather than a conversation.

| Role | Who | Produces | Cannot |
|---|---|---|---|
| **Author** | agents, humans | commits, built artifacts, a release manifest | touch live state, approve their own release |
| **Judge** | the staging gates (`scripts/selfhost/gates/`) | a promotion stamp: evidence that the candidate passed | decide whether to ship |
| **Approver** | the owner | an authorization on a specific manifest | perform the deploy |
| **Executor** | `deploy.sh`, run by `buzz-deployer.service` | a swap, a verdict, a rollback | choose what to deploy, or where a binary goes |

The executor is deliberately the dumbest of the four. It does not decide what
should ship; it decides whether what it was handed is what it says it is, and
whether the box is still healthy afterwards. That narrowness is what makes it
safe to give it root.

Two rules make the separation real rather than decorative:

1. **A document cannot be its own authorisation.** The manifest is authored by
   whoever built the release. So every acknowledgement — `--ack-migrations`,
   `--ack-waivers`, `--allow-unstamped`, `--allow-unattributed` — is a
   command-line flag, not a manifest field, and the systemd unit passes none of
   them. An unattended drop can never soften its own requirements.
2. **An unverified claim is refused, not ignored.** Both promotion claims are
   re-derived from something outside the manifest: the promote stamp is
   re-hashed and re-joined from the file staged beside it, and `promoted_by` is
   re-asked of the live relay. A manifest asserting an approval nothing checked
   would read like authority in an audit log, which is worse than asserting
   nothing — so nothing is taken on the manifest's word.

## What travels with a release, and who is making the claim

Two fields, two different claims, deliberately not merged. Collapsing them is
how an audit log stops being able to say which one was actually made.

| Field | The claim | Made by | Re-derived from |
|---|---|---|---|
| `staging_stamp` | *the gates passed on these exact bytes* | the staging judge (`scripts/selfhost/gates/`) | `promote-stamp.json`, staged in the drop, re-hashed against `staging_stamp.stamp_sha256` |
| `promoted_by` | *a human with a key approved this revision* | the owner | the live relay, via `buzz projects release-check` |

Neither implies the other. A green suite is not permission to ship, and an
owner's approval is not evidence that anything was tested.

**The stamp's hash joins, and the one that does not exist.** The stamp records
a sha256 per gated artifact and the manifest records one per shipped component,
so `acp` and `cli` join exactly: the bytes that were tested are provably the
bytes being installed. The relay does **not** join. The gates exercise a
`buzz-relay` built from source; the manifest ships a Docker image id. Those are
different bytes of the same commit, so the relay half of a stamp is bound by
`source_commit` alone. The deployer prints that distinction as
`relay-image=COMMIT-ONLY` rather than letting a green line imply a check it did
not make. Closing the gap means having the gates build and exercise the image
itself; until then, do not describe the relay as hash-bound.

**The approval's commit join.** `release-check` prints
`{authorized, reason, root, revision, commit, owner, decided_at}` and exits 0
only when authorized. The deployer requires exit 0 **and**
`verdict.commit == manifest.source_commit`. Without that second half, any
approval by that owner of any revision would authorise any manifest — the
deployer would be checking that a decision exists, not that it is about these
bytes.

**"No verdict" is not "no".** `release-check` prints its verdict whenever it
reached one and prints nothing when it could not (network down, relay
unreachable, subcommand missing). The deployer treats empty stdout as
**blocked**. Reading a missing answer as `false` would mean refusing forever the
day the relay is slow; reading it as `true` would mean shipping unapproved code
the day the relay is down. Only "blocked" is safe, and it is the one that
requires the absence of output to be part of the contract — which is why
`release-check` is specified that way.

## The enforcement matrix

|  | `--inbox` (unattended) | direct invocation |
|---|---|---|
| no `staging_stamp` | **refused** | refused unless `--allow-unstamped` (loud `WARN`) |
| verdict `promotable` | proceeds | proceeds |
| verdict `promotable_with_waivers` | **refused** — the unit passes no flag | requires `--ack-waivers` |
| any other verdict | **refused** | **refused** — no flag exists |
| stamp file missing or re-hashes differently | **refused** | **refused** — no flag exists |
| `promoted_by: null` | **refused** | refused unless `--allow-unattributed` (loud `WARN`) |
| `release-check` unauthorized, or its `commit` ≠ `source_commit` | **refused** | **refused** — no flag exists |
| `release-check` reached no verdict | **refused** | **refused** |
| `migrations: ack-required` | **refused** — the unit passes no flag | requires `--ack-migrations` |

`--allow-unstamped`, `--allow-unattributed` and `--stamp-file` are **usage
errors** under `--inbox`, not warnings. A flag that is accepted and quietly does
nothing is a flag somebody will believe worked; refusing it outright means "an
unattended deploy needs the whole ceremony" cannot be softened even by a human
typing `--inbox` by hand. If you must ship an unstamped or unapproved release,
point the deployer at the manifest directly — which is also the shape that keeps
a person in the room.

The two rows with no escape hatch at all are the ones where a flag would be
meaningless. A stamp that does not re-hash, or an approval for a different
commit, is not a policy you can acknowledge — it is a document that does not
describe this release.

## Verifying with the deployed CLI, not the candidate

When `cli` is a component there are two `buzz` binaries on the box: the deployed
`/opt/buzz/bin/buzz` and the candidate staged in the drop. The deployer runs
`release-check` with the **deployed** one.

The candidate is the thing being authorised. Asking it whether it is authorised
makes the release its own auditor: a `release-check` that always printed
`{"authorized": true}` would install itself, and nothing between here and there
would notice. The deployed binary was, by construction, admitted by a previous
run of this same gate. That is not a strong guarantee — it is a chain of
custody, not a proof — but it is a guarantee the current release cannot
manufacture, which is the only property that matters at this step.

The cost is real: an older deployed CLI may not know the subcommand at all.
That lands in the "no verdict" row above and is refused, which means the upgrade
path for a box whose CLI predates `release-check` is a human installing one CLI
by hand. That is the correct amount of friction for "teach the box how to check
approvals".

## What the executor guarantees

Five properties, each of which exists because its opposite has a failure mode
nobody wants to debug at 3am.

**Dry run is the default.** `deploy.sh <manifest>` reports; only `--execute`
touches anything. Deploying is the deliberate spelling because the cost of the
opposite default is unrecoverable and the cost of this one is a wasted minute.
A dry run runs the entire read-only preflight for real, then prints every
mutation it would make. In a dry run a failed check does **not** abort the
walk — you want the complete list of blockers, not the first one.

**Nothing is fetched.** Every artifact must already be on the box. Images live
in the local-only `buzz-local:` namespace and are verified by content id, not
by tag. Binaries are verified by SHA-256 against the manifest at preflight and
again immediately before install, because the interesting window is between
those two moments. A deployer that can reach out and get what it is missing is
a deployer that can be told to get something else.

**Destinations are not negotiable.** Install paths, owners and modes live in
`deploy.sh`, never in the manifest. The manifest says what an artifact is; the
deployer knows where it goes.

**Back up before, verify after.** Every destructive step announces its intent
*before* acting — so the journal shows what was in flight even if the box died
mid-step — and re-reads the result afterwards. Backups are re-hashed against
their own `SHA256SUMS`; a backup nobody checked is a rumour.

**Failure rolls back.** Any gate failure restores exactly what this run
changed, in reverse order, restarts, and re-gates. Not "restores a remembered
good state" — *undoes what it did*. Those are different operations and only the
first is safe to run unattended.

## One leg stable

The single ordering rule, applied at three scales:

**Relay first, proven healthy, then agents. Never both in one step.** The relay
is the leg every agent stands on. Move one leg at a time and something is
always holding you up; move both and a bad build takes the box down with no
working component left to tell you about it. The manifest's `deploy_order` must
put `relay-image` first, and the validator enforces it.

**Agents one at a time.** `buzz-claude` is restarted and gated before
`buzz-codex` is touched. Restarting both together halves the wall clock and
doubles the blast radius: if the new binary is bad, the second unit is already
down before the first one's gate has said so.

**One release at a time.** The inbox is drained oldest-first, one release per
run. Two releases interleaving would make the ordering rule meaningless.

## Gates

A gate is not "did the process start". It is "did the process come up
*working*", because `systemctl is-active` cheerfully reports success for a
binary that started, failed to reach the relay and is sitting in a retry loop.

**Relay gate** — polls up to 120s for all of: the compose container reporting
`(healthy)`; the container actually running the image the manifest named;
`http://127.0.0.1:3100/_liveness` returning `ok`; and
`https://hermes.tail81f3.ts.net:9443/_liveness` returning `ok`. The last one is
not redundant — it proves the Tailscale Serve mapping survived the container
recreate, which is the failure that takes the agents offline while the relay
itself looks fine.

**Agent gate** — watches the unit's journal from a cursor captured immediately
before the restart, for 60s, requiring all four startup lines:

```
buzz-acp starting:
agent initialized
project activity publisher enabled
enrolment history reconstruction complete   (… dropped=0)
```

and **zero** `ERROR` lines. It fails fast on an ERROR line or a unit that
stops, but it does not pass early: "started cleanly" is a claim about the first
sixty seconds, and a crash at second 45 is exactly what this catches.

**CLI gate** — `buzz --help` exits 0. A smoke test, not a feature test: it
proves the binary is the right architecture, its links resolve and its argument
parser builds. Anything deeper belongs in the staging gates.

## Drain: finish what is running before you swap it

`SIGTERM` gives an in-flight prompt a thirty-second grace and then aborts it, so
a model turn three minutes into a refactor dies mid-sentence — and a queued
project event, already announced on its issue as `state=queued`, is dropped on
the floor with the indicator still lit. Restarting an agent is therefore never
invisible: somebody's turn is cut, or somebody's issue promises work no process
is holding.

Drain is the third lever. Before each agent unit is replaced, the deployer
publishes an **owner-signed control frame** — `buzz agents drain --agent <hex>`
— telling the runtime to stop admitting work, finish what it holds, and exit 0.
Because the units are `Restart=on-failure`, exit 0 means the unit stays down
until the deployer starts it again, which is exactly the window a binary swap
needs. The full wire contract lives in `crates/buzz-acp/src/drain.rs`; the
sender is `crates/buzz-cli/src/agent_drain.rs`.

Two properties of the sequence are worth stating:

* **Publishing is not draining.** The CLI can only report that the relay
  accepted the frame; its ack says `drain_confirmed: false` and always will.
  What the deployer waits on is the unit reaching `inactive` — the process
  stopping is the only evidence the drain was honoured.
* **The wait is bounded, and the bound is a compromise.** The agent's own drain
  bound is `max_turn + 100s` (7300s at the default `BUZZ_ACP_MAX_TURN_DURATION`),
  which is far longer than `buzz-deployer.service`'s `TimeoutStartSec=45min`.
  Waiting the full agent bound would get the *deployer* killed by systemd
  mid-swap, which is strictly worse than giving up loudly. So the deployer waits
  `BUZZ_DEPLOY_DRAIN_WAIT_SECONDS` (default 600) per unit and then falls back to
  a restart, saying so. Raising it means raising `TimeoutStartSec` too:
  `(wait × agent units) + gates + backup` must fit inside the unit's budget.

**Every fallback is loud.** Drain not configured, key file unreadable, no pubkey
for the unit, a deployed CLI too old to have the subcommand, publish failed, or
the wait expired — each emits a `status=WARN` naming the missing precondition
and then does the old SIGTERM restart. It is never skipped and never silent: an
operator who believes in-flight turns were finished when they were killed is
worse off than one who knows they were killed.

Rollback does **not** drain. Draining is a courtesy to work in flight, and the
work in flight during a rollback is being done by a binary this run has just
proven bad. Asking it politely to finish would be waiting on the thing that
failed — and a restart is also the only path that works when the process is
already wedged.

Configuration is two variables, both documented in
[`scripts/selfhost/units/README.md`](../scripts/selfhost/units/README.md):

```bash
BUZZ_DEPLOY_OWNER_KEY_FILE=/opt/buzz/agents/deploy-owner.env   # the OWNER's key
BUZZ_DEPLOY_AGENT_PUBKEYS="buzz-claude:<64-hex> buzz-codex:<64-hex>"
```

The agent **public** keys come from configuration and never from the agents' own
env files. Those files hold `BUZZ_PRIVATE_KEY`, and a root process opening them
to derive something public would be reading a secret it has no business knowing.
The cheapest way to keep that true is to never open the file.

## Migrations: expand → migrate → contract

`BUZZ_AUTO_MIGRATE=true` on this relay. The new image applies pending
migrations on startup, which means **by the time the relay gate passes, the
schema has already moved.** There is no later "run the migrations" step to
reconsider at, so the decision has to be made before the swap.

The rule that keeps rollback meaningful is the standard three-step, and it is
worth stating plainly because it is what `backward-safe` means:

1. **Expand** — add the new column/table/index, nullable or defaulted. Ship it.
   Both the old and new binaries run against this schema.
2. **Migrate** — backfill, and move readers and writers to the new shape.
   Ship it. Still both-binary compatible.
3. **Contract** — drop the old column. Ship it *only once you would never want
   to roll back past step 2.*

A release containing only steps 1 and 2 is `backward-safe`: the previous binary
still works against the new schema, so binary rollback restores service. A
release containing step 3 — or any destructive or non-defaulted change — is
`ack-required`: rolling the binary back leaves it talking to a schema it does
not understand, and recovery needs `/opt/buzz/backups/latest` and a database
restore.

**Say the quiet part out loud: rollback-after-migrate is only safe for
backward-compatible migrations.** Every other case is a restore, not a
rollback, and it costs whatever was written since the backup. This is why
`ack-required` demands both a fresh postgres-inclusive backup
(`/opt/buzz/scripts/backup-buzz-latest.py`) and a human typing
`--ack-migrations`.

Prefer three boring releases over one clever one. The whole point of expand →
migrate → contract is that each step is individually reversible.

## Green-baseline release branches

A release branch is cut from a commit that was **green**, and the manifest
records which commit that was. `source_commit` is not a label; it is checked:

* At mint time, in the build worktree, `HEAD` must equal `source_commit`. The
  minter also refuses an uncommitted tree, because a manifest minted over local
  edits describes a tree that no longer exists.
* At deploy time the executor may be a systemd unit pointed at the shared
  object store rather than at any particular worktree. There it can only
  honestly claim that `source_commit` names a real, fetched commit — and it
  says so, with a `WARN`, rather than quietly weakening the stronger claim.
  What actually binds the manifest to the bytes being installed is the artifact
  hash; pretending otherwise is how a check becomes decoration.

The live commit is not recorded anywhere separate: it is recovered from the
running image tag, `buzz-local:<name>-<short-sha>`. That naming convention is
not decoration — it is the only place the box records which source it is
running, and the migrations preflight parses it back out to compute the delta.

## Rollback semantics

What rollback restores, in reverse order of what was done, and only what this
run actually did:

| Changed | Restored from |
|---|---|
| `/opt/buzz/bin/buzz` | `<backup>/bin/buzz` |
| `/opt/buzz/bin/buzz-acp` | `<backup>/bin/buzz-acp`, and `/opt/buzz/bin/buzz-acp.rollback-<what-it-contains>` sits beside the live binary |
| `BUZZ_IMAGE` in `relay/.env` | the previous tag, read at preflight |
| relay container | restarted onto the previous image, then re-gated |
| agent units | restarted, then re-gated, one at a time |

Two details that matter:

* **Backups are named for the release they precede; the rollback binary is
  named for what it contains.** `backups/projects-merge-20260804T164024Z/`
  precedes the `projects-merge` release; `buzz-acp.rollback-unified-13acbaf2`
  *is* the `unified-13acbaf2` binary. Conflating those is how someone restores
  the wrong thing.
* **The rollback is re-gated.** A rollback that is not verified is a second
  unverified deploy performed under worse conditions than the first.

Exit codes are the triage:

| Code | Meaning |
|---|---|
| 0 | success, or a dry run with no blockers |
| 1 | preflight refused, or a dry run found blockers — **nothing was mutated** |
| 2 | usage error or invalid manifest — **nothing was mutated** |
| 3 | deploy failed after mutation and **rolled back cleanly** |
| 4 | deploy failed **and rollback failed** — a human must intervene now |

Codes 1 and 2 mean the box is untouched. Code 3 means the box is back where it
started and you have a transcript explaining why. Code 4 is the only one that
is an incident.

## Reading the journal

Every line is `buzz-deploy ts=<iso8601> step=<name> status=<STATUS> <detail>`,
with `STATUS` in `PASS FAIL SKIP PLAN INFO WARN`. Step names are `<phase>.<thing>`
and are stable — treat them as an interface.

```bash
journalctl -u buzz-deployer.service -o cat | grep 'status=FAIL'
cat /opt/buzz/releases/processed/<name>-<ts>/result.json
```

`result.json` carries the exit code, the reason and the full transcript, and
travels with the release into `processed/` whether it succeeded or failed.

## Announcements

`deploy.sh --announce-root <event-id>` posts progress comments through
`/opt/buzz/bin/buzz issues comment` (the same event shape `buzz pr comment`
produces, so one code path serves both). Comments go out at start, after the
relay is healthy, after each agent gate, and on the final result or rollback.

Two rules:

* **One-way.** An announcement failure is a `WARN` and never anything more. A
  relay that is down is exactly when you most want the deploy to proceed to its
  gates and its rollback.
* **Its own identity.** The deployer signs with its own key, never an agent's,
  so "deploy failed, rolled back" cannot be mistaken for a claim an agent made
  about its own work.

## The division of labour between the gates and the deployer

**The gates decide whether a candidate is fit to ship; the deployer decides
whether the box survived shipping it.** Those are different questions with
different evidence, which is why the deploy-time gates in `deploy.sh`
deliberately stay shallow (liveness, startup lines, a `--help`) — depth belongs
on the staging side, before anything is live. `scripts/selfhost/gates/README.md`
documents what each staging gate does and does not prove; read it before
treating a `promotable` as a guarantee about, say, the relay's spec conformance.

The deployer's verdict policy, matching the table the gates publish:

| Stamp verdict | Deployer |
|---|---|
| `promotable` | proceed |
| `promotable_with_waivers` | requires `--ack-waivers`; refused unattended, because the unit passes no flag |
| `blocked` / `incomplete` | refuse — no flag exists |
| `refused` | refuse — the evidence does not describe the current bytes, and gate results are irrelevant in that state |

The last three are also refused one step earlier: `mint-manifest.py` will not
mint a release from a stamp that is not promotable, so a `blocked` candidate
does not become a drop in the first place. Both ends state the rule so neither
can quietly become the exception.

## Still ahead

**Drain for the relay.** `restart_agents()` drains; `restart_relay()` still
force-recreates the container. The relay has no equivalent of the ACP control
frame — there is nothing to send it — so closing this means giving the relay a
"stop admitting, finish in-flight requests" mode first. Until then a relay swap
is a hard recreate and the deploy-time window is whatever a container restart
costs.

**Hash-binding the relay image.** The gates test a from-source `buzz-relay` and
the manifest ships an image id, so the relay is bound to a stamp by
`source_commit` alone (see above). The fix is on the gates' side: build and
exercise the `buzz-local:<tag>` image via the repo `Dockerfile`, so the artifact
gated is the artifact shipped and the stamp can carry an `image_id` that joins
exactly.

**Stamp freshness.** `staging_stamp.stamped_at` is advisory: nothing expires a
stamp. A month-old `promotable` for a commit that is still `source_commit` is
still accepted, and it should be — the bytes have not changed — but the box it
was gated against has. Enforcing an age would need a policy nobody has argued
for yet, and inventing one here would be worse than saying it is not enforced.

**Desktop updater.** The desktop app is released through
`.release/desktop-candidate.json` and `scripts/desktop_release.py`, a pipeline
this one deliberately mirrors rather than merges with: desktop ships to users
through an updater and a signed tag, self-host ships to one box through a
directory drop. The seam is the shared vocabulary — an integer `schema`, an
immutable candidate, a validator that re-derives every claim — so that a future
"one release, both surfaces" change is a matter of adding a component to a
manifest rather than reconciling two philosophies.
