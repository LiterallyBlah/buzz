import * as React from "react";
import {
  createFileRoute,
  useNavigate,
  useParams,
} from "@tanstack/react-router";

import { usePreviewFeatureWarning } from "@/shared/features";
import { ViewLoadingFallback } from "@/shared/ui/ViewLoadingFallback";

const ExtensionFrameScreen = React.lazy(async () => {
  const module = await import("@/features/extensions/ui/ExtensionFrameScreen");
  return { default: module.ExtensionFrameScreen };
});

export const Route = createFileRoute("/extensions/$extensionId")({
  component: ExtensionFrameRouteComponent,
});

function ExtensionFrameRouteComponent() {
  usePreviewFeatureWarning("extensions");
  const { extensionId } = useParams({ from: "/extensions/$extensionId" });
  const navigate = useNavigate();

  return (
    <React.Suspense fallback={<ViewLoadingFallback kind="extensions" />}>
      <ExtensionFrameScreen
        extensionId={extensionId}
        onBack={() => void navigate({ to: "/extensions" })}
      />
    </React.Suspense>
  );
}
