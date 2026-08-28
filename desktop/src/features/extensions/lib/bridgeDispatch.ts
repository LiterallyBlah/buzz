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

/**
 * The host event carrying one stream frame. Must match `STREAM_EVENT` in
 * `extensions/query/connection.rs`.
 */
const STREAM_EVENT = "extension-stream";

/** What Rust emits: the owning lease, and the §2 frame for the extension. */
type StreamEnvelope = {
  lease: string;
  frame: { sub: string; kind: string };
};

/**
 * Is this payload an envelope this port should act on?
 *
 * The lease comparison is the **second** of two independent walls. Rust keys
 * its registry by `(lease, sub)` and so cannot address a frame to a successor
 * port; this refuses to deliver one that somehow arrives anyway. They are not
 * redundant, because they fall at different times — the lease is released when
 * the tab closes or the extension is disabled, while this listener goes when
 * the port is disposed, and the contract says those are unordered.
 */
function envelopeFor(lease: string, payload: unknown): StreamEnvelope | null {
  if (typeof payload !== "object" || payload === null) {
    return null;
  }
  const { lease: to, frame } = payload as Record<string, unknown>;
  if (to !== lease) {
    return null;
  }
  if (typeof frame !== "object" || frame === null) {
    return null;
  }
  const { sub, kind } = frame as Record<string, unknown>;
  if (typeof sub !== "string" || typeof kind !== "string") {
    return null;
  }
  return { lease, frame: frame as StreamEnvelope["frame"] };
}

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
  call?: (
    lease: string,
    v: number,
    method: string,
    params: unknown,
  ) => Promise<BridgeReply>;
  /**
   * Injected for tests, so budget exhaustion is reachable without driving
   * twenty thousand real round trips through the port.
   */
  registry?: Registry;
  /** Injected for tests; defaults to Tauri's event `listen`. */
  listen?: (
    event: string,
    handler: (event: { payload: unknown }) => void,
  ) => Promise<() => void>;
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
    ((l: string, v: number, method: string, params: unknown) =>
      tauriInvoke<BridgeReply>("plugin:extension-bridge|invoke", {
        lease: l,
        v,
        method,
        params,
      }));

  const registry = options.registry ?? createRegistry();
  const listen = options.listen ?? tauriListen;
  let disposed = false;
  let unlisten: (() => void) | undefined;

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

  /**
   * Record a subscription the host just opened, or refuse it and close it.
   *
   * The host mints the id; this port decides whether it will carry it. Those
   * can disagree — the per-port ceiling is enforced here — and a refusal must
   * not leave a live host-side subscription that nothing forwards, so the
   * refusal path tells the host to drop it. Failing to do that would leak a
   * relay branch for the life of the frame.
   */
  const adopt = (body: BridgeReply): BridgeReply => {
    const sub = (body.result as { sub?: unknown } | undefined)?.sub;
    if (typeof sub !== "string") {
      // A successful `subscribe` that names no sub is a host bug, and there is
      // nothing to forward frames to. Reported as `internal` rather than
      // handed on, because the caller would otherwise get `{ok:true}` for a
      // stream it can never receive.
      return {
        ok: false,
        error: { code: "internal", message: "the host opened no subscription" },
      };
    }
    const admitted = registry.adoptSub(sub);
    if (admitted.kind === "refused") {
      void call(lease, WIRE_VERSION, "unsubscribe", { sub }).catch(() => {
        // Best effort. The host also closes this lease's subscriptions when
        // the frame goes, so a failure here delays the release rather than
        // stranding it.
      });
      return {
        ok: false,
        error: { code: admitted.code, message: admitted.message },
      };
    }
    return body;
  };

  /**
   * Deliver one host stream frame to the extension.
   *
   * Frames carry a `sub` and never an `id`, so they cannot settle a correlated
   * request, and they consume none of the port's request budget. A frame for a
   * sub this port does not hold live is dropped: after `closed`, after
   * `unsubscribe`, or after teardown, there is nothing the extension should
   * still be hearing.
   *
   * **There is deliberately no separate `disposed` check.** Teardown drains
   * every live sub before it returns, so a post-teardown frame is already one
   * for a sub that is not live, and the liveness check refuses it. An added
   * `disposed` guard read as defence in depth but was unreachable — deleting it
   * broke no test, which is the definition of a gate that is not there. One
   * check, one probe.
   */
  const onStream = (payload: unknown) => {
    const envelope = envelopeFor(lease, payload);
    if (!envelope) {
      return;
    }
    const { frame } = envelope;
    if (!registry.isSubLive(frame.sub)) {
      return;
    }
    if (frame.kind === "closed") {
      // Terminal: stop forwarding before the frame goes out, so anything
      // racing behind it finds the sub already gone.
      registry.closeSub(frame.sub);
    }
    port.postMessage(frame);
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
      if (admission.terminal) {
        // The port has spent its budget. §2 admits one `ready` per frame, so
        // no successor port can be negotiated here — the honest end is to tear
        // this one down, settling anything outstanding, and let re-opening the
        // frame mint a fresh lease and port. Refusing forever instead would
        // leave the extension talking to a channel that will never answer.
        dispose();
      }
      return;
    }

    // `params` is carried through untouched. This layer bounds and type-checks
    // the frame (`bridgeFrame`), but it does not interpret the template: the
    // signer checks the canonical event it will actually sign, and a second
    // opinion here would be a second place for the two to disagree.
    void call(lease, frame.v, frame.method, frame.params)
      .then((body) => {
        if (frame.method === "subscribe" && body.ok) {
          reply(frame.id, adopt(body));
          return;
        }
        reply(frame.id, body);
      })
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

  const dispose = () => {
    if (disposed) {
      return;
    }
    // Stop listening first, then close admission and settle. Requests already
    // dispatched are answered below; anything arriving after this point has no
    // listener to reach.
    port.removeEventListener("message", onMessage);
    unlisten?.();
    // Marked disposed before draining subs so a stream frame arriving from the
    // host mid-teardown finds the listener gone and, if it beat that, finds
    // `disposed` already set.
    disposed = true;

    // Every live subscription is told to close, on both sides. The host's own
    // lease wall will also reach these, but the two are unordered, so a port
    // that disposed first must not leave the extension believing a stream is
    // still running.
    for (const sub of registry.closeAndDrainSubs()) {
      port.postMessage({ sub, kind: "closed", reason: "unsubscribed" });
      void call(lease, WIRE_VERSION, "unsubscribe", { sub }).catch(() => {});
    }

    const outstanding = registry.closeAndDrain();
    for (const id of outstanding) {
      // Written directly: `closeAndDrain` already marked these terminal, so
      // `reply` would find nothing to settle and write nothing. The caller
      // must hear about them before the channel goes.
      write(id, { ok: false, error: { ...TEARDOWN_ERROR } });
    }
    // Then actually close it.
    //
    // Removing the listener alone leaves an **open but unserved** channel: a
    // later request is posted successfully and simply never answered, which is
    // the hang the terminal contract exists to remove — it would merely arrive
    // after a warning. The replies above are queued before this runs, and the
    // close is what makes the end of the port observable rather than silent.
    //
    // `MessagePort.close()` is idempotent, so the handshake owner closing the
    // same port again on unmount is harmless.
    port.close();
  };

  port.addEventListener("message", onMessage);
  port.start();

  // `listen` resolves asynchronously, so a port disposed before it settles
  // would otherwise install a listener nobody ever removes. Checking
  // `disposed` on resolution is what stops the subscription outliving the port
  // it was opened for.
  // Wrapped so a `listen` that throws *synchronously* — the Tauri one does
  // when the IPC globals are absent — becomes a rejection the catch below
  // handles, rather than taking the whole port down at construction.
  void (async () => listen(STREAM_EVENT, (event) => onStream(event.payload)))()
    .then((stop) => {
      if (disposed) {
        stop();
        return;
      }
      unlisten = stop;
    })
    .catch(() => {
      // No stream transport. Requests still work; a `subscribe` will be
      // answered and simply never deliver, which the host's own EOSE deadline
      // then closes.
    });

  return { dispose };
}

export { WIRE_VERSION as BRIDGE_WIRE_VERSION };
