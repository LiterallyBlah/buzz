import assert from "node:assert/strict";
import { after, afterEach, before, test } from "node:test";

import { JSDOM } from "jsdom";

const AUTHOR = "b".repeat(64);
const OTHER = "c".repeat(64);
const ISSUE_ID = "e".repeat(64);

const dom = new JSDOM("<!doctype html><html><body></body></html>", {
  url: "http://localhost",
});

let cleanup;
const queryClients = [];

before(async () => {
  const { window } = dom;
  // Radix's dropdown and alert dialog measure and capture pointers; jsdom has
  // none of that, so the primitives below are the minimum that lets the menu
  // open at all. Without them the trigger renders and nothing ever happens,
  // which reads as a missing affordance rather than a missing polyfill.
  window.ResizeObserver = class {
    observe() {}
    unobserve() {}
    disconnect() {}
  };
  window.matchMedia = () => ({
    matches: false,
    addEventListener() {},
    removeEventListener() {},
  });
  window.HTMLElement.prototype.hasPointerCapture = () => false;
  window.HTMLElement.prototype.setPointerCapture = () => {};
  window.HTMLElement.prototype.releasePointerCapture = () => {};
  window.HTMLElement.prototype.scrollIntoView = () => {};
  window.DOMRect =
    window.DOMRect ??
    class {
      constructor(x = 0, y = 0, width = 0, height = 0) {
        Object.assign(this, {
          x,
          y,
          width,
          height,
          top: y,
          left: x,
          bottom: y + height,
          right: x + width,
        });
      }
    };

  Object.assign(globalThis, {
    CustomEvent: window.CustomEvent,
    document: window.document,
    Element: window.Element,
    Event: window.Event,
    EventTarget: window.EventTarget,
    HTMLElement: window.HTMLElement,
    KeyboardEvent: window.KeyboardEvent,
    MouseEvent: window.MouseEvent,
    MutationObserver: window.MutationObserver,
    Node: window.Node,
    IS_REACT_ACT_ENVIRONMENT: true,
    getComputedStyle: window.getComputedStyle.bind(window),
    requestAnimationFrame: (callback) => window.setTimeout(callback, 0),
    cancelAnimationFrame: (handle) => window.clearTimeout(handle),
    window,
  });

  ({ cleanup } = await import("@testing-library/react"));
});

after(() => dom.window.close());
afterEach(() => {
  cleanup?.();
  // React Query keeps a five-minute garbage-collection timer per cache entry,
  // which is enough to hold the test process open long after the assertions.
  while (queryClients.length > 0) queryClients.pop().clear();
});

async function renderMenu({ viewerPubkey }) {
  const { createElement } = await import("react");
  const { render } = await import("@testing-library/react");
  const { QueryClient, QueryClientProvider } = await import(
    "@tanstack/react-query"
  );
  const { ProjectItemDeleteMenu } = await import("./ProjectItemDeleteMenu.tsx");

  const queryClient = new QueryClient({
    defaultOptions: { queries: { gcTime: 0, retry: false } },
  });
  queryClients.push(queryClient);
  // `useIdentityQuery` never expires its cache entry, so seeding it is the
  // whole of "who is signed in" for this component.
  queryClient.setQueryData(["identity"], { pubkey: viewerPubkey });

  return render(
    createElement(
      QueryClientProvider,
      { client: queryClient },
      createElement(ProjectItemDeleteMenu, {
        author: AUTHOR,
        label: "More options for Something is broken",
        project: { id: "project-1" },
        rootId: ISSUE_ID,
        subject: "issue",
        targetId: ISSUE_ID,
        testId: `issue-${ISSUE_ID}`,
        title: "Something is broken",
      }),
    ),
  );
}

test("someone who did not write the issue is offered nothing", async () => {
  const view = await renderMenu({ viewerPubkey: OTHER });

  assert.equal(
    view.container.innerHTML,
    "",
    "no trigger, no menu, no dialog — the row is unchanged for everyone else",
  );
});

test("the author gets a delete entry behind an explicit confirmation", async () => {
  const { fireEvent, screen } = await import("@testing-library/react");
  await renderMenu({ viewerPubkey: AUTHOR });

  const trigger = screen.getByLabelText("More options for Something is broken");
  fireEvent.pointerDown(trigger, { button: 0, ctrlKey: false });

  const deleteItem = await screen.findByTestId(
    `project-delete-issue-${ISSUE_ID}`,
  );
  assert.equal(deleteItem.textContent, "Delete issue");

  fireEvent.click(deleteItem);

  const dialog = await screen.findByTestId(
    `project-delete-confirm-issue-${ISSUE_ID}`,
  );
  assert.match(dialog.textContent, /Delete issue\?/);
  // Explicit about the Buzz-local blast radius, the immutable-protocol limit,
  // and who the operation is available to.
  assert.match(dialog.textContent, /“Something is broken”/);
  assert.match(dialog.textContent, /for everyone in this community/);
  assert.match(dialog.textContent, /Copies saved elsewhere may remain/);
  assert.match(dialog.textContent, /cannot be undone here/);
  // Confirming is a deliberate second step, never the menu entry itself.
  assert.ok(
    screen.getByTestId(`project-delete-confirm-button-issue-${ISSUE_ID}`),
  );
  assert.ok(screen.getByText("Cancel"));
});
