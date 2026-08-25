import assert from "node:assert/strict";
import test from "node:test";

import {
  EXTENSION_ID_PATTERN,
  V1_SIGNABLE_KINDS,
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
    extensionData: false,
    identity: false,
    read: [],
    sign: [],
    storage: false,
  });
  assert.deepEqual(result.manifest.egress, []);
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

test("parseExtensionManifest rejects a sign kind outside the v1 allowlist", () => {
  // 30177 (managed-agent redefinition) is exactly the kind BRIDGE_SPEC §4 names
  // as the reason grantability is an allowlist rather than a blocklist.
  const result = parseExtensionManifest(
    validManifest({
      scopes: { sign: [{ kind: 30177, channels: [CHANNEL] }] },
    }),
  );
  assert.equal(result.ok, false);
  assert.ok(
    result.errors.some((message) => message.includes("30177")),
    `expected the rejected kind to be named, got: ${result.errors.join(" | ")}`,
  );
});

for (const kind of V1_SIGNABLE_KINDS) {
  test(`parseExtensionManifest accepts allowlisted sign kind ${kind}`, () => {
    const result = parseExtensionManifest(
      validManifest({ scopes: { sign: [{ kind, channels: [CHANNEL] }] } }),
    );
    assert.equal(result.ok, true);
  });
}

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
