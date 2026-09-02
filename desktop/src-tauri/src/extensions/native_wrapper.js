(() => {
  // biome-ignore lint/suspicious/noRedundantUseStrict: served as a classic inline script, not a module
  "use strict";

  const lease = __BUZZ_NATIVE_LEASE_JSON__;
  const frame = document.getElementById("ext");
  const internals = window.__TAURI_INTERNALS__;
  const eventInternals = window.__TAURI_EVENT_PLUGIN_INTERNALS__;
  const VERSION = 1;
  const MAX_FRAME_BYTES = 256 * 1024;
  const MAX_DEPTH = 32;
  const MAX_NODES = 2048;
  const MAX_IN_FLIGHT = 64;
  const MAX_SEEN = 512;
  const MAX_BATCH_FRAMES = 16;
  const MAX_BATCH_BYTES = 640 * 1024;
  const MAX_QUEUED_BATCHES_PER_SUB = 2;
  const MAX_QUEUED_BYTES_PER_SUB = 2 * 1024 * 1024;
  const MAX_QUEUED_BATCHES_PER_PORT = 8;
  const MAX_QUEUED_BYTES_PER_PORT = 3 * 1024 * 1024;
  const uuid =
    /^[0-9a-f]{8}-[0-9a-f]{4}-[1-5][0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/i;
  const encoder = new TextEncoder();

  let port = null;
  let settled = false;
  let disposed = false;
  let eventId = null;
  let drainScheduled = false;
  let queuedBytes = 0;
  const inFlight = new Set();
  const seen = new Set();
  const subscriptions = new Map();
  const queued = [];
  const queuedBySub = new Map();

  function invoke(command, args) {
    if (!internals || typeof internals.invoke !== "function") {
      return Promise.reject(new Error("Tauri invoke is unavailable"));
    }
    return internals.invoke(command, args);
  }

  function write(id, body) {
    if (port && !disposed) port.postMessage({ id, ...body });
  }

  function refuse(id, code, message) {
    write(id, { ok: false, error: { code, message } });
  }

  function remember(id) {
    seen.add(id);
    if (seen.size > MAX_SEEN) {
      const oldest = seen.values().next().value;
      if (oldest) seen.delete(oldest);
    }
  }

  function jsonCompatible(value) {
    const stack = [{ value, depth: 0 }];
    // Public frames are JSON-compatible data, not arbitrary structured-clone
    // graphs. Reject cycles and shared object identity fail-closed.
    const objects = new WeakSet();
    let nodes = 0;
    while (stack.length) {
      const item = stack.pop();
      if (!item) break;
      nodes += 1;
      if (nodes > MAX_NODES || item.depth > MAX_DEPTH) return false;
      const current = item.value;
      if (
        current === null ||
        typeof current === "string" ||
        typeof current === "boolean"
      )
        continue;
      if (typeof current === "number") {
        if (!Number.isFinite(current)) return false;
        continue;
      }
      if (typeof current !== "object") return false;
      if (objects.has(current)) return false;
      objects.add(current);
      if (Array.isArray(current)) {
        for (let i = current.length - 1; i >= 0; i -= 1) {
          stack.push({ value: current[i], depth: item.depth + 1 });
        }
      } else {
        const proto = Object.getPrototypeOf(current);
        if (proto !== Object.prototype && proto !== null) return false;
        for (const key of Object.keys(current)) {
          stack.push({ value: current[key], depth: item.depth + 1 });
        }
      }
    }
    try {
      return (
        encoder.encode(JSON.stringify(value)).byteLength <= MAX_FRAME_BYTES
      );
    } catch (_) {
      return false;
    }
  }

  function parseRequest(value) {
    if (!value || typeof value !== "object") return null;
    const id = value.id;
    if (typeof id !== "string" || !uuid.test(id)) return null;
    if (
      value.v !== VERSION ||
      typeof value.method !== "string" ||
      value.method.length < 1 ||
      value.method.length > 96 ||
      !jsonCompatible(value)
    ) {
      return { refused: true, id };
    }
    return {
      refused: false,
      id,
      v: value.v,
      method: value.method,
      params: value.params,
    };
  }

  function streamControl(action, sub, detail = {}) {
    return invoke("plugin:extension-bridge|stream_control", {
      lease,
      action,
      sub,
      seq: detail.seq,
      token: detail.token,
      frameCount: detail.frameCount,
      encodedBytes: detail.encodedBytes,
    });
  }

  function closeSubscription(sub, reason) {
    if (!subscriptions.has(sub)) return;
    subscriptions.delete(sub);
    if (port && !disposed) port.postMessage({ sub, kind: "closed", reason });
    void invoke("plugin:extension-bridge|invoke", {
      lease,
      v: VERSION,
      method: "unsubscribe",
      params: { sub },
    }).catch(() => {});
  }

  function encodedFrames(frames) {
    let total = 0;
    try {
      for (const frameValue of frames) {
        total += encoder.encode(JSON.stringify(frameValue)).byteLength;
        if (!Number.isSafeInteger(total)) return null;
      }
      return total;
    } catch (_) {
      return null;
    }
  }

  function parseBatch(payload) {
    if (!payload || typeof payload !== "object") return null;
    if (
      payload.generation !== lease ||
      typeof payload.sub !== "string" ||
      !subscriptions.has(payload.sub) ||
      !Number.isSafeInteger(payload.seq) ||
      payload.seq < 1 ||
      typeof payload.token !== "string" ||
      !uuid.test(payload.token) ||
      !Array.isArray(payload.frames) ||
      payload.frames.length < 1 ||
      payload.frames.length > MAX_BATCH_FRAMES ||
      payload.frameCount !== payload.frames.length ||
      !Number.isSafeInteger(payload.encodedBytes) ||
      payload.encodedBytes < 1 ||
      payload.encodedBytes > MAX_BATCH_BYTES ||
      typeof payload.terminal !== "boolean" ||
      encodedFrames(payload.frames) !== payload.encodedBytes
    )
      return null;
    for (const frameValue of payload.frames) {
      if (
        !frameValue ||
        typeof frameValue !== "object" ||
        frameValue.sub !== payload.sub ||
        typeof frameValue.kind !== "string"
      )
        return null;
    }
    if (
      payload.terminal &&
      (payload.frames.length !== 1 || payload.frames[0].kind !== "closed")
    )
      return null;
    return payload;
  }

  function flowViolation(sub) {
    if (!subscriptions.has(sub)) return;
    void streamControl("violation", sub).catch(() => {});
    closeSubscription(sub, "flow-control");
  }

  function drain() {
    drainScheduled = false;
    while (!disposed && port && queued.length) {
      const batch = queued.shift();
      queuedBytes -= batch.encodedBytes;
      const totals = queuedBySub.get(batch.sub);
      if (totals) {
        totals.batches -= 1;
        totals.bytes -= batch.encodedBytes;
        if (totals.batches === 0) queuedBySub.delete(batch.sub);
      }
      const state = subscriptions.get(batch.sub);
      if (!state || batch.seq !== state.nextSeq) {
        flowViolation(batch.sub);
        continue;
      }
      state.nextSeq += 1;
      port.postMessage({ buzz: "stream-batch", ...batch });
      if (batch.terminal) subscriptions.delete(batch.sub);
    }
  }

  function onStream(event) {
    const batch = parseBatch(event?.payload);
    if (!batch) return;
    const totals = queuedBySub.get(batch.sub) || { batches: 0, bytes: 0 };
    if (
      !batch.terminal &&
      (totals.batches + 1 > MAX_QUEUED_BATCHES_PER_SUB ||
        totals.bytes + batch.encodedBytes > MAX_QUEUED_BYTES_PER_SUB ||
        queued.length + 1 > MAX_QUEUED_BATCHES_PER_PORT ||
        queuedBytes + batch.encodedBytes > MAX_QUEUED_BYTES_PER_PORT)
    ) {
      flowViolation(batch.sub);
      return;
    }
    queued.push(batch);
    queuedBytes += batch.encodedBytes;
    queuedBySub.set(batch.sub, {
      batches: totals.batches + 1,
      bytes: totals.bytes + batch.encodedBytes,
    });
    if (!drainScheduled) {
      drainScheduled = true;
      queueMicrotask(drain);
    }
  }

  async function listenStreams() {
    if (!internals || typeof internals.transformCallback !== "function") {
      throw new Error("Tauri event callbacks are unavailable");
    }
    const callback = internals.transformCallback(onStream);
    eventId = await invoke("plugin:event|listen", {
      event: "extension-stream",
      target: { kind: "Any" },
      handler: callback,
    });
  }

  function parseAck(value) {
    if (
      !value ||
      typeof value !== "object" ||
      value.buzz !== "stream-ack" ||
      value.generation !== lease ||
      typeof value.sub !== "string" ||
      !subscriptions.has(value.sub) ||
      !Number.isSafeInteger(value.seq) ||
      value.seq < 1 ||
      typeof value.token !== "string" ||
      !uuid.test(value.token) ||
      !Number.isSafeInteger(value.frameCount) ||
      !Number.isSafeInteger(value.encodedBytes)
    )
      return null;
    return value;
  }

  async function execute(request) {
    try {
      const reply = await invoke("plugin:extension-bridge|invoke", {
        lease,
        v: request.v,
        method: request.method,
        params: request.params,
      });
      if (disposed || !inFlight.delete(request.id)) return;
      remember(request.id);
      if (request.method === "subscribe" && reply && reply.ok) {
        const sub = reply.result?.sub;
        if (typeof sub !== "string" || subscriptions.has(sub)) {
          refuse(request.id, "internal", "the host opened no subscription");
          return;
        }
        subscriptions.set(sub, { nextSeq: 1 });
        write(request.id, reply);
        try {
          await streamControl("activate", sub);
        } catch (_) {
          closeSubscription(sub, "internal");
        }
        return;
      }
      if (request.method === "unsubscribe" && request.params) {
        const sub = request.params.sub;
        if (typeof sub === "string") subscriptions.delete(sub);
      }
      write(request.id, reply);
    } catch (_) {
      if (!inFlight.delete(request.id) || disposed) return;
      remember(request.id);
      refuse(request.id, "internal", "the bridge call failed");
    }
  }

  function onPortMessage(event) {
    const ack = parseAck(event.data);
    if (ack) {
      void streamControl("ack", ack.sub, ack).catch(() =>
        flowViolation(ack.sub),
      );
      return;
    }
    const request = parseRequest(event.data);
    if (!request) return;
    if (request.refused) {
      refuse(request.id, "invalid_request", "invalid bridge request");
      return;
    }
    if (seen.has(request.id) || inFlight.has(request.id)) {
      refuse(request.id, "replayed_request", "request id was already used");
      return;
    }
    if (inFlight.size >= MAX_IN_FLIGHT) {
      refuse(request.id, "over_quota", "too many requests are in flight");
      return;
    }
    inFlight.add(request.id);
    void execute(request);
  }

  function dispose() {
    if (disposed) return;
    disposed = true;
    window.removeEventListener("message", onReady);
    window.removeEventListener("beforeunload", dispose);
    queued.length = 0;
    queuedBySub.clear();
    queuedBytes = 0;
    for (const id of inFlight) {
      if (port)
        port.postMessage({
          id,
          ok: false,
          error: { code: "closed", message: "the extension window closed" },
        });
    }
    inFlight.clear();
    for (const sub of Array.from(subscriptions.keys())) {
      closeSubscription(sub, "unsubscribed");
    }
    if (eventId !== null) {
      try {
        if (
          eventInternals &&
          typeof eventInternals.unregisterListener === "function"
        ) {
          eventInternals.unregisterListener("extension-stream", eventId);
        }
        void invoke("plugin:event|unlisten", {
          event: "extension-stream",
          eventId,
        }).catch(() => {});
      } catch (_) {}
      eventId = null;
    }
    if (port) {
      port.removeEventListener("message", onPortMessage);
      port.close();
      port = null;
    }
  }

  async function establish() {
    await listenStreams();
    const channel = new MessageChannel();
    port = channel.port1;
    port.addEventListener("message", onPortMessage);
    port.start();
    frame.contentWindow.postMessage({ buzz: "port", v: VERSION }, "*", [
      channel.port2,
    ]);
    await invoke("plugin:extension-bridge|native_ready", { lease });
  }

  function onReady(event) {
    if (disposed || settled || event.source !== frame.contentWindow) return;
    if (
      !event.data ||
      typeof event.data !== "object" ||
      event.data.buzz !== "ready"
    )
      return;
    // Never adopt a MessagePort from the hostile child.
    settled = true;
    void establish().catch(dispose);
  }

  window.addEventListener("message", onReady);
  window.addEventListener("beforeunload", dispose);
})();
