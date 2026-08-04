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
   whoever built the release. So `--ack-migrations` — the acknowledgement that
   a schema change is not backward-compatible — is a command-line flag, not a
   manifest field, and the systemd unit does not pass it. An unattended drop
   can never run a forward-only migration by itself.
2. **An unverified claim is refused, not ignored.** `promoted_by` must be
   `null` until something actually verifies it (Phase 2). A manifest asserting
   an approval nothing checked would read like authority in an audit log, which
   is worse than asserting nothing.

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

## Later phases

The seams are already cut. Each is one function or one field, with one caller,
so a later phase is a diff you can read in one sitting rather than a policy
sprinkled through the runbook.

**Phase 2 — authorization.** `gate_authorization()` in `deploy.sh` is the only
caller-facing hook, and `promoted_by` in the manifest is the only field. Today
the function logs `SKIP` and the field must be `null`. Phase 2 replaces the
function body with signature verification against the owner pubkey and drops
the `null` allowance from the schema in the same change. Nothing else moves.

**Phase 3 — staging promotion stamp.** `scripts/selfhost/gates/` is the staging
judge: `run-gates.sh` runs the suite and `stamp.sh` emits
`gates/promote-stamp.json` (`schema: "buzz.staging.promote-stamp/v1"`). The
split is already meaningful today: **the gates decide whether a candidate is
fit to ship; the deployer decides whether the box survived shipping it.** Those
are different questions with different evidence, which is why the deploy-time
gates in `deploy.sh` deliberately stay shallow (liveness, startup lines, a
`--help`) — depth belongs on the staging side, before anything is live.

The two halves already agree on the thing that makes them composable: the stamp
binds its verdict to artifact hashes, and the manifest binds its identity to
the same hashes. Phase 3 is therefore a *cross-check*, not a new mechanism —
the stamp's `candidate.artifacts[].sha256` must equal the manifest's
`components.<name>.sha256`, and a mismatch means the evidence describes
different bytes than the release. The manifest grows one reserved field for the
stamp (or a reference to it) beside `promoted_by`, verified in the same
function. The intended verdict policy, stated now so it is not improvised
later:

| Stamp verdict | Deployer |
|---|---|
| `promotable` | proceed |
| `promotable_with_waivers` | refuse unattended; require an explicit command-line acknowledgement, the same shape as `--ack-migrations`. A human waived red tests; a human should be present when they ship. |
| `blocked` / `incomplete` | refuse |
| `refused` | refuse, loudly — the evidence does not describe the current bytes, and gate results are irrelevant in that state |

**Phase 4 — drain signal.** Today the relay is force-recreated and the agents
are restarted outright; in-flight work is whatever the process was doing when
it got `SIGTERM`. The seam is the moment between "gates passed" and "restart":
a drain step would tell the component to stop accepting new work, wait for
in-flight turns to finish, and only then restart. `restart_relay()` and
`restart_agents()` are the two functions that grow a pre-step; the manifest
grows a per-component drain budget. Nothing about ordering or rollback changes,
which is why it is safe to defer.

**Phase 5 — desktop updater.** The desktop app is released through
`.release/desktop-candidate.json` and `scripts/desktop_release.py`, a pipeline
this one deliberately mirrors rather than merges with: desktop ships to users
through an updater and a signed tag, self-host ships to one box through a
directory drop. The seam is the shared vocabulary — an integer `schema`, an
immutable candidate, a validator that re-derives every claim — so that a future
"one release, both surfaces" change is a matter of adding a component to a
manifest rather than reconciling two philosophies.
