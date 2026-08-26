/**
 * The frontend end of the BRIDGE_SPEC §2 request/response loop.
 *
 * After the §2 handshake the host holds `port1` and the extension holds
 * `port2`. Requests arrive here; this module validates the *frame* and hands
 * the decision to Rust, which owns attribution and every permission check.
 *
 * ```text
 * extension --{id,v,method,params}--> port1 --> Rust dispatch (lease -> extension,
 *                                                              scope check, execute)
 * extension <--{id,ok,result|error}-- port1 <--
 * ```
 *
 * # What this layer is, and is not
 *
 * It is a **correlator**, not a decision-maker. It knows the request `id` — Rust
 * does not, because it does not need to — and it knows the lease, which the host
 * minted. It does not decide who the caller is, whether a scope is granted, or
 * what a method does. Those live behind the IPC boundary so that a bug here
 * cannot widen a grant.
 *
 * # If it can be correlated, it is answered
 *
 * §9 requires that in-flight requests settle rather than dangle, so silence is
 * reserved for the one case where an answer is impossible: a frame with no
 * usable `id`. Answering that would mean inventing a correlation the caller
 * never established, so it is dropped.
 *
 * Everything else gets a reply. A frame that is correlatable but outside the
 * wire shape — no version, no method, over the size caps — is refused here
 * with §8 `invalid_params` and never reaches a handler. A frame that is well
 * shaped but wrong — unsupported version, unknown method, missing scope — is
 * Rust's to refuse, because those are authority decisions and this layer makes
 * none.
 */

import { invoke as tauriInvoke } from "@tauri-apps/api/core";

/** Wire version this client speaks (§2). */
const WIRE_VERSION = 1;

/**
 * Wire limits. The spec sets no numbers, so these are ours.
 *
 * Each is far above any legitimate caller and far below anything that could
 * make the host do unbounded work. `MAX_FRAME_CHARS` is measured on the
 * frame's JSON encoding — characters, not bytes — which is the cheapest bound
 * that covers `params` however deeply it nests.
 */
const MAX_ID_CHARS = 128;
const MAX_METHOD_CHARS = 64;
const MAX_FRAME_CHARS = 64 * 1024;

/** §8 code for a frame that never reaches a handler. */
const INVALID_PARAMS = "invalid_params";

type RequestFrame = {
  id: string;
  v: number;
  method: string;
  params?: unknown;
};

type BridgeReply = {
  ok: boolean;
  result?: unknown;
  error?: { code: string; message: string };
};

/**
 * The three things a frame can be: dispatchable, refusable, or unanswerable.
 */
type FrameCheck =
  | { kind: "ok"; frame: RequestFrame }
  | { kind: "invalid"; id: string; reason: string }
  | { kind: "drop" };

/**
 * Classify an inbound frame without letting it reach a handler.
 *
 * `id` decides whether a reply is even possible, so it is checked first: a
 * missing, non-string, empty or over-length `id` leaves nothing to correlate
 * against and the frame is dropped.
 *
 * Size is checked before shape. Walking an enormous object field by field to
 * discover it is enormous is the work the bound exists to avoid.
 *
 * `v` and `method` are checked for *type and size* here and for *value* in
 * Rust. This layer must not decide that a version is unsupported or a method
 * unknown — those are §8 codes with defined semantics that Rust owns.
 */
function checkFrame(data: unknown): FrameCheck {
  if (typeof data !== "object" || data === null) {
    return { kind: "drop" };
  }
  const frame = data as Partial<RequestFrame>;
  if (
    typeof frame.id !== "string" ||
    frame.id.length === 0 ||
    frame.id.length > MAX_ID_CHARS
  ) {
    return { kind: "drop" };
  }
  const id = frame.id;

  let encoded: string;
  try {
    encoded = JSON.stringify(data);
  } catch {
    // Structured clone carries shapes JSON cannot — cycles, BigInt — so a
    // hostile frame reaches this. It cannot be measured, so it is refused
    // rather than guessed at.
    return { kind: "invalid", id, reason: "request frame is not serialisable" };
  }
  if (encoded.length > MAX_FRAME_CHARS) {
    return {
      kind: "invalid",
      id,
      reason: "request frame exceeds the wire limits",
    };
  }

  if (typeof frame.v !== "number" || !Number.isFinite(frame.v)) {
    return {
      kind: "invalid",
      id,
      reason: "request frame has no usable version",
    };
  }
  if (
    typeof frame.method !== "string" ||
    frame.method.length === 0 ||
    frame.method.length > MAX_METHOD_CHARS
  ) {
    return {
      kind: "invalid",
      id,
      reason: "request frame has no usable method",
    };
  }
  return { kind: "ok", frame: { id, v: frame.v, method: frame.method } };
}

export type DispatchHandle = {
  /** Stop serving requests on this port. Idempotent. */
  readonly dispose: () => void;
};

type StartOptions = {
  /** The host's end of the §2 channel. */
  port: MessagePort;
  /** The opaque host-minted lease identifying which extension this port serves. */
  lease: string;
  /** Injected for tests; defaults to Tauri's `invoke`. */
  call?: (lease: string, v: number, method: string) => Promise<BridgeReply>;
};

/**
 * Serve §2 requests arriving on `port` until disposed.
 *
 * The lease is captured here and sent with every call. The extension never
 * supplies it and cannot influence it — it is the same token the host minted
 * when it opened the frame.
 */
export function startBridgeDispatch(options: StartOptions): DispatchHandle {
  const { port, lease } = options;
  const call =
    options.call ??
    ((l: string, v: number, method: string) =>
      tauriInvoke<BridgeReply>("plugin:extension-bridge|invoke", {
        lease: l,
        v,
        method,
      }));

  let disposed = false;

  /**
   * The single write path to the port.
   *
   * Every reply goes through here so the disposed check cannot be forgotten on
   * one branch: after teardown the port belongs to the handshake, which closes
   * it, and writing to it then is a late write to a dead channel.
   */
  const reply = (id: string, body: BridgeReply) => {
    if (disposed) {
      return;
    }
    port.postMessage({ id, ...body });
  };

  const onMessage = (event: MessageEvent) => {
    if (disposed) {
      return;
    }
    const checked = checkFrame(event.data);
    if (checked.kind === "drop") {
      // Not correlatable: there is no id to answer to. Dropping is the only
      // honest response.
      return;
    }
    if (checked.kind === "invalid") {
      // Refused here, never handed to a handler — the frame is outside the
      // wire shape, which is a §8 `invalid_params` and not an authority call.
      reply(checked.id, {
        ok: false,
        error: { code: INVALID_PARAMS, message: checked.reason },
      });
      return;
    }
    const { frame } = checked;
    void call(lease, frame.v, frame.method)
      .then((body) => reply(frame.id, body))
      .catch(() => {
        // An IPC failure is the host's fault, not the extension's, and §8 has
        // a code for it. The request still gets an answer so the client's
        // promise settles rather than hanging.
        reply(frame.id, {
          ok: false,
          error: { code: "internal", message: "the bridge call failed" },
        });
      });
  };

  port.addEventListener("message", onMessage);
  port.start();

  return {
    dispose: () => {
      disposed = true;
      port.removeEventListener("message", onMessage);
    },
  };
}

export const BRIDGE_WIRE_VERSION = WIRE_VERSION;
