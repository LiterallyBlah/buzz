# The self-hosted release manifest

A manifest is a **statement of fact about artifacts that already exist**. It is
never a request to go build or fetch something. Everything in the format
follows from that one sentence: there are no build instructions, no registry
references, no URLs, and no install paths — because each of those would turn a
description into an instruction, and an instruction is a thing an author can
use to make the deployer do something the author was not authorised to do.

It is the sibling of `.release/desktop-candidate.json`, and shares its
discipline: an integer `schema`, immutable once minted, and a validator that
re-derives every claim rather than trusting the file.

* Normative schema: [`selfhost-release.schema.json`](selfhost-release.schema.json)
* Minter and validator: [`mint-manifest.py`](mint-manifest.py)
* Executor: [`deploy.sh`](deploy.sh)
* Policy: [`docs/selfhost-releases.md`](../../docs/selfhost-releases.md)

The schema file is the single source of truth. `mint-manifest.py` reads it at
runtime rather than restating it, so the two cannot drift; if you add a field,
add it there and the validator picks it up.

## Example

A full three-component release, schema-neutral:

```json
{
  "schema": 1,
  "name": "projects-merge",
  "source_commit": "0fa54b3cfc7e7041b13e6a7d13a74dd0c18e5b89",
  "previous_commit": "13acbaf2982cec4497f26420de4e88ec2f05b3b4",
  "created_at": "2026-08-04T16:38:42Z",
  "components": {
    "relay-image": {
      "image": "buzz-local:projects-merge-0fa54b3c",
      "image_id": "sha256:2a7d2fd12cb82e0789df2f195186256859c997df60f6f6ccaea4f54925fab2dc"
    },
    "acp": {
      "artifact": "buzz-acp",
      "sha256": "75dd37c3f0a80208d5235d9a305faf2a54fbf4ec832a1bf91b06797e1d5f3bb1",
      "bytes": 17224144
    },
    "cli": {
      "artifact": "buzz",
      "sha256": "b421e22cbe2e6f49997f677a45c045515e638ae9e8f1312c58b37d3b3963c473",
      "bytes": 16919256
    }
  },
  "deploy_order": ["relay-image", "acp", "cli"],
  "migrations": "none",
  "promoted_by": null,
  "notes": "Projects feature merge. Watch buzz-claude for enrolment replay."
}
```

A relay-only hotfix is a complete, normal manifest too — and is the shape you
should reach for by default, because the smallest change that fixes the problem
is the safest change to ship:

```json
{
  "schema": 1,
  "name": "relay-hotfix",
  "source_commit": "…",
  "components": { "relay-image": { "image": "buzz-local:relay-hotfix-abc12345", "image_id": "sha256:…" } },
  "deploy_order": ["relay-image"],
  "migrations": "none",
  "promoted_by": null,
  "notes": "Fixes the 502 on /pair. No agent or CLI change."
}
```

## Fields

| Field | Required | Meaning |
|---|---|---|
| `schema` | yes | Always `1`. The deployer refuses anything it was not written against; a deployer that guesses at an unknown field is a deployer that installs the wrong binary. |
| `name` | yes | Release label. Becomes a Docker tag component, a backup directory name, an inbox directory name and part of a rollback binary's filename, so the pattern is narrow on purpose — validated once here rather than sanitised at four call sites. |
| `source_commit` | yes | Full 40-hex commit the artifacts were built from. |
| `previous_commit` | no | What was believed live at mint time. **Advisory.** The deployer re-derives the live commit from the running `BUZZ_IMAGE` tag, because the box is the authority on what the box is running. |
| `created_at` | no | UTC ISO-8601 mint time. Advisory. |
| `components` | yes | A subset of `relay-image`, `acp`, `cli`. Identity only — see below. |
| `deploy_order` | yes | A permutation of the `components` keys. Not a subset, not a superset. `relay-image` must be first if present. |
| `migrations` | yes | `none` \| `backward-safe` \| `ack-required`. See below. |
| `promoted_by` | yes | **Reserved — Phase 2 authorization slot.** Must be `null` today. |
| `notes` | yes | Free text for whoever reads the journal at 3am. May be empty; an empty `notes` on a migration-bearing release is a smell. |

## Component identity, and what is deliberately absent

Each component carries only enough to answer "is this the artifact that was
tested?":

