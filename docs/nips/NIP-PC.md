NIP-PC
======

Peer Agent Calls
----------------

`draft` `optional` `agent`

This NIP defines how one trusted agent explicitly calls another to perform a
single bounded task, and how the callee returns exactly one correlated result
to the surface the call was made from.

It exists because "a structured call envelope" is not implementable twice.
Buzz has two independent agent runtimes — the Rust `buzz-acp` harness and the
Python Hermes Buzz adapter — and two teams reading that phrase produce two
incompatible schemas and discover it at integration time. Everything a second
implementation needs to interoperate is pinned here: exact kinds, exact tag
names, required versus optional fields, canonical call-id derivation, how the
originating route is bound into the envelope, the wire representation of hop
count and visited set, and which signature is checked against what.

## Motivation

An agent that can only be woken by a human is a dead end for multi-agent work.
But the obvious mechanism — let agents mention each other — is unsafe, because
an ordinary reply already carries a `p` tag for its recipient. Two agents that
trust each other's `p` tags wake each other forever without either one ever
deciding to make a call.

So invocation must be *explicit and distinguishable from a reply*. A peer call
is its own event kind carrying its own envelope. A bare `p` tag from an agent
is never an invocation, in any runtime, at any trust level.

## Definitions

- **Caller**: the agent that publishes a call. Always the call event's `pubkey`.
- **Callee**: the agent named by the call's `p` tag.
- **Trusted**: a cryptographically verified same-owner [NIP-OA](NIP-OA.md)
  sibling, or a pubkey on the owner's explicit external-agent allow-list. It
  never means "any relay identity" and never means "the repository owner".
- **Route**: the conversation surface the call was made from — a channel, or a
  project issue/pull-request root.
- **Call path**: the ordered set of agents that have already participated in
  this chain of calls.

## Event kinds

| Kind | Name | Role |
|---|---|---|
| `43001` | `KIND_JOB_REQUEST` | The call envelope |
| `43004` | `KIND_JOB_RESULT` | The correlated result |

These are the existing Buzz agent-job kinds. They are reused rather than
duplicated: they are already registered in `ALL_KINDS`, they appear in none of
the relay's restricted read classes (`P_GATED_KINDS`, `AUTHOR_ONLY_KINDS`,
`RESULT_GATED_KINDS`, `SHARED_GATED_KINDS` — `crates/buzz-core/src/kind.rs`), an
event with no `h` tag is stored community-globally rather than refused (`channel_id
IS NULL`, `crates/buzz-db/src/event.rs`) so a project-routed call is expressible,
and no other producer emits them. A call and its result are **different kinds**
on purpose — a result must not be mistakable for a fresh invocation by any
receiver, including one that ignores this document.

Kinds `43002`, `43003`, `43005` and `43006` are not used by this NIP.

## The call event (kind 43001)

```json
{
  "kind": 43001,
  "pubkey": "<caller_pubkey>",
  "created_at": 1754150400,
  "content": "<the bounded task, plain text>",
  "tags": [
    ["p",       "<callee_pubkey>"],
    ["call",    "<call_id>"],
    ["nonce",   "<32 lowercase hex chars>"],
    ["hop",     "1"],
    ["visited", "<caller_pubkey>"],
    ["h",       "<channel_uuid>"]
  ],
  "sig": "..."
}
```

### Required tags

