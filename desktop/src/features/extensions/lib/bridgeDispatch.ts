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
 * # Malformed frames are ignored, not answered
 *
 * §2 gives no error shape for a frame that has no usable `id`, and answering
 * one would mean inventing a correlation the caller never established. A frame
 * that is not an object, or whose `id` is not a string, is dropped silently.
 * Anything that *is* correlatable but wrong — bad version, unknown method — is
 * Rust's to refuse, so it gets a proper `{ id, ok:false, error }`.
 */

import { invoke as tauriInvoke } from "@tauri-apps/api/core";

/** Wire version this client speaks (§2). */
const WIRE_VERSION = 1;

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
 * A frame is dispatchable only if it can be answered.
 *
 * `id` must be a non-empty string: without it there is nothing to correlate a
 * reply to. `v` and `method` are checked for *type* here and for *value* in
 * Rust — this layer must not decide that a version is unsupported, because that
 * is a §8 error with a defined code and Rust owns those.
 */
function asRequestFrame(data: unknown): RequestFrame | null {
  if (typeof data !== "object" || data === null) {
    return null;
  }
  const frame = data as Partial<RequestFrame>;
  if (typeof frame.id !== "string" || frame.id.length === 0) {
    return null;
  }
  if (typeof frame.v !== "number" || !Number.isFinite(frame.v)) {
    return null;
  }
  if (typeof frame.method !== "string") {
    return null;
  }
  return { id: frame.id, v: frame.v, method: frame.method };
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

  const onMessage = (event: MessageEvent) => {
    if (disposed) {
      return;
    }
    const frame = asRequestFrame(event.data);
    if (!frame) {
      // Not correlatable: there is no id to answer to. Dropping is the only
      // honest response.
      return;
    }
    void call(lease, frame.v, frame.method)
      .then((reply) => {
        if (!disposed) {
          port.postMessage({ id: frame.id, ...reply });
        }
      })
      .catch(() => {
        // An IPC failure is the host's fault, not the extension's, and §8 has
        // a code for it. The request still gets an answer so the client's
        // promise settles rather than hanging.
        if (!disposed) {
          port.postMessage({
            id: frame.id,
            ok: false,
            error: { code: "internal", message: "the bridge call failed" },
          });
        }
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
