import {
  ExtensionFrame,
  ExtensionFrameHeader,
} from "@/features/extensions/ui/ExtensionFrame";
import { useFeatureEnabled } from "@/shared/features";

type ExtensionFrameScreenProps = {
  extensionId: string;
  onBack: () => void;
};

/**
 * The hosted-extension tab.
 *
 * The frame is gated on the preview flag rather than only the route, so
 * switching `extensions` off in Settings unmounts the frame — which is what
 * releases the frame host. A route that kept rendering behind a disabled flag
 * would leave a localhost listener serving a feature the user turned off.
 */
export function ExtensionFrameScreen({
  extensionId,
  onBack,
}: ExtensionFrameScreenProps) {
  const enabled = useFeatureEnabled("extensions");

  return (
    <div
      className="flex min-h-0 min-w-0 flex-1 flex-col overflow-hidden"
      data-testid="extension-frame-view"
    >
      <ExtensionFrameHeader extensionId={extensionId} onBack={onBack} />
      {enabled ? (
        <ExtensionFrame extensionId={extensionId} />
      ) : (
        <div
          className="flex min-h-0 flex-1 items-center justify-center text-sm text-muted-foreground"
          data-testid="extension-frame-disabled"
        >
          Extensions is turned off in Settings → Experiments.
        </div>
      )}
    </div>
  );
}
