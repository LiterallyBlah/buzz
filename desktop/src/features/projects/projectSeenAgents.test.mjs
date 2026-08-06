import assert from "node:assert/strict";
import test from "node:test";

/**
 * A stand-in for the webview's localStorage, installed before the module under
 * test is imported so its module-init read path is exercised for real. The
 * store reads `globalThis.localStorage` directly (the idiom used by
 * `threadViewModePreference`), so this is the whole seam.
 */
class MemoryStorage {
  values = new Map();
  getItem(key) {
    return this.values.get(key) ?? null;
  }
  setItem(key, value) {
    this.values.set(key, String(value));
  }
  removeItem(key) {
    this.values.delete(key);
  }
}

const storage = new MemoryStorage();
const STORAGE_KEY = "buzz-project-seen-agents.v1";
const RESTORED_AT = Date.now() - 60_000;
storage.setItem(
  STORAGE_KEY,
  JSON.stringify({
    version: 1,
    // Recent, and stored under a mixed-case pubkey: the init path runs both
    // the parse and the prune, and a fixture aged past the TTL would prove
    // only that pruning works.
    roots: { "root-restored": { ["A".repeat(64)]: RESTORED_AT } },
  }),
);
globalThis.localStorage = storage;

const {
  EMPTY_SEEN_AGENTS_STORE,
  getProjectSeenAgents,
  MAX_SEEN_AGENTS_PER_ROOT,
  MAX_SEEN_ROOTS,
  parseSeenAgentsStore,
  pruneSeenAgentsStore,
  recordProjectSeenAgents,
  recordSeenAgentsIn,
  resetProjectSeenAgentsForTests,
  SEEN_AGENT_REFRESH_MS,
  SEEN_AGENT_TTL_MS,
} = await import("./projectSeenAgents.ts");

const AGENT_A = "a".repeat(64);
const AGENT_B = "b".repeat(64);

// Captured here, not inside the test: `beforeEach` is registered on the file's
// root suite and so runs before *every* test regardless of declaration order,
// including one declared above it. Module init happens exactly once and its
// result has to be grabbed before the first reset wipes it.
const initialStore = getProjectSeenAgents();

test.beforeEach(() => {
  resetProjectSeenAgentsForTests();
});

test("restores the persisted memory at module init", () => {
  assert.deepEqual(initialStore.roots["root-restored"], {
    [AGENT_A]: RESTORED_AT,
  });
});

test("records a sighting and persists it", () => {
  recordProjectSeenAgents("root-1", [AGENT_A], 10_000);

  assert.deepEqual(getProjectSeenAgents().roots["root-1"], {
    [AGENT_A]: 10_000,
  });
  assert.deepEqual(JSON.parse(storage.getItem(STORAGE_KEY)), {
    version: 1,
    roots: { "root-1": { [AGENT_A]: 10_000 } },
  });
});

test("a repeat sighting inside the refresh window is an exact no-op", () => {
  recordProjectSeenAgents("root-1", [AGENT_A], 10_000);
  const before = getProjectSeenAgents();

  // This is the live hook's every-two-seconds case. Identity equality is the
  // assertion that matters: a new object here would re-render every subscriber
  // for the length of every turn.
  recordProjectSeenAgents("root-1", [AGENT_A], 10_000 + 1);
  recordProjectSeenAgents(
    "root-1",
    [AGENT_A],
    10_000 + SEEN_AGENT_REFRESH_MS - 1,
  );
  assert.equal(getProjectSeenAgents(), before);

  recordProjectSeenAgents("root-1", [AGENT_A], 10_000 + SEEN_AGENT_REFRESH_MS);
  assert.notEqual(getProjectSeenAgents(), before);
});

test("a newly seen agent lands immediately, refresh window or not", () => {
  recordProjectSeenAgents("root-1", [AGENT_A], 10_000);
  recordProjectSeenAgents("root-1", [AGENT_A, AGENT_B], 10_001);

  assert.deepEqual(getProjectSeenAgents().roots["root-1"], {
    [AGENT_A]: 10_000,
    [AGENT_B]: 10_001,
  });
});

test("changes the snapshot only when the sighting was new", () => {
  // Snapshot identity is what `useSyncExternalStore` re-renders on, so this is
  // the render-cost contract stated directly.
  let changes = 0;
  const roots = [];
  const record = (nowMs, pubkeys) => {
    const before = getProjectSeenAgents();
    recordProjectSeenAgents("root-1", pubkeys, nowMs);
    if (getProjectSeenAgents() !== before) changes += 1;
    roots.push(getProjectSeenAgents().roots["root-1"]);
  };

  record(1_000, [AGENT_A]);
  record(1_001, [AGENT_A]);
  record(1_002, [AGENT_B]);
  assert.equal(changes, 2);
  // The untouched pass hands back the very same sub-object.
  assert.equal(roots[0], roots[1]);
});

test("leaves other roots referentially stable", () => {
  recordProjectSeenAgents("root-1", [AGENT_A], 10_000);
  const rootOne = getProjectSeenAgents().roots["root-1"];

  recordProjectSeenAgents("root-2", [AGENT_B], 10_000);
  assert.equal(getProjectSeenAgents().roots["root-1"], rootOne);
});

test("normalises pubkey casing on record", () => {
  recordProjectSeenAgents("root-1", [AGENT_A.toUpperCase()], 10_000);
  assert.deepEqual(getProjectSeenAgents().roots["root-1"], {
    [AGENT_A]: 10_000,
  });
});

