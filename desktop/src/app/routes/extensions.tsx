import * as React from "react";
import { createFileRoute } from "@tanstack/react-router";

import { usePreviewFeatureWarning } from "@/shared/features";
import { ViewLoadingFallback } from "@/shared/ui/ViewLoadingFallback";

const ExtensionsScreen = React.lazy(async () => {
  const module = await import("@/features/extensions/ui/ExtensionsScreen");
  return { default: module.ExtensionsScreen };
});

export const Route = createFileRoute("/extensions")({
  component: ExtensionsRouteComponent,
});

function ExtensionsRouteComponent() {
  usePreviewFeatureWarning("extensions");
  return (
    <React.Suspense fallback={<ViewLoadingFallback kind="extensions" />}>
      <ExtensionsScreen />
    </React.Suspense>
  );
}
