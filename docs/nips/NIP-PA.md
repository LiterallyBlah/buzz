NIP-PA
======

Project Activity
----------------

`draft` `optional` `agent`

This NIP defines how an agent signals, live, that it is working on a git issue
or pull request, and how a client shows that on the root the work belongs to.

## Motivation

Buzz already has an activity signal, and it is channel-shaped in three
independent places. The typing indicator ([`kind:20002`]) carries an `h` tag and
is routed by the relay into a channel topic. The agent observer frame
(`kind:24200`, [NIP-AO](NIP-AO.md)) carries the channel as a UUID *inside its
encrypted payload*, and every desktop store that consumes it is keyed by that
UUID. Neither can express "on this issue".

The result is not a missing feature but a wrong one. A project turn runs under
a route key that is a UUIDv5 of the root — a UUID that names no channel — so the
harness emitted observer frames claiming a `channelId` nothing could resolve.
The desktop dutifully created a turn under it, reported an agent as working in a
channel that does not exist, and rendered the activity panel empty for the issue
that actually caused the work.

So this is a separate kind rather than an `h` tag on an existing one. An issue
is not a channel; giving it a synthetic channel id makes every channel-keyed
consumer subtly wrong instead of visibly silent, which is the harder failure to
find.

## The activity event (kind 20003)

```json
{
  "kind": 20003,
  "pubkey": "<agent_pubkey>",
  "created_at": 1754150400,
  "content": "",
  "tags": [
    ["a",     "30617:<owner_pubkey>:<identifier>"],
    ["e",     "<root_event_id>", "", "root"],
    ["agent", "<agent_pubkey>"],
    ["state", "working"],
    ["turn",  "<turn_id>"],
    ["stage", "reading files"]
  ],
  "sig": "..."
}
```

Ephemeral (20000–29999): relays fan it out and never store it.

### Required tags

| Tag | Cardinality | Value |
|---|---|---|
| `a` | exactly 1 | `30617:<owner>:<identifier>` repository coordinate |
| `e` | exactly 1 | issue/PR root event id, marked `root` |
| `agent` | exactly 1 | the working agent's pubkey, 64 lowercase hex |
| `state` | exactly 1 | `queued`, `working` or `idle` |
| `turn` | exactly 1 | opaque per-turn identifier |

`stage` is optional: at most one, a short human-readable label for what the
agent is doing now. `content` is unused and MUST be empty.

A publisher SHOULD take `stage` from what the agent itself says it is doing —
in the reference implementation, the `title` of the ACP `session/update`
`tool_call` the agent sent, falling back to a label derived from that update's
`kind` when it carries no title. It MUST NOT be derived from the direction of
transport traffic: "the harness read a line from the agent" is not "the agent is
reading files", and a caption built that way is true only by coincidence.

**Command execution is the exception, and it is a MUST.** For a tool call of
`kind` `execute` the title is not a description of the work — it is the command
line. A publisher MUST NOT put it on the wire. This event is unencrypted and
readable by everyone who can read the issue, so a quoted command line discloses
absolute paths, environment variable names, hostnames and flags from a machine
nobody chose to publish by filing an issue; it is also unreadable, since the
caption renders as one short line and a wrapped invocation truncates to noise.
The reference implementation publishes `running a command`, optionally suffixed
with the command's first token when that token is a bare program name — no
path separator, no `=`, at most 20 characters, and not a wrapper such as `env`
or `sudo` — giving `running a command (cargo)`. Anything else gets the bare
label.

Because the label otherwise originates with the agent it is free text, and it is
published to everyone who can read the issue. A publisher MUST flatten it to one
line — whitespace runs collapsed, control characters removed — and bound its
length; the reference implementation caps it at 80 characters. A label left with
nothing in it after flattening MUST be omitted rather than published blank.

**There is no `h`.** A receiver MUST refuse an event carrying one rather than
guessing which binding wins — an activity event that names both a channel and a
root names two different places for one signal to appear.

`agent` MUST equal the event's `pubkey`. It is carried as a tag anyway so a
consumer can filter by `#agent` without reading authorship, and a mismatch is a
refusal rather than a preference for one of the two.

### `turn`

`turn` is what makes `idle` safe. Without it, a late `idle` from a turn that
finished ten seconds ago clears the indicator for the turn that started two
seconds ago, and the agent appears idle while it is working.

A consumer MUST therefore ignore an `idle` whose `turn` is not the turn it is
currently showing.

A `queued` frame is the one state announced before any turn exists, so it has no
turn id to carry. It names **the event that caused the queueing** instead, as
`queued:<event_id>`. This is still opaque to a consumer and still unique; what
it must not do is expect a later frame to reuse it. The `working` that follows
carries the real turn id and supersedes the queued frame *by root*, not by turn.

### States

| State | Meaning |
|---|---|
| `queued` | This agent has accepted work on this root that has not started. |
| `working` | This agent is working on this root, as of `created_at`. |
| `idle` | The named announcement has ended — completed, failed or cancelled. |

