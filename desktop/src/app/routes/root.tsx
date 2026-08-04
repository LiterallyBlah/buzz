import { createRootRoute } from "@tanstack/react-router";

import { AppShell } from "@/app/AppShell";
import { ProjectNotificationsBridge } from "@/app/ProjectNotificationsBridge";

export const Route = createRootRoute({
  component: RootRouteComponent,
});

/**
 * The root route renders the shell plus any headless, app-global listeners
 * that need router context (location-driven clear/suppress rules).
 *
 * `ProjectNotificationsBridge` renders nothing and is mounted as a sibling
 * rather than inside `AppShell` so it does not add surface to a file already
 * at the repo's size ceiling, and so its lifetime is the app's rather than a
 * screen's.
 */
function RootRouteComponent() {
  return (
    <>
      <ProjectNotificationsBridge />
      <AppShell />
    </>
  );
}
