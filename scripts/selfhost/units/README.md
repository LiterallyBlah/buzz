# Deployer units — install steps

Two units: a `.path` that watches the release inbox and a `.service` that runs
`deploy.sh --execute --inbox /opt/buzz/releases`. Everything in this directory
is a repo file; nothing here is live until someone runs the steps below.

Read `buzz-deployer.service` before installing it. It runs as **root**, and the
header explains why that is the honest choice rather than a shortcut, and what
contains it instead (`ProtectSystem=strict` plus a four-path allow-list,
install destinations hard-coded in `deploy.sh` rather than read from a
manifest, hash verification before install, and no network fetch ever).

## Install

Run as a user who can `sudo`. These are the only steps; there is no bootstrap
script, because a deployer install is a thing you should read line by line.

```bash
# 1. Place the executor and the minter where the unit expects them.
#    They are copied out of the repo rather than symlinked into it: the
#    deployer must not change underneath itself when an agent edits the
#    worktree it is deploying from.
sudo install -d -m 0755 /opt/buzz/scripts/selfhost
sudo install -m 0755 -o root -g root \
  /opt/buzz/src/scripts/selfhost/deploy.sh \
  /opt/buzz/src/scripts/selfhost/mint-manifest.py \
  /opt/buzz/scripts/selfhost/
sudo install -m 0644 -o root -g root \
  /opt/buzz/src/scripts/selfhost/selfhost-release.schema.json \
  /opt/buzz/scripts/selfhost/

# 2. Create the inbox. hermes writes drops; root consumes them.
sudo install -d -m 0755 -o hermes -g hermes /opt/buzz/releases
sudo install -d -m 0755 -o hermes -g hermes /opt/buzz/releases/incoming
sudo install -d -m 0755 -o root   -g root   /opt/buzz/releases/processed

# 3. Install the units.
sudo install -m 0644 -o root -g root \
  /opt/buzz/src/scripts/selfhost/units/buzz-deployer.service \
  /opt/buzz/src/scripts/selfhost/units/buzz-deployer.path \
  /etc/systemd/system/
sudo systemctl daemon-reload

# 4. Verify BEFORE enabling. A dry run against a real manifest is the
#    acceptance test for the install; it touches nothing.
sudo /opt/buzz/scripts/selfhost/deploy.sh --inbox /opt/buzz/releases

# 5. Arm the trigger.
sudo systemctl enable --now buzz-deployer.path
systemctl status buzz-deployer.path
```

`buzz-deployer.service` is deliberately **not** enabled and has no
`[Install] WantedBy`. It is started by the path unit or by hand. Enabling it
would mean a box that reboots mid-incident comes up and immediately deploys
whatever happened to be sitting in the inbox.

### What the unit deliberately does not pass

`ExecStart` is exactly `deploy.sh --execute --inbox /opt/buzz/releases`. It
passes **none** of `--ack-migrations`, `--ack-waivers`, `--allow-unstamped`,
`--allow-unattributed`, and that omission is the enforcement:

| A drop that… | …is |
|---|---|
| carries no promote stamp | refused |
| carries a `promotable_with_waivers` stamp | refused |
| has `promoted_by: null` | refused |
| declares `migrations: ack-required` | refused |

Every one of those has a command-line escape hatch, and none of them is
reachable by writing a file into `incoming/`. If you add flags to `ExecStart`
you are removing rows from that table — do it deliberately or not at all. The
first three are additionally *usage errors* under `--inbox`, so even a hand-run
`deploy.sh --inbox --allow-unstamped` refuses rather than proceeding.

## Optional: the announcement identity

`deploy.sh --announce-root <event-id>` posts progress comments through
`/opt/buzz/bin/buzz`. Give the deployer its **own** nostr key — never an
agent's — so that "deploy failed, rolled back" cannot be mistaken for a claim
an agent made about its own work.

```bash
sudo install -m 0600 -o root -g root /dev/null /opt/buzz/agents/deployer.env
sudo tee /opt/buzz/agents/deployer.env >/dev/null <<'ENV'
BUZZ_RELAY_URL=https://hermes.tail81f3.ts.net:9443
BUZZ_PRIVATE_KEY=<the deployer's own key>
BUZZ_ANNOUNCE_REPO_OWNER=<64-hex repo owner pubkey>
BUZZ_ANNOUNCE_REPO_ID=<repo d-tag>
BUZZ_ANNOUNCE_ROOT=<64-hex issue or PR root event>
ENV
```

The unit references this file with a leading `-`, so it is optional. Announcing
is one-way reporting: a missing key, an unreachable relay or a bad event id
produces a `status=WARN` line and never changes the outcome of a deploy.

## Required for authorization: reaching the relay

`preflight.authorization` runs `buzz projects release-check` through the
**deployed** `/opt/buzz/bin/buzz` — never the candidate staged in the drop,
because a release must not vouch for itself. That command needs a relay and an
identity, which come from the same `deployer.env` above (`BUZZ_RELAY_URL`,
`BUZZ_PRIVATE_KEY`). Without them the check reaches no verdict, and **no verdict
is treated as blocked**, so an unconfigured deployer refuses every attributed
release rather than shipping it.

Optionally pin which repository an approval may come from:

```ini
[Service]
Environment=BUZZ_DEPLOY_RELEASE_REPO=30617:<64-hex owner>:buzz
```

This is configuration and not a manifest field on purpose: "which repository may
authorise a release of mine" is a statement about the box, and a manifest that
stated its own scope would be choosing its own auditor.

## Optional: drain before restart

