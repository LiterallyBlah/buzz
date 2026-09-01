/**
 * Closing the two parked active-realm escapes (P3 round 5).
 *
 * Round 4 left both open by agreement:
 *   1. an external script in an extension-created `srcdoc` child;
 *   2. same-origin self-navigation into a non-entry active document (SVG).
 *
 * Both recover a fresh realm the document prologue never reached, and both
 * produced a working `RTCPeerConnection` and live sink packets.
 *
 * **This round closes route 2 only.**
 *   - Route 2 is closed by the host setting `script-src 'none'` on the active
 *     non-HTML document family (`asset_content_security_policy`). The document
 *     still loads and still renders as an image; it cannot execute.
 *   - **Route 1 remains open.** It is assigned to the isolation phase, not
 *     closed here.
 *
 * Two mechanisms that earlier revisions of this comment claimed are **not** in
 * production and were deliberately reverted: the wrapper's `frame-src` does
 * *not* pin the exact entry URL (it names the origin, so multi-page packages
 * keep working), and this host does *not* prepend neutralisation to every
 * served script. Do not re-derive route-1 coverage from either.
 *
 * Chromium only, as ever. WebKitGTK confirmation is Hermes's.
 */
import { expect, test } from "@playwright/test";

import { installMockBridge } from "../helpers/bridge";

const HOST = "http://127.0.0.1:51234";
const ENTRY = `${HOST}/ext/demo/index.html`;

type Mark = { mark: string; rtc?: string; connected?: boolean };

/**
 * Navigate a locked extension frame into a package SVG and report what ran.
 *
 * `svgScriptNone` toggles the one wall under test, so the control row proves
 * the route is genuinely live before the protected row asserts it is closed.
 */
async function runRealms(
  page: import("@playwright/test").Page,
  options: { svgScriptNone: boolean },
) {
  // Mirrors the shipped extension policy, including the image source — without
  // it `default-src 'none'` blocks the legitimate <img> row for reasons that
  // have nothing to do with the repair under test.
  const extCsp = [
    "default-src 'none'",
    `script-src ${HOST}`,
    `img-src ${HOST} data: blob:`,
    "connect-src 'none'",
  ].join("; ");
  const wrapCsp = [
    "default-src 'none'",
    `frame-src ${HOST}`,
    "script-src 'unsafe-inline'",
    "style-src 'unsafe-inline'",
  ].join("; ");

  // Server-side request witnesses. The page-side marks alone cannot distinguish
  // "the script was blocked" from "the navigation never happened", so the
  // protected row asserts the document *was* fetched and the script was *not*.
  const requested: string[] = [];

  await page.route(`${HOST}/**`, async (route) => {
    const url = new URL(route.request().url());
    requested.push(url.pathname);
    const send = (body: string, contentType: string, csp: string) =>
      route.fulfill({
        status: 200,
        contentType,
        headers: { "Content-Security-Policy": csp },
        body,
      });

    if (url.pathname.startsWith("/frame/")) {
      return send(
        `<!doctype html><style>html,body{margin:0;height:100%}iframe{border:0;width:100%;height:100%}</style><iframe id="ext" sandbox="allow-scripts" src="${ENTRY}"></iframe>`,
        "text/html; charset=utf-8",
        wrapCsp,
      );
    }
    if (url.pathname.endsWith("svg.js")) {
      return send(
        `top.postMessage({ mark: "svg-external-ran", rtc: typeof RTCPeerConnection }, "*");`,
        "text/javascript; charset=utf-8",
        extCsp,
      );
    }
    if (url.pathname.endsWith("plain.svg")) {
      // An ordinary decorative asset — the legitimate case Hermes asked to keep
      // green. It carries the same policy as the hostile one.
      return send(
        `<svg xmlns="http://www.w3.org/2000/svg" width="8" height="8"><rect width="8" height="8" fill="red"/></svg>`,
        "image/svg+xml",
        options.svgScriptNone
          ? extCsp.replace(`script-src ${HOST}`, "script-src 'none'")
          : extCsp,
      );
    }
    if (url.pathname.endsWith("asset.svg")) {
      // Both forms Hermes named: an inline handler and an external script.
      return send(
        `<svg xmlns="http://www.w3.org/2000/svg" onload="top.postMessage({mark:'svg-inline-ran'},'*')"><script href="${HOST}/ext/demo/svg.js"/></svg>`,
        "image/svg+xml",
        options.svgScriptNone
          ? extCsp.replace(`script-src ${HOST}`, "script-src 'none'")
          : extCsp,
      );
    }
    if (url.pathname.endsWith("probe.js")) {
      return send(
        `top.postMessage({ mark: "parent-ran", rtc: typeof RTCPeerConnection }, "*");
         var img = new Image();
         img.onload = function () { top.postMessage({ mark: "img-rendered" }, "*"); };
         img.onerror = function () { top.postMessage({ mark: "img-failed" }, "*"); };
         img.src = "${HOST}/ext/demo/plain.svg";
         setTimeout(function () {
           // Posted before navigating so a broken timer is distinguishable from
           // a blocked script: no mark means the row never attempted the route.
           top.postMessage({ mark: "svg-nav-attempted" }, "*");
           location.href = "${HOST}/ext/demo/asset.svg";
         }, 1200);`,
        "text/javascript; charset=utf-8",
        extCsp,
      );
    }
    return send(
      `<!doctype html><body><script src="${HOST}/ext/demo/probe.js"></script>`,
      "text/html; charset=utf-8",
      extCsp,
    );
  });

  await installMockBridge(page);
  await page.goto("/#/extensions");
  const marks = (await page.evaluate(
    (host) =>
      new Promise((resolve) => {
        const seen: unknown[] = [];
        const timer = setTimeout(() => resolve(seen), 5000);
        window.addEventListener("message", (event) => {
          const data = event.data as { mark?: string };
          if (data?.mark) seen.push(data);
          if (seen.length >= 5) {
            clearTimeout(timer);
            setTimeout(() => resolve(seen), 300);
          }
        });
        const frame = document.createElement("iframe");
        frame.setAttribute("sandbox", "allow-scripts");
        frame.src = `${host}/frame/demo`;
        document.body.append(frame);
      }),
    HOST,
  )) as Mark[];
  return {
    marks,
    of: (name: string) => marks.find((m) => m.mark === name),
    fetched: (suffix: string) => requested.some((p) => p.endsWith(suffix)),
  };
}

