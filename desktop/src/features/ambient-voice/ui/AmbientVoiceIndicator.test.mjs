/**
 * The listening pill's placement and its press behaviour.
 *
 * Dogfood found the pill hard-pinned to the bottom-left corner, on top of the
 * sidebar's profile card, with no way to move it. Making it draggable puts two
 * things at risk that are worth pinning here rather than in a screenshot:
 *
 *  1. The mute toggle. The pill is dragged by its own body, so every press is
 *     ambiguous until the pointer moves — a drag that also muted the
 *     microphone would be worse than the overlap it fixed.
 *  2. Reachability. A position saved on a large display, or a window shrunk
 *     afterwards, must never leave the pill outside the viewport: it is the
 *     only one-click mute in the app, and an off-screen one cannot be pressed.
 *
 * The geometry itself is covered by `lib/indicatorPosition.test.mjs`; this
 * file drives the real component through real pointer events.
 */

import assert from "node:assert/strict";
import test from "node:test";

import {
  ambientReport,
  deafAmbientReport,
  setViewport,
  withAmbientDom,
} from "../lib/ambientVoiceTestDom.mjs";

const VIEWPORT = { width: 1200, height: 800 };
/** jsdom reports `offsetWidth` as 0, so the component's fallback box applies. */
const PILL = { width: 176, height: 26 };
const MARGIN = 12;
const BOTTOM_RIGHT = {
  left: `${VIEWPORT.width - PILL.width - MARGIN}px`,
  top: `${VIEWPORT.height - PILL.height - MARGIN}px`,
};

async function mountIndicator({ report = ambientReport() } = {}, body) {
  await withAmbientDom(
    {
      ...VIEWPORT,
      invoke: (command, args) => {
        switch (command) {
          case "get_ambient_voice_status":
            return report;
          case "set_ambient_voice_muted":
            return { ...report, muted: args.muted };
          case "set_ambient_indicator_position":
            return { ...report, indicatorPosition: args.position };
          default:
            throw new Error(`unexpected command: ${command}`);
        }
      },
    },
    async ({ dom, calls }) => {
      const { OVERRIDES_KEY } = await import("@/shared/features/store.ts");
      dom.window.localStorage.setItem(
        OVERRIDES_KEY,
        JSON.stringify({ ambientVoice: true }),
      );

      const React = await import("react");
      const testing = await import("@testing-library/react");
      const { AmbientVoiceIndicator } = await import(
        "./AmbientVoiceIndicator.tsx"
      );

      const view = testing.render(React.createElement(AmbientVoiceIndicator));
      await flush(testing);
      try {
        await body({
          calls,
          dom,
          flush: () => flush(testing),
          pill: () => view.getByTestId("ambient-voice-indicator"),
          testing,
        });
      } finally {
        testing.cleanup();
      }
    },
  );
}

async function flush(testing) {
  await testing.act(async () => {
    await new Promise((resolve) => setTimeout(resolve, 0));
  });
}

const commandsIn = (calls) =>
  calls
    .map((call) => call.command)
    .filter((command) => !command.startsWith("plugin:"));

test("the pill parks in the bottom-right corner, clear of the profile card", async () => {
  // The bottom-left corner is the sidebar profile card's and the reconnect
  // overlay's; the default has to be somewhere else, and the bottom-right is
  // the only corner with no fixed chrome of its own.
  await mountIndicator({}, async ({ pill }) => {
    assert.equal(pill().style.left, BOTTOM_RIGHT.left);
    assert.equal(pill().style.top, BOTTOM_RIGHT.top);
  });
});

test("dragging moves the pill and saves where it was dropped", async () => {
  await mountIndicator({}, async ({ calls, flush, pill, testing }) => {
    await testing.act(async () => {
      testing.fireEvent.pointerDown(pill(), {
        button: 0,
        clientX: 1050,
        clientY: 770,
        pointerId: 1,
      });
      testing.fireEvent.pointerMove(pill(), {
        clientX: 650,
        clientY: 370,
        pointerId: 1,
      });
      testing.fireEvent.pointerUp(pill(), {
        clientX: 650,
        clientY: 370,
        pointerId: 1,
      });
    });
    await flush();

    assert.equal(pill().style.left, "612px");
    assert.equal(pill().style.top, "362px");
    assert.deepEqual(
      calls.find((call) => call.command === "set_ambient_indicator_position")
        ?.args.position,
      { x: 612, y: 362 },
    );
  });
});

