import assert from "node:assert/strict";
import test from "node:test";

import {
  deriveProfileChannels,
  parseProfilePanelTab,
  parseProfilePanelView,
  personaManagedAgentUpdate,
  profilePanelTabFromSearch,
  profilePanelViewFromSearch,
} from "./UserProfilePanelUtils.ts";

function agent(overrides = {}) {
  return {
    pubkey: "deadbeef".repeat(8),
    name: "Fizz",
    personaId: "persona-1",
    relayUrl: "ws://localhost:3000",
    acpCommand: "buzz-acp",
    agentCommand: "goose",
    agentArgs: [],
    mcpCommand: "",
    turnTimeoutSeconds: 320,
    idleTimeoutSeconds: null,
    maxTurnDurationSeconds: null,
    parallelism: 1,
    systemPrompt: "Old prompt",
    avatarUrl: "app-avatar://old",
    model: "old-model",
    envVars: { OLD_KEY: "1" },
    status: "stopped",
    pid: null,
    createdAt: "2026-01-01T00:00:00Z",
    updatedAt: "2026-01-01T00:00:00Z",
    lastStartedAt: null,
    lastStoppedAt: null,
    lastExitCode: null,
    lastError: null,
    logPath: null,
    startOnAppLaunch: true,
    backend: { type: "local" },
    backendAgentId: null,
    respondTo: "owner-only",
    respondToAllowlist: [],
    ...overrides,
  };
}

function persona(overrides = {}) {
  return {
    id: "persona-1",
    displayName: "Fizz Prime",
    avatarUrl: null,
    systemPrompt: "New prompt",
    runtime: "goose",
    model: "new-model",
    provider: null,
    namePool: [],
    isBuiltIn: false,
    isActive: true,
    envVars: { NEW_KEY: "2" },
    createdAt: "2026-01-01T00:00:00Z",
    updatedAt: "2026-01-01T00:00:00Z",
    ...overrides,
  };
}

function runtime(overrides = {}) {
  return {
    id: "claude",
    label: "Claude Code",
    avatarUrl: "app-avatar://claude",
    availability: "available",
    command: "claude",
    binaryPath: "/usr/local/bin/claude",
    defaultArgs: ["mcp", "serve"],
    mcpCommand: "claude-mcp",
    installHint: "",
    installInstructionsUrl: "",
    canAutoInstall: false,
    underlyingCliPath: null,
    ...overrides,
  };
}

test("personaManagedAgentUpdate syncs edited persona identity to linked agent", () => {
  assert.deepEqual(personaManagedAgentUpdate(agent(), persona()), {
    pubkey: "deadbeef".repeat(8),
    name: "Fizz Prime",
    systemPrompt: "New prompt",
    model: "new-model",
    envVars: { NEW_KEY: "2" },
  });
});

test("personaManagedAgentUpdate skips unrelated or unchanged agents", () => {
  assert.equal(
    personaManagedAgentUpdate(agent({ personaId: "persona-2" }), persona()),
    null,
  );
  assert.equal(
    personaManagedAgentUpdate(
      agent({
        name: "Fizz Prime",
        avatarUrl: null,
        systemPrompt: "New prompt",
        model: "new-model",
        envVars: { NEW_KEY: "2" },
      }),
      persona(),
    ),
    null,
  );
});

test("personaManagedAgentUpdate maps changed persona runtime to linked agent commands", () => {
  assert.deepEqual(
    personaManagedAgentUpdate(agent(), persona({ runtime: "claude" }), {
      previousPersona: persona({ runtime: "goose" }),
      runtimes: [runtime()],
    }),
    {
      pubkey: "deadbeef".repeat(8),
      name: "Fizz Prime",
      systemPrompt: "New prompt",
      model: "new-model",
      envVars: { NEW_KEY: "2" },
      agentCommand: "claude",
      agentArgs: ["mcp", "serve"],
      mcpCommand: "claude-mcp",
    },
  );
});

