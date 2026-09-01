# Granted extension conversations

`agent.converse` is an additive `window.buzz.v1` method. It is unavailable unless all of the following are true:

1. the installed package manifest requests `scopes.agentConverse: true`;
2. the owner explicitly grants **Local agent conversation** for the exact installed package digest;
3. the extension frame holds a live lease bound to the current identity, extension ID, package digest, grant generation, and frame nonce;
4. Buzz's preferred managed-agent runtime is the bundled `buzz-agent`, with a configured provider and model.

No grant is selected by default. Reinstalling or replacing a package requires fresh review.

## Request

```json
{
  "method": "agent.converse",
  "params": {
    "context": {
      "schemaVersion": 1,
      "challengeId": "challenge-id",
      "registryEntryId": "reviewed-entry-id",
      "object": { "id": "object-id", "kind": "node", "label": "Square", "status": "exploring" },
      "parent": { "id": "parent-id", "label": "Residual" },
      "children": [],
      "confusion": "Why square?",
      "learnerHow": "",
      "learnerWhy": "",
      "instruction": "Stay on this object and its immediate relationship."
    },
    "message": "Why is this operation needed?",
    "history": [
      { "role": "agent", "content": "What changes?", "at": "2026-09-01T00:00:00Z", "evidence": false }
    ]
  }
}
```

The host rejects unknown fields at every level. It bounds context, history, individual strings, prompt bytes, per-frame calls, in-flight calls, runtime duration, and reply bytes.

## Result

```json
{ "message": "…", "evidence": false }
```

A reply is conversation only. It never constitutes learner evidence or advances challenge state.

## Execution and privacy boundary

Buzz invokes the bundled `buzz-agent one-shot-no-tools` sidecar through the existing managed-agent provider configuration. The sidecar performs exactly one provider request with an empty tool list. It cannot execute MCP or built-in tools, run hooks, request permissions, publish relay messages, or expose provider credentials/process handles to the extension. The extension receives no raw provider, ACP, relay, or stderr transcript.

The host rejects other preferred runtimes because they do not carry this tool-free contract. Missing configuration, offline providers, timeouts, cancellation, malformed replies, and internal failures return normalized bridge errors. Frame release, disable, replacement, identity change, grant revocation, or shutdown cancels the dedicated process group and suppresses late output.
