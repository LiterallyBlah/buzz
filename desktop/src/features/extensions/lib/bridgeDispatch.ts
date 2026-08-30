/**
 * Frontend end of BRIDGE_SPEC §2, including literal MessagePort ACK/window
 * backpressure. Rust remains the authority owner; this layer correlates public
 * requests, gates stream readiness, and transports exact-generation controls.
 */

import { invoke as tauriInvoke } from "@tauri-apps/api/core";
import { listen as tauriListen } from "@tauri-apps/api/event";

import { checkFrame, isUuid, WIRE_VERSION } from "./bridgeFrame";
import {
  createRegistry,
  type Registry,
  TEARDOWN_ERROR,
} from "./bridgeRegistry";

type BridgeReply = {
  ok: boolean;
  result?: unknown;
  error?: { code: string; message: string };
};

const STREAM_EVENT = "extension-stream";

export const MAX_STREAM_BATCH_FRAMES = 16;
export const MAX_STREAM_BATCH_BYTES = 640 * 1024;
export const MAX_TS_QUEUED_BATCHES_PER_SUB = 2;
export const MAX_TS_QUEUED_BYTES_PER_SUB = 2 * 1024 * 1024;
export const MAX_TS_QUEUED_BATCHES_PER_PORT = 8;
export const MAX_TS_QUEUED_BYTES_PER_PORT = 3 * 1024 * 1024;

const encoder = new TextEncoder();

type StreamBatch = {
  generation: string;
  sub: string;
  seq: number;
  token: string;
  frames: Array<{ sub: string; kind: string }>;
  frameCount: number;
  encodedBytes: number;
  terminal: boolean;
};

type StreamControl = (
  lease: string,
  action: "activate" | "ack" | "violation",
  sub: string,
  detail?: {
    seq?: number;
    token?: string;
    frameCount?: number;
    encodedBytes?: number;
  },
) => Promise<void>;

function encodedFrameBytes(frames: unknown[]): number | null {
  let total = 0;
  try {
    for (const frame of frames) {
      total += encoder.encode(JSON.stringify(frame)).byteLength;
      if (!Number.isSafeInteger(total)) {
        return null;
      }
    }
  } catch {
    return null;
  }
  return total;
}

function parseBatch(lease: string, payload: unknown): StreamBatch | null {
  if (typeof payload !== "object" || payload === null) {
    return null;
  }
  const batch = payload as Record<string, unknown>;
  if (
    batch.generation !== lease ||
    typeof batch.sub !== "string" ||
    typeof batch.seq !== "number" ||
    !Number.isSafeInteger(batch.seq) ||
    batch.seq < 1 ||
    typeof batch.token !== "string" ||
    !isUuid(batch.token) ||
    !Array.isArray(batch.frames) ||
    typeof batch.frameCount !== "number" ||
    !Number.isSafeInteger(batch.frameCount) ||
    typeof batch.encodedBytes !== "number" ||
    !Number.isSafeInteger(batch.encodedBytes) ||
    typeof batch.terminal !== "boolean" ||
    batch.frameCount !== batch.frames.length ||
    batch.frameCount < 1 ||
    batch.frameCount > MAX_STREAM_BATCH_FRAMES ||
    batch.encodedBytes < 1 ||
    batch.encodedBytes > MAX_STREAM_BATCH_BYTES
  ) {
    return null;
  }
  for (const frame of batch.frames) {
    if (
      typeof frame !== "object" ||
      frame === null ||
      (frame as Record<string, unknown>).sub !== batch.sub ||
      typeof (frame as Record<string, unknown>).kind !== "string"
    ) {
      return null;
    }
  }
  if (encodedFrameBytes(batch.frames) !== batch.encodedBytes) {
    return null;
  }
  if (
    batch.terminal &&
    (batch.frames.length !== 1 || batch.frames[0]?.kind !== "closed")
  ) {
    return null;
  }
  return batch as StreamBatch;
}

function parseAck(lease: string, payload: unknown) {
  if (typeof payload !== "object" || payload === null) {
    return null;
  }
  const ack = payload as Record<string, unknown>;
  if (
    ack.buzz !== "stream-ack" ||
    ack.generation !== lease ||
    typeof ack.sub !== "string" ||
    typeof ack.seq !== "number" ||
    !Number.isSafeInteger(ack.seq) ||
    ack.seq < 1 ||
    typeof ack.token !== "string" ||
    !isUuid(ack.token) ||
    typeof ack.frameCount !== "number" ||
    !Number.isSafeInteger(ack.frameCount) ||
    typeof ack.encodedBytes !== "number" ||
    !Number.isSafeInteger(ack.encodedBytes)
  ) {
    return null;
  }
  return ack as {
    sub: string;
    seq: number;
    token: string;
    frameCount: number;
    encodedBytes: number;
  };
}

