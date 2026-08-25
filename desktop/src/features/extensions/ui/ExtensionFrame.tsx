import * as React from "react";

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
 * There is no bridge here. The frame gets no `window.buzz`, no port and no
 * Tauri IPC — a page loaded in it can render itself and nothing else. P4 adds
 * the handshake; this component only has to leave that door open, which it does
 * by keeping a ref to the iframe (the host must later verify
 * `event.source === frame.contentWindow`, an identity check — an opaque frame's
 * `event.origin` is the string `"null"` and cannot be used).
 */
export function ExtensionFrame({ extensionId }: ExtensionFrameProps) {
  const [target, setTarget] = React.useState<ExtensionFrameTarget | null>(null);
  const [error, setError] = React.useState<string | null>(null);
  const frameRef = React.useRef<HTMLIFrameElement>(null);

  React.useEffect(() => {
    let live = true;
    setTarget(null);
    setError(null);

    void openExtensionFrame(extensionId)
      .then((opened) => {
        if (!live) {
          // Unmounted while the host was starting. Release immediately or the
          // listener has a holder that no longer exists.
          void closeExtensionFrame();
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
      void closeExtensionFrame();
    };
  }, [extensionId]);

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
