#!/usr/bin/env python3
"""Mint and validate self-hosted Buzz release manifests.

Sibling of scripts/desktop_release.py, and deliberately the same shape: a
``generate`` that writes an immutable statement of fact, a ``validate`` that
re-derives every claim from scratch and refuses on the first disagreement, and
a hard rule that generate ends by running validate on its own output. A minter
that cannot validate what it just minted is a minter nobody should trust.

What this tool is NOT allowed to do, and why:

* It never builds. ``cargo build`` and ``docker build`` are the operator's (or
  CI's) job. A minter that builds is a minter whose output describes a tree
  that existed for the duration of the build rather than the tree you asked
  about, and the whole point of the manifest is that source_commit and the
  artifacts are the same fact stated twice.
* It never pulls or pushes. Every artifact must already exist locally. The
  deployer enforces the same rule; stating it in both places means neither can
  quietly become the exception.
* It never classifies a migration. An empty ``migrations/`` delta it will call
  ``none``, because that is arithmetic. A non-empty delta demands an explicit
  ``--migrations backward-safe|ack-required`` from a human who read the SQL.
  Guessing here is how you find out at 3am that rollback does not roll back.

Usage:

    scripts/selfhost/mint-manifest.py generate --name projects-merge \\
        --components relay-image,acp,cli --notes "..." --out /tmp/manifest.json

    scripts/selfhost/mint-manifest.py generate --name projects-merge \\
        --stage /opt/buzz/releases/incoming        # atomic inbox drop

    scripts/selfhost/mint-manifest.py validate --manifest /path/to/manifest.json
"""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import re
import shutil
import subprocess
import sys
from datetime import datetime, timezone
from pathlib import Path

# ROOT is the repo the manifest describes. Two levels up from scripts/selfhost/
# by the same reckoning desktop_release.py uses, overridable for the case where
# the minter is copied out of the tree it is minting.
ROOT = Path(os.environ.get("BUZZ_DEPLOY_REPO", Path(__file__).resolve().parent.parent.parent))
SCHEMA_PATH = Path(__file__).resolve().parent / "selfhost-release.schema.json"

# Where the live deployment keeps the answer to "what is running right now".
# Read-only in this tool: the minter describes a candidate, it never touches
# the box. The deployer re-reads all of this at preflight anyway.
BUZZ_ROOT = Path(os.environ.get("BUZZ_ROOT", "/opt/buzz"))
RELAY_ENV = BUZZ_ROOT / "relay" / ".env"

# Build outputs, by convention. These are the paths the manual runbook produces
# with `cargo build --release -p buzz-acp -p buzz-cli`.
DEFAULT_ARTIFACTS = {
    "acp": "target/release/buzz-acp",
    "cli": "target/release/buzz",
}
COMPONENTS = ("relay-image", "acp", "cli")

# The relay is the leg everything else stands on. If it is in the release it
# moves first — encoded here so a hand-edited deploy_order cannot invert it.
FIRST_IF_PRESENT = "relay-image"


def git(*args: str) -> str:
    return subprocess.check_output(["git", *args], cwd=ROOT, text=True).strip()


def sha256_file(path: Path) -> str:
    hasher = hashlib.sha256()
    with path.open("rb") as handle:
        for chunk in iter(lambda: handle.read(8 * 1024 * 1024), b""):
            hasher.update(chunk)
    return hasher.hexdigest()


def docker_image_id(tag: str) -> str:
    """Resolve a local tag to its content id, without ever reaching a registry.

    `docker image inspect` is a purely local lookup — unlike `docker pull` or
    even `docker manifest inspect`, it cannot silently acquire the image it was
    asked about. That property is the reason this is the only docker call the
    minter makes.
    """
    try:
        out = subprocess.check_output(
            ["docker", "image", "inspect", "--format", "{{.Id}}", tag],
            text=True,
            stderr=subprocess.DEVNULL,
        ).strip()
    except (subprocess.CalledProcessError, FileNotFoundError):
        raise SystemExit(
            f"image {tag} is not present in the local docker daemon. "
            "Build it first (DOCKER_BUILDKIT=1 docker build -t "
            f"{tag} .); the minter never builds and never pulls."
        )
    if not re.fullmatch(r"sha256:[0-9a-f]{64}", out):
        raise SystemExit(f"unexpected image id for {tag}: {out!r}")
    return out


