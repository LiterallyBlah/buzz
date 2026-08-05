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
  await expect(group).toContainText("Managed elsewhere");
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

test("a remote card exposes no local lifecycle controls", async ({ page }) => {
  await openAgentsView(page);

  const group = page.getByTestId("owned-relay-agents-group");
  await expect(group).toBeVisible({ timeout: 10_000 });

  // This desktop holds no private key, no local record and no process for the
  // agent, and the tree has no owner-signed relay command to drive it, so no
  // control may be painted that would need one.
  await expect(group.getByTestId(`agent-runtime-start-${NADIA}`)).toHaveCount(
    0,
  );
  await expect(
    group.getByRole("button", { name: /Start|Stop|Deploy|Shutdown|Delete/ }),
  ).toHaveCount(0);
  await expect(
    group.getByRole("button", { name: "Agent actions" }),
  ).toHaveCount(0);

  // The card itself is the only affordance: one button, opening the profile.
  await expect(
    page.getByTestId(`owned-relay-agent-${NADIA}`).getByRole("button"),
  ).toHaveCount(1);
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
