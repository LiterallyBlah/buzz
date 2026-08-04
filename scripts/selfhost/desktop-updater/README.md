# Buzz Desktop self-update, self-hosted

The desktop app can update itself from this box. It polls one URL on the relay
host, downloads an installer, checks that the installer was signed by a key
that never leaves the box, and — only when the user clicks — installs it and
restarts.

This directory holds the three scripts that make that work and the one config
overlay that turns it on at build time.

> **The currently installed app can never self-update.** Buzz
> `0.5.5-unified.1` was built with `plugins.updater.endpoints: []` and without
> the build-time environment that compiles the updater plugin in. It has no
> endpoint to poll and no plugin to poll it with. **The first updater-enabled
> build must be installed by hand.** Everything after that is automatic. If you
> take one thing from this document, take that.

---

## The pieces

| Thing | Where | What it does |
| --- | --- | --- |
| Config overlay | `desktop/src-tauri/tauri.updater.conf.json` | Turns on `createUpdaterArtifacts` and points the app at this box. Committed with a `__BUZZ_UPDATER_PUBKEY__` placeholder. |
| `generate-keys.sh` | here | Mints the box's minisign signing key. Once, ever. |
| `render-updater-config.sh` | here | Substitutes the real public key into the overlay and prints the build environment. |
| `publish-update.sh` | here | Puts a built bundle on the channel, atomically. |
| Update UI | `desktop/src/features/settings/` | Already exists in the app — see [The app side](#the-app-side). Nothing to add. |

Serving directory on the box:

```
/opt/buzz/releases/desktop-updates/          → https://hermes.tail81f3.ts.net:9443/desktop-updates/
├── latest.json                              ← the only file the app polls
└── artifacts/
    ├── 0.5.5-unified.2/
    │   ├── Buzz_0.5.5-unified.2_x64-setup.exe
    │   ├── Buzz_0.5.5-unified.2_x64-setup.exe.sig
    │   ├── latest.json      ← this version's manifest, kept for rollback
    │   └── publish.json     ← audit record (sha256, size, source, who, when)
    └── …                    ← prior versions are never deleted
```

`latest.json` is the only mutable file, and it is replaced by `rename(2)`.
Everything under `artifacts/<version>/` is write-once: `publish-update.sh`
refuses a version directory that already exists.

---

## One-time setup

### 1. Mint the signing key

```bash
sudo scripts/selfhost/desktop-updater/generate-keys.sh
```

It prompts for a password (choose one — an unprotected key is a file that
signs releases for anyone who can read it), writes:

- `/opt/buzz/keys/desktop-updater.key` — private, `0600`, **root:root**
- `/opt/buzz/keys/desktop-updater.key.pub` — public, `0644`

and refuses outright if either already exists.

Two things to do immediately:

- **Harden the directory.** `0600` stops another user reading the key; it does
  not stop a user with write access to `/opt/buzz/keys` from deleting and
  replacing it. Today that directory is `hermes:hermes 0700`, which means the
  agents that run as `hermes` can swap the key they are about to be signed by.
  `sudo chown root:root /opt/buzz/keys && sudo chmod 0700 /opt/buzz/keys`
  closes that.
- **Back the private key up off this box, encrypted.** If it is lost, every
  installed copy of Buzz stops accepting updates permanently — the public key
  is compiled into the installed binary and there is no channel left to ship a
  new one over. The only recovery is a manual reinstall on every machine.

### 1b. Rotating the key (read before you ever need it)

There is no clean rotation. The public key is compiled into every installed
binary, so a new key can only be trusted by a build that was made *after* the
rotation — and that build has to reach the machine somehow. The sequence is:

1. Generate the new key to a **different path** (`--keyfile`). Do not delete
   the old one; it is the only thing that can sign an update the currently
   installed apps will accept.
2. Build and publish one last release **signed with the OLD key** whose only
   job is to carry the NEW public key in its config. Users update to it
   normally.
3. Once every install is on that release, switch signing to the new key.
4. Only then retire the old key.

Skipping step 2 means every installed app is orphaned and has to be reinstalled
by hand. `generate-keys.sh` refuses to overwrite an existing key precisely to
make this a decision rather than an accident.

### 2. Serve the directory

```bash
sudo mkdir -p /opt/buzz/releases/desktop-updates/artifacts
sudo chmod 755 /opt/buzz/releases/desktop-updates /opt/buzz/releases/desktop-updates/artifacts

sudo tailscale serve --bg --https=9443 \
    --set-path=/desktop-updates \
    /opt/buzz/releases/desktop-updates
```

`tailscale serve` takes a directory as its target and serves it as static
files, alongside the existing proxies on the same port — `9443` currently
carries `/` → the relay on `127.0.0.1:3100` and `/pair` → `127.0.0.1:3101`.
Longest path prefix wins, so `/desktop-updates` does not disturb either.

Verify with `tailscale serve status` (the new mount should appear under the
`:9443` block alongside the two existing proxies) and then, from any tailnet
machine:

```bash
curl -fsS https://hermes.tail81f3.ts.net:9443/desktop-updates/latest.json
```

TLS is terminated by Tailscale with a publicly-chaining certificate, so the
updater's HTTPS client trusts it with no extra configuration — but the client
machine must be **on the tailnet** to resolve `hermes.tail81f3.ts.net` at all.
Off the tailnet, update checks fail; the app treats that as an ordinary check
failure and carries on.

### 3. Build the first updater-enabled bundle

```bash
cd /opt/buzz/workspaces/claude/buzz-projects-merge

# Substitute the public key and export the build environment.
eval "$(scripts/selfhost/desktop-updater/render-updater-config.sh --print-env)"

# The signing key. Both variables; the password one is required if the key has
# a password, and the build fails late and confusingly if it is missing.
export TAURI_SIGNING_PRIVATE_KEY="$(sudo cat /opt/buzz/keys/desktop-updater.key)"
export TAURI_SIGNING_PRIVATE_KEY_PASSWORD='…'

# Set the version everywhere (package.json, tauri.conf.json, Cargo.toml).
(cd desktop && node scripts/set-version-from-tag.mjs 0.5.5-unified.2)

cargo build --release --target x86_64-pc-windows-msvc \
    -p buzz-acp -p buzz-agent -p buzz-dev-mcp -p git-credential-nostr -p buzz-cli
./scripts/bundle-sidecars.sh x86_64-pc-windows-msvc

cd desktop && pnpm tauri build \
    --target x86_64-pc-windows-msvc \
    --bundles nsis \
    --config "$BUZZ_UPDATER_CONFIG"
```

Output: `desktop/src-tauri/target/x86_64-pc-windows-msvc/release/bundle/nsis/`
containing `Buzz_<version>_x64-setup.exe` **and** `Buzz_<version>_x64-setup.exe.sig`.

**If there is no `.sig`, stop.** Either `createUpdaterArtifacts` did not reach
the build (wrong `--config`) or the signing key was not in the environment.
Publishing without it is impossible and `publish-update.sh` will tell you so.

### 4. Install it by hand, once

Copy the `.exe` to the Windows machine and run it. It replaces
`0.5.5-unified.1`. From this install onwards, updates arrive over the channel.

Confirm it worked: **Settings → Software Updates** should say "You're on the
latest version" after a check. If it says *"Automatic updates aren't available
on this build"*, the updater plugin was not compiled in — see
[Why the overlay alone is not enough](#why-the-overlay-alone-is-not-enough).

---

## Per-release

```bash
# 1. Build (steps 3 above, with the new version number).

# 2. Dry run — this is the default; it prints the exact manifest it would write.
scripts/selfhost/desktop-updater/publish-update.sh \
    --bundle-dir desktop/src-tauri/target/x86_64-pc-windows-msvc/release/bundle \
    --version 0.5.5-unified.2 \
    --notes "Projects view lands. Fixes reconnect storm on relay restart."

# 3. Publish.
scripts/selfhost/desktop-updater/publish-update.sh … --execute
```

Then the user clicks. There is no forced install: the app downloads in the
background and waits on **Update Now**.

What `--execute` does, in order — the order is the point:

1. Stages artifact + `.sig` + manifests in `artifacts/.staging.<version>.<pid>/`.
2. `rename(2)`s that into `artifacts/<version>/`. Clients are unaffected: the
   new directory is not referenced by anything yet.
3. Writes the new `latest.json` beside the old one and `rename(2)`s it over.
   Atomic — a poller reads the whole old manifest or the whole new one.
4. Re-reads what is on disk and checks the manifest's URL resolves to a real
   file whose sha256 matches what was signed.

Any failure after step 2 rolls back exactly what the run did and exits 3
(or 4, loudly, if the rollback itself failed).

### Rolling back a bad release

Every version keeps the manifest it published, so:

```bash
cd /opt/buzz/releases/desktop-updates
sudo cp artifacts/0.5.5-unified.1/latest.json .latest.json.new
sudo mv -f .latest.json.new latest.json
```

Clients on the bad version will not downgrade — the updater only moves
forward. Rolling back stops the bleeding for anyone who has not updated yet;
fixing the ones who have means publishing a *higher* version.

---

## Version ordering — read this before picking a number

`tauri-plugin-updater` decides there is an update with a single comparison:
`release.version > current_version`, both parsed as `semver::Version`. That is
strict semver precedence, and the prerelease rules are where releases get
silently swallowed:

- A version **with** a prerelease tag ranks **below** the same version without
  one: `0.5.5-unified.1 < 0.5.5`.
- Prerelease identifiers compare left to right; numeric ones compare
  numerically: `0.5.5-unified.1 < 0.5.5-unified.2 < 0.5.5-unified.10`.
- Build metadata (`+abc`) is ignored entirely for ordering. Never encode a
  release number after `+`.

The installed app is **`0.5.5-unified.1`**.

**Recommended next version: `0.5.5-unified.2`.** It continues the existing
train, sorts unambiguously above what is installed, and stays below a future
upstream `0.5.5`/`0.5.6` so an upstream build would still register as an
upgrade. Every subsequent self-hosted release bumps the trailing number:
`-unified.3`, `-unified.4`, …

If the self-hosted line ever needs to jump ahead of the whole `unified` train
— for instance after merging upstream — go to `0.5.6` or higher, which
outranks every `0.5.5-*`. Do not go from `0.5.5-unified.2` to `0.5.5`: it is a
*higher* version but reads to everyone as a downgrade, and there is no way back
into the prerelease train afterwards.

`publish-update.sh` refuses a version that does not sort above the currently
published one, using the same comparison the client uses. That check is the
reason to always publish through the script.

---

## The app side

**Nothing to build.** The desktop app already has the whole update UX, wired
to `@tauri-apps/plugin-updater`:

| Piece | File |
| --- | --- |
| Check/download/install state machine | `desktop/src/features/settings/hooks/use-updater.ts` |
| App-wide provider (mounted in `main.tsx`) | `desktop/src/features/settings/hooks/UpdaterProvider.tsx` |
| Settings → Software Updates panel | `desktop/src/features/settings/UpdateChecker.tsx` |
| Sidebar "update ready" card | `desktop/src/features/settings/SidebarUpdateCard.tsx` |
| Header indicator | `desktop/src/features/settings/UpdateIndicator.tsx` |
| Linux `.deb` guard command | `desktop/src-tauri/src/commands/updater.rs` |

Behaviour: a check runs at startup and every 6 hours; a found update is
downloaded in the background; installing requires a click on **Update Now**,
which installs and relaunches. Settings → Software Updates has a **Check for
Updates** button and reports every state honestly, including *"Automatic
updates aren't available on this build"* when the plugin is absent.

The permissions (`updater:allow-check`, `allow-download`, `allow-install`,
`process:allow-restart`) are already in
`desktop/src-tauri/capabilities/default.json`, and
`@tauri-apps/plugin-updater` is already in `desktop/package.json`.

### Why the overlay alone is not enough

This is the trap. `desktop/src-tauri/src/lib.rs` registers the plugin like
this:

```rust
#[cfg(buzz_updater_enabled)]
let builder = if cfg!(debug_assertions) { builder } else {
    builder.plugin(tauri_plugin_updater::Builder::new().build())
};
```

and `desktop/src-tauri/build.rs` emits that cfg only when **both**
`BUZZ_UPDATER_PUBLIC_KEY` and `BUZZ_UPDATER_ENDPOINT` are set in the *build
environment*:

```rust
if updater_public_key.is_some() && updater_endpoint.is_some() {
    println!("cargo:rustc-cfg=buzz_updater_enabled");
}
```

So a build that uses the config overlay but forgets the two environment
variables produces an app with an endpoint in its config and **no updater
plugin compiled in**. Every check fails with `plugin updater not found`, and
the UI reports "Automatic updates aren't available on this build" — which is
true, and easy to mistake for "no update yet".

`render-updater-config.sh --print-env` emits both variables along with
`BUZZ_UPDATER_CONFIG`, so the build command above cannot get this wrong. Note
that `build.rs` uses the two variables purely as a presence gate — it never
embeds their values. The endpoint and public key the running app actually uses
come from the merged Tauri config.

Also note: it is a **release** build gate. `pnpm tauri dev` never has the
updater plugin, by design.

### How the config overlay merges

`tauri build --config <file>` merges with **JSON Merge Patch (RFC 7396)**, in
this order:

```
tauri.conf.json  →  tauri.windows.conf.json  →  --config overlay(s), left to right
```

Which means, concretely:

- `plugins.updater.endpoints` — merge patch **replaces arrays wholesale**. The
  base `[]` becomes `["https://…/desktop-updates/latest.json"]`. It does not
  append, and there is no way to append.
- `plugins.updater.pubkey` — absent from the base, added by the overlay.
  `tauri-plugin-updater`'s config struct makes `pubkey` a **required** field
  (no serde default), which is why the base config's endpoint-only `updater`
  block is safe today: the plugin is never registered, so its config is never
  deserialized. Register the plugin without a pubkey and setup fails.
- `bundle.externalBin` — the overlay must **never** define this. The Windows
  sidecar list comes from `tauri.windows.conf.json`, which is merged *before*
  `--config`; an `externalBin` key in the overlay would replace it and a
  `null` would delete it. `desktop/scripts/build-release-config.mjs` guards
  against exactly this for the OSS release path.

The upstream OSS release path does the same thing with a generated
`tauri.release.conf.json`; the canary workflows use the same `--config`
mechanism to turn `createUpdaterArtifacts` *off*. This overlay is the
self-hosted member of that family.

---

## Trust model

There are two entirely separate signatures in play, and only one of them
exists here.

**The updater signature is a minisign key held on this box.** Tauri signs the
NSIS installer at bundle time with `TAURI_SIGNING_PRIVATE_KEY`, producing
`<installer>.exe.sig`. That signature's base64 goes into `latest.json`. The
installed app carries the matching public key compiled into its binary and
verifies the downloaded bytes against it before running anything. This means:

- Whoever holds `/opt/buzz/keys/desktop-updater.key` can ship code that runs on
  the user's machine with no further prompt. That is the whole of the trust
  boundary. Treat the key accordingly.
- Compromising the *serving directory* alone is not enough: an attacker who
  replaces the `.exe` cannot produce a matching signature, and the app refuses
  the install.
- Compromising `latest.json` alone is not enough either, for the same reason.
- TLS and the tailnet are transport protection, not the trust anchor. The
  signature is the trust anchor and it is checked after download regardless.

**Windows Authenticode remains unsigned.** These builds are not code-signed for
Windows. SmartScreen will warn on the first manual install, and it may warn
again when the updater runs the NSIS installer. That is expected and unchanged
from the current build — the updater signature says "this came from the box",
not "Microsoft vouches for this". If Authenticode signing is added later it is
an orthogonal step at bundle time and does not affect anything in this
directory.

**These keys are not the upstream OSS keys.** Upstream Buzz signs its releases
with `TAURI_SIGNING_PRIVATE_KEY` from the GitHub org's secrets and publishes
`latest.json` to a GitHub release. An app built from this overlay trusts *only*
this box's key and *only* this box's endpoint. The two channels cannot update
each other's installs, which is the correct behaviour — but it does mean a
self-hosted install will never pick up an upstream release automatically.

---

## Platforms

**Windows x64 only, for now.** `latest.json` carries a single platform key,
`windows-x86_64`, and the build instructions above target
`x86_64-pc-windows-msvc`.

Adding a platform later is additive and needs no change to the layout:

- `publish-update.sh --platform <key>` writes the key you give it. The keys
  `tauri-plugin-updater` looks up are `{os}-{arch}` — `darwin-aarch64`,
  `darwin-x86_64`, `linux-x86_64` — with an optional `{os}-{arch}-{installer}`
  (e.g. `windows-x86_64-nsis`) tried first. `windows-x86_64` is the general
  form and the one this uses.
- Today the script publishes one platform per run and rewrites `latest.json`
  each time, so a genuine multi-platform release needs it extended to merge
  platform entries rather than replace them — one `jq` change, in the
  `MANIFEST` assembly. Upstream's `desktop/scripts/generate-oss-latest-json.sh`
  already does the N-platform version of this and is the right thing to copy.
- Linux is the one with a caveat that is already handled in the app: the Tauri
  updater can only replace an AppImage, so `.deb` installs are detected by
  `is_auto_update_supported` and shown a manual-download card instead.

---

## Why a directory mount and not the relay media store

The relay's media store is hash-addressed and immutable — `…/media/<sha256>.<ext>`
— which is the right shape for a release artifact, and it was the alternative
considered here. It is not used, for one hard reason and one soft one:

- **It denies `.exe`, and the Windows updater artifact is an `.exe`.** Tauri
  signs the NSIS installer *in place*; the `.sig` covers the installer's bytes.
  Re-wrapping it in an allowed `.zip` would invalidate that signature, so the
  zip would need signing as its own artifact — a second signing operation with
  the same key, producing a second thing to keep in sync with the manifest, to
  arrive at the same place.
- **The immutability it buys is already available for free.** `artifacts/<version>/`
  is write-once by policy, enforced by `publish-update.sh` refusing an existing
  version directory, and `publish.json` records the sha256 next to the bytes.
  The only mutable file is `latest.json`, which *must* be mutable — it is the
  pointer — and which is swapped atomically.

The remaining advantage of the media store would be dedup and a URL that proves
its own contents. Neither is worth an extra signing step on the one artifact
whose signature is the entire security model.

---

## Troubleshooting

**"Automatic updates aren't available on this build."** The updater plugin is
not compiled in. The build was missing `BUZZ_UPDATER_PUBLIC_KEY` /
`BUZZ_UPDATER_ENDPOINT`, or it was a debug build. Rebuild via
`render-updater-config.sh --print-env`.

**Check succeeds, always says up to date.** Version ordering. Compare the
installed version against `jq -r .version /opt/buzz/releases/desktop-updates/latest.json`
and re-read the ordering rules above — a prerelease tag on the new version is
the usual cause.

**"Signature verification failed" after download.** The `.sig` in `latest.json`
does not match the bytes being served, or the app was built with a different
public key than the one that signed. `publish-update.sh` verifies the first
after every publish; the second means the app predates a key rotation and must
be reinstalled by hand.

**Nothing at all happens.** Check the client is on the tailnet and that
`curl -fsS https://hermes.tail81f3.ts.net:9443/desktop-updates/latest.json` returns
the manifest from a tailnet machine.