Configured, the deployer asks each agent to stop admitting work, finish what it
holds and exit 0 before its binary is swapped. Unconfigured, it SIGTERMs — the
old behaviour — and says so with a `status=WARN` on every affected unit. It is
never silent: an operator who believes in-flight turns were finished when they
were killed is worse off than one who knows they were killed.

Two settings, both in a drop-in so they survive a unit reinstall:

```bash
# The OWNER's key. A drain frame is only honoured when signed by the agent's
# resolved owner, so this is the one identity that can send one — and it is NOT
# the deployer's announcement key unless that key is also the agents' owner.
sudo install -m 0600 -o root -g root /dev/null /opt/buzz/agents/deploy-owner.env
sudo tee /opt/buzz/agents/deploy-owner.env >/dev/null <<'ENV'
BUZZ_RELAY_URL=https://hermes.tail81f3.ts.net:9443
BUZZ_PRIVATE_KEY=<the agents' owner key>
ENV

sudo install -d -m 0755 /etc/systemd/system/buzz-deployer.service.d
sudo tee /etc/systemd/system/buzz-deployer.service.d/drain.conf >/dev/null <<'CONF'
# Drain configuration for the release executor. See docs/selfhost-releases.md.
[Service]
Environment=BUZZ_DEPLOY_OWNER_KEY_FILE=/opt/buzz/agents/deploy-owner.env
Environment=BUZZ_DEPLOY_AGENT_PUBKEYS=buzz-claude:<64-hex> buzz-codex:<64-hex>
CONF
sudo systemctl daemon-reload
```

`BUZZ_DEPLOY_AGENT_PUBKEYS` maps a unit name (without `.service`) to that
agent's **public** key, space separated. Get them from the agents' own
identities — `buzz users get` or whatever minted them — and never from
`/opt/buzz/agents/*.env`. Those files hold `BUZZ_PRIVATE_KEY`, and a root
process opening one to derive something public would be reading a secret it has
no business knowing. The deployer therefore has no code path that opens an
agent env file at all, which is the cheapest way to keep that true.

The key file is *parsed*, not sourced: `deploy.sh` reads `BUZZ_PRIVATE_KEY=` and
`BUZZ_RELAY_URL=` out of it with `sed`. Sourcing an env file executes it, and
this runs as root — the file is meant to hold a key, not a program. The key is
exported into a subshell rather than passed as an argument, so it never appears
in the journal's `would run:` / `running:` lines.

Two more knobs, rarely needed:

| Variable | Default | Why |
|---|---|---|
| `BUZZ_DEPLOY_DRAIN_WAIT_SECONDS` | `600` | How long to wait for a drained unit to reach `inactive` before giving up loudly and restarting it. |
| `BUZZ_DEPLOY_DRAIN_PUBLISH_TIMEOUT` | `90` | Ceiling on the `buzz agents drain` publish itself. |

**The wait is shorter than the agent's own bound, on purpose.** A draining
`buzz-acp` waits `max_turn + 100s` for the work it inherited — 7300s at the
default `BUZZ_ACP_MAX_TURN_DURATION` — which is far longer than this unit's
`TimeoutStartSec=45min`. Waiting the full agent bound would get the *deployer*
killed by systemd mid-swap, which is strictly worse than a loud fallback. If you
raise the wait, raise `TimeoutStartSec` too: `(wait × agent units) + gates +
backup` has to fit inside the unit's budget.

Verify the seam without deploying anything — a dry run shows the plan:

```bash
sudo -E /opt/buzz/scripts/selfhost/deploy.sh --inbox /opt/buzz/releases \
  | grep -E 'acp\.(drain|drained|restart)'
```

`status=PLAN` on `acp.drain.*` means it is configured. `status=WARN` names the
missing precondition.

## Shipping a release

```bash
# In the build worktree, after `docker build`, `cargo build --release`, and a
# green `scripts/selfhost/gates/run-gates.sh all --execute`:
scripts/selfhost/mint-manifest.py generate \
  --name <release-name> --components relay-image,acp,cli \
  --staging-stamp scripts/selfhost/gates/promote-stamp.json \
  --promoted-by <pr-root>:<revision>:<owner> \
  --notes "what changed and what to watch" \
  --stage /opt/buzz/releases/incoming
```

`--stage` assembles `incoming/.staging-<name>/` and renames it to
`incoming/<name>/` when complete. The rename is atomic, so the path unit only
ever sees a release that is all there. Do not hand-assemble a drop by writing
`manifest.json` first and copying binaries afterwards.

A staged drop contains `manifest.json`, the binaries, `SHA256SUMS`, and
`promote-stamp.json` — the gates' evidence, copied in so the deployer can
re-derive the promotion from the drop alone, on a box where the gate run
directory was cleaned up an hour ago.

Both promotion flags are omissible and the drop is still a valid manifest — it
is just one an unattended deploy refuses. See the table above.

## Watching and triaging

```bash
journalctl -u buzz-deployer.service -f -o cat            # live
journalctl -u buzz-deployer.service -o cat | grep 'status=FAIL'
cat /opt/buzz/releases/processed/<name>-<ts>/result.json # verdict + transcript
```

Every release leaves `incoming/` whether it succeeded or failed, and takes a
`result.json` with it. That is what re-arms the path unit — `PathExistsGlob`
will not fire again until the glob stops matching. If a release is ever stuck
in `incoming/`, the deployer could not archive it: fix that by hand rather than
expecting a retry, because a stuck release means the box is already unhappy.

## Uninstall

```bash
sudo systemctl disable --now buzz-deployer.path
sudo rm /etc/systemd/system/buzz-deployer.{path,service}
sudo systemctl daemon-reload
```

`/opt/buzz/scripts/selfhost/` and `/opt/buzz/releases/` are left alone: the
processed archive is deployment history and should outlive the units.
