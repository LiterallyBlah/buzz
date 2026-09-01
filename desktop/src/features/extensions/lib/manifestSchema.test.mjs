import assert from "node:assert/strict";
import test from "node:test";

import {
  EXTENSION_ID_PATTERN,
  MAX_EXTENSION_ID_LENGTH,
  parseExtensionManifest,
} from "./manifestSchema.ts";

const CHANNEL = "c8fb8f44-993d-4166-810e-ebdad7b8b944";

function validManifest(overrides = {}) {
  return {
    id: "equation-explorer",
    name: "Equation Explorer",
    version: "0.1.0",
    entry: "index.html",
    ...overrides,
  };
}

test("parseExtensionManifest accepts a minimal manifest and defaults scopes closed", () => {
  const result = parseExtensionManifest(validManifest());
  assert.equal(result.ok, true);
  assert.deepEqual(result.manifest.scopes, {
    agentConverse: false,
    extensionData: false,
    identity: false,
    read: [],
    sign: [],
    storage: false,
  });
  assert.deepEqual(result.manifest.egress, []);
});

test("parseExtensionManifest accepts the explicitly granted agent conversation scope", () => {
  const result = parseExtensionManifest(
    validManifest({ scopes: { agentConverse: true } }),
  );
  assert.equal(result.ok, true);
  assert.equal(result.manifest.scopes.agentConverse, true);
});

test("parseExtensionManifest accepts the BRIDGE_SPEC §7 worked example", () => {
  const result = parseExtensionManifest(
    validManifest({
      scopes: {
        identity: true,
        storage: true,
        extensionData: true,
        sign: [{ kind: 9, channels: [CHANNEL] }],
        read: [{ kinds: [9, 45001], channels: [CHANNEL] }],
      },
      egress: [],
    }),
  );
  assert.equal(result.ok, true);
  assert.equal(result.manifest.scopes.sign[0].kind, 9);
});

test("parseExtensionManifest rejects unknown fields", () => {
  const result = parseExtensionManifest(
    validManifest({ permissions: ["everything"] }),
  );
  assert.equal(result.ok, false);
  assert.ok(
    result.errors.some((message) => /unrecognized|unknown/i.test(message)),
    `expected an unknown-field error, got: ${result.errors.join(" | ")}`,
  );
});

test("parseExtensionManifest rejects an unknown field inside scopes", () => {
  const result = parseExtensionManifest(
    validManifest({ scopes: { identity: true, everything: true } }),
  );
  assert.equal(result.ok, false);
});

for (const field of ["id", "name", "version", "entry"]) {
  test(`parseExtensionManifest rejects a manifest missing ${field}`, () => {
    const manifest = validManifest();
    delete manifest[field];
    const result = parseExtensionManifest(manifest);
    assert.equal(result.ok, false);
    assert.ok(
      result.errors.some((message) => message.startsWith(`${field}:`)),
      `expected an error naming ${field}, got: ${result.errors.join(" | ")}`,
    );
  });
}

const REJECTED_IDS = [
  "../evil",
  "..",
  "./evil",
  "Evil",
  "-lead",
  "",
  "a/b",
  "a\\b",
  "a.b",
  "a b",
  "ä",
];

for (const id of REJECTED_IDS) {
  test(`extension id grammar rejects ${JSON.stringify(id)}`, () => {
    assert.equal(EXTENSION_ID_PATTERN.test(id), false);
    const result = parseExtensionManifest(validManifest({ id }));
    assert.equal(result.ok, false);
  });
}

for (const id of ["a", "0", "_", "equation-explorer", "ee_2", "a-b_c9"]) {
  test(`extension id grammar accepts ${JSON.stringify(id)}`, () => {
    assert.equal(EXTENSION_ID_PATTERN.test(id), true);
    assert.equal(parseExtensionManifest(validManifest({ id })).ok, true);
  });
}

test("parseExtensionManifest leaves the signable-kind question to the Rust loader", () => {
  // 30177 (managed-agent redefinition) is exactly the kind BRIDGE_SPEC §4 names
  // as the reason grantability is an allowlist rather than a blocklist — and it
  // is the Rust loader that enforces that, from `EXTENSION_SIGNABLE_KINDS`.
  // This layer must NOT carry a second copy of the set, so a non-signable kind
  // is well-formed here and rejected there. This test pins that division: if
  // someone reintroduces the allowlist to the frontend, it fails.
  const result = parseExtensionManifest(
    validManifest({
      scopes: { sign: [{ kind: 30177, channels: [CHANNEL] }] },
    }),
  );
  assert.equal(result.ok, true);
});

test("parseExtensionManifest still rejects a malformed sign kind", () => {
  for (const kind of [-1, 1.5, "9", null]) {
    const result = parseExtensionManifest(
      validManifest({ scopes: { sign: [{ kind, channels: [CHANNEL] }] } }),
    );
    assert.equal(result.ok, false, `kind ${JSON.stringify(kind)} should fail`);
  }
});

test("parseExtensionManifest accepts an id at exactly the byte cap", () => {
  const id = "a".repeat(MAX_EXTENSION_ID_LENGTH);
  assert.equal(id.length, 64);
  assert.equal(parseExtensionManifest(validManifest({ id })).ok, true);
});

test("parseExtensionManifest rejects an id one byte over the cap", () => {
  // BRIDGE_SPEC §7: ids are <= 64 bytes, because an unbounded id can name a
  // kind-30800 d-tag coordinate the relay refuses (D_TAG_MAX_LEN = 1024).
  const id = "a".repeat(MAX_EXTENSION_ID_LENGTH + 1);
  const result = parseExtensionManifest(validManifest({ id }));
  assert.equal(result.ok, false);
  assert.ok(
    result.errors.some((message) => message.includes("64 bytes")),
    `expected the cap to be named, got: ${result.errors.join(" | ")}`,
  );
});

test("parseExtensionManifest rejects an empty channel list on a sign scope", () => {
  const result = parseExtensionManifest(
    validManifest({ scopes: { sign: [{ kind: 9, channels: [] }] } }),
  );
  assert.equal(result.ok, false);
});

test("parseExtensionManifest rejects an empty channel list on a read scope", () => {
  const result = parseExtensionManifest(
    validManifest({ scopes: { read: [{ kinds: [9], channels: [] }] } }),
  );
  assert.equal(result.ok, false);
});

test("parseExtensionManifest rejects a non-UUID channel", () => {
  const result = parseExtensionManifest(
    validManifest({ scopes: { sign: [{ kind: 9, channels: ["*"] }] } }),
  );
  assert.equal(result.ok, false);
});

test("parseExtensionManifest rejects an uppercase channel UUID", () => {
  // The Rust loader's is_canonical_channel_uuid requires the lowercase
  // hyphenated form. Accepting uppercase here would pass a manifest the
  // authoritative validator then rejects.
  const result = parseExtensionManifest(
    validManifest({
      scopes: { sign: [{ kind: 9, channels: [CHANNEL.toUpperCase()] }] },
    }),
  );
  assert.equal(result.ok, false);
});

for (const entry of ["/etc/passwd", "\\windows\\evil", "C:\\evil", "../out"]) {
  test(`parseExtensionManifest rejects entry ${JSON.stringify(entry)}`, () => {
    assert.equal(parseExtensionManifest(validManifest({ entry })).ok, false);
  });
}

test("parseExtensionManifest accepts a nested relative entry", () => {
  assert.equal(
    parseExtensionManifest(validManifest({ entry: "web/index.html" })).ok,
    true,
  );
});
