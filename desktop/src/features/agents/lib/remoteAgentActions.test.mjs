import assert from "node:assert/strict";
import { describe, it } from "node:test";
import {
  buildRemoteAgentActions,
  describeLatestRemoteAgentControl,
  remoteAgentConfirmationCopy,
} from "./remoteAgentActions.ts";

describe("remote agent actions", () => {
  it("shows only controls to members and adds role-gated ban state", () => {
    assert.deepEqual(
      buildRemoteAgentActions({ relayRole: "member", banned: false }).map(
        (a) => a.label,
      ),
      ["Stop current work", "Finish work and shut down"],
    );
    assert.equal(
      buildRemoteAgentActions({ relayRole: "owner", banned: false }).at(-1)
        ?.label,
      "Ban from this community",
    );
    assert.equal(
      buildRemoteAgentActions({ relayRole: "admin", banned: true }).at(-1)
        ?.label,
      "Lift ban",
    );
  });
  it("shows the most recently invoked control while prioritising an in-flight request", () => {
    const stopped = {
      phase: "settled",
      acknowledgement: {
        kind: "accepted",
        activeTurns: 1,
        signalledTurns: 1,
        queuedEvents: 0,
      },
    };
    const idleDrain = { phase: "idle" };
    assert.match(
      describeLatestRemoteAgentControl(stopped, idleDrain, "cancel_all").badge,
      /Stop requested/,
    );

    const sendingDrain = { phase: "sending" };
    assert.equal(
      describeLatestRemoteAgentControl(stopped, sendingDrain, "drain").badge,
      "Sending drain…",
    );
    const draining = { phase: "settled", acknowledgement: "draining" };
    assert.equal(
      describeLatestRemoteAgentControl(stopped, draining, "drain").badge,
      "Draining — finishing current work",
    );

    assert.equal(
      describeLatestRemoteAgentControl({ phase: "sending" }, draining, "drain")
        .badge,
      "Stopping current work…",
    );
  });

  it("names the security and process consequences honestly", () => {
    assert.match(
      remoteAgentConfirmationCopy("cancel_all", "Nadia").description,
      /stay online and can accept new work/,
    );
    assert.match(
      remoteAgentConfirmationCopy("drain", "Nadia").description,
      /cannot start it again/,
    );
    assert.match(
      remoteAgentConfirmationCopy("ban", "Nadia").description,
      /including Buzz memories/,
    );
    assert.match(
      remoteAgentConfirmationCopy("ban", "Nadia").description,
      /host process may continue/,
    );
  });
});
