import { expect, test } from "@playwright/test";

import { installMockBridge } from "../helpers/bridge";

// The mock bridge's default identity — the owner whose Agents tab we open.
const ME = "deadbeef".repeat(8);
const SOMEBODY_ELSE = "fe".repeat(32);

// Relay-registered agents. `NADIA` is the bug: on the relay, owned by ME, with
// no local managed-agent record. `REX` is owned by somebody else and `SCOUT`
// is owned by ME *and* managed locally.
const NADIA = "11".repeat(32);
const REX = "22".repeat(32);
const SCOUT = "33".repeat(32);

async function openAgentsView(page: import("@playwright/test").Page) {
  await page.goto("/", { waitUntil: "domcontentloaded" });
  await page.getByTestId("open-agents-view").click();
  await expect(page.getByTestId("unified-agents-groups")).toBeVisible({
    timeout: 10_000,
  });
}

test.beforeEach(async ({ page }) => {
  await installMockBridge(page, {
    relayAgents: [
      { pubkey: NADIA, name: "nadia", agentType: "goose" },
      { pubkey: REX, name: "rex", agentType: "goose" },
      { pubkey: SCOUT, name: "scout", agentType: "goose" },
    ],
    managedAgents: [{ pubkey: SCOUT, name: "scout" }],
    // kind:0 NIP-OA owner declarations. This is the only ownership evidence
    // the client trusts — the kind:10100 entries above assert nothing.
    searchProfiles: [
      { pubkey: NADIA, displayName: "nadia", ownerPubkey: ME, isAgent: true },
      {
        pubkey: REX,
        displayName: "rex",
        ownerPubkey: SOMEBODY_ELSE,
        isAgent: true,
      },
      { pubkey: SCOUT, displayName: "scout", ownerPubkey: ME, isAgent: true },
    ],
  });
});

test("an owned relay agent with no local record is listed", async ({
  page,
}) => {
  await openAgentsView(page);

  const group = page.getByTestId("owned-relay-agents-group");
  await expect(group).toBeVisible({ timeout: 10_000 });
  await expect(group).toContainText("Remote Agents");
  await expect(page.getByTestId(`owned-relay-agent-${NADIA}`)).toBeVisible();
  await expect(page.getByTestId(`owned-relay-agent-${NADIA}`)).toContainText(
    "nadia",
  );
});

test("a relay agent owned by somebody else is never listed", async ({
  page,
}) => {
  await openAgentsView(page);

  // The group renders (NADIA is in it), so this is an ownership exclusion and
  // not merely a group that failed to appear.
  await expect(page.getByTestId("owned-relay-agents-group")).toBeVisible({
    timeout: 10_000,
  });
  await expect(page.getByTestId(`owned-relay-agent-${REX}`)).toHaveCount(0);
  await expect(page.getByText("rex", { exact: true })).toHaveCount(0);
});

test("an owned agent that is also managed locally appears exactly once", async ({
  page,
}) => {
  await openAgentsView(page);

  await expect(page.getByTestId("owned-relay-agents-group")).toBeVisible({
    timeout: 10_000,
  });
  await expect(page.getByTestId(`managed-agent-${SCOUT}`)).toHaveCount(1);
  await expect(page.getByTestId(`owned-relay-agent-${SCOUT}`)).toHaveCount(0);
});

test("a remote card exposes only applicable remote controls", async ({
  page,
}) => {
  await openAgentsView(page);
  const group = page.getByTestId("owned-relay-agents-group");
  const card = page.getByTestId(`owned-relay-agent-${NADIA}`);
  await card.getByRole("button", { name: "Open actions for nadia" }).click();
  await expect(
    page.getByRole("menuitem", { name: "Stop current work" }),
  ).toBeVisible();
  await expect(
    page.getByRole("menuitem", { name: "Finish work and shut down" }),
  ).toBeVisible();
  // The default smoke identity is a member, so moderation remains role-gated.
  await expect(
    page.getByRole("menuitem", { name: /Ban from this community|Lift ban/ }),
  ).toHaveCount(0);
  await expect(
    page.getByRole("menuitem", {
      name: /Start|Restart|Pause|Edit|Duplicate|Share|Delete/,
    }),
  ).toHaveCount(0);
  await expect(group.getByTestId(`agent-runtime-start-${NADIA}`)).toHaveCount(
    0,
  );
  await expect(card.getByRole("button")).toHaveCount(2);
});

test("a relay agent owned by somebody else offers no drain control", async ({
  page,
}) => {
  await openAgentsView(page);

  await expect(page.getByTestId("owned-relay-agents-group")).toBeVisible({
    timeout: 10_000,
  });

  // The authority for a drain is verified NIP-OA ownership, and REX has
  // somebody else's. The agent would refuse a frame we signed anyway
  // (`a_drain_frame_from_a_non_owner_is_dropped` in buzz-acp), so a control
  // here would be an affordance whose only outcome is a silent refusal.
  await expect(page.getByTestId(`owned-relay-agent-${REX}`)).toHaveCount(0);
  await expect(page.getByRole("button", { name: "Drain rex" })).toHaveCount(0);
});

/** Seed the agent's `control_result` answer to a drain, as the runtime emits it. */
async function emitDrainAcknowledgement(
  page: import("@playwright/test").Page,
  agentPubkey: string,
  status: string,
) {
  await page.evaluate(
    ([pubkey, ackStatus]) => {
      window.__BUZZ_E2E_SEED_OBSERVER_EVENTS__?.({
        agentPubkey: pubkey,
        events: [
          {
            seq: 1,
            timestamp: new Date().toISOString(),
            kind: "control_result",
            agentIndex: null,
            channelId: null,
            sessionId: null,
            turnId: null,
            payload: { type: "drain", status: ackStatus, reason: "" },
          },
        ],
      });
    },
    [agentPubkey, status] as const,
  );
}

