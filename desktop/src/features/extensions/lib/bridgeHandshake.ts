/**
 * The host end of the BRIDGE_SPEC §2 handshake, moved one hop for Route A.
 *
 * The frame tree is `host → wrapper → extension`. The host talks to the
 * **wrapper**, never to the extension directly:
 *
 * ```text
 * extension  --{buzz:"ready"}-->  wrapper  --{buzz:"ready"}-->  host
 * extension  <--{buzz:"port"}--   wrapper  <--{buzz:"port"}--   host   [port2]
 * extension  <=====================  MessageChannel  =====================> host
 * ```
 *
 * After the handshake the channel runs **directly** between the host's `port1`
 * and the extension's `port2`; the wrapper transferred the port through and is
 * out of the data path.
 *
 * Four rules from §2, each of which is a security property rather than a
 * detail:
 *
 * 1. **Attribute by host-held handle, never by payload.** The only accepted
 *    source is the `contentWindow` of the iframe this host created. An opaque
 *    sandboxed frame reports `event.origin === "null"`, so origin cannot be
 *    used and identity is all there is. One wrapper embeds exactly one
 *    extension, so this stays 1:1.
 * 2. **The host originates the channel.** A `MessagePort` arriving from the
 *    frame side is ignored — adopting one would let the frame choose the
 *    channel the host then trusts.
 * 3. **Exactly one `ready` per frame.** A second is ignored, so a frame cannot
 *    force re-issue of a fresh port and leave the host holding two.
 * 4. **Only the handshake envelope is acted on.** Anything else from the
 *    accepted source is dropped.
 */

/** The envelope the extension sends when its document is ready. */
const READY = "ready";
/** The envelope carrying `port2` back down to the extension. */
const PORT = "port";
/** Wire version of the handshake itself. */
const HANDSHAKE_VERSION = 1;

export type HostHandshake = {
  /**
   * The host's end of the channel, or `null` until the handshake completes.
   *
   * Held rather than returned so the caller cannot be handed a port the host
   * has not finished attributing.
   */
  readonly port: () => MessagePort | null;
  /** Stop listening and drop the port. Idempotent. */
  readonly dispose: () => void;
};

type StartOptions = {
  /** The iframe the host created for this extension's wrapper. */
  frame: HTMLIFrameElement;
  /** Injected for tests; defaults to the ambient window. */
  view?: Window;
  /** Called once, when the channel is established. */
  onEstablished?: (port: MessagePort) => void;
};

function isEnvelope(data: unknown, name: string): boolean {
  return (
    typeof data === "object" &&
    data !== null &&
    (data as { buzz?: unknown }).buzz === name
  );
}

/**
 * Begin listening for the wrapper's relayed `ready` and complete the handshake.
 *
 * Returns a handle whose `dispose()` removes the listener and closes the port.
 * Safe to call before the frame has loaded — the listener simply waits.
 */
export function startHostHandshake(options: StartOptions): HostHandshake {
  const { frame, onEstablished } = options;
  const view = options.view ?? window;

  let port: MessagePort | null = null;
  let settled = false;
  let disposed = false;

  const onMessage = (event: MessageEvent) => {
    if (disposed || settled) {
      // Rule 3: exactly one `ready` per frame. A later one is not an error to
      // report to the frame — it is simply not acted on.
      return;
    }
    // Rule 1: identity, not origin. `frame.contentWindow` is re-read each time
    // because a navigation replaces it, and a stale capture would accept a
    // window the host no longer hosts.
    if (event.source !== frame.contentWindow) {
      return;
    }
    // Rule 4: only the handshake envelope.
    if (!isEnvelope(event.data, READY)) {
      return;
    }
    // Rule 2: never adopt a port from the frame side. `event.ports` is
    // deliberately not read at all — there is no code path here that could
    // retain one.
    settled = true;

    const channel = new MessageChannel();
    port = channel.port1;
    port.start();

    // `"*"` is acceptable only because the target was pinned by
    // source-identity above; an opaque frame has no origin to name.
    frame.contentWindow?.postMessage(
      { buzz: PORT, v: HANDSHAKE_VERSION },
      "*",
      [channel.port2],
    );

    onEstablished?.(channel.port1);
  };

  view.addEventListener("message", onMessage);

  return {
    port: () => port,
    dispose: () => {
      if (disposed) {
        return;
      }
      disposed = true;
      view.removeEventListener("message", onMessage);
      port?.close();
      port = null;
    },
  };
}
