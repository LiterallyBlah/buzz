import { expect, test } from "@playwright/test";

import { installMockBridge } from "../helpers/bridge";

const MOCK_VIEWER_PUBKEY = "deadbeef".repeat(8);
const RELAY_ONLY_AGENT_PUBKEY = "e".repeat(64);

async function openBuzzProject(page: import("@playwright/test").Page) {
  await page.goto("/", { waitUntil: "domcontentloaded" });
  await page.getByTestId("open-projects-view").click();
  await page.getByTestId("projects-section-projects").click();
  const projectEntry = page
    .locator(
      '[data-testid="project-card-buzz"], [data-testid="project-row-buzz"]',
    )
    .first();
  await expect(projectEntry).toBeVisible({ timeout: 10_000 });
  await projectEntry.click();
}

test("project issue composer offers viewer-authorised relay agents outside the linked channel", async ({
  page,
}) => {
  await installMockBridge(page, {
    relayAgents: [
      {
        pubkey: RELAY_ONLY_AGENT_PUBKEY,
        name: "quinn",
        respondTo: "allowlist",
        respondToAllowlist: [MOCK_VIEWER_PUBKEY],
        channelNames: ["agents"],
      },
    ],
  });
  await openBuzzProject(page);

  await page.getByRole("tab", { name: "Issues", exact: true }).click();
  const issueRow = page.getByTestId("project-issue-row").first();
  await expect(issueRow).toBeVisible({ timeout: 10_000 });
  await issueRow.getByTitle("View issue").click();

  const detail = page.getByTestId("project-issue-detail");
  await expect(detail).toBeVisible({ timeout: 10_000 });
  await expect
    .poll(() =>
      page.evaluate(
        () =>
          (window.__BUZZ_E2E_COMMANDS__ ?? []).filter(
            (command) => command === "list_relay_agents",
          ).length,
      ),
    )
    .toBeGreaterThan(0);

  const input = detail.getByTestId("message-input");
  await input.click();
  await expect(
    detail.getByRole("button", { name: "Mention someone" }),
  ).toBeVisible();
  await input.fill("@quinn");

  await expect(
    page.getByTestId("mention-autocomplete").getByText("quinn"),
  ).toBeVisible();
});