async function confirmCancelAll(page: import("@playwright/test").Page) {
  const card = page.getByTestId(`owned-relay-agent-${NADIA}`);
  await card.getByRole("button", { name: "Open actions for nadia" }).click();
  await page.getByRole("menuitem", { name: "Stop current work" }).click();
  const dialog = page.getByRole("alertdialog");
  await expect(dialog).toContainText(
    "stay online and can accept new work afterwards",
  );
  await dialog.getByRole("button", { name: "Stop current work" }).click();
}

test("stopping current work sends one whole-agent cancel_all frame", async ({
  page,
}) => {
  await openAgentsView(page);
  await confirmCancelAll(page);
  const frames = await page.evaluate(async () => {
    await new Promise((resolve) => setTimeout(resolve, 250));
    return window.__BUZZ_E2E_OBSERVER_CONTROL_FRAMES__ ?? [];
  });
  expect(frames).toHaveLength(1);
  expect(JSON.parse(frames[0]?.content ?? "{}")).toEqual({
    type: "cancel_all",
  });
});

async function confirmDrain(page: import("@playwright/test").Page) {
  const card = page.getByTestId(`owned-relay-agent-${NADIA}`);
  await card.getByRole("button", { name: "Open actions for nadia" }).click();
  await page
    .getByRole("menuitem", { name: "Finish work and shut down" })
    .click();

  const dialog = page.getByRole("alertdialog");
  await expect(dialog).toBeVisible();
  // The consequence is named before the owner commits: nothing here can start
  // it again.
  await expect(dialog).toContainText("cannot start it again");
  await dialog
    .getByRole("button", { name: "Finish work and shut down" })
    .click();
}

test("draining sends an owner-signed drain frame for that agent", async ({
  page,
}) => {
  await openAgentsView(page);
  await confirmDrain(page);

  const frames = await page.evaluate(async () => {
    // The publish is asynchronous; give the mock relay a turn to receive it.
    await new Promise((resolve) => setTimeout(resolve, 250));
    return window.__BUZZ_E2E_OBSERVER_CONTROL_FRAMES__ ?? [];
  });

  expect(frames).toHaveLength(1);
  const [frame] = frames;
  expect(frame.kind).toBe(24_200);
  // Signed by the owner, not by the agent — this is the whole authority check
  // on the receiving side.
  expect(frame.pubkey).toBe(ME);
  expect(frame.tags).toContainEqual(["p", NADIA]);
  expect(frame.tags).toContainEqual(["agent", NADIA]);
  expect(frame.tags).toContainEqual(["frame", "control"]);
  expect(JSON.parse(frame.content)).toEqual({ type: "drain" });
});

test("an acknowledged drain reports draining, never stopped", async ({
  page,
}) => {
  await openAgentsView(page);
  await confirmDrain(page);

  const card = page.getByTestId(`owned-relay-agent-${NADIA}`);
  await expect(card).toContainText("Sending drain…");

  await emitDrainAcknowledgement(page, NADIA, "draining");

  await expect(card).toContainText("Draining — finishing current work");
  // The ack means admission closed. The process is still finishing its turn,
  // so nothing here may say it stopped.
  await expect(card).not.toContainText("Stopped");
});

test("a repeat drain is reported as already draining", async ({ page }) => {
  await openAgentsView(page);
  await confirmDrain(page);
  await emitDrainAcknowledgement(page, NADIA, "already_draining");

  await expect(page.getByTestId(`owned-relay-agent-${NADIA}`)).toContainText(
    "Already draining",
  );
});

test("a drain the agent never answers is reported as unanswered", async ({
  page,
}) => {
  await openAgentsView(page);
  await confirmDrain(page);

  const card = page.getByTestId(`owned-relay-agent-${NADIA}`);
  await expect(card).toContainText("Sending drain…");

  // No `control_result` is ever seeded. After the fallback window the card
  // says what we know — we heard nothing — and does not guess that the drain
  // failed: a running agent with owner telemetry switched off never acks.
  await expect(card).toContainText("Sent — no reply from the agent", {
    timeout: 15_000,
  });
  await expect(card).not.toContainText("Draining — finishing");
});

test("a drain the relay refuses is reported as a send failure", async ({
  page,
}) => {
  await openAgentsView(page);
  await page.evaluate(() => {
    window.__BUZZ_E2E_OBSERVER_CONTROL_ERRORS__ = [
      "relay refused the control frame",
    ];
  });
  await confirmDrain(page);

  // Distinct from silence: the frame never landed, so retrying may work.
  await expect(page.getByTestId(`owned-relay-agent-${NADIA}`)).toContainText(
    "Could not send drain",
    { timeout: 15_000 },
  );
});

test("a remote card opens the existing read-only profile Runtime view", async ({
  page,
}) => {
  await openAgentsView(page);

  await page.getByTestId(`owned-relay-agent-${NADIA}`).click();

  const panel = page.getByTestId("user-profile-panel");
  await expect(panel).toBeVisible({ timeout: 10_000 });
  await expect(panel.getByRole("tab", { name: "Runtime" })).toHaveAttribute(
    "aria-selected",
    "true",
  );
  // The panel's own relay-agent projection, unchanged by this work.
  await expect(panel.getByTestId("user-profile-runtime")).toContainText(
    "Goose",
  );
  await expect(
    panel.getByRole("button", { name: /Start|Stop|Deploy/ }),
  ).toHaveCount(0);
});
