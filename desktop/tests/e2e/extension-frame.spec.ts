/**
 * Hosting an installed extension (M1-P3).
 *
 * **What this spec pins, and what it cannot.** The BX-09 containment result —
 * that a real sandboxed frame cannot reach a Tauri app command — was observed
 * on Windows against a live WebView2 runtime and lives in `probe/bx09/`. It is
 * not reproducible here: this harness *mocks* Tauri IPC, so asserting that
 * `__TAURI_INTERNALS__` is absent inside the frame would be asserting a
 * property of the mock, not of the product. That assertion is deliberately not
 * written.
 *
 * What CI can pin is the shape of what ships, which is what actually decides
 * the containment: the sandbox token list, and that the frame's origin is
 * remote-class rather than a registered custom scheme. Both are asserted below
 * against the DOM the app really renders.
 */
import { expect, test } from "@playwright/test";

import { installMockBridge } from "../helpers/bridge";

const MANIFEST = JSON.stringify({
  id: "equation-explorer",
  name: "Equation Explorer",
  version: "0.1.0",
  entry: "index.html",
});

async function installOne(page: import("@playwright/test").Page) {
  await page.goto("/#/extensions");
  await page.getByTestId("install-extension-from-folder").click();
  await expect(
    page.getByTestId("installed-extension-equation-explorer"),
  ).toBeVisible();
}

test("opening an installed extension renders it in a sandboxed frame", async ({
  page,
}) => {
  await installMockBridge(page, {
    extensionPickPath: "/tmp/equation-explorer",
    extensionPreviewManifest: MANIFEST,
    extensionFrameOrigin: "http://127.0.0.1:51234",
  });
  await installOne(page);

  await page.getByTestId("open-extension-equation-explorer").click();

  const frame = page.getByTestId("extension-frame");
  await expect(frame).toBeVisible();

  // Exactly `allow-scripts`. `allow-same-origin` would collapse the opaque
  // origin the whole route depends on, so this is an equality check, not a
  // "contains" — a token added later must fail here.
  await expect(frame).toHaveAttribute("sandbox", "allow-scripts");
  const tokens = ((await frame.getAttribute("sandbox")) ?? "").split(/\s+/);
  expect(tokens).toEqual(["allow-scripts"]);

  // Remote-class origin. A registered custom scheme is classified *local* by
  // Tauri and bypasses the app ACL, which is the failure decision 002 exists to
  // prevent — so the src must be plain http on a loopback address.
  const src = (await frame.getAttribute("src")) ?? "";
  // The wrapper, not the extension document: framing the extension directly
  // removes the container whose `frame-src` is the navigation wall.
  expect(src).toMatch(/^http:\/\/127\.0\.0\.1:\d+\/frame\/equation-explorer$/);
  expect(src.startsWith("tauri://")).toBe(false);
  expect(src.startsWith("buzz-media://")).toBe(false);
  expect(src.startsWith("asset://")).toBe(false);
});

test("leaving the tab releases the frame host", async ({ page }) => {
  // The leak this guards against is a localhost listener that outlives the tab
  // that needed it.
  await installMockBridge(page, {
    extensionPickPath: "/tmp/equation-explorer",
    extensionPreviewManifest: MANIFEST,
  });
  await installOne(page);

  const holders = async () =>
    await page.evaluate(() =>
      window.__BUZZ_E2E_INVOKE_MOCK_COMMAND__("__mock_extension_frame_holders"),
    );

  expect(await holders()).toBe(0);

  await page.getByTestId("open-extension-equation-explorer").click();
  await expect(page.getByTestId("extension-frame")).toBeVisible();
  expect(await holders()).toBe(1);

  await page.getByTestId("extension-frame-back").click();
  await expect(page.getByTestId("extensions-view")).toBeVisible();
  await expect.poll(holders).toBe(0);
});

