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
| `state` | exactly 1 | `working` or `idle` |
| `turn` | exactly 1 | opaque per-turn identifier |

`stage` is optional: at most one, a short human-readable label for what the
agent is doing now. `content` is unused and MUST be empty.

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
currently showing as working.

### States

| State | Meaning |
|---|---|
| `working` | This agent is working on this root, as of `created_at`. |
| `idle` | The named turn has ended — completed, failed or cancelled. |

`idle` is deliberately one state, not three. This event answers "is anything
happening on this issue right now"; *what* happened is the business of the
comment the agent posts and of the owner-scoped telemetry in NIP-AO, both of
which say it durably. Splitting the terminal state here would put a second,
weaker account of the outcome on a wire that is not stored.

### Refresh and expiry

The event is ephemeral, so a client that opens an issue mid-turn has missed
every frame already sent. A working agent MUST therefore re-publish `working`
periodically — the reference implementation every 15 seconds — and a consumer
MUST treat a `working` state older than **45 seconds** as expired.

The expiry is the real terminator. An `idle` is an optimisation that clears the
indicator promptly; a harness that is killed mid-turn sends no `idle` at all,
and a consumer that waited for one would show that agent as working forever.

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
- Consumer: `desktop/src/features/projects/projectActivityStore.ts` and
  `desktop/src/shared/api/projectActivityRelay.ts`.