def deployed_image() -> str | None:
    """The BUZZ_IMAGE the live relay is configured with, or None.

    Best effort by design: the minter may run on a build host that has no live
    relay, and that is not an error. The deployer re-derives this on the box
    where it actually matters.
    """
    try:
        text = RELAY_ENV.read_text()
    except OSError:
        return None
    match = re.search(r"(?m)^BUZZ_IMAGE=(.+)$", text)
    return match.group(1).strip() if match else None


def deployed_commit() -> str | None:
    """Recover the live commit from the live image tag.

    The deployment's naming convention (buzz-local:<name>-<short-sha>) is not
    decoration: it is the only place the box records which source it is running.
    Parsing it back out is how a release knows what it is replacing.
    """
    image = deployed_image()
    if not image or ":" not in image:
        return None
    short = image.rsplit(":", 1)[1].rsplit("-", 1)[-1]
    if not re.fullmatch(r"[0-9a-f]{7,40}", short):
        return None
    try:
        return git("rev-parse", f"{short}^{{commit}}")
    except subprocess.CalledProcessError:
        # The live commit is not in this worktree — a legitimate state after a
        # branch prune. Say nothing rather than guess.
        return None


def migrations_delta(previous: str, candidate: str) -> list[str]:
    """Files under migrations/ that differ between live and candidate.

    This is the whole schema-neutrality test from the manual runbook, and it is
    a *file* diff rather than a "did anyone touch the DB" judgement because a
    file diff is the only version of the question that has one right answer.
    """
    out = git("diff", "--name-only", f"{previous}..{candidate}", "--", "migrations/")
    return [line for line in out.splitlines() if line]


# --------------------------------------------------------------------------
# Schema checking
#
# The mini-validator below reads selfhost-release.schema.json rather than
# restating it. Two copies of a schema is two schemas, and the second one is
# always the wrong one. It supports exactly the keyword subset that file uses;
# an unknown keyword is a hard error so that extending the schema without
# extending the checker fails loudly instead of silently checking less.
# --------------------------------------------------------------------------

SUPPORTED_KEYWORDS = {
    "$schema", "$id", "$defs", "$ref", "title", "description",
    "type", "const", "enum", "pattern", "not",
    "required", "properties", "additionalProperties", "minProperties",
    "items", "minItems", "uniqueItems", "minimum",
}
JSON_TYPES = {
    "object": dict,
    "array": list,
    "string": str,
    "integer": int,
    "number": (int, float),
    "boolean": bool,
    "null": type(None),
}


def check_schema(value, schema: dict, root: dict, path: str, errors: list[str]) -> None:
    if "$ref" in schema:
        ref = schema["$ref"]
        if not ref.startswith("#/"):
            raise SystemExit(f"unsupported $ref {ref}")
        target = root
        for part in ref[2:].split("/"):
            target = target[part]
        check_schema(value, target, root, path, errors)
        # A sibling description next to $ref is documentation, not a constraint.
        return

    unknown = set(schema) - SUPPORTED_KEYWORDS
    if unknown:
        raise SystemExit(f"schema at {path} uses unsupported keywords: {sorted(unknown)}")

    if "type" in schema:
        names = schema["type"] if isinstance(schema["type"], list) else [schema["type"]]
        allowed = tuple(JSON_TYPES[n] for n in names)
        # bool is a subclass of int in Python; JSON Schema disagrees.
        ok = isinstance(value, allowed) and not (
            isinstance(value, bool) and "boolean" not in names
        )
        if not ok:
            errors.append(f"{path}: expected {'|'.join(names)}, got {type(value).__name__}")
            return

    if "const" in schema and value != schema["const"]:
        errors.append(f"{path}: expected {schema['const']!r}, got {value!r}")
    if "enum" in schema and value not in schema["enum"]:
        errors.append(f"{path}: {value!r} not one of {schema['enum']}")
    if "pattern" in schema and isinstance(value, str):
        if not re.search(schema["pattern"], value):
            errors.append(f"{path}: {value!r} does not match {schema['pattern']}")
    if "not" in schema and isinstance(value, str) and "pattern" in schema["not"]:
        if re.search(schema["not"]["pattern"], value):
            errors.append(f"{path}: {value!r} matches forbidden {schema['not']['pattern']}")
    if "minimum" in schema and isinstance(value, (int, float)) and value < schema["minimum"]:
        errors.append(f"{path}: {value} below minimum {schema['minimum']}")

    if isinstance(value, dict):
        for key in schema.get("required", []):
            if key not in value:
                errors.append(f"{path}: missing required field {key!r}")
        if "minProperties" in schema and len(value) < schema["minProperties"]:
            errors.append(f"{path}: needs at least {schema['minProperties']} entries")
        props = schema.get("properties", {})
        if schema.get("additionalProperties") is False:
            for key in value:
                if key not in props:
                    errors.append(f"{path}: unexpected field {key!r}")
        for key, sub in props.items():
            if key in value:
                check_schema(value[key], sub, root, f"{path}.{key}", errors)

    if isinstance(value, list):
        if "minItems" in schema and len(value) < schema["minItems"]:
            errors.append(f"{path}: needs at least {schema['minItems']} items")
        if schema.get("uniqueItems") and len(value) != len({json.dumps(v, sort_keys=True) for v in value}):
            errors.append(f"{path}: entries must be unique")
        if "items" in schema:
            for index, item in enumerate(value):
                check_schema(item, schema["items"], root, f"{path}[{index}]", errors)


