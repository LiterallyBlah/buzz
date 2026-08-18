import assert from "node:assert/strict";
import { describe, it } from "node:test";

import { settingsSections } from "./SettingsPanels.tsx";
import { settingsNavGroups } from "./SettingsView.tsx";

// Sections that exist but are deliberately absent from the sidebar; they are
// reachable only via the settings route's `?section=` deep link.
const SIDEBAR_HIDDEN = ["moderation"];

describe("settingsNavGroups", () => {
  // A descriptor in settingsSections (with its gate and panel case) renders
  // nowhere unless its value is also listed in a nav group — a gated section
  // left out of the groups is silently unreachable from the UI, which is
  // exactly how the ambient-voice section shipped invisible.
  it("lists every settings section in exactly one sidebar group", () => {
    const grouped = settingsNavGroups.flatMap((group) => group.sections);
    const known = settingsSections.map((section) => section.value);

    const missing = known.filter(
      (value) => !grouped.includes(value) && !SIDEBAR_HIDDEN.includes(value),
    );
    assert.deepEqual(missing, []);

    const unknown = grouped.filter((value) => !known.includes(value));
    assert.deepEqual(unknown, []);

    assert.equal(grouped.length, new Set(grouped).size);
  });
});