test("the frame does not render when the preview flag is off", async ({
  page,
}) => {
  await installMockBridge(page, {
    extensionPickPath: "/tmp/equation-explorer",
    extensionPreviewManifest: MANIFEST,
  });
  await page.addInitScript(() => {
    for (let index = 0; index < window.localStorage.length; index += 1) {
      const key = window.localStorage.key(index);
      if (key?.startsWith("buzz-feature-overrides-v")) {
        const overrides = JSON.parse(
          window.localStorage.getItem(key) ?? "{}",
        ) as Record<string, boolean>;
        overrides.extensions = false;
        window.localStorage.setItem(key, JSON.stringify(overrides));
      }
    }
  });

  await page.goto("/#/extensions/equation-explorer");

  await expect(page.getByTestId("extension-frame-disabled")).toBeVisible();
  await expect(page.getByTestId("extension-frame")).toHaveCount(0);
});

test("a sandboxed document under the served policy can still reach its parent", async ({
  page,
}) => {
  // BRIDGE_SPEC §2's handshake — the frame posts `{buzz:"ready"}` and the host
  // replies by transferring a MessagePort — has to remain possible. Asserting
  // that from the CSP spec would be reasoning; this runs it.
  //
  // The policy and the sandbox token are the ones the frame host serves,
  // including **no** `'unsafe-inline'`, so the probe ships as an external
  // script exactly as a real package must.
  const origin = "http://127.0.0.1:51234";
  const policy = [
    "default-src 'none'",
    `script-src ${origin}`,
    "connect-src 'none'",
    "webrtc 'block'",
    "base-uri 'none'",
    "form-action 'none'",
  ].join("; ");

  await page.route(`${origin}/**`, async (route) => {
    const url = route.request().url();
    if (url.endsWith("ready.js")) {
      await route.fulfill({
        status: 200,
        contentType: "text/javascript; charset=utf-8",
        headers: { "Content-Security-Policy": policy },
        body: `parent.postMessage({ buzz: "ready" }, "*");`,
      });
      return;
    }
    await route.fulfill({
      status: 200,
      contentType: "text/html; charset=utf-8",
      headers: { "Content-Security-Policy": policy },
      body: `<!doctype html><script src="${origin}/ext/demo/ready.js"></script>`,
    });
  });

  await installMockBridge(page);
  await page.goto("/#/extensions");

  const outcome = await page.evaluate(
    (host) =>
      new Promise<{ got: unknown; origin: string }>((resolve) => {
        const frame = document.createElement("iframe");
        frame.setAttribute("sandbox", "allow-scripts");
        frame.src = `${host}/ext/demo/index.html`;

        const timer = window.setTimeout(
          () => resolve({ got: null, origin: "" }),
          5000,
        );
        window.addEventListener("message", function onMessage(event) {
          if (event.source !== frame.contentWindow) return;
          window.clearTimeout(timer);
          window.removeEventListener("message", onMessage);
          resolve({ got: event.data, origin: event.origin });
        });
        document.body.append(frame);
      }),
    origin,
  );

  expect(outcome.got).toEqual({ buzz: "ready" });
  // §2 relies on source-identity, not origin, precisely because a sandboxed
  // frame's origin is the string "null". Pin that so a future change that
  // starts trusting `event.origin` is caught here.
  expect(outcome.origin).toBe("null");
});

