import * as React from "react";
import { listen } from "@tauri-apps/api/event";

import {
  closeNativeExtensionWindow,
  getNativeExtensionWindowStatus,
  openNativeExtensionWindow,
  type NativeExtensionWindowStatus,
} from "@/features/extensions/lib/extensionsApi";
import { Button } from "@/shared/ui/button";
import { Card } from "@/shared/ui/card";

const STATUS_EVENT = "extension-native-window-status";

function readableState(status: NativeExtensionWindowStatus | null): string {
  if (!status) return "Checking secure window…";
  switch (status.state) {
    case "opening":
      return "Secure window is opening…";
    case "open":
      return "Secure window is open.";
    case "failed":
      return status.error ?? "Secure window failed to open.";
    default:
      return "Secure window is closed.";
  }
}

/**
 * Windows route surface for one dedicated extension WebView2 environment.
 *
 * Opening is always explicit. Rust owns label/lease/UDF authority and repeated
 * opens focus the one exact live window. Unmount invalidates the ownership epoch;
 * any late Open completion immediately closes itself instead of becoming orphaned.
 */
export function NativeExtensionWindow({
  extensionId,
}: {
  extensionId: string;
}) {
  const [status, setStatus] =
    React.useState<NativeExtensionWindowStatus | null>(null);
  const [busy, setBusy] = React.useState(false);
  const mounted = React.useRef(false);
  const ownershipEpoch = React.useRef(0);

  React.useEffect(() => {
    let live = true;
    mounted.current = true;
    const setupEpoch = ownershipEpoch.current;
    let unlisten: (() => void) | undefined;
    void (async () => {
      unlisten = await listen<NativeExtensionWindowStatus>(
        STATUS_EVENT,
        (event) => {
          if (live && event.payload.extensionId === extensionId) {
            setStatus(event.payload);
          }
        },
      );
      const current = await getNativeExtensionWindowStatus(extensionId);
      if (live && ownershipEpoch.current === setupEpoch) setStatus(current);
    })().catch((cause: unknown) => {
      if (live && ownershipEpoch.current === setupEpoch) {
        setStatus({
          extensionId,
          state: "failed",
          label: null,
          error: typeof cause === "string" ? cause : String(cause),
        });
      }
    });

    return () => {
      live = false;
      mounted.current = false;
      ownershipEpoch.current += 1;
      unlisten?.();
      void closeNativeExtensionWindow(extensionId);
    };
  }, [extensionId]);

  const open = React.useCallback(async () => {
    const epoch = ownershipEpoch.current + 1;
    ownershipEpoch.current = epoch;
    setBusy(true);
    try {
      const opened = await openNativeExtensionWindow(extensionId);
      if (!mounted.current || ownershipEpoch.current !== epoch) {
        await closeNativeExtensionWindow(extensionId);
        return;
      }
      setStatus(opened);
    } catch (cause) {
      if (!mounted.current || ownershipEpoch.current !== epoch) return;
      setStatus({
        extensionId,
        state: "failed",
        label: null,
        error: typeof cause === "string" ? cause : String(cause),
      });
    } finally {
      if (mounted.current && ownershipEpoch.current === epoch) setBusy(false);
    }
  }, [extensionId]);

  const close = React.useCallback(async () => {
    const epoch = ownershipEpoch.current + 1;
    ownershipEpoch.current = epoch;
    setBusy(true);
    try {
      const closed = await closeNativeExtensionWindow(extensionId);
      if (mounted.current && ownershipEpoch.current === epoch)
        setStatus(closed);
    } finally {
      if (mounted.current && ownershipEpoch.current === epoch) setBusy(false);
    }
  }, [extensionId]);

  const isLive = status?.state === "open" || status?.state === "opening";

  return (
    <div className="flex min-h-0 flex-1 items-start justify-center overflow-auto p-4">
      <Card
        className="w-full max-w-2xl p-4"
        data-state={status?.state ?? "checking"}
        data-testid="extension-native-window-status"
      >
        <p className="text-sm font-medium">Secure Windows extension window</p>
        <p
          className="mt-1 break-all text-xs text-muted-foreground"
          data-testid="extension-native-window-id"
        >
          {extensionId}
        </p>
        <p
          aria-live="polite"
          className="mt-3 text-sm text-muted-foreground"
          data-testid="extension-native-window-state"
        >
          {readableState(status)}
        </p>
        <div className="mt-4 flex gap-2">
          <Button
            data-testid="extension-native-window-open"
            disabled={busy}
            onClick={() => void open()}
            size="sm"
          >
            {status?.state === "open" ? "Focus" : "Open secure window"}
          </Button>
          <Button
            data-testid="extension-native-window-close"
            disabled={busy || !isLive}
            onClick={() => void close()}
            size="sm"
            variant="outline"
          >
            Close
          </Button>
        </div>
      </Card>
    </div>
  );
}
