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
  "promoted_by": {
    "root": "c3f1…64 hex…",
    "revision": "9b02…64 hex…",
    "owner": "7a41…64 hex…"
  },
  "staging_stamp": {
    "stamp_sha256": "397d199181ea9eb01beec3cb1dfd3438d9814f77df785431064f367300d32586",
    "verdict": "promotable",
    "stamped_at": "2026-08-04T17:05:00Z",
    "waived": 0
  },
  "notes": "Projects feature merge. Watch buzz-claude for enrolment replay."
}
```

That release drop also contains `promote-stamp.json` — the stamp itself, staged
beside the manifest and re-hashed by the deployer against `stamp_sha256`. The
summary above is for reading; the file is the evidence.

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
  "staging_stamp": null,
  "notes": "Fixes the 502 on /pair. No agent or CLI change."
}
```

Both promotion fields are `null` there, which is a legal manifest and an
**unshippable** one under `--inbox`: the deployer refuses an unattended deploy
that nothing judged and nobody approved. A hotfix in that shape has to be run by
hand with `--allow-unstamped --allow-unattributed`, which is the correct amount
of friction for "ship something at 3am that no gate has seen".

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
| `promoted_by` | yes | `{root, revision, owner}` — where the owner's approval can be re-derived from, or `null` for a release nobody approved. See below. |
| `staging_stamp` | no | `{stamp_sha256, verdict, stamped_at, waived}` — a summary of the promote stamp the gates issued, or `null`. Optional in the schema so that older manifests still validate; **required by the deployer under `--inbox`.** See below. |
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

## The two promotion claims

`promoted_by` says *a human with a key approved this*. `staging_stamp` says
*the gates passed*. They are separate fields because they are separate claims,
and a pipeline that collapsed them would leave an audit log unable to say which
one was actually made.

### `promoted_by` — where the approval can be found

```json
"promoted_by": {
  "root": "<64-hex pull-request root event>",
  "revision": "<64-hex 1619 update event, or the root's own id>",
  "owner": "<64-hex pubkey whose approval counts>"
}
```

Three ids, and deliberately **not** a signature. A signature copied into a
manifest proves only that whoever wrote the manifest could copy a signature, and
re-checking it locally would be re-checking the manifest against itself. These
fields name a *question* instead:

```bash
buzz projects release-check --root <root> --revision <revision> --owner <owner>
```

The deployer asks it of the live relay using the **deployed** `/opt/buzz/bin/buzz`
— never the candidate staged in the drop, because a release must not vouch for
itself — and requires exit 0 **and** a verdict whose `commit` equals this
manifest's `source_commit`. That last equality is what stops an approval of one
commit authorising a release of another.

`null` means nobody approved it. That is legal, refused outright under
`--inbox`, and allowed by hand with `--allow-unattributed`.

### `staging_stamp` — what the gates found

```json
"staging_stamp": {
  "stamp_sha256": "<64-hex of promote-stamp.json as staged>",
  "verdict": "promotable" | "promotable_with_waivers",
  "stamped_at": "<the stamp's own UTC timestamp>",
  "waived": 0
}
```

A *summary*. The evidence is `promote-stamp.json`
(`schema: "buzz.staging.promote-stamp/v1"`, written by
`scripts/selfhost/gates/stamp.sh`), which `--stage` copies into the release drop
beside `manifest.json` under that fixed name. The deployer re-hashes the file
against `stamp_sha256` and re-derives every claim from the original, so the
summary is never load-bearing on its own.

The minter refuses to embed a stamp that is not `promotable` or
`promotable_with_waivers`, whose `binding.bound` is not `true`, whose
`candidate.source_commit` is not this manifest's, whose `acp`/`cli` hashes
disagree with the components being shipped, or whose `verdict` and waiver count
contradict each other. `waived` is summed from
`gates[].details.waivers.applied` rather than read off a total, because a total
is a claim and a list is evidence.

The filename is fixed in the code, never a path in the manifest — the same rule
that keeps `install_path` out of the format. `deploy.sh --stamp-file <path>`
exists for the un-staged shape (`--out`, where the stamp lives wherever the
operator keeps it) and is a usage error under `--inbox`, where the drop is
self-contained by construction.

**The relay does not hash-join.** The gates test a `buzz-relay` built from
source; the manifest ships a Docker image id. Different bytes, same commit — so
the relay half of a stamp is bound by `source_commit` alone, and the deployer
prints `relay-image=COMMIT-ONLY` rather than implying a check it did not make.

See `docs/selfhost-releases.md` for the full enforcement matrix.

## Minting

```bash
# From the build worktree, after docker build, cargo build --release, and a
# green run of scripts/selfhost/gates/run-gates.sh.
scripts/selfhost/mint-manifest.py generate \
  --name projects-merge \
  --components relay-image,acp,cli \
  --staging-stamp scripts/selfhost/gates/promote-stamp.json \
  --promoted-by <root>:<revision>:<owner> \
  --notes "…" \
  --stage /opt/buzz/releases/incoming

# Or to a file, for a deploy driven by hand:
scripts/selfhost/mint-manifest.py generate --name projects-merge --out /tmp/m.json
scripts/selfhost/deploy.sh --artifact-root "$PWD" /tmp/m.json          # dry run
scripts/selfhost/deploy.sh --artifact-root "$PWD" --execute /tmp/m.json
```

A drop minted without `--staging-stamp` and `--promoted-by` is refused by an
unattended deploy. That is the point: the flags are how the two claims get made,
and a release that makes neither is one the deployer will only ship with a human
typing `--allow-unstamped --allow-unattributed`.

The minter refuses an uncommitted worktree (`--allow-dirty` overrides, and
records nothing about the edits — use it only when you genuinely know the
binaries predate them), refuses to build or pull anything, refuses to classify a
non-empty migrations delta for you, and refuses to embed a stamp that does not
describe this release. It ends by validating its own output: a minter that
cannot validate what it just minted is a minter nobody should trust.

Re-validate at any time — this is also (bar two deliberate exceptions) exactly
what `deploy.sh` runs at preflight:

```bash
scripts/selfhost/mint-manifest.py validate --manifest /tmp/m.json --artifact-root "$PWD"
```

`deploy.sh` passes `--no-repo` and `--no-stamp-file` and takes those two checks
back itself, for two different reasons. `--no-repo` because HEAD equality is
only provable in the build worktree and the deployer may be a systemd unit
pointed at the shared object store. `--no-stamp-file` because the check is
right, but its *name* matters: "the promote stamp did not verify" has to reach
the journal as `preflight.stamp` rather than as one line inside a validator's
output. Step names are an interface.