# --------------------------------------------------------------------------
# Validation
# --------------------------------------------------------------------------


def validate_manifest(
    data: dict,
    *,
    artifact_root: Path,
    check_artifacts: bool = True,
    check_repo: bool = True,
    check_image: bool = True,
) -> list[str]:
    """Every check the deployer relies on, in one place both tools can call.

    Returns a list of problems rather than raising on the first, because an
    operator fixing a manifest wants the whole list, and because a deployer
    reporting one problem per run turns a five minute fix into an afternoon.
    """
    schema = json.loads(SCHEMA_PATH.read_text())
    errors: list[str] = []
    check_schema(data, schema, schema, "manifest", errors)
    if errors:
        # Semantic checks below index into fields the schema just said are
        # missing or mistyped. Stop here; the operator has enough to act on.
        return errors

    components = data["components"]
    order = data["deploy_order"]

    if sorted(order) != sorted(components):
        errors.append(
            f"deploy_order {order} is not a permutation of components "
            f"{sorted(components)} — every declared component must be placed, "
            "and nothing may be placed that was not declared"
        )
    elif FIRST_IF_PRESENT in components and order[0] != FIRST_IF_PRESENT:
        errors.append(
            f"deploy_order must start with {FIRST_IF_PRESENT} when it is part of "
            "the release: the relay is the leg everything else stands on"
        )

    if data["promoted_by"] is not None:
        errors.append(
            "promoted_by must be null in Phase 1. Nothing verifies a promotion "
            "claim yet, and an unverified claim that reads as authority in an "
            "audit log is worse than no claim at all. See gate_authorization() "
            "in deploy.sh for the Phase 2 seam."
        )

    if check_repo:
        try:
            head = git("rev-parse", "HEAD")
        except subprocess.CalledProcessError as exc:
            errors.append(f"cannot read repo HEAD at {ROOT}: {exc}")
        else:
            if head != data["source_commit"]:
                errors.append(
                    f"source_commit {data['source_commit'][:12]} does not match "
                    f"{ROOT} HEAD {head[:12]} — the manifest describes a tree "
                    "this checkout is not on"
                )

    if check_artifacts:
        for name in ("acp", "cli"):
            spec = components.get(name)
            if not spec:
                continue
            path = artifact_root / spec["artifact"]
            # Belt and braces over the schema's traversal-free pattern: resolve
            # and confirm containment, because the deployer that consumes this
            # runs as root and symlinks resolve at open() time, not at parse.
            try:
                resolved = path.resolve()
                resolved.relative_to(artifact_root.resolve())
            except (OSError, ValueError):
                errors.append(f"{name}: {spec['artifact']} resolves outside {artifact_root}")
                continue
            if not resolved.is_file():
                errors.append(f"{name}: artifact {resolved} does not exist")
                continue
            size = resolved.stat().st_size
            if size != spec["bytes"]:
                errors.append(f"{name}: {resolved} is {size} bytes, manifest says {spec['bytes']}")
                continue
            digest = sha256_file(resolved)
            if digest != spec["sha256"]:
                errors.append(
                    f"{name}: {resolved} hashes to {digest[:16]}…, manifest says "
                    f"{spec['sha256'][:16]}…"
                )

    if check_image and "relay-image" in components:
        spec = components["relay-image"]
        try:
            actual = docker_image_id(spec["image"])
        except SystemExit as exc:
            errors.append(str(exc))
        else:
            if actual != spec["image_id"]:
                errors.append(
                    f"relay-image: tag {spec['image']} now resolves to {actual[:19]}…, "
                    f"manifest recorded {spec['image_id'][:19]}… — the tag was "
                    "moved after minting; re-mint or re-tag, do not deploy"
                )

    return errors