* `relay-image` — `image` (a tag in the local-only `buzz-local:` namespace) and
  `image_id` (docker's content id). A tag is a mutable pointer; the id is the
  identity. The deployer refuses if the tag has been moved since minting.
* `acp` / `cli` — `artifact` (a **relative**, traversal-free path resolved
  against the artifact root), `sha256` and `bytes`.

There is no `install_path`, no `owner` and no `mode`, and their absence is the
single most important security property of the format. `deploy.sh` runs as
root. If a manifest could name its own install path it could name
`/etc/systemd/system/anything.service`, and the release pipeline would become
an arbitrary-root-write pipeline. **The manifest says what an artifact is; the
deployer's own table says where it goes:**

| Component | Destination | Owner | Mode | Restart |
|---|---|---|---|---|
| `relay-image` | `BUZZ_IMAGE=` in `/opt/buzz/relay/.env` | — | — | relay container, force-recreated |
| `acp` | `/opt/buzz/bin/buzz-acp` | `root:root` | `0755` | `buzz-claude.service`, then `buzz-codex.service` |
| `cli` | `/opt/buzz/bin/buzz` | `hermes:hermes` | `0755` | none |

`buzz-acp` is root-owned so the hermes-run agents cannot rewrite the binary
they are about to execute. The CLI is hermes-owned because hermes runs it
interactively and root ownership would only add `sudo` to every invocation.

## The artifact root

`artifact` paths resolve against the artifact root, which is `--artifact-root`
if given and otherwise **the directory holding the manifest**. Two shapes are
supported and they are not interchangeable:

* **Staged drop** (`mint-manifest.py --stage`): artifacts sit beside the
  manifest, so `artifact` is a bare filename. This is what the inbox and the
  systemd path unit consume.
* **In-tree** (`mint-manifest.py --out`): `artifact` is `target/release/…` and
  the caller passes `--artifact-root <repo>`. This is the shape for a human
  running a deploy from their build worktree.

Absolute paths and `..` are rejected by the schema and re-checked by the
validator after symlink resolution, because the schema sees a string and
`open()` sees a filesystem.

## Migration classes

The class is a human judgement about SQL. Nothing derives it automatically, and
the deployer only ever checks that the manifest **does not understate** what
`git diff --name-only <live>..<candidate> -- migrations/` actually shows.
Overstating is always safe: a manifest that asks for a full backup it did not
need costs a few minutes; the opposite costs the database.

| Class | When | What the deployer does |
|---|---|---|
| `none` | the delta is empty | Schema-neutral fast lane. Binary/env backup only. The only class an unattended inbox drop may carry. |
| `backward-safe` | delta is non-empty, every change is expand-only — the previous binary still runs against the new schema | Requires a fresh postgres-inclusive backup (`backup-buzz-latest.py`) before proceeding. Binary rollback is clean. |
| `ack-required` | delta is non-empty and at least one change the previous binary cannot tolerate | Requires the fresh full backup **and** `--ack-migrations` on the command line. Binary rollback will **not** restore service without a database restore. |

`--ack-migrations` is a command-line act rather than a manifest field on
purpose. The manifest is authored by whoever built the release — possibly an
agent — and a document cannot be its own authorisation. Because the systemd
unit does not pass the flag, an inbox drop can never run a forward-only
migration by itself.

This relay runs `BUZZ_AUTO_MIGRATE=true`: the new relay image applies pending
migrations on startup. That is why the class matters at deploy time rather than
at some later "run the migrations" step — by the time the relay gate passes,
the schema has already moved.

## `promoted_by` — the Phase 2 seam

Must be `null` today, and a non-null value is **refused rather than ignored**.
A manifest that claims an approval nothing verifies is strictly worse than one
that claims nothing, because it reads like authority to the next person who
greps the audit log.

Phase 2 replaces the single `gate_authorization()` function in `deploy.sh` with
real signature verification and drops the `null` allowance from the schema in
the same change. The anticipated shape is already documented in the schema:

```json
"promoted_by": {
  "kind": "owner-approval",
  "pubkey": "<64-hex owner>",
  "event": "<64-hex approval event>",
  "sig": "<128-hex>"
}
```

One function, one caller, one schema field. See `docs/selfhost-releases.md` for
the rest of the Phase 2–5 seams.

## Minting

```bash
# From the build worktree, after docker build and cargo build --release.
scripts/selfhost/mint-manifest.py generate \
  --name projects-merge \
  --components relay-image,acp,cli \
  --notes "…" \
  --stage /opt/buzz/releases/incoming

# Or to a file, for a deploy driven by hand:
scripts/selfhost/mint-manifest.py generate --name projects-merge --out /tmp/m.json
scripts/selfhost/deploy.sh --artifact-root "$PWD" /tmp/m.json          # dry run
scripts/selfhost/deploy.sh --artifact-root "$PWD" --execute /tmp/m.json
```

The minter refuses an uncommitted worktree (`--allow-dirty` overrides, and
records nothing about the edits — use it only when you genuinely know the
binaries predate them), refuses to build or pull anything, and refuses to
classify a non-empty migrations delta for you. It ends by validating its own
output: a minter that cannot validate what it just minted is a minter nobody
should trust.

Re-validate at any time — this is also exactly what `deploy.sh` runs at
preflight:

```bash
scripts/selfhost/mint-manifest.py validate --manifest /tmp/m.json --artifact-root "$PWD"
```
