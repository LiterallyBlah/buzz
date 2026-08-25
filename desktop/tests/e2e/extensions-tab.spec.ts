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

// ── Should-fix B: the install path, exercised rather than asserted ───────────

const VALID_MANIFEST = JSON.stringify({
  id: "equation-explorer",
  name: "Equation Explorer",
  version: "0.1.0",
  entry: "index.html",
  scopes: {
    identity: true,
    sign: [{ kind: 9, channels: ["c8fb8f44-993d-4166-810e-ebdad7b8b944"] }],
  },
});

test("the Extensions area starts empty", async ({ page }) => {
  await installMockBridge(page);
  await page.goto("/#/extensions");

  await expect(page.getByTestId("extensions-view")).toBeVisible();
  await expect(page.getByText("No extensions installed")).toBeVisible();
});

test("a successful install appears in the installed list", async ({ page }) => {
  await installMockBridge(page, {
    extensionPickPath: "/tmp/equation-explorer",
    extensionPreviewManifest: VALID_MANIFEST,
  });
  await page.goto("/#/extensions");

  await expect(page.getByText("No extensions installed")).toBeVisible();
  await page.getByTestId("install-extension-from-folder").click();

  const card = page.getByTestId("installed-extension-equation-explorer");
  await expect(card).toBeVisible();
  await expect(card).toContainText("Equation Explorer");
  await expect(card).toContainText("0.1.0");
  // The declared scopes are rendered, which is what P5's grant UI builds on.
  await expect(card).toContainText("Identity");
  await expect(card).toContainText("Sign kind 9 in 1 channel");
  await expect(page.getByText("No extensions installed")).toBeHidden();
});

test("an authoritative Rust rejection is surfaced verbatim", async ({
  page,
}) => {
  // The loader can refuse a package whose shape the frontend accepted. That
  // message is written for the user and must reach them unaltered.
  const rejection = 'extension id "../evil" is not valid';
  await installMockBridge(page, {
    extensionPickPath: "/tmp/hostile",
    extensionPreviewManifest: VALID_MANIFEST,
    extensionInstallError: rejection,
  });
  await page.goto("/#/extensions");

  await page.getByTestId("install-extension-from-zip").click();

  const error = page.getByTestId("extension-install-error");
  await expect(error).toBeVisible();
  await expect(error).toContainText(rejection);
  await expect(page.getByText("No extensions installed")).toBeVisible();
});

test("a manifest that fails frontend shape validation never reaches install", async ({
  page,
}) => {
  // Decision 006's frontend half: zod explains the problem before the install
  // command is called. `extensionInstallError` is set to a message that must
  // NOT appear — if it does, the shape gate did not hold.
  await installMockBridge(page, {
    extensionPickPath: "/tmp/broken",
    extensionPreviewManifest: JSON.stringify({
      id: "../evil",
      name: "Broken",
      version: "0.1.0",
      entry: "index.html",
    }),
    extensionInstallError: "RUST-LOADER-WAS-CALLED",
  });
  await page.goto("/#/extensions");

  await page.getByTestId("install-extension-from-folder").click();

  const error = page.getByTestId("extension-install-error");
  await expect(error).toBeVisible();
  await expect(error).toContainText("extension.json");
  await expect(error).toContainText("id must match");
  await expect(error).not.toContainText("RUST-LOADER-WAS-CALLED");
});