`queued` exists because the alternative to it is not a smaller signal but an
ambiguous one. Between the comment reaching the relay and the first `working`,
an agent that had picked the comment up and an agent that was never addressed
produced the identical wire — nothing — and that interval is as long as the pool
is busy, routinely minutes. With `queued` published at dispatch, silence means
one thing: nobody was addressed.

It is a state rather than a `working` with a `stage` of "queued" because it is a
different claim. `stage` is presentation — a label for work in progress — and
nothing is in progress yet. A consumer that treated the two alike would show a
progress indicator for an agent that has not started.

`idle` is deliberately one state, not three. This event answers "is anything
happening on this issue right now"; *what* happened is the business of the
comment the agent posts and of the owner-scoped telemetry in NIP-AO, both of
which say it durably. Splitting the terminal state here would put a second,
weaker account of the outcome on a wire that is not stored.

A consumer MUST NOT let a `queued` displace a `working` from the same agent on
the same root that is not older than it. The two belong to different `turn`s by
construction, so the ordinary same-turn ordering check cannot pair them, and
`created_at` is whole seconds — the queued frame and the turn's first `working`
routinely share one. Delivered in the wrong order, an unguarded consumer walks
the indicator backwards from "working" to "queued" for a full refresh cycle.

### Refresh and expiry

The event is ephemeral, so a client that opens an issue mid-turn has missed
every frame already sent. An agent MUST therefore re-publish its current state
periodically — the reference implementation every 15 seconds — and a consumer
MUST treat any state older than **45 seconds** as expired.

`queued` refreshes on the same cadence as `working`, and that is deliberate. The
cheaper alternative — announce it once and let the 45-second expiry cap it —
fails the only case the state exists for: a comment waits precisely when the
pool is busy, which is routinely longer than 45 seconds, so the issue would fall
back into silence while the work was still genuinely pending. That would move
the ambiguity one refresh cycle later rather than remove it.

The expiry is the real terminator. An `idle` is an optimisation that clears the
indicator promptly; a harness that is killed mid-turn sends no `idle` at all,
and a consumer that waited for one would show that agent as working forever.

A publisher MUST NOT refresh `queued` for a root whose queue has drained. In the
reference implementation any terminal frame on the root retires a still-`queued`
announcement, under that announcement's own `turn` — an `idle` naming the
turn that just ended would be ignored by every consumer, by the rule above.
This is what bounds an indefinitely refreshed state.

## Consumers

Subscribe per root:

```json
{"kinds": [20003], "#e": ["<root_event_id>"]}
```

A client MUST scope by the root it is displaying. Filtering on `#a` alone shows
every issue in the repository as busy whenever any one of them is.

## Relation to NIP-AO

They are different signals and both are wanted.

- **NIP-PA** is public-to-the-project, unencrypted, and answers "is an agent
  working on this root". Anyone who can read the issue can see it.
- **NIP-AO** is owner-scoped, encrypted, and carries the transcript. It answers
  "what exactly is my agent doing".

A NIP-AO frame from a project turn carries the project route
(`project.coordinate`, `project.root`) in its payload and leaves `channelId`
null, so the owner's raw-activity panel can scope to a root by the same key this
NIP publishes. That is the fix to the empty panel: the payload stops claiming a
channel it does not have.

## Relay behaviour

None required. Kind 20003 is an ordinary channel-less ephemeral event: it has no
`h`, so it takes the relay's existing global ephemeral path (published to
`EventTopic::Global`, fanned out through the global subscriber index) exactly as
NIP-AB pairing events do. It is not `p`-gated and adds no read-authorization
rule.

## Reference implementation

- Kind: `crates/buzz-core/src/kind.rs` — `KIND_PROJECT_ACTIVITY`.
- Builder: `crates/buzz-sdk/src/builders.rs` — `build_project_activity`.
- Emission: `crates/buzz-acp/src/lib.rs` — the observer publisher projects a
  project-routed turn's lifecycle onto this wire; `crates/buzz-acp/src/pool.rs`
  binds the route from the flushed batch's project origin.
- `stage`: also `crates/buzz-acp/src/lib.rs` — `ProjectActivityPublisher::stage_for`
  reads the ACP `session/update` out of the `acp_read` frame the harness already
  puts on the observer bus, which is the same payload the desktop transcript
  renders. It is protocol data, so every compliant agent captions its own work
  and no branch anywhere asks which harness is on the other end.
- `queued`: also `crates/buzz-acp/src/lib.rs` — the dispatch gate emits a
  synthetic `project_event_queued` frame onto the same in-process observer bus
  the moment the queue accepts the event, so the publisher stays the single
  authority on what a root is announcing. A gate with its own publisher would be
  a second opinion about which root is live, and the two would disagree the
  first time either missed a frame.
- Consumer: `desktop/src/features/projects/projectAgentActivity.ts` and
  `desktop/src/shared/api/projectActivityRelay.ts`.
