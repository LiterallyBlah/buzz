#!/usr/bin/env node
// =============================================================================
// acp-stub-agent.mjs — a deterministic ACP agent for the promotion gates.
// =============================================================================
// WHY THIS EXISTS
//   buzz-acp spawns an agent subprocess and runs the ACP `initialize` handshake
//   before it will connect to the relay at all (crates/buzz-acp/src/lib.rs:8578
//   `initialize_agent_pool`, called from lib.rs:1887 during boot). Its default
//   --agent-command is `goose`, which (a) is not installed on this host and
//   (b) is a real LLM: non-deterministic, rate-limited, and billable. A
//   promotion gate that depends on an LLM answering correctly is not a gate,
//   it is a coin flip.
//
//   So the skew and soak gates point BUZZ_ACP_AGENT_COMMAND at this file. It
//   speaks exactly enough of the protocol for buzz-acp to boot, take a turn,
//   and produce a reply — which is what those gates actually assert on. The
//   agent's *intelligence* is explicitly not under test; the harness's
//   connect/discover/enrol/reply/shutdown path is.
//
// PROTOCOL
//   NDJSON JSON-RPC 2.0 over stdio (crates/buzz-acp/src/acp.rs). Implemented:
//     initialize      -> protocolVersion 2 + agentInfo   (acp.rs:749-764)
//     session/new     -> { sessionId }                   (acp.rs:818-828)
//     session/prompt  -> agent_message_chunk notification, then
//                        { stopReason: "end_turn" }      (acp.rs:942, :2158)
//     session/cancel  -> notification; ends the turn as "cancelled" (acp.rs:994)
//   Any other request is answered with `{}` rather than a JSON-RPC error:
//   buzz-acp probes several optional adapter methods (session/set_model,
//   session/set_config_option, the _goose/* extensions) and a stub that
//   refused them would exercise error paths the gate is not trying to test.
//
// DIAGNOSTICS
//   Set GATES_STUB_LOG=<path> to capture every frame in/out. stderr is left
//   clean because buzz-acp surfaces agent stderr in its own logs, and the gates
//   assert "no ERROR lines" on those.
// =============================================================================

import { appendFileSync } from "node:fs";
import { execSync } from "node:child_process";

const LOG = process.env.GATES_STUB_LOG || "";
const REPLY =
  process.env.GATES_STUB_REPLY ||
  "ack from buzz-gates stub agent: turn completed deterministically";
const AGENT_NAME = process.env.GATES_STUB_NAME || "buzz-gates-stub";

function trace(direction, obj) {
  if (!LOG) return;
  try {
    appendFileSync(LOG, `${direction} ${JSON.stringify(obj)}\n`);
  } catch {
    /* diagnostics must never take down the stub */
  }
}

function send(obj) {
  trace("<-", obj);
  process.stdout.write(`${JSON.stringify(obj)}\n`);
}

function reply(id, result) {
  send({ jsonrpc: "2.0", id, result });
}

function notify(method, params) {
  send({ jsonrpc: "2.0", method, params });
}

let sessionSeq = 0;

function handle(msg) {
  const { id, method, params } = msg;

  // Notifications (no id) need no response. session/cancel is the only one
  // that matters and the turn it cancels has already been answered.
  if (id === undefined || id === null) return;

  switch (method) {
    case "initialize":
      // protocolVersion 2 mirrors the version buzz-acp requests
      // (acp.rs:750-751 "intentional temporary pin ... squatting on ACP v2").
      reply(id, {
        protocolVersion: 2,
        agentInfo: { name: AGENT_NAME, version: "1.0.0" },
        agentCapabilities: {
          loadSession: false,
          promptCapabilities: { image: false, audio: false, embeddedContext: false },
        },
        authMethods: [],
      });
      return;

    case "session/new":
      sessionSeq += 1;
      reply(id, { sessionId: `buzz-gates-stub-${sessionSeq}` });
      return;

    case "session/prompt": {
      const sessionId = params?.sessionId ?? `buzz-gates-stub-${sessionSeq}`;
      // On PROJECT turns the runtime does not publish the agent's text: the
      // [Project] prompt hands the agent an exact fenced `buzz issues comment
      // …` command and a real agent EXECUTES it (the runtime's own doc says
      // the command is "spelled out rather than described" for precisely this
      // reason). A text-only stub therefore proves nothing about the reply
      // path — the first live gate day established that no comment can ever
      // appear. So: when the prompt carries a fenced reply command, run it,
      // exactly as Claude-with-a-terminal would, with the body on stdin. The
      // stub inherits BUZZ_PRIVATE_KEY / BUZZ_RELAY_URL from buzz-acp, which
      // is the same inheritance the production adapters rely on; `buzz` is
      // resolved via PATH, which the harness points at the gate's CLI.
      const promptText = (params?.prompt ?? [])
        .map((p) => (p?.type === "text" ? p.text : ""))
        .join("\n");
      const fence = promptText.match(/```bash\n([\s\S]*?)```/);
      let text = REPLY;
      if (fence && /\b(issues|pr) comment\b/.test(fence[1])) {
        try {
          execSync(`set -euo pipefail\n${fence[1]}`, {
            input: REPLY,
            shell: "/bin/bash",
            stdio: ["pipe", "pipe", "pipe"],
            timeout: 30_000,
          });
          text = `${REPLY} (reply command executed)`;
        } catch (e) {
          // Surface the failure in the streamed text so the gate's evidence
          // shows WHY no comment landed, rather than a silent no-reply.
          text = `reply command failed: ${String(e?.stderr ?? e).slice(0, 300)}`;
        }
      }
      notify("session/update", {
        sessionId,
        update: {
          sessionUpdate: "agent_message_chunk",
          content: { type: "text", text },
        },
      });
      reply(id, { stopReason: "end_turn" });
      return;
    }

    default:
      // Permissive: optional adapter methods answer empty rather than erroring.
      reply(id, {});
  }
}

let buffer = "";
process.stdin.setEncoding("utf8");
process.stdin.on("data", (chunk) => {
  buffer += chunk;
  let nl;
  while ((nl = buffer.indexOf("\n")) >= 0) {
    const line = buffer.slice(0, nl).trim();
    buffer = buffer.slice(nl + 1);
    if (!line) continue;
    let msg;
    try {
      msg = JSON.parse(line);
    } catch {
      trace("!!", { unparseable: line.slice(0, 200) });
      continue;
    }
    trace("->", msg);
    try {
      handle(msg);
    } catch (e) {
      trace("!!", { handlerError: String(e) });
    }
  }
});

process.stdin.on("end", () => process.exit(0));
