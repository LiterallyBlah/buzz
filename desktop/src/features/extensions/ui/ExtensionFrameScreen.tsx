import * as React from "react";

import {
  closeNativeExtensionWindow,
  type ExtensionSurfaceMode,
  getExtensionSurfaceMode,
} from "@/features/extensions/lib/extensionsApi";
import {
  ExtensionFrame,
  ExtensionFrameHeader,
} from "@/features/extensions/ui/ExtensionFrame";
import { NativeExtensionWindow } from "@/features/extensions/ui/NativeExtensionWindow";
import { useFeatureEnabled } from "@/shared/features";

type ExtensionFrameScreenProps = {
  extensionId: string;
  onBack: () => void;
};

/**
 * The hosted-extension route.
 *
 * Linux retains the accepted iframe composition. Windows renders an explicit
 * status/action surface for a dedicated native window; merely navigating here
 * never opens one. Preview disable and route teardown unmount the owning
 * component, which closes the exact native label/lease.
 */
export function ExtensionFrameScreen({
  extensionId,
  onBack,
}: ExtensionFrameScreenProps) {
  const enabled = useFeatureEnabled("extensions");
  const [mode, setMode] = React.useState<ExtensionSurfaceMode | null>(null);
  const [modeError, setModeError] = React.useState<string | null>(null);

  React.useEffect(() => {
    let live = true;
    void getExtensionSurfaceMode()
      .then((value) => {
        if (live) setMode(value);
      })
      .catch((cause: unknown) => {
        if (live) {
          setModeError(typeof cause === "string" ? cause : String(cause));
        }
      });
    return () => {
      live = false;
    };
  }, []);

  const back = React.useCallback(() => {
    if (mode === "windows-native-window") {
      void closeNativeExtensionWindow(extensionId).finally(onBack);
      return;
    }
    onBack();
  }, [extensionId, mode, onBack]);

  let surface: React.ReactNode;
  if (!enabled) {
    surface = (
      <div
        className="flex min-h-0 flex-1 items-center justify-center text-sm text-muted-foreground"
        data-testid="extension-frame-disabled"
      >
        Extensions is turned off in Settings → Experiments.
      </div>
    );
  } else if (modeError) {
    surface = (
      <div
        className="flex min-h-0 flex-1 items-center justify-center p-4 text-sm text-destructive"
        data-testid="extension-surface-mode-error"
      >
        The secure extension surface could not be selected: {modeError}
      </div>
    );
  } else if (mode === "windows-native-window") {
    surface = <NativeExtensionWindow extensionId={extensionId} />;
  } else if (mode === "linux-iframe") {
    surface = <ExtensionFrame extensionId={extensionId} />;
  } else {
    surface = (
      <div
        className="flex min-h-0 flex-1 items-center justify-center text-sm text-muted-foreground"
        data-testid="extension-surface-mode-loading"
      >
        Selecting a secure extension surface…
      </div>
    );
  }

  return (
    <div
      className="flex min-h-0 min-w-0 flex-1 flex-col overflow-hidden"
      data-testid="extension-frame-view"
    >
      <ExtensionFrameHeader extensionId={extensionId} onBack={back} />
      {surface}
    </div>
  );
}
