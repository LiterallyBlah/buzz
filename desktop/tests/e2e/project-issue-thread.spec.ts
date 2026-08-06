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

async function openProject(page: Page) {
  await page.goto("/", { waitUntil: "domcontentloaded" });
  await page.getByTestId("open-projects-view").click();
  await page.getByRole("button", { name: "Repositories", exact: true }).click();
  await page
    .locator(
      '[data-testid="project-card-buzz"], [data-testid="project-row-buzz"]',
    )
    .first()
    .click();
}

async function openFirstIssue(page: Page) {
  await openProject(page);
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
 * Seed an issue with a body of our choosing and open it.
 *
 * The seeded fixtures all carry their subject as their content, so the two
 * cases that break the layout — no description at all, and one long enough to
 * clamp — are unreachable without publishing one.
 */
async function seedAndOpenIssue(page: Page, subject: string, body: string) {
  const id = await page.evaluate(
    ({ author, content, repoAddress, title }) =>
      window.__BUZZ_E2E_PUSH_MOCK_PROJECT_EVENT__?.({
        content,
        kind: 1621,
        pubkey: author,
        tags: [
          ["a", repoAddress],
          ["subject", title],
        ],
      })?.id ?? null,
    {
      author: TEST_IDENTITIES.alice.pubkey,
      content: body,
      repoAddress: BUZZ_REPO_ADDRESS,
      title: subject,
    },
  );
  // The list is a query, not a live subscription, so the tab has to be entered
  // after the publish for the row to exist.
  await page.getByRole("tab", { name: "Issues", exact: true }).click();
  const row = page.locator(`[data-project-event-id="${id}"]`).first();
  await expect(row).toBeVisible({ timeout: 10_000 });
  await row.getByTitle("View issue").click();
  await expect(page.getByTestId("project-issue-detail")).toBeVisible();
  return id as string;
}

/**
 * The stacked border widths across one boundary, top to bottom.
 *
 * Returned as data rather than asserted in the page, because the failure this
 * guards is "two hairlines where there should be one" and the only honest way
 * to say that is to show both numbers.
 */
async function boundaryRules(page: Page) {
  return page.evaluate(() => {
    const scroll = document.querySelector(
      '[data-testid="project-issue-thread-scroll"]',
    ) as HTMLElement;
    const content = scroll?.firstElementChild as HTMLElement;
    const children = [...content.children] as HTMLElement[];
    const rules: { gap: number; total: number }[] = [];
    for (let index = 0; index < children.length - 1; index += 1) {
      const above = children[index];
      const below = children[index + 1];
      const aboveRect = above.getBoundingClientRect();
      const belowRect = below.getBoundingClientRect();
      const aboveBorder = Number.parseFloat(
        getComputedStyle(above).borderBottomWidth,
      );
      const belowBorder = Number.parseFloat(
        getComputedStyle(below).borderTopWidth,
      );
      // Only boundaries where the two blocks actually touch: a sticky header
      // overlapping scrolled content is not a shared edge.
      if (Math.abs(belowRect.top - aboveRect.bottom) > 1) continue;
      rules.push({
        gap: index,
        total: aboveBorder + belowBorder,
      });
    }
    return rules;
  });
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
 * The description has to read as the issue document, not the first comment.
 *
 * The three assertions are the three things a reader's eye actually uses, and
 * each one was absent before: clear air under the sticky header instead of the
 * first line touching its border; a container the description sits inside and
 * comments do not; and something between the end of it and the first reply.
 *
 * Deliberately not a screenshot comparison — what matters is that the two kinds
 * of prose are told apart by structure, and a pixel diff would also fail on
 * every unrelated restyle of the surrounding surface.
 */
test("the issue description is presented as a document, not as a comment", async ({
  page,
}) => {
  await enableProjectsFeature(page);
  await installMockBridge(page);
  await page.setViewportSize({ width: 1440, height: 620 });

  const issueId = await openFirstIssue(page);
  await pushComment(page, issueId, "A reply that is definitely a comment.");

  const description = page.getByTestId("project-work-item-description");
  await expect(description).toBeVisible();

  // 1. It is not flush against the sticky header's bottom border. Measured
  //    from the first thing that is actually drawn — the section's own box
  //    abuts the header because the new spacing is padding *inside* it, so
  //    measuring the section would report zero air while the reader sees 12px.
  const headerBox = await page
    .getByTestId("project-issue-thread-header")
    .boundingBox();
  const labelBox = await description
    .getByText("Description", { exact: true })
    .boundingBox();
  expect(
    (labelBox?.y ?? 0) - ((headerBox?.y ?? 0) + (headerBox?.height ?? 0)),
  ).toBeGreaterThanOrEqual(8);
  const descriptionBox = await description.boundingBox();

  // 2. The description is inside a container; the comment is not inside it.
  //    This is the distinction itself, so it is asserted as containment rather
  //    than as a class name that a restyle would rename.
  await expect(description).toContainText("Description");
  await expect(
    description.getByText("A reply that is definitely a comment."),
  ).toHaveCount(0);

  // 3. Something separates the end of the description from the first reply.
  const conversation = page.getByRole("heading", { name: "Conversation" });
  await expect(conversation).toBeVisible();
  const conversationBox = await conversation.boundingBox();
  expect(conversationBox?.y ?? 0).toBeGreaterThan(
    (descriptionBox?.y ?? 0) + (descriptionBox?.height ?? 0) - 1,
  );

  // And the description carries no byline — attribution is what would make it
  // read as a message again, and the header already names the author.
  await expect(description.getByRole("img")).toHaveCount(0);
});

/** Multi-paragraph, with a list — enough to clamp, and enough block children
 *  that `-webkit-line-clamp` has more than one thing to count. */
const LONG_DESCRIPTION = [
  "The relay drops a reconnect attempt when the backoff timer and the socket close race, and the window is small but reachable on a flaky link.",
  "",
  "Reproduction is unreliable by hand. The clearest signal is that the attempt counter stops incrementing while the socket stays in CLOSING.",
  "",
  "- The jitter is applied before the cap rather than after.",
  "- The timer is not cancelled on an explicit disconnect.",
  "- Nothing resets the attempt counter on a successful open.",
  "",
  "Fixing the ordering is the small half. The counter reset is the one that changes what a user feels, because it turns a brief blip into a minutes-long outage on their screen.",
].join("\n");

/**
 * An issue with no description at all.
 *
 * `IssueBody` renders nothing without content, so the block that follows the
 * sticky header changes identity — and the header is `border-b`, so whatever
 * lands there must not draw its own top rule. Both widths are covered because
 * the block is a different one in each: the conversation heading above `xl`,
 * the meta rail below it.
 */
test("an issue with no description draws one rule under the header, not two", async ({
  page,
}) => {
  await enableProjectsFeature(page);
  await installMockBridge(page);
  await page.setViewportSize({ width: 1440, height: 620 });

  await openProject(page);
  await seedAndOpenIssue(page, "An issue with no description at all", "");
  await expect(page.getByTestId("project-work-item-description")).toHaveCount(
    0,
  );

  // Every boundary where two blocks actually touch carries at most one
  // hairline. Asserted over all of them rather than the one that regressed,
  // because the point is the invariant, not the instance.
  for (const rule of await boundaryRules(page)) {
    expect(rule.total).toBeLessThanOrEqual(1);
  }

  // Below xl the rail takes the first slot instead, and it brings a border on
  // both edges. Same invariant, different block.
  await page.setViewportSize({ width: 900, height: 620 });
  await page
    .getByTestId("project-issue-thread-scroll")
    .evaluate((element) => element.scrollTo({ top: 0 }));
  await expect(page.getByTestId("project-issue-meta-rail")).toBeVisible();
  for (const rule of await boundaryRules(page)) {
    expect(rule.total).toBeLessThanOrEqual(1);
  }
});

/**
 * A description long enough to clamp, inside the panel.
 *
 * `-webkit-line-clamp` needs `display: -webkit-box`, and putting that on a
 * container of several block children is the combination worth having eyes on:
 * it can silently count blocks instead of lines. Asserted through the toggle
 * because that is the reader's whole interface to it.
 */
test("a long description clamps inside the panel and expands back", async ({
  page,
}) => {
  await enableProjectsFeature(page);
  await installMockBridge(page);
  await page.setViewportSize({ width: 1440, height: 620 });

  await openProject(page);
  await seedAndOpenIssue(
    page,
    "Fix reconnect backoff jitter",
    LONG_DESCRIPTION,
  );

  const description = page.getByTestId("project-work-item-description");
  const toggle = page.getByTestId("project-issue-body-toggle");
  await expect(toggle).toBeVisible();
  await expect(toggle).toHaveText("Show more");

  const measure = () =>
    description.evaluate((section) => {
      const panel = section.querySelector("div") as HTMLElement;
      const clamped = panel.querySelector("div") as HTMLElement;
      const button = section.querySelector(
        '[data-testid="project-issue-body-toggle"]',
      ) as HTMLElement;
      const panelRect = panel.getBoundingClientRect();
      const buttonRect = button.getBoundingClientRect();
      return {
        clipped: clamped.scrollHeight - clamped.clientHeight > 1,
        clampedHeight: Math.round(clamped.getBoundingClientRect().height),
        panelHeight: Math.round(panelRect.height),
        // The toggle belongs to the document, so it has to be inside the
        // panel's box and not floating in the gutter beneath it.
        buttonInsidePanel:
          buttonRect.top >= panelRect.top - 1 &&
          buttonRect.bottom <= panelRect.bottom + 1,
      };
    });

  const collapsed = await measure();
  expect(collapsed.clipped).toBe(true);
  expect(collapsed.buttonInsidePanel).toBe(true);

  await toggle.click();
  await expect(toggle).toHaveText("Show less");
  const expanded = await measure();
  // Really showing more: the clamp is off and the panel grew with it.
  expect(expanded.clipped).toBe(false);
  expect(expanded.clampedHeight).toBeGreaterThan(collapsed.clampedHeight);
  expect(expanded.panelHeight).toBeGreaterThan(collapsed.panelHeight);
  expect(expanded.buttonInsidePanel).toBe(true);

  await toggle.click();
  await expect(toggle).toHaveText("Show more");
  const recollapsed = await measure();
  expect(recollapsed.clipped).toBe(true);
  // Exactly back, not approximately: a clamp that re-measures itself wrong on
  // the way back is the failure mode worth naming.
  expect(recollapsed.panelHeight).toBe(collapsed.panelHeight);
});

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
 * Focusing the composer must not cost the floor.
 *
 * The docked composer is a flex sibling *outside* the scroll region, and it
 * expands from a one-line bar when focused. That expansion is taken out of the
 * thread's height: measured here, `clientHeight` drops by ~28px while
 * `scrollTop` and `scrollHeight` are untouched, which is the newest comment
 * sliding above the floor at the exact moment the reader is answering it.
 *
 * Nothing in the hook noticed until the ResizeObserver was pointed at the
 * scroll container as well as its content — the behaviour was held up by the
 * composer refocusing its own editor after expanding, which re-ran the focus
 * handler against the new height. That is another component's internal
 * sequencing. This test is what makes it an invariant instead of a coincidence.
 */
test("focusing the composer keeps the thread on its newest comment", async ({
  page,
}) => {
  await enableProjectsFeature(page);
  await installMockBridge(page);
  await page.setViewportSize({ width: 1440, height: 620 });

  const issueId = await openFirstIssue(page);
  for (let index = 0; index < 12; index += 1) {
    await pushComment(page, issueId, `Reply before composing ${index + 1}.`);
  }
  await expect(page.getByText("Reply before composing 12.")).toBeVisible({
    timeout: 10_000,
  });
  await expect
    .poll(async () => {
      const { clientHeight, scrollHeight, scrollTop } =
        await threadMetrics(page);
      return scrollHeight - clientHeight - scrollTop <= 32;
    })
    .toBe(true);

  const before = await threadMetrics(page);
  expect(before.scrollTop).toBeGreaterThan(0);

  await page.locator('[data-placeholder="Add a comment…"]').first().click();

  // The composer really did take height out of the thread, so what follows is
  // a claim about a container that shrank rather than one that never moved.
  await expect
    .poll(async () => (await threadMetrics(page)).clientHeight)
    .toBeLessThan(before.clientHeight);

  // And the floor still holds — *exactly*, not within the 32px at-bottom
  // threshold. The measured slip is 28px, which is inside that threshold, so
  // asserting `<= 32` here would pass with the defect fully present and this
  // test would be decoration. What is being claimed is that the thread is on
  // its floor, which is what a reader answering the newest comment is owed.
  await expect
    .poll(async () => {
      const { clientHeight, scrollHeight, scrollTop } =
        await threadMetrics(page);
      return scrollHeight - clientHeight - scrollTop;
    })
    .toBeLessThanOrEqual(2);

  // The newest comment is inside the *thread*, not merely inside the window:
  // at a 28px slip it is still on screen, just clipped by the scroll box it
  // lives in, which `toBeInViewport` would not notice.
  const clipped = await page.evaluate(() => {
    const scroller = document.querySelector(
      '[data-testid="project-issue-thread-scroll"]',
    );
    const comments = [
      ...document.querySelectorAll(
        '[data-testid="project-issue-thread-scroll"] p',
      ),
    ];
    const last = comments.find((node) =>
      node.textContent?.includes("Reply before composing 12."),
    );
    if (!scroller || !last) return null;
    return (
      last.getBoundingClientRect().bottom -
      scroller.getBoundingClientRect().bottom
    );
  });
  expect(clipped).not.toBeNull();
  expect(clipped as number).toBeLessThanOrEqual(0);
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
