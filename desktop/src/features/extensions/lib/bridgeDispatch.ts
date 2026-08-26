/**
 * The frontend end of the BRIDGE_SPEC §2 request/response loop.
 *
 * After the §2 handshake the host holds `port1` and the extension holds
 * `port2`. Requests arrive here; this module validates the *frame*
 * (`bridgeFrame`), admits it (`bridgeRegistry`), and hands the decision to
 * Rust, which owns attribution and every permission check.
 *
 * ```text
 * extension --{id,v,method,params}--> port1 --> Rust dispatch (lease -> extension,
 *                                                              scope check, execute)
 * extension <--{id,ok,result|error}-- port1 <--
 * ```
 *
 * # What this layer is, and is not
 *
 * It is a **correlator and an admission gate**, not a decision-maker. It knows
 * the request `id` — Rust does not, because it does not need to — and it knows
 * the lease, which the host minted. It does not decide who the caller is,
 * whether a scope is granted, or what a method does. Those live behind the IPC
 * boundary so that a bug here cannot widen a grant.
 *
 * # If it can be correlated, it is answered
 *
 * §9 requires in-flight requests to settle rather than dangle, so silence is
 * reserved for the one case where an answer is impossible: a frame with no
 * usable `id`. Everything else gets a reply — including at teardown, where
 * outstanding requests are **settled**, not dropped. A dropped reply leaves the
 * caller's promise pending forever, which is indistinguishable from a host that
 * is still working.
 */

import { invoke as tauriInvoke } from "@tauri-apps/api/core";

import { checkFrame, isUuid, WIRE_VERSION } from "./bridgeFrame";
import { createRegistry, TEARDOWN_ERROR } from "./bridgeRegistry";

type BridgeReply = {
  ok: boolean;
  result?: unknown;
  error?: { code: string; message: string };
};

export type DispatchHandle = {
  /** Stop serving, and settle everything outstanding. Idempotent. */
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
  if (!isUuid(lease)) {
    // The lease is host-minted, so a malformed one is our bug, not a caller's.
    // Refusing to serve at all is safer than serving with a lease Rust will
    // reject on every request.
    throw new Error("startBridgeDispatch requires a host-minted uuid lease");
  }
  const call =
    options.call ??
    ((l: string, v: number, method: string) =>
      tauriInvoke<BridgeReply>("plugin:extension-bridge|invoke", {
        lease: l,
        v,
        method,
      }));

  const registry = createRegistry();
  let disposed = false;

  /** The single write path to the port. */
  const write = (id: string, body: BridgeReply) => {
    port.postMessage({ id, ...body });
  };

  /**
   * Answer a request exactly once.
   *
   * The registry decides whether this is the first terminal transition, so a
   * call that resolves after teardown finds its id already settled and writes
   * nothing — it cannot produce a second result for the same id.
   */
  const reply = (id: string, body: BridgeReply) => {
    if (!registry.settle(id)) {
      return;
    }
    write(id, body);
  };

  const onMessage = (event: MessageEvent) => {
    const checked = checkFrame(event.data);
    if (checked.kind === "drop") {
      // Not correlatable: there is no id to answer to. Dropping is the only
      // honest response.
      return;
    }
    if (checked.kind === "refuse") {
      // Refused before admission, so it never entered the registry and must be
      // written directly rather than through `reply`.
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
      return;
    }

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
      if (disposed) {
        return;
      }
      // Stop listening first, then close admission and settle. Requests
      // already dispatched are answered below; anything arriving after this
      // point has no listener to reach.
      port.removeEventListener("message", onMessage);
      const outstanding = registry.closeAndDrain();
      for (const id of outstanding) {
        // Written directly: `closeAndDrain` already marked these terminal, so
        // `reply` would find nothing to settle and write nothing. The caller
        // must hear about them before the port closes.
        write(id, { ok: false, error: { ...TEARDOWN_ERROR } });
      }
      disposed = true;
    },
  };
}

export { WIRE_VERSION as BRIDGE_WIRE_VERSION };
