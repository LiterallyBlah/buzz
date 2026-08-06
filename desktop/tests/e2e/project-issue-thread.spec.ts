import { expect, type Page, test } from "@playwright/test";

import { installMockBridge, TEST_IDENTITIES } from "../helpers/bridge";

const DEFAULT_MOCK_PUBKEY = "deadbeef".repeat(8);
const BUZZ_REPO_ADDRESS = `30617:${DEFAULT_MOCK_PUBKEY}:buzz`;

/** The projects surface is a preview feature — opt in before the app mounts. */
async function enableProjectsFeature(page: Page) {
  await page.addInitScript(() => {
    window.localStorage.setItem(
      "buzz-feature-overrides-v1",
      JSON.stringify({ projects: true }),
    );
  });
}

async function openFirstIssue(page: Page) {
  await page.goto("/", { waitUntil: "domcontentloaded" });
  await page.getByTestId("open-projects-view").click();
  await page.getByRole("button", { name: "Repositories", exact: true }).click();
  await page
    .locator(
      '[data-testid="project-card-buzz"], [data-testid="project-row-buzz"]',
    )
    .first()
    .click();
  await page.getByRole("tab", { name: "Issues", exact: true }).click();

  const firstIssue = page.getByTestId("project-issue-row").first();
  await expect(firstIssue).toBeVisible({ timeout: 10_000 });
  const issueId = await firstIssue.getAttribute("data-project-event-id");
  expect(issueId).toBeTruthy();
  // The row is not itself a button — its id cell is the "open" affordance.
  await firstIssue.getByTitle("View issue").click();

  await expect(page.getByTestId("project-issue-detail")).toBeVisible({
    timeout: 10_000,
  });
  return issueId as string;
}

/**
 * Deliver a comment on an open issue the way an agent's reply arrives: from
 * another author, over the live subscription, without the viewer touching
 * anything.
 */
async function pushComment(page: Page, issueId: string, body: string) {
  await page.evaluate(
    ({ author, content, id, repoAddress }) => {
      window.__BUZZ_E2E_PUSH_MOCK_PROJECT_EVENT__?.({
        content,
        pubkey: author,
        tags: [
          ["a", repoAddress],
          ["e", id, "", "root"],
        ],
      });
    },
    {
      author: TEST_IDENTITIES.alice.pubkey,
      content: body,
      id: issueId,
      repoAddress: BUZZ_REPO_ADDRESS,
    },
  );
}

/**
 * Announce a NIP-PA turn on the open issue.
 *
 * `state: "idle"` is the terminal frame — the working→done transition a reader
 * scrolled up has no other way to learn about, because an agent handed an
 * issue by a peer call can finish its turn without leaving a comment.
 */
async function pushActivity(
  page: Page,
  issueId: string,
  state: "working" | "idle",
) {
  await page.evaluate(
    ({ agent, id, turnState }) => {
      window.__BUZZ_E2E_PUSH_MOCK_PROJECT_EVENT__?.({
        content: "",
        kind: 20003,
        pubkey: agent,
        tags: [
          ["e", id],
          ["agent", agent],
          ["state", turnState],
          ["turn", "turn-1"],
          ...(turnState === "working" ? [["stage", "reading files"]] : []),
        ],
      });
    },
    {
      agent: TEST_IDENTITIES.alice.pubkey,
      id: issueId,
      turnState: state,
    },
  );
}

async function threadMetrics(page: Page) {
  return page
    .getByTestId("project-issue-thread-scroll")
    .evaluate((element) => ({
      clientHeight: element.clientHeight,
      scrollHeight: element.scrollHeight,
      scrollTop: element.scrollTop,
    }));
}

/**
 * The complaint this layout answers: with an agent replying every few seconds
 * you had to be at the bottom of the page to see the reply and at the top to
 * see that the agent was working, and the composer was below every comment.
 *
 * So the invariants worth holding are about *reachability without scrolling*,
 * not about pixels — each assertion below is one thing a reader no longer has
 * to travel to.
 */