test("ignores empty roots and empty sighting lists", () => {
  const before = getProjectSeenAgents();
  recordProjectSeenAgents("", [AGENT_A], 10_000);
  recordProjectSeenAgents("root-1", [], 10_000);
  assert.equal(getProjectSeenAgents(), before);
});

test("prunes sightings older than the TTL", () => {
  const now = SEEN_AGENT_TTL_MS * 2;
  const pruned = pruneSeenAgentsStore(
    {
      version: 1,
      roots: {
        stale: { [AGENT_A]: now - SEEN_AGENT_TTL_MS - 1 },
        mixed: {
          [AGENT_A]: now - SEEN_AGENT_TTL_MS - 1,
          [AGENT_B]: now - 5,
        },
      },
    },
    now,
  );

  // A root whose every sighting aged out disappears rather than lingering as
  // an empty object that would still count against the root cap.
  assert.deepEqual(Object.keys(pruned.roots), ["mixed"]);
  assert.deepEqual(pruned.roots.mixed, { [AGENT_B]: now - 5 });
});

test("prune is identity-stable when nothing aged out", () => {
  const store = { version: 1, roots: { "root-1": { [AGENT_A]: 1_000 } } };
  assert.equal(pruneSeenAgentsStore(store, 2_000), store);
});

test("prune preserves the sub-object of roots it did not touch", () => {
  const now = SEEN_AGENT_TTL_MS * 2;
  const untouched = { [AGENT_B]: now - 5 };
  const pruned = pruneSeenAgentsStore(
    {
      version: 1,
      roots: {
        untouched,
        expiring: {
          [AGENT_A]: now - SEEN_AGENT_TTL_MS - 1,
          [AGENT_B]: now - 5,
        },
      },
    },
    now,
  );

  // One root aging out must not re-render every component watching another.
  assert.equal(pruned.roots.untouched, untouched);
});

test("caps agents per root, keeping the most recent", () => {
  const agents = {};
  for (let index = 0; index < MAX_SEEN_AGENTS_PER_ROOT + 5; index += 1) {
    agents[index.toString(16).padStart(64, "0")] = 1_000 + index;
  }
  const pruned = pruneSeenAgentsStore({ version: 1, roots: { r: agents } }, 0);

  const kept = Object.keys(pruned.roots.r);
  assert.equal(kept.length, MAX_SEEN_AGENTS_PER_ROOT);
  assert.ok(!kept.includes((0).toString(16).padStart(64, "0")));
  assert.ok(
    kept.includes(
      (MAX_SEEN_AGENTS_PER_ROOT + 4).toString(16).padStart(64, "0"),
    ),
  );
});

test("caps roots, keeping the most recently active", () => {
  const roots = {};
  for (let index = 0; index < MAX_SEEN_ROOTS + 3; index += 1) {
    roots[`root-${index}`] = { [AGENT_A]: 1_000 + index };
  }
  const pruned = pruneSeenAgentsStore({ version: 1, roots }, 0);

  const kept = Object.keys(pruned.roots);
  assert.equal(kept.length, MAX_SEEN_ROOTS);
  assert.ok(!kept.includes("root-0"));
  assert.ok(kept.includes(`root-${MAX_SEEN_ROOTS + 2}`));
});

test("recording prunes, so the memory cannot grow without bound", () => {
  for (let index = 0; index < MAX_SEEN_ROOTS + 3; index += 1) {
    recordProjectSeenAgents(`root-${index}`, [AGENT_A], 1_000 + index);
  }
  assert.equal(
    Object.keys(getProjectSeenAgents().roots).length,
    MAX_SEEN_ROOTS,
  );
});

test("rejects persisted blobs that are not the shape we wrote", () => {
  for (const value of [
    null,
    "nope",
    42,
    {},
    { version: 2, roots: { r: { [AGENT_A]: 1 } } },
    { version: 1 },
    { version: 1, roots: "no" },
  ]) {
    assert.deepEqual(parseSeenAgentsStore(value), EMPTY_SEEN_AGENTS_STORE);
  }
});

test("drops unusable entries rather than half-believing a blob", () => {
  const parsed = parseSeenAgentsStore({
    version: 1,
    roots: {
      good: { [AGENT_A]: 5, [AGENT_B]: "later", bad: Number.NaN, "": 7 },
      empty: { [AGENT_A]: 0 },
      "": { [AGENT_A]: 5 },
    },
  });

  assert.deepEqual(parsed, { version: 1, roots: { good: { [AGENT_A]: 5 } } });
});

test("collapses two spellings of one pubkey to the later sighting", () => {
  const parsed = parseSeenAgentsStore({
    version: 1,
    roots: { r: { [AGENT_A]: 5, [AGENT_A.toUpperCase()]: 9 } },
  });
  assert.deepEqual(parsed.roots.r, { [AGENT_A]: 9 });
});

test("recordSeenAgentsIn is pure and identity-stable", () => {
  const store = { version: 1, roots: {} };
  const next = recordSeenAgentsIn(store, "root-1", [AGENT_A], 1_000);

  assert.notEqual(next, store);
  assert.deepEqual(store.roots, {});
  assert.equal(recordSeenAgentsIn(next, "root-1", [AGENT_A], 1_001), next);
});