test("a drag does not toggle mute", async () => {
  // The click the browser fires after the pointer is released has to be
  // swallowed, or moving the pill would also close the microphone.
  await mountIndicator({}, async ({ calls, flush, pill, testing }) => {
    await testing.act(async () => {
      testing.fireEvent.pointerDown(pill(), {
        button: 0,
        clientX: 1050,
        clientY: 770,
        pointerId: 1,
      });
      testing.fireEvent.pointerMove(pill(), {
        clientX: 650,
        clientY: 370,
        pointerId: 1,
      });
      testing.fireEvent.pointerUp(pill(), {
        clientX: 650,
        clientY: 370,
        pointerId: 1,
      });
      testing.fireEvent.click(pill());
    });
    await flush();

    assert.ok(
      !commandsIn(calls).includes("set_ambient_voice_muted"),
      `a drag muted the microphone: ${commandsIn(calls).join(", ")}`,
    );
  });
});

test("a press that barely moves is still a click on mute", async () => {
  // Pointers wander a pixel or two between press and release. Treating that as
  // a drag would make the only one-click mute in the app unusable.
  await mountIndicator({}, async ({ calls, flush, pill, testing }) => {
    await testing.act(async () => {
      testing.fireEvent.pointerDown(pill(), {
        button: 0,
        clientX: 1050,
        clientY: 770,
        pointerId: 1,
      });
      testing.fireEvent.pointerMove(pill(), {
        clientX: 1052,
        clientY: 771,
        pointerId: 1,
      });
      testing.fireEvent.pointerUp(pill(), {
        clientX: 1052,
        clientY: 771,
        pointerId: 1,
      });
      testing.fireEvent.click(pill());
    });
    await flush();

    assert.deepEqual(
      calls.find((call) => call.command === "set_ambient_voice_muted")?.args,
      { muted: true },
    );
    // And it did not move.
    assert.equal(pill().style.left, BOTTOM_RIGHT.left);
  });
});

test('a session that is hearing nothing says so instead of "listening"', async () => {
  // The shipped bug, as the user meets it: the native session is alive, so the
  // status is `listening`, and not one frame of audio has reached it. The pill
  // said "Listening for the wake word" for the whole run while the wake word
  // was deaf — the one thing this control exists to be honest about.
  await mountIndicator({ report: deafAmbientReport() }, async ({ pill }) => {
    assert.equal(
      pill().textContent,
      "No audio arriving from the microphone",
      "the pill claimed to be listening while nothing was arriving",
    );
    // And it does not look live either: truthful copy beside a lit
    // microphone still reads as "it is hearing me".
    assert.equal(pill().querySelector(".text-primary"), null);
  });
});

test("a session that is being fed keeps the ordinary listening copy", async () => {
  // The control: the deafness copy must not appear on a working session, or it
  // would train users to ignore it.
  await mountIndicator({}, async ({ pill }) => {
    assert.equal(pill().textContent, "Listening for the wake word");
  });
});

test("a position saved on a larger display is pulled back into the window", async () => {
  await mountIndicator(
    { report: ambientReport({ indicatorPosition: { x: 5_000, y: 4_000 } }) },
    async ({ pill }) => {
      assert.equal(pill().style.left, BOTTOM_RIGHT.left);
      assert.equal(pill().style.top, BOTTOM_RIGHT.top);
    },
  );
});

test("shrinking the window brings the pill back with it", async () => {
  await mountIndicator({}, async ({ dom, flush, pill, testing }) => {
    assert.equal(pill().style.left, BOTTOM_RIGHT.left);

    setViewport(dom, { width: 400, height: 300 });
    await testing.act(async () => {
      dom.window.dispatchEvent(new dom.window.Event("resize"));
    });
    await flush();

    assert.equal(pill().style.left, `${400 - PILL.width - MARGIN}px`);
    assert.equal(pill().style.top, `${300 - PILL.height - MARGIN}px`);
  });
});
