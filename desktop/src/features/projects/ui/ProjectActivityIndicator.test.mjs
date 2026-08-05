/**
 * The project activity indicator's click-through.
 *
 * Two things are worth pinning and neither needs a relay:
 *
 *  1. Which characters are a handle. The announcement is one sentence, but
 *     only the names in it open anything — the stage caption is a description
 *     of the turn, and a reader who clicks "reading files" expecting a
 *     destination has been lied to by the affordance.
 *  2. Which agent each handle carries. With two agents working on one root the
 *     sentence is "A, B are working", and the whole failure mode of building
 *     that as a string is that both names end up pointing at whichever pubkey
 *     the loop last saw.
 *
 * `ProjectActivitySegments` is hook-free, so it is exercised by calling it and
 * walking the returned tree — a real per-name invocation, not an assertion
 * about markup that happens to contain a pubkey.
 */

import assert from "node:assert/strict";
import test from "node:test";

import { renderToStaticMarkup } from "react-dom/server";

import {
  buildProjectActivitySegments,
  ProjectActivitySegments,
} from "./ProjectActivityIndicator.tsx";

const ALICE = "a".repeat(64);
const BOB = "b".repeat(64);

const LABELS = { [ALICE]: "Claude", [BOB]: "Goose" };
const label = (pubkey) => LABELS[pubkey] ?? "Unknown";

function entry(agent, state, stage = null) {
  return {
    agent,
    turnId: `turn-${agent.slice(0, 4)}`,
    state,
    stage,
    announcedAt: 1,
  };
}

/** The sentence a reader sees, handles and prose alike. */
function sentence(segments) {
  return segments
    .map((segment) => (segment.kind === "agent" ? segment.label : segment.text))
    .join("");
}

/** Every <button> in the rendered tree, in document order. */
function buttonsOf(node) {
  const children = node.props.children;
  return (Array.isArray(children) ? children : [children]).filter(
    (child) => child?.type === "button",
  );
}

test("a lone working agent is a handle; its stage caption is not", () => {
  const segments = buildProjectActivitySegments({
    entries: [entry(ALICE, "working", "reading files")],
    label,
  });

  assert.deepEqual(segments, [
    { kind: "agent", agent: ALICE, label: "Claude" },
    { kind: "text", text: " is working — reading files" },
  ]);
  assert.equal(sentence(segments), "Claude is working — reading files");
});

test("two working agents get one handle each, carrying their own pubkey", () => {
  const segments = buildProjectActivitySegments({
    entries: [
      entry(ALICE, "working", "reading files"),
      entry(BOB, "working", "running tests"),
    ],
    label,
  });

  assert.deepEqual(
    segments.filter((segment) => segment.kind === "agent"),
    [
      { kind: "agent", agent: ALICE, label: "Claude" },
      { kind: "agent", agent: BOB, label: "Goose" },
    ],
  );
  // No stage with two agents: one agent's caption attached to both names is a
  // claim about both that is wrong about one.
  assert.equal(sentence(segments), "Claude, Goose are working");
});

test("a working agent and a queued agent are separate handles in separate phrases", () => {
  const segments = buildProjectActivitySegments({
    entries: [entry(ALICE, "working", "reading files"), entry(BOB, "queued")],
    label,
  });

  assert.equal(sentence(segments), "Claude is working · Goose is queued");
  assert.deepEqual(
    segments.map((segment) => segment.kind),
    ["agent", "text", "agent", "text"],
  );
});

test("nothing live is no sentence at all", () => {
  assert.deepEqual(buildProjectActivitySegments({ entries: [], label }), []);
});

test("clicking a name opens that agent's activity, and only that agent's", () => {
  const segments = buildProjectActivitySegments({
    entries: [entry(ALICE, "working"), entry(BOB, "working")],
    label,
  });
  const opened = [];
  const buttons = buttonsOf(
    ProjectActivitySegments({
      onOpenAgent: (pubkey) => opened.push(pubkey),
      segments,
    }),
  );

  assert.equal(buttons.length, 2, "one handle per working agent");
  buttons[1].props.onClick();
  assert.deepEqual(opened, [BOB], "the second name opens the second agent");
  buttons[0].props.onClick();
  assert.deepEqual(
    opened,
    [BOB, ALICE],
    "each name is independently clickable",
  );
});

test("the prose between the names is inert text, not an element", () => {
  const segments = buildProjectActivitySegments({
    entries: [entry(ALICE, "working", "reading files"), entry(BOB, "queued")],
    label,
  });
  const children = ProjectActivitySegments({
    onOpenAgent: () => assert.fail("prose must not open anything"),
    segments,
  }).props.children;

  const prose = children.filter((child) => child?.type !== "button");
  assert.deepEqual(prose, [" is working · ", " is queued"]);
  // Nothing to carry a handler: a string cannot be clicked into a destination
  // the way a <span onClick> silently could.
  assert.ok(prose.every((child) => typeof child === "string"));
});

test("each handle renders as a keyboard-reachable button keyed to its pubkey", () => {
  const segments = buildProjectActivitySegments({
    entries: [entry(ALICE, "working", "reading files"), entry(BOB, "queued")],
    label,
  });
  const html = renderToStaticMarkup(
    ProjectActivitySegments({ onOpenAgent: () => {}, segments }),
  );

  for (const [pubkey, name] of [
    [ALICE, "Claude"],
    [BOB, "Goose"],
  ]) {
    assert.match(
      html,
      new RegExp(
        `<button[^>]*data-testid="project-activity-agent-${pubkey}"[^>]*>${name}</button>`,
      ),
      `${name} must render as its own button`,
    );
  }
  // A <button type="button"> is focusable and Enter/Space-activated by the
  // platform; a div with an onClick would look identical and be reachable by
  // mouse only.
  assert.equal(html.match(/type="button"/g)?.length, 2);
  // The connective prose sits outside both buttons.
  assert.match(html, /<\/button> is working · <button/);
});

test("the stage caption renders outside the button, not inside it", () => {
  const html = renderToStaticMarkup(
    ProjectActivitySegments({
      onOpenAgent: () => {},
      segments: buildProjectActivitySegments({
        entries: [entry(ALICE, "working", "reading files")],
        label,
      }),
    }),
  );

  assert.match(
    html,
    /<button[^>]*>Claude<\/button> is working — reading files$/,
    "the caption must not be part of the click target",
  );
});
