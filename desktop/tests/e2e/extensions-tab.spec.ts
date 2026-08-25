/**
 * Extensions preview tab (M1 P1).
 *
 * Covers the flag-gated route: the sidebar item only exists behind the
 * `extensions` preview flag, and selecting it routes to `/extensions` and
 * renders the Extensions area with both local install actions.
 *
 * `installMockBridge` seeds every `preview-features.json` desktop id to `true`
 * (`helpers/bridge.ts` -> `seedPreviewFeaturesEnabled`), so the flag is on here
 * without per-spec setup.
 *
 * Not covered yet: the installed-extension list and the install error surface.
 * Both need `list_installed_extensions` / `install_extension_from_*` handlers
 * in the mock IPC bridge, which do not exist — with the commands unmocked the
 * list query rejects and the area renders its load-error state. That is a
 * follow-up when the install path graduates to an E2E-mockable surface.
 */
import { expect, test } from "@playwright/test";

import { installMockBridge } from "../helpers/bridge";

test("extensions tab routes from the sidebar to the Extensions area", async ({
  page,
}) => {
  await installMockBridge(page);
  await page.goto("/");

  const navItem = page.getByTestId("open-extensions-view");
  await expect(navItem).toBeVisible();

  await navItem.click();

  await expect(page).toHaveURL(/#\/extensions$/);
  await expect(page.getByTestId("extensions-view")).toBeVisible();
  await expect(page.getByRole("heading", { name: "Extensions" })).toBeVisible();
  await expect(page.getByTestId("install-extension-from-folder")).toBeVisible();
  await expect(page.getByTestId("install-extension-from-zip")).toBeVisible();
});

test("extensions tab is hidden when the preview flag is off", async ({
  page,
}) => {
  await installMockBridge(page);
  // Overwrite the seeded overrides before the app reads them on mount.
  await page.addInitScript(() => {
    for (let index = 0; index < window.localStorage.length; index += 1) {
      const key = window.localStorage.key(index);
      if (key?.startsWith("buzz-feature-overrides-v")) {
        const overrides = JSON.parse(
          window.localStorage.getItem(key) ?? "{}",
        ) as Record<string, boolean>;
        overrides.extensions = false;
        window.localStorage.setItem(key, JSON.stringify(overrides));
      }
    }
  });
  await page.goto("/");

  await expect(page.getByTestId("sidebar-primary-menu")).toBeVisible();
  await expect(page.getByTestId("open-extensions-view")).toHaveCount(0);
});