test("an open issue owns its scroll region and stays on the newest comment", async ({
  page,
}) => {
  await enableProjectsFeature(page);
  await installMockBridge(page);
  // Wide enough for the meta rail to sit beside the conversation, short enough
  // that a handful of comments overflow the thread.
  await page.setViewportSize({ width: 1440, height: 620 });

  const issueId = await openFirstIssue(page);

  // 1. The thread owns the scroll region. Asserted by walking up from the
  //    thread outwards: the first scrollable ancestor must be the thread
  //    container itself, so nothing above it — hero, tab strip, page — is a
  //    second scroller the reader has to travel through.
  const threadScroll = page.getByTestId("project-issue-thread-scroll");
  await expect(threadScroll).toBeVisible();
  const outerScrollers = await threadScroll.evaluate((element) => {
    const found: string[] = [];
    for (
      let node = element.parentElement;
      node && node !== document.body;
      node = node.parentElement
    ) {
      if (node.scrollHeight - node.clientHeight > 1) {
        found.push(node.className || node.tagName);
      }
    }
    return found;
  });
  expect(outerScrollers).toEqual([]);

  // 2. The composer is on screen without any scrolling at all. This is the
  //    "scroll all the way down to write a comment" half of the complaint.
  const composer = page.locator('[data-placeholder="Add a comment…"]').first();
  await expect(composer).toBeInViewport();

  // 3. The duplicated status is gone. Live agent state is reported once, in
  //    the rail, and the inline strip that forced a scroll position on it no
  //    longer exists.
  await expect(page.getByTestId("project-activity-indicator")).toHaveCount(0);

  // 4. New comments arrive pinned: the reader standing at the bottom stays
  //    standing on the newest one.
  for (let index = 0; index < 12; index += 1) {
    await pushComment(page, issueId, `Mock agent reply number ${index + 1}.`);
  }
  await expect(page.getByText("Mock agent reply number 12.")).toBeVisible({
    timeout: 10_000,
  });
  await expect
    .poll(async () => {
      const { clientHeight, scrollHeight, scrollTop } =
        await threadMetrics(page);
      return scrollHeight - clientHeight - scrollTop <= 32;
    })
    .toBe(true);

  // The thread really did overflow, so "at the bottom" above is a claim about
  // a scrolled container rather than one short enough to have no bottom.
  const overflowing = await threadMetrics(page);
  expect(overflowing.scrollHeight).toBeGreaterThan(overflowing.clientHeight);

  // 5. The header that says which issue this is has not scrolled away with
  //    the content above it. Asserted from the *pinned* position — the thread
  //    is scrolled to its floor here, so the header being flush with the top
  //    of the scroll box is stickiness rather than the header simply being
  //    where an unscrolled container would leave it.
  const header = page.getByTestId("project-issue-thread-header");
  expect(overflowing.scrollTop).toBeGreaterThan(0);
  const pinnedScrollBox = await threadScroll.boundingBox();
  const pinnedHeaderBox = await header.boundingBox();
  expect(pinnedHeaderBox?.y ?? 0).toBeCloseTo(pinnedScrollBox?.y ?? -1, 0);
  // The rail is a grid cell beside a column that scrolls on its own, so
  // status stays reachable from anywhere in the thread rather than living at
  // a scroll position.
  await expect(
    page.getByTestId("project-issue-status-chip").first(),
  ).toBeInViewport();

  // 6. Leaving the bottom is not a commitment: what arrived while away is
  //    offered, and taking the offer returns to the newest comment.
  await threadScroll.evaluate((element) => element.scrollTo({ top: 0 }));
  await expect(page.getByTestId("project-issue-jump-to-latest")).toHaveCount(0);
  await pushComment(page, issueId, "One more while the reader is scrolled up.");
  const jump = page.getByTestId("project-issue-jump-to-latest");
  await expect(jump).toBeVisible({ timeout: 10_000 });
  await expect(jump).toContainText("1 new");

  await jump.click();
  await expect(jump).toHaveCount(0);
  await expect
    .poll(async () => {
      const { clientHeight, scrollHeight, scrollTop } =
        await threadMetrics(page);
      return scrollHeight - clientHeight - scrollTop <= 32;
    })
    .toBe(true);
  await expect(
    page.getByText("One more while the reader is scrolled up."),
  ).toBeInViewport();
});

