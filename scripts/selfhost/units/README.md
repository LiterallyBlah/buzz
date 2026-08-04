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

## Shipping a release

```bash
# In the build worktree, after `docker build` and `cargo build --release`:
scripts/selfhost/mint-manifest.py generate \
  --name <release-name> --components relay-image,acp,cli \
  --notes "what changed and what to watch" \
  --stage /opt/buzz/releases/incoming
```

`--stage` assembles `incoming/.staging-<name>/` and renames it to
`incoming/<name>/` when complete. The rename is atomic, so the path unit only
ever sees a release that is all there. Do not hand-assemble a drop by writing
`manifest.json` first and copying binaries afterwards.

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