test("CONTROL: the SVG document really does execute package script", async ({
  page,
}) => {
  // Permanent. Proves the route is observable — without it the protected row
  // below could pass because the navigation never happened.
  const result = await runRealms(page, { svgScriptNone: false });

  expect(result.of("parent-ran")).toBeTruthy();
  expect(result.of("img-rendered")).toBeTruthy();
  // Navigation witnesses: the route was attempted and the document was served.
  expect(result.of("svg-nav-attempted")).toBeTruthy();
  expect(result.fetched("asset.svg")).toBe(true);
  // The external script is the live route: it executes in the SVG's own realm
  // with a working constructor, which is what shed the initial lockdown.
  expect(result.fetched("svg.js")).toBe(true);
  expect(result.of("svg-external-ran")?.rtc).toBe("function");
  // The inline handler is already dead — closed earlier by the no-inline
  // ratification, not by this repair. Asserted so the distinction is on record
  // rather than silently credited to the wrong wall.
  expect(result.of("svg-inline-ran")).toBeUndefined();
});

test("the SVG document loads but cannot execute under the repair", async ({
  page,
}) => {
  const result = await runRealms(page, { svgScriptNone: true });

  expect(result.of("parent-ran")).toBeTruthy();
  // Ordinary rendering is untouched: an image never runs script.
  expect(result.of("img-rendered")).toBeTruthy();
  expect(result.of("img-failed")).toBeUndefined();

  // The witnesses that make the silence below mean something. Without these,
  // a timer that never fired would satisfy "no script ran" just as well as the
  // repair does. The document must be reached before its emptiness is evidence.
  expect(result.of("svg-nav-attempted")).toBeTruthy();
  expect(result.fetched("asset.svg")).toBe(true);

  // The external route — the one that was live — no longer executes. Under
  // `script-src 'none'` the script is not even fetched, so the server-side
  // witness is the stronger assertion of the two.
  expect(result.fetched("svg.js")).toBe(false);
  expect(result.of("svg-external-ran")).toBeUndefined();
  expect(result.of("svg-inline-ran")).toBeUndefined();
});

// ── Required row: the repair must not change legitimate worker behaviour ────

/**
 * Measure what a worker can do from inside the extension realm.
 *
 * **What this row is, precisely:** engine-capability evidence. It hand-writes
 * the CSP headers below; it does **not** invoke the production selector
 * `asset_content_security_policy()`. And because this row's frame goes straight
 * to `index.html` → `probe.js`, the one asset the repair downgrades is never
 * requested — so the two runs are identical by construction rather than
 * differing by the repair.
 *
 * It therefore does not independently test the Rust policy selector. That
 * coupling is established by the Rust policy test; the two together are what
 * cover the claim. Read this row only for what a worker can reach from inside
 * the extension realm.
 */
