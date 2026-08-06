import assert from "node:assert/strict";
import test from "node:test";

import {
  parseRepoCoordinate,
  repoCoordinateToProjectId,
} from "./repoCoordinate.ts";

const OWNER = "a".repeat(64);
const UPPER_OWNER = "A".repeat(64);

test("a repo coordinate splits into its owner and repo id", () => {
  assert.deepEqual(parseRepoCoordinate(`30617:${OWNER}:buzz`), {
    owner: OWNER,
    identifier: "buzz",
  });
});

test("the owner is lowercased so pubkey comparisons match", () => {
  // Every other pubkey comparison in the app goes through normalizePubkey,
  // which lowercases; an owner parsed straight out of a tag has to agree.
  assert.deepEqual(parseRepoCoordinate(`30617:${UPPER_OWNER}:buzz`), {
    owner: OWNER,
    identifier: "buzz",
  });
});

test("a repo id containing a colon survives the split", () => {
  // The `d` tag is opaque and may contain colons — a greedy split would
  // truncate `release:train` to `release` and address the wrong repository.
  assert.deepEqual(parseRepoCoordinate(`30617:${OWNER}:release:train`), {
    owner: OWNER,
    identifier: "release:train",
  });
});

test("a coordinate for another kind is refused", () => {
  // An `a` tag naming some other addressable event must not be read as a
  // repository, or the failure downstream would again name the wrong thing.
  assert.equal(parseRepoCoordinate(`30023:${OWNER}:buzz`), null);
  assert.equal(parseRepoCoordinate(`1618:${OWNER}:buzz`), null);
});

test("malformed coordinates are refused rather than guessed at", () => {
  assert.equal(parseRepoCoordinate(null), null);
  assert.equal(parseRepoCoordinate(undefined), null);
  assert.equal(parseRepoCoordinate(""), null);
  assert.equal(parseRepoCoordinate("30617"), null);
  assert.equal(parseRepoCoordinate(`30617:${OWNER}`), null);
  // Empty repo id.
  assert.equal(parseRepoCoordinate(`30617:${OWNER}:`), null);
  // Owner that is not a 64-hex pubkey.
  assert.equal(parseRepoCoordinate("30617:nothex:buzz"), null);
  assert.equal(parseRepoCoordinate(`30617:${"z".repeat(64)}:buzz`), null);
});

test("a coordinate converts to the route id shape Project.id uses", () => {
  // `useProjectQuery` resolves `<owner>:<dtag>` — the coordinate without its
  // kind prefix. This is the bridge from "the PR names its repo" to "the app
  // can fetch that repo's announcement".
  assert.equal(
    repoCoordinateToProjectId(`30617:${OWNER}:buzz`),
    `${OWNER}:buzz`,
  );
  assert.equal(repoCoordinateToProjectId("30617:nothex:buzz"), null);
  assert.equal(repoCoordinateToProjectId(null), null);
});