test("the app really does load a frame from a remote-class localhost origin", async ({
  page,
}) => {
  // The required empirical check for the `csp: null` finding: with no parent
  // CSP in the tree, nothing should block framing an http://127.0.0.1 origin.
  // Rather than reason from "there is no policy", this serves a real response
  // on that origin — carrying the same headers the Rust frame host sends — and
  // asserts the document loads *and* reaches the parent.
  //
  // What this does not cover: the Rust server's own bytes (its behaviour is
  // covered by the Rust suite) and the BX-09 IPC rejection (platform-probed).
  const origin = "http://127.0.0.1:51234";
  const policy = [
    "default-src 'none'",
    `script-src ${origin} 'unsafe-inline'`,
    "connect-src 'none'",
    "base-uri 'none'",
    "form-action 'none'",
  ].join("; ");

  await page.route(`${origin}/**`, async (route) => {
    await route.fulfill({
      status: 200,
      contentType: "text/html; charset=utf-8",
      headers: {
        "Content-Security-Policy": policy,
        "X-Content-Type-Options": "nosniff",
      },
      body: `<!doctype html><title>hosted</title><script>parent.postMessage({ buzz: "ready" }, "*");</script>`,
    });
  });

  await installMockBridge(page, {
    extensionPickPath: "/tmp/equation-explorer",
    extensionPreviewManifest: MANIFEST,
    extensionFrameOrigin: origin,
  });

  // Install first: `installOne` navigates, and a navigation would destroy the
  // execution context holding the listener.
  await installOne(page);

  // Route changes are hash-only from here, so this context survives the click.
  const handshake = page.evaluate(
    () =>
      new Promise<string>((resolve) => {
        const timer = window.setTimeout(() => resolve("timeout"), 5000);
        window.addEventListener("message", function onMessage(event) {
          if (
            typeof event.data === "object" &&
            event.data !== null &&
            (event.data as { buzz?: string }).buzz === "ready"
          ) {
            window.clearTimeout(timer);
            window.removeEventListener("message", onMessage);
            resolve(event.origin);
          }
        });
      }),
  );

  await page.getByTestId("open-extension-equation-explorer").click();
  await expect(page.getByTestId("extension-frame")).toBeVisible();

  // The frame loaded, ran its script, and reached the parent — so nothing in
  // the app blocked framing this origin.
  expect(await handshake).toBe("null");
});

test("the extension fills the wrapper instead of a default 300x150 box", async ({
  page,
}) => {
  // Regression for the wrapper's own CSP blocking its own inline style. With
  // `default-src 'none'` and no `style-src`, the wrapper's layout rules were
  // rejected and the extension rendered in the browser's default iframe box
  // with a border — a security header quietly becoming a layout bug.
  //
  // The wrapper markup and CSP below are the ones the frame host serves. It is
  // served from the page's own origin purely so the test can measure inside it:
  // a cross-origin frame's `contentDocument` is null, and `style-src
  // 'unsafe-inline'` behaves identically either way. The production origin is
  // asserted by the other tests in this file.
  const wrapperCsp = [
    "default-src 'none'",
    "frame-src 'self'",
    "script-src 'unsafe-inline'",
    "style-src 'unsafe-inline'",
    "connect-src 'none'",
  ].join("; ");

  await page.route("**/__wrapper-under-test", async (route) => {
    await route.fulfill({
      status: 200,
      contentType: "text/html; charset=utf-8",
      headers: { "Content-Security-Policy": wrapperCsp },
      body: `<!doctype html><meta charset="utf-8"><style>html,body{margin:0;height:100%}iframe{display:block;border:0;width:100%;height:100%}</style><iframe id="ext" sandbox="allow-scripts" src="about:blank"></iframe>`,
    });
  });

  await installMockBridge(page);
  await page.goto("/#/extensions");

  const box = await page.evaluate(
    () =>
      new Promise<{ outer: number[]; inner: number[]; border: string }>(
        (resolve) => {
          const outer = document.createElement("iframe");
          outer.style.cssText = "width:800px;height:600px;border:0";
          outer.src = "/__wrapper-under-test";
          outer.onload = () => {
            window.setTimeout(() => {
              const doc = outer.contentDocument;
              const inner = doc?.getElementById("ext") as HTMLIFrameElement;
              const rect = inner.getBoundingClientRect();
              const style = doc?.defaultView?.getComputedStyle(inner);
              resolve({
                outer: [outer.clientWidth, outer.clientHeight],
                inner: [Math.round(rect.width), Math.round(rect.height)],
                border: style?.borderTopWidth ?? "?",
              });
            }, 200);
          };
          document.body.append(outer);
        },
      ),
  );

  expect(box.inner[0]).toBe(box.outer[0]);
  expect(box.inner[1]).toBe(box.outer[1]);
  expect(box.border).toBe("0px");
  // The bug this guards: the browser's default iframe box.
  expect(box.inner).not.toEqual([300, 150]);
});