async function workerBehaviour(
  page: import("@playwright/test").Page,
  svgScriptNone: boolean,
) {
  const extCsp = [
    "default-src 'none'",
    `script-src ${HOST}`,
    `img-src ${HOST} data: blob:`,
    "connect-src 'none'",
  ].join("; ");

  await page.route(`${HOST}/**`, async (route) => {
    const url = new URL(route.request().url());
    const send = (body: string, contentType: string, csp: string) =>
      route.fulfill({
        status: 200,
        contentType,
        headers: { "Content-Security-Policy": csp },
        body,
      });

    // The asset the route-2 repair downgrades. Note it is **never requested in
    // this row** — the frame loads `index.html` → `probe.js` and stops — so it
    // does not make the two runs differ. Kept only so the fixture's policy
    // shape matches the other rows; it is not what makes this test meaningful.
    if (url.pathname.endsWith("asset.svg")) {
      return send(
        `<svg xmlns="http://www.w3.org/2000/svg" width="4" height="4"></svg>`,
        "image/svg+xml",
        svgScriptNone
          ? extCsp.replace(`script-src ${HOST}`, "script-src 'none'")
          : extCsp,
      );
    }
    if (url.pathname.endsWith("imported.js")) {
      return send(
        `self.__imported = true;`,
        "text/javascript; charset=utf-8",
        extCsp,
      );
    }
    if (url.pathname.endsWith("worker.js")) {
      return send(
        `try { importScripts("${HOST}/ext/demo/imported.js");
               postMessage({ ok: true, imported: self.__imported === true }); }
         catch (e) { postMessage({ ok: false, error: String(e && e.name) }); }`,
        "text/javascript; charset=utf-8",
        extCsp,
      );
    }
    if (url.pathname.endsWith("probe.js")) {
      return send(
        `(function () {
           var report = { sameOrigin: null, blob: null, worklet: null };
           function done() { top.postMessage({ mark: "worker-report", report: report }, "*"); }
           var pending = 2;
           function settle() { if (--pending === 0) done(); }

           try {
             var w = new Worker("${HOST}/ext/demo/worker.js");
             w.onmessage = function (e) { report.sameOrigin = JSON.stringify(e.data); settle(); };
             w.onerror = function () { report.sameOrigin = "error"; settle(); };
           } catch (e) { report.sameOrigin = "threw:" + (e && e.name); settle(); }

           try {
             var src = 'try { importScripts("${HOST}/ext/demo/imported.js"); postMessage({ ok: true, imported: self.__imported === true }); } catch (e) { postMessage({ ok: false, error: String(e && e.name) }); }';
             var b = new Worker(URL.createObjectURL(new Blob([src], { type: "text/javascript" })));
             b.onmessage = function (e) { report.blob = JSON.stringify(e.data); settle(); };
             b.onerror = function () { report.blob = "error"; settle(); };
           } catch (e) { report.blob = "threw:" + (e && e.name); settle(); }

           try {
             report.worklet = typeof CSS !== "undefined" && CSS.paintWorklet ? "available" : "absent";
           } catch (e) { report.worklet = "threw"; }

           setTimeout(done, 3000);
         })();`,
        "text/javascript; charset=utf-8",
        extCsp,
      );
    }
    return send(
      `<!doctype html><body><script src="${HOST}/ext/demo/probe.js"></script>`,
      "text/html; charset=utf-8",
      extCsp,
    );
  });

  await installMockBridge(page);
  await page.goto("/#/extensions");
  return (await page.evaluate(
    (host) =>
      new Promise((resolve) => {
        const timer = setTimeout(() => resolve({ timeout: true }), 8000);
        window.addEventListener("message", (event) => {
          const data = event.data as { mark?: string; report?: unknown };
          if (data?.mark === "worker-report") {
            clearTimeout(timer);
            resolve(data.report);
          }
        });
        const frame = document.createElement("iframe");
        frame.setAttribute("sandbox", "allow-scripts");
        frame.src = `${host}/ext/demo/index.html`;
        document.body.append(frame);
      }),
    HOST,
  )) as Record<string, string | null>;
}

test("the route-2 repair leaves worker behaviour unchanged", async ({
  page,
}) => {
  // Hermes flagged that response CSP is contextual and workers derive their
  // execution policy from their own response. The repair deliberately does not
  // touch `text/javascript`.
  //
  // Read the equality below for what it is: the downgraded asset is never
  // requested in this row, so `before === after` is expected by construction
  // and is a regression guard, not proof that the repair spares workers. The
  // load-bearing assertions are the concrete outcomes pinned underneath it.
  const before = await workerBehaviour(page, false);
  const after = await workerBehaviour(page, true);

  expect(after).toEqual(before);

  // Pin the concrete outcome too. "before equals after" is satisfied by two
  // runs that both timed out, so the measured values are asserted rather than
  // only compared.
  //
  // The finding: a worker is not reachable from the extension realm *at all*,
  // and not because of this repair. A sandboxed document has an opaque origin,
  // so a same-origin worker URL is not same-origin to it; and `blob:` is not a
  // `script-src` source under the no-inline policy. Hermes's concern about
  // response-CSP and workers is therefore moot in this context — which is why
  // the repair still correctly leaves `text/javascript` alone rather than
  // relying on that being true.
  expect(after.sameOrigin).toBe("threw:SecurityError");
  expect(after.blob).toBe("error");
});
