import * as React from "react";

import {
  type DispatchHandle,
  startBridgeDispatch,
} from "@/features/extensions/lib/bridgeDispatch";
import { startHostHandshake } from "@/features/extensions/lib/bridgeHandshake";
import {
  type ExtensionFrameTarget,
  closeExtensionFrame,
  openExtensionFrame,
} from "@/features/extensions/lib/extensionsApi";
import { Button } from "@/shared/ui/button";
import { Card } from "@/shared/ui/card";

/**
 * The sandbox an extension document is hosted under.
 *
 * Exactly `allow-scripts`, and deliberately nothing else — decision 002.
 * Omitting `allow-same-origin` is what forces the document to an **opaque**
 * origin, which is what (a) keeps it from reading the parent's storage, relay
 * session or Tauri invoke key, and (b) isolates extensions from one another
 * even though they share a serving origin.
 *
 * Adding a token here is a security change, not a UX tweak:
 * - `allow-same-origin` collapses the containment entirely;
 * - `allow-top-navigation` lets a page navigate the Buzz window away;
 * - `allow-popups` re-opens a window the sandbox does not cover.
 *
 * Exported so the negative test asserts against the shipped value rather than
 * a copy of it.
 */
export const EXTENSION_FRAME_SANDBOX = "allow-scripts";

type ExtensionFrameProps = {
  extensionId: string;
};

/**
 * Hosts one installed extension's page.
 *
 * The iframe points at the **wrapper**, not the extension — Route A puts a
 * trusted host-authored document in between, and `target.url` is its URL. So
 * the §2 handshake this component starts is with the wrapper's
 * `contentWindow`, which is the handle the host itself created; the wrapper
 * relays to the one extension it embeds, keeping attribution 1:1.
 *
 * The frame still gets no `window.buzz` and no Tauri IPC. What it gets is one
 * `MessagePort`, and only after the host has attributed the `ready` by
 * identity — an opaque frame's `event.origin` is the string `"null"` and
 * cannot be used. See `bridgeHandshake.ts` for the four rules.
 */
export function ExtensionFrame({ extensionId }: ExtensionFrameProps) {
  const [target, setTarget] = React.useState<ExtensionFrameTarget | null>(null);
  const [error, setError] = React.useState<string | null>(null);
  const frameRef = React.useRef<HTMLIFrameElement>(null);

  React.useEffect(() => {
    let live = true;
    // The lease this effect actually acquired — not "a" lease. Cleanup releases
    // only what it holds, so an open that failed releases nothing and cannot
    // stop the host still serving another frame.
    let held: string | null = null;
    setTarget(null);
    setError(null);

    const releaseHeld = () => {
      if (held === null) {
        return;
      }
      const lease = held;
      held = null;
      void closeExtensionFrame(lease);
    };

    void openExtensionFrame(extensionId)
      .then((opened) => {
        held = opened.lease;
        if (!live) {
          // Unmounted while the host was starting: hand the lease straight
          // back, or it outlives the component that owns it.
          releaseHeld();
          return;
        }
        setTarget(opened);
      })
      .catch((cause: unknown) => {
        if (live) {
          setError(typeof cause === "string" ? cause : String(cause));
        }
      });

    return () => {
      live = false;
      // Closing the tab, navigating away, or switching the preview flag off all
      // unmount this component — which is what stops the frame host.
      releaseHeld();
    };
  }, [extensionId]);

  // Started only once the wrapper frame exists, and torn down with it. The
  // effect depends on `target.url` because a new url means a new document, and
  // therefore a new `contentWindow` that must be attributed afresh.
  React.useEffect(() => {
    const frame = frameRef.current;
    if (!target || !frame) {
      return;
    }
    // The dispatcher is attached to the port the handshake produced, and only
    // then: before that there is no channel, and the lease it will carry is
    // this component's own — never anything the frame supplies.
    let dispatch: DispatchHandle | null = null;
    const handshake = startHostHandshake({
      frame,
      onEstablished: (port) => {
        dispatch = startBridgeDispatch({ port, lease: target.lease });
      },
    });
    return () => {
      // Stop serving before the port closes, so a request in flight cannot try
      // to answer on a closed channel.
      dispatch?.dispose();
      handshake.dispose();
    };
  }, [target]);

  if (error) {
    return (
      <Card
        className="m-4 border-destructive/50 p-4"
        data-testid="extension-frame-error"
      >
        <p className="text-sm font-medium text-destructive">
          This extension could not be opened
        </p>
        <p className="mt-1 break-words text-sm text-muted-foreground">
          {error}
        </p>
      </Card>
    );
  }

  if (!target) {
    return (
      <div
        className="flex min-h-0 flex-1 items-center justify-center text-sm text-muted-foreground"
        data-testid="extension-frame-loading"
      >
        Starting {extensionId}…
      </div>
    );
  }

  return (
    <iframe
      className="min-h-0 min-w-0 flex-1 border-0 bg-background"
      data-testid="extension-frame"
      key={target.url}
      ref={frameRef}
      sandbox={EXTENSION_FRAME_SANDBOX}
      src={target.url}
      title={extensionId}
    />
  );
}

/** Header chrome above a hosted extension. */
export function ExtensionFrameHeader({
  extensionId,
  onBack,
}: {
  extensionId: string;
  onBack: () => void;
}) {
  return (
    <div className="flex items-center gap-2 border-b border-border/60 px-4 py-2">
      <Button
        data-testid="extension-frame-back"
        onClick={onBack}
        size="sm"
        variant="ghost"
      >
        Back
      </Button>
      <span className="truncate text-sm font-medium">{extensionId}</span>
    </div>
  );
}