export type DispatchHandle = {
  readonly dispose: () => void;
};

type StartOptions = {
  port: MessagePort;
  lease: string;
  call?: (
    lease: string,
    v: number,
    method: string,
    params: unknown,
  ) => Promise<BridgeReply>;
  control?: StreamControl;
  registry?: Registry;
  listen?: (
    event: string,
    handler: (event: { payload: unknown }) => void,
  ) => Promise<() => void>;
};

export function startBridgeDispatch(options: StartOptions): DispatchHandle {
  const { port, lease } = options;
  if (!isUuid(lease)) {
    throw new Error("startBridgeDispatch requires a host-minted uuid lease");
  }
  const call =
    options.call ??
    ((l: string, v: number, method: string, params: unknown) =>
      tauriInvoke<BridgeReply>("plugin:extension-bridge|invoke", {
        lease: l,
        v,
        method,
        params,
      }));
  const control: StreamControl =
    options.control ??
    ((l, action, sub, detail = {}) =>
      tauriInvoke<void>("plugin:extension-bridge|stream_control", {
        lease: l,
        action,
        sub,
        seq: detail.seq,
        token: detail.token,
        frameCount: detail.frameCount,
        encodedBytes: detail.encodedBytes,
      }));
  const registry = options.registry ?? createRegistry();
  const listen = options.listen ?? tauriListen;
  let disposed = false;
  let unlisten: (() => void) | undefined;

  const queued: StreamBatch[] = [];
  let queuedBytes = 0;
  const queuedBySub = new Map<string, { batches: number; bytes: number }>();
  const nextSeq = new Map<string, number>();
  const terminalSubs = new Set<string>();
  const flowFailedSubs = new Set<string>();
  let drainScheduled = false;

  const write = (id: string, body: BridgeReply) => {
    port.postMessage({ id, ...body });
  };

  const reply = (id: string, body: BridgeReply): boolean => {
    if (!registry.settle(id)) {
      return false;
    }
    write(id, body);
    return true;
  };

  const flowViolation = (sub: string) => {
    if (flowFailedSubs.has(sub)) {
      return;
    }
    flowFailedSubs.add(sub);
    void control(lease, "violation", sub).catch(() => {});
  };

  const drain = () => {
    drainScheduled = false;
    while (!disposed && queued.length > 0) {
      const batch = queued.shift();
      if (!batch) {
        break;
      }
      queuedBytes -= batch.encodedBytes;
      const totals = queuedBySub.get(batch.sub);
      if (totals) {
        totals.batches -= 1;
        totals.bytes -= batch.encodedBytes;
        if (totals.batches === 0) {
          queuedBySub.delete(batch.sub);
        }
      }
      if (
        (terminalSubs.has(batch.sub) || flowFailedSubs.has(batch.sub)) &&
        !batch.terminal
      ) {
        continue;
      }
      // Internal transport envelope. The extension-side client validates and
      // adopts every public frame, then returns one exact ACK for this batch.
      port.postMessage({ buzz: "stream-batch", ...batch });
      if (batch.terminal) {
        terminalSubs.add(batch.sub);
        registry.closeSub(batch.sub);
      }
    }
  };

  const onStream = (payload: unknown) => {
    const batch = parseBatch(lease, payload);
    if (!batch || !registry.isSubLive(batch.sub)) {
      if (batch && registry.isSubLive(batch.sub)) {
        flowViolation(batch.sub);
      }
      return;
    }
    const expected = nextSeq.get(batch.sub) ?? 1;
    if (
      batch.seq !== expected ||
      terminalSubs.has(batch.sub) ||
      (flowFailedSubs.has(batch.sub) && !batch.terminal)
    ) {
      if (flowFailedSubs.has(batch.sub) && !batch.terminal) {
        return;
      }
      flowViolation(batch.sub);
      return;
    }
    nextSeq.set(batch.sub, expected + 1);

    const subTotals = queuedBySub.get(batch.sub) ?? { batches: 0, bytes: 0 };
    const nextSubBatches = subTotals.batches + 1;
    const nextSubBytes = subTotals.bytes + batch.encodedBytes;
    if (
      !batch.terminal &&
      (nextSubBatches > MAX_TS_QUEUED_BATCHES_PER_SUB ||
        nextSubBytes > MAX_TS_QUEUED_BYTES_PER_SUB ||
        queued.length + 1 > MAX_TS_QUEUED_BATCHES_PER_PORT ||
        queuedBytes + batch.encodedBytes > MAX_TS_QUEUED_BYTES_PER_PORT)
    ) {
      flowViolation(batch.sub);
      return;
    }
    queued.push(batch);
    queuedBytes += batch.encodedBytes;
    queuedBySub.set(batch.sub, {
      batches: nextSubBatches,
      bytes: nextSubBytes,
    });
    if (!drainScheduled) {
      drainScheduled = true;
      queueMicrotask(drain);
    }
  };

  const listenerReady = (async () => {
    try {
      const stop = await listen(STREAM_EVENT, (event) =>
        onStream(event.payload),
      );
      if (disposed) {
        stop();
        return false;
      }
      unlisten = stop;
      return true;
    } catch {
      return false;
    }
  })();
  void listenerReady.then((ready) => {
    if (!ready && !disposed) {
      dispose();
    }
  });

  const adopt = (body: BridgeReply): BridgeReply => {
    const sub = (body.result as { sub?: unknown } | undefined)?.sub;
    if (typeof sub !== "string") {
      return {
        ok: false,
        error: { code: "internal", message: "the host opened no subscription" },
      };
    }
    const admitted = registry.adoptSub(sub);
    if (admitted.kind === "refused") {
      void call(lease, WIRE_VERSION, "unsubscribe", { sub }).catch(() => {});
      return {
        ok: false,
        error: { code: admitted.code, message: admitted.message },
      };
    }
    return body;
  };

  const execute = async (frame: {
    id: string;
    v: number;
    method: string;
    params?: unknown;
  }) => {
    if (frame.method === "subscribe") {
      const ready = await listenerReady;
      if (!ready || disposed) {
        if (!disposed) {
          dispose();
        }
        return;
      }
    }
    try {
      const body = await call(lease, frame.v, frame.method, frame.params);
      if (frame.method === "subscribe" && body.ok) {
        const adopted = adopt(body);
        const sub = (adopted.result as { sub?: string } | undefined)?.sub;
        const written = reply(frame.id, adopted);
        if (written && adopted.ok && sub) {
          try {
            // This occurs only after the correlated reply was queued to the
            // exact port. Rust releases no stream frame before this receipt.
            await control(lease, "activate", sub);
          } catch {
            registry.closeSub(sub);
            port.postMessage({ sub, kind: "closed", reason: "internal" });
            void call(lease, WIRE_VERSION, "unsubscribe", { sub }).catch(
              () => {},
            );
          }
        }
        return;
      }
      reply(frame.id, body);
    } catch {
      reply(frame.id, {
        ok: false,
        error: { code: "internal", message: "the bridge call failed" },
      });
    }
  };

  const onMessage = (event: MessageEvent) => {
    const ack = parseAck(lease, event.data);
    if (ack) {
      if (!registry.isSubLive(ack.sub)) {
        return;
      }
      void control(lease, "ack", ack.sub, ack).catch(() => {
        flowViolation(ack.sub);
      });
      return;
    }

    const checked = checkFrame(event.data);
    if (checked.kind === "drop") {
      return;
    }
    if (checked.kind === "refuse") {
      if (!disposed) {
        write(checked.id, {
          ok: false,
          error: { code: checked.code, message: checked.message },
        });
      }
      return;
    }
    const { frame } = checked;
    const admission = registry.admit(frame.id);
    if (admission.kind === "refused") {
      if (!disposed) {
        write(frame.id, {
          ok: false,
          error: { code: admission.code, message: admission.message },
        });
      }
      if (admission.terminal) {
        dispose();
      }
      return;
    }
    void execute(frame);
  };

  function dispose() {
    if (disposed) {
      return;
    }
    port.removeEventListener("message", onMessage);
    unlisten?.();
    disposed = true;
    queued.length = 0;
    queuedBytes = 0;
    queuedBySub.clear();

    for (const sub of registry.closeAndDrainSubs()) {
      port.postMessage({ sub, kind: "closed", reason: "unsubscribed" });
      void call(lease, WIRE_VERSION, "unsubscribe", { sub }).catch(() => {});
    }
    for (const id of registry.closeAndDrain()) {
      write(id, { ok: false, error: { ...TEARDOWN_ERROR } });
    }
    port.close();
  }

  port.addEventListener("message", onMessage);
  port.start();
  return { dispose };
}

export { WIRE_VERSION as BRIDGE_WIRE_VERSION };