# --------------------------------------------------------------------------
# Generation
# --------------------------------------------------------------------------


def worktree_is_dirty() -> str:
    # Re-indent rather than pass git's output through: git() strips, which eats
    # the leading status column of the first line and makes the report lie about
    # one file's state.
    out = git("status", "--porcelain", "--untracked-files=no")
    return "\n".join(f"  {line.strip()}" for line in out.splitlines()) if out else ""


def build_manifest(args: argparse.Namespace) -> tuple[dict, Path]:
    selected = [c.strip() for c in args.components.split(",") if c.strip()]
    unknown = [c for c in selected if c not in COMPONENTS]
    if unknown:
        raise SystemExit(f"unknown component(s): {', '.join(unknown)}; choose from {', '.join(COMPONENTS)}")
    if not selected:
        raise SystemExit("at least one component is required")

    dirty = worktree_is_dirty()
    if dirty and not args.allow_dirty:
        raise SystemExit(
            "worktree has uncommitted tracked changes; source_commit would name a "
            "tree that does not match the artifacts:\n" + dirty +
            "\nCommit, or pass --allow-dirty if you truly know the binaries predate the edits."
        )

    head = git("rev-parse", "HEAD")
    short = head[:8]
    artifact_root = Path(args.artifact_root).resolve() if args.artifact_root else ROOT

    components: dict[str, dict] = {}
    if "relay-image" in selected:
        tag = args.image or f"buzz-local:{args.name}-{short}"
        components["relay-image"] = {"image": tag, "image_id": docker_image_id(tag)}
    for name in ("acp", "cli"):
        if name not in selected:
            continue
        rel = DEFAULT_ARTIFACTS[name]
        path = artifact_root / rel
        if not path.is_file():
            raise SystemExit(
                f"{name}: {path} does not exist. Build it first "
                "(cargo build --release -p buzz-acp -p buzz-cli); the minter never builds."
            )
        components[name] = {
            "artifact": rel,
            "sha256": sha256_file(path),
            "bytes": path.stat().st_size,
        }

    # Order: relay first if present, then the binaries in their declared order.
    order = [c for c in COMPONENTS if c in components]

    previous = deployed_commit()
    migrations = args.migrations
    if migrations == "auto":
        if previous is None:
            raise SystemExit(
                "cannot auto-classify migrations: the live commit is unknown "
                f"(no readable {RELAY_ENV}, or its BUZZ_IMAGE tag is not in this "
                "worktree). Pass --migrations explicitly."
            )
        delta = migrations_delta(previous, head)
        if delta:
            raise SystemExit(
                "migrations/ delta is non-empty:\n  " + "\n  ".join(delta) +
                "\nClassify it yourself: --migrations backward-safe (every change is "
                "expand-only, the previous binary still runs against the new schema, "
                "rollback is clean) or --migrations ack-required (it is not, and "
                "rollback needs a database restore). This tool will not read SQL for you."
            )
        migrations = "none"

    manifest = {
        "schema": 1,
        "name": args.name,
        "source_commit": head,
        "previous_commit": previous,
        "created_at": datetime.now(timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ"),
        "components": components,
        "deploy_order": order,
        "migrations": migrations,
        # Phase 1 has no authorization authority to record. See the schema.
        "promoted_by": None,
        "notes": args.notes,
    }
    return manifest, artifact_root


def stage(manifest: dict, artifact_root: Path, inbox: Path) -> Path:
    """Assemble a self-contained release directory and hand it over atomically.

    The staging-then-rename dance is the drop protocol the deployer's .path
    unit depends on. systemd triggers on `incoming/*/manifest.json` existing;
    if we wrote the manifest in place and then spent thirty seconds copying two
    17 MB binaries next to it, the deployer would wake up and find a release
    that is not all there yet. rename(2) within one filesystem is atomic, so
    the glob only ever matches a complete drop.
    """
    inbox.mkdir(parents=True, exist_ok=True)
    final = inbox / manifest["name"]
    staging = inbox / f".staging-{manifest['name']}"
    if final.exists():
        raise SystemExit(f"{final} already exists; a release name is used once")
    if staging.exists():
        shutil.rmtree(staging)
    staging.mkdir(mode=0o755)

    staged = json.loads(json.dumps(manifest))
    sums = []
    for name in ("acp", "cli"):
        spec = staged["components"].get(name)
        if not spec:
            continue
        src = artifact_root / spec["artifact"]
        basename = Path(spec["artifact"]).name
        shutil.copy2(src, staging / basename)
        # Artifacts now sit beside the manifest, so the recorded path becomes a
        # bare filename and the artifact root is simply "wherever this is".
        spec["artifact"] = basename
        sums.append(f"{spec['sha256']}  {basename}")

    (staging / "manifest.json").write_text(json.dumps(staged, indent=2) + "\n")
    if sums:
        (staging / "SHA256SUMS").write_text("\n".join(sums) + "\n")

    problems = validate_manifest(
        json.loads((staging / "manifest.json").read_text()),
        artifact_root=staging,
        check_repo=True,
        check_image="relay-image" in staged["components"],
    )
    if problems:
        shutil.rmtree(staging)
        raise SystemExit("staged manifest failed validation:\n  " + "\n  ".join(problems))

    staging.rename(final)
    return final


def generate(args: argparse.Namespace) -> None:
    manifest, artifact_root = build_manifest(args)

    if args.stage:
        final = stage(manifest, artifact_root, Path(args.stage))
        print(json.dumps({"staged": str(final), "name": manifest["name"],
                          "source_commit": manifest["source_commit"],
                          "components": manifest["deploy_order"],
                          "migrations": manifest["migrations"]}, indent=2))
        return

    problems = validate_manifest(
        manifest,
        artifact_root=artifact_root,
        check_image="relay-image" in manifest["components"],
    )
    if problems:
        raise SystemExit("minted manifest failed its own validation:\n  " + "\n  ".join(problems))

    text = json.dumps(manifest, indent=2) + "\n"
    if args.out:
        out = Path(args.out)
        out.parent.mkdir(parents=True, exist_ok=True)
        out.write_text(text)
        print(f"wrote {out} ({manifest['name']} @ {manifest['source_commit'][:12]}, "
              f"components: {', '.join(manifest['deploy_order'])}, "
              f"migrations: {manifest['migrations']})", file=sys.stderr)
    else:
        sys.stdout.write(text)


def validate(args: argparse.Namespace) -> None:
    path = Path(args.manifest)
    try:
        data = json.loads(path.read_text())
    except (OSError, json.JSONDecodeError) as exc:
        raise SystemExit(f"cannot read manifest {path}: {exc}")
    artifact_root = Path(args.artifact_root).resolve() if args.artifact_root else path.resolve().parent
    problems = validate_manifest(
        data,
        artifact_root=artifact_root,
        check_artifacts=not args.no_artifacts,
        check_repo=not args.no_repo,
        check_image=not args.no_image and "relay-image" in data.get("components", {}),
    )
    if problems:
        raise SystemExit("manifest invalid:\n  " + "\n  ".join(problems))
    print(
        f"validated selfhost release manifest {path} "
        f"({data['name']} @ {data['source_commit'][:12]}, "
        f"components: {', '.join(data['deploy_order'])}, "
        f"migrations: {data['migrations']})"
    )


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    sub = parser.add_subparsers(dest="command", required=True)

    gen = sub.add_parser("generate", help="mint a manifest from the current worktree and built artifacts")
    gen.add_argument("--name", required=True, help="release label; becomes the image tag suffix, backup dir and inbox dir")
    gen.add_argument("--components", default="relay-image,acp,cli",
                     help="comma-separated subset of relay-image,acp,cli (default: all three)")
    gen.add_argument("--migrations", default="auto",
                     choices=["auto", "none", "backward-safe", "ack-required"],
                     help="auto succeeds only when the migrations/ delta is empty")
    gen.add_argument("--image", help="override the derived buzz-local:<name>-<short> tag")
    gen.add_argument("--artifact-root", help="root for target/release/... lookups (default: the repo)")
    gen.add_argument("--notes", default="", help="free text for whoever reads the deploy journal")
    gen.add_argument("--out", help="write the manifest here (default: stdout)")
    gen.add_argument("--stage", help="assemble a self-contained release dir under this inbox and rename it into place")
    gen.add_argument("--allow-dirty", action="store_true",
                     help="mint despite uncommitted tracked changes (source_commit will not describe the tree)")

    val = sub.add_parser("validate", help="re-derive every claim in a manifest")
    val.add_argument("--manifest", required=True)
    val.add_argument("--artifact-root", help="default: the directory holding the manifest")
    val.add_argument("--no-artifacts", action="store_true", help="skip hashing (structure only)")
    val.add_argument("--no-repo", action="store_true", help="skip the source_commit/HEAD check")
    val.add_argument("--no-image", action="store_true", help="skip the docker image identity check")

    args = parser.parse_args()
    generate(args) if args.command == "generate" else validate(args)


if __name__ == "__main__":
    main()