/**
 * The half of "new activity" that is not a comment.
 *
 * An agent handed an issue by a peer call announces NIP-PA for the length of
 * its turn and can finish without commenting, so a pill that only counted
 * comments would let the one event a waiting reader cares about pass in
 * silence. The stage caption is asserted in the same test because it is the
 * other thing the removed inline strip used to say: the rail has to report
 * *what* the agent is doing, not just that it is doing something.
 */
test("a turn ending while the reader is scrolled up is offered as new activity", async ({
  page,
}) => {
  await enableProjectsFeature(page);
  await installMockBridge(page);
  await page.setViewportSize({ width: 1440, height: 620 });

  const issueId = await openFirstIssue(page);

  await pushActivity(page, issueId, "working");
  // The rail is the single live surface now, and it keeps the caption.
  await expect(page.getByTestId("project-root-agent-stage")).toHaveText(
    "reading files",
    { timeout: 10_000 },
  );

  // Enough comments to overflow, so scrolling up is possible at all.
  for (let index = 0; index < 12; index += 1) {
    await pushComment(page, issueId, `Reply while working ${index + 1}.`);
  }
  await expect(page.getByText("Reply while working 12.")).toBeVisible({
    timeout: 10_000,
  });

  const threadScroll = page.getByTestId("project-issue-thread-scroll");
  await threadScroll.evaluate((element) => element.scrollTo({ top: 0 }));
  await expect(page.getByTestId("project-issue-jump-to-latest")).toHaveCount(0);

  // The turn ends, and it ends silently — no comment accompanies it.
  await pushActivity(page, issueId, "idle");

  const jump = page.getByTestId("project-issue-jump-to-latest");
  await expect(jump).toBeVisible({ timeout: 10_000 });
  // Not "1 new": there is no comment at the bottom to have arrived, and
  // promising one would send the reader looking for a reply that never
  // existed.
  await expect(jump).toHaveText(/New activity/);
  await expect(page.getByTestId("project-root-agent-stage")).toHaveCount(0);

  await jump.click();
  await expect(jump).toHaveCount(0);
});

/**
 * Too narrow for a rail beside the conversation.
 *
 * The failure this guards against is specific: collapsing the grid without
 * moving status puts the only copy of it underneath every comment, which is
 * worse than the layout being replaced. So the narrow case has to keep the
 * thread's own scrolling *and* put status somewhere that does not scroll.
 */
test("a narrow window keeps status in the sticky header and the thread scrolling", async ({
  page,
}) => {
  await enableProjectsFeature(page);
  await installMockBridge(page);
  await page.setViewportSize({ width: 900, height: 620 });

  const issueId = await openFirstIssue(page);

  const header = page.getByTestId("project-issue-thread-header");
  await expect(header.getByTestId("project-issue-status-chip")).toBeVisible();

  // The rail joins the region that scrolls rather than becoming a fixed block
  // between the conversation and the composer.
  const threadScroll = page.getByTestId("project-issue-thread-scroll");
  await expect(threadScroll.getByTestId("project-issue-meta-rail")).toHaveCount(
    1,
  );

  // The composer is still docked and still reachable without scrolling.
  await expect(
    page.locator('[data-placeholder="Add a comment…"]').first(),
  ).toBeInViewport();

  // And the header stays put once the thread is scrolled, so the status chip
  // it carries never leaves the viewport.
  for (let index = 0; index < 12; index += 1) {
    await pushComment(page, issueId, `Narrow-window reply ${index + 1}.`);
  }
  await expect(page.getByText("Narrow-window reply 12.")).toBeVisible({
    timeout: 10_000,
  });
  await expect(
    header.getByTestId("project-issue-status-chip"),
  ).toBeInViewport();
  // Pinning lands on the newest comment, not on the rail: the rail sits above
  // the conversation here precisely so the floor of the thread is a comment.
  await expect(page.getByText("Narrow-window reply 12.")).toBeInViewport();
});