test("personaManagedAgentUpdate leaves runtime fields alone when runtime is unchanged", () => {
  assert.equal(
    personaManagedAgentUpdate(
      agent({
        name: "Fizz Prime",
        avatarUrl: null,
        systemPrompt: "New prompt",
        model: "new-model",
        envVars: { NEW_KEY: "2" },
        agentArgs: ["custom"],
      }),
      persona({ runtime: "goose" }),
      {
        previousPersona: persona({ runtime: "goose" }),
        runtimes: [runtime({ id: "goose", command: "goose" })],
      },
    ),
    null,
  );
});

test("parseProfilePanelView accepts all profile panel subviews", () => {
  for (const view of [
    "summary",
    "info",
    "configuration",
    "diagnostics",
    "memories",
    "channels",
    "logs",
  ]) {
    assert.equal(parseProfilePanelView(view), view);
  }
});

test("parseProfilePanelView maps legacy agent config subviews to configuration", () => {
  for (const view of ["model", "settings"]) {
    assert.equal(parseProfilePanelView(view), "configuration");
  }
});

test("profilePanelViewFromSearch falls back to summary for invalid values", () => {
  assert.equal(parseProfilePanelView("missing"), null);
  assert.equal(profilePanelViewFromSearch("missing"), "summary");
  assert.equal(profilePanelViewFromSearch(null), "summary");
});

test("parseProfilePanelTab accepts profile summary tabs", () => {
  for (const tab of ["info", "runtime", "channels", "memories"]) {
    assert.equal(parseProfilePanelTab(tab), tab);
  }
});

test("profilePanelTabFromSearch falls back to info for invalid values", () => {
  assert.equal(parseProfilePanelTab("missing"), null);
  assert.equal(profilePanelTabFromSearch("missing"), "info");
  assert.equal(profilePanelTabFromSearch(null), "info");
});

// ── deriveProfileChannels ───────────────────────────────────────────────────

const AGENT_PUBKEY = "aa".repeat(32);

function channel(id, name, memberPubkeys = []) {
  return { id, name, memberPubkeys };
}

function relayAgent(channels, channelIds) {
  return { pubkey: AGENT_PUBKEY, name: "Fable", channels, channelIds };
}

test("deriveProfileChannels unions membership into a relay agent's stale list", () => {
  // The agent's kind:10100 snapshot predates `new-chan`, which it is now a
  // member of. The tab must show both, not just the self-reported one.
  const links = deriveProfileChannels(
    AGENT_PUBKEY,
    relayAgent(["buzz"], ["chan-buzz"]),
    undefined,
    [
      channel("chan-buzz", "buzz", [AGENT_PUBKEY]),
      channel("chan-new", "new-chan", [AGENT_PUBKEY]),
    ],
  );

  assert.deepEqual(links, [
    { id: "chan-buzz", name: "buzz" },
    { id: "chan-new", name: "new-chan" },
  ]);
});

test("deriveProfileChannels does not duplicate a channel in both lists", () => {
  const links = deriveProfileChannels(
    AGENT_PUBKEY,
    relayAgent(["buzz"], ["chan-buzz"]),
    undefined,
    [channel("chan-buzz", "buzz", [AGENT_PUBKEY])],
  );

  assert.deepEqual(links, [{ id: "chan-buzz", name: "buzz" }]);
});

test("deriveProfileChannels keeps a declared channel the viewer cannot see", () => {
  // The viewer is not in `private-chan`, so it is absent from their channel
  // list; the agent's own declaration is the only evidence and must survive.
  const links = deriveProfileChannels(
    AGENT_PUBKEY,
    relayAgent(["private-chan"], ["chan-private"]),
    undefined,
    [channel("chan-buzz", "buzz", [])],
  );

  assert.deepEqual(links, [{ id: "chan-private", name: "private-chan" }]);
});

test("deriveProfileChannels ignores membership for a non-agent pubkey", () => {
  const links = deriveProfileChannels(AGENT_PUBKEY, undefined, undefined, [
    channel("chan-buzz", "buzz", [AGENT_PUBKEY]),
  ]);

  assert.deepEqual(links, []);
});

test("deriveProfileChannels matches membership case-insensitively", () => {
  const links = deriveProfileChannels(
    AGENT_PUBKEY,
    relayAgent([], []),
    undefined,
    [channel("chan-new", "new-chan", [AGENT_PUBKEY.toUpperCase()])],
  );

  assert.deepEqual(links, [{ id: "chan-new", name: "new-chan" }]);
});