| Tag | Cardinality | Value |
|---|---|---|
| `p` | exactly 1 | callee pubkey, 64 lowercase hex |
| `call` | exactly 1 | call id, 64 lowercase hex — see [Call id](#call-id) |
| `nonce` | exactly 1 | 32 lowercase hex chars (16 bytes) |
| `hop` | exactly 1 | decimal integer, no sign, no leading zeros, `1`–`3` |
| `visited` | 1 or more | pubkeys already in the call path, 64 lowercase hex |
| route | exactly one form | see [Route binding](#route-binding) |

`content` is the task. It MUST NOT be empty and MUST NOT exceed 16 KiB.

Any other tag is ignored. A receiver MUST NOT infer meaning from an unknown
tag, and MUST NOT accept an envelope whose required tags are absent,
duplicated, or malformed. Every rule in this section is a rejection rule:
there are no lenient readings and no defaults. A call that does not parse is
refused, not repaired.

### Route binding

A call is bound to the surface it was made from, and the result lands there.
Exactly one of these two forms MUST be present. An event carrying both, or
neither, is malformed.

**Channel route**

| Tag | Cardinality | Value |
|---|---|---|
| `h` | exactly 1 | channel UUID, lowercase, hyphenated |
| `e` | 0 or 1 | thread root event id, marked `root` |

**Project route**

| Tag | Cardinality | Value |
|---|---|---|
| `a` | exactly 1 | `30617:<owner_pubkey>:<identifier>` repo coordinate |
| `e` | exactly 1 | issue/PR root event id, marked `root` |

The project form requires its `e`: a project call with no root names no
conversation. The channel form's `e` is optional because a channel call may be
made at top level.

### Call id

The call id is derived, not chosen, so it cannot be lifted from one call onto
a different callee or a different route. A receiver MUST recompute it and
reject any mismatch.

```
call_id = lowercase_hex(sha256(
    "buzz/peer-call/v1" || 0x0a ||
    caller_pubkey_hex  || 0x0a ||
    callee_pubkey_hex  || 0x0a ||
    route_token        || 0x0a ||
    nonce_hex
))
```

All pubkeys and the nonce are lowercase hex. `route_token` is:

- channel route: `channel:<uuid>:<thread_root_or_empty>`
- project route: `project:<coordinate>:<root_event_id>`

with `<uuid>` lowercase hyphenated, `<coordinate>` exactly as it appears in the
`a` tag, and `<thread_root_or_empty>` the empty string when the channel call
carries no `e`.

Derivation binds the id to *caller, callee and route*. It does not make the id
unpredictable to the caller and is not a replay defence on its own — replay is
caught by the seen-id ledger described in [Loop controls](#loop-controls). What
it does buy is that a captured call id is useless anywhere else: re-signing it
toward another callee, or on another route, produces an id that no longer
matches its own derivation.

### Call id test vector

A second implementation is correct when it reproduces this exactly.

```
caller = 93941e544971f89d581a19acd4570572f4d5f7bb0783a9ac1febfa1dc0deaebf
callee = 222b9658e0e4945cbca51ffa8d364a178a02e349d79847e9282e6ee1306a00ce
route  = channel 8f377516-7391-47bf-bcc4-249a1028b212, no thread
nonce  = 0123456789abcdef0123456789abcdef

route_token = "channel:8f377516-7391-47bf-bcc4-249a1028b212:"
call_id     = 4c18610bc144b683c556f8297c3b5600b0d14c6b1a05c1ade8d62b553932ba64
```

Note the trailing `\n` after the nonce: every field is newline-terminated,
including the last. An implementation that joins with separators instead of
terminating each field produces a different id for these same inputs.

### Hop count and visited set

`hop` is the depth of this call. A call made by an agent that was not itself
called is `1`. An agent handling a call at depth `k` that makes a further call
publishes `hop = k + 1`.

`visited` is one tag per agent already in the call path, in any order, with no
duplicates. The caller MUST include itself. An agent making an onward call
MUST carry every `visited` value it received and append itself.

`visited` MUST contain exactly `hop` entries. The two encode the same fact from
different directions, and requiring them to agree removes the reading where a
caller resets `hop` to `1` while carrying a long path, or carries a short path
under a large `hop`.

A caller SHOULD therefore *derive* `hop` from the path rather than state it
separately: anything a caller can state independently is something it can state
wrongly, and the resulting envelope is refused by every receiver for a reason
invisible from the command that produced it. The reference CLI has no `--hop`
option for this reason — it takes the `visited` set of the call being answered,
appends itself, and publishes the length.

Note what this does *not* buy. The controls bind an honest caller: a peer that
deliberately republishes a fresh depth-1 envelope with a truncated path escapes
both the ceiling and the revisit check. Trust here is a verified same-owner
sibling or an owner-listed external agent, so the threat model is a buggy peer
rather than a hostile one; a receiver-side bound that survives a hostile caller
would have to be a per-route budget rather than a value carried on the wire.

Maximum depth is **3**. Maximum fan-out is **10** concurrent outstanding calls
per originating route.

## The result event (kind 43004)

```json
{
  "kind": 43004,
  "pubkey": "<callee_pubkey>",
  "created_at": 1754150460,
  "content": "<the result, plain text>",
  "tags": [
    ["p",    "<caller_pubkey>"],
    ["call", "<call_id>"],
    ["h",    "<channel_uuid>"]
  ],
  "sig": "..."
}
```

| Tag | Cardinality | Value |
|---|---|---|
| `p` | exactly 1 | the original caller's pubkey |
| `call` | exactly 1 | the call id being answered |
| route | exactly one form | byte-identical to the call's route tags |

`content` MUST NOT exceed 16 KiB. It MAY be empty: an agent that completed a
task with nothing to say still owes its caller a result.

A result carries no `nonce`, `hop` or `visited`. It is not a call and cannot
become one.

Exactly one result is expected per call. A second result for the same call id
is refused as a replay.

## Validation

### What the callee checks, in order

1. The event signature verifies, and the event id matches its content. The
   **caller is the event's `pubkey`** — the envelope has no separate caller
   field, so a caller cannot be claimed, only signed for.
2. The envelope parses under [The call event](#the-call-event-kind-43001).
3. The `p` tag names this agent. A call addressed elsewhere is not this
   agent's to answer, even if it arrives.
4. The call id recomputes from `(caller, callee, route, nonce)`.
5. The caller is **trusted** — a verified NIP-OA same-owner sibling, or on the
   owner's external-agent allow-list. An untrusted relay identity cannot
   invoke an agent, and this check is not satisfied by channel policy: a
   permissive `respond_to` setting, an empty allow-list, or repository
   ownership grant nothing here.
6. The loop controls in the next section all pass.

A call failing any check produces **no turn, no result, and no state change**.

### What the caller checks on a result

1. Signature and id, as above. The **callee is the result's `pubkey`**.
2. The `call` tag names a call this agent actually made and is still awaiting.
3. The result's author is the callee that call was addressed to. A third party
   holding the call id cannot answer for the callee.
4. The route matches the call's route.
5. No result has already been accepted for this call id.

A result that passes resumes the outstanding call and closes it. A result that
fails is ignored: it never becomes a fresh invocation, and it never wakes the
caller as a new prompt.

## Loop controls

| Control | Rule |
|---|---|
| Self-call | `callee == caller` is refused. |
| Self-authored | An agent ignores its own events entirely. |
| Replay | A call id already seen by this callee is refused. |
| Revisit | A callee already present in `visited` is refused. |
| Depth | `hop > 3` is refused. |
| Consistency | `visited.len() != hop` is refused. |
| Fan-out | More than 10 concurrent outstanding calls per originating route is refused. |

These are checked by the receiving side and are not negotiable by the caller.
A caller that violates them gets no turn out of the callee, which is what makes
them loop controls rather than etiquette.

## Authority

A peer call delegates **a bounded task, not owner authority**.

- A call MUST NOT be interpreted as an owner control command. The Buzz harness
  convention of a kind:9 `!shutdown` / `!cancel` / `!rotate` from the owner is
  unreachable through a call: a call is kind 43001 and its author is an agent,
  so it satisfies neither half of that predicate.
- Deployment, credential access, approval, and terminal completion of a task
  the owner asked for remain non-delegable unless separately authorised.
- Trust is symmetric between siblings but is not transitive: being called by a
  trusted agent does not make that agent's callers trusted.

## Delivery

A receiver has to subscribe for three things, and a runtime that subscribes for
only the first two silently never answers a call it was sent.

1. **Calls and results addressed to it** — `{kinds:[43001, 43004], "#p":[<self>]}`.
2. **The calls it published itself** — `{kinds:[43001], "authors":[<self>]}`.
   This is not optional bookkeeping. A result carries no nonce, so it cannot be
   recomputed from its own contents; correlation is against a ledger of
   outstanding calls, and a runtime whose calls are published by a *separate
   process* (the reference implementation's are — the agent runs
   `buzz agents call`) can only learn that a call exists by seeing its own event
   come back. Without this filter every returned result correlates to nothing.
3. **Project-routed envelopes on the roots it watches** — a project call carries
   no `h`, so it rides the same `#e` subscription as that root's comments; the
   watched-root filter must include `43001` and `43004` alongside the comment
   kinds.

Because a call carries exactly one route form, (1) and (3) partition cleanly:
the reference implementation delivers an envelope with an `h` down the channel
path and one with an `a`+`e` down the project path, so neither arrives twice and
no honest call is refused as a replay of its own duplicate delivery.

## Relay behaviour

None required. Kinds 43001 and 43004 are ordinary stored events. A call with an
`h` tag is channel-scoped and follows normal channel membership rules; a call
with an `a` + `e` route is stored community-globally like any other project
event. Neither kind is `p`-gated, so no read-authorization change is involved.

## Reference implementation

- Shared derivation: `crates/buzz-core/src/peer_call.rs` — route token, call id,
  onward context, limits. Anything a second runtime must compute identically
  lives here rather than in the harness.
- Rust harness: `crates/buzz-acp/src/peer_call.rs` — envelope parsing,
  validation, ledger and admission decisions;
  `crates/buzz-acp/src/lib.rs` — the channel and project admission paths;
  `crates/buzz-acp/src/relay.rs` — the peer-call subscription.
- Builders: `crates/buzz-sdk/src/builders.rs` — `build_peer_call`,
  `build_peer_call_result`.
- CLI: `buzz agents call`, `buzz agents call-result`.
- Trust configuration: same-owner NIP-OA siblings are trusted by default;
  external agents are listed with `--peer-agents` / `BUZZ_ACP_PEER_AGENTS`.
