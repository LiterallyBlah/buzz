//! Project (issue / pull-request) routing primitives.
//!
//! Channel routing keys off the subscription id (`ch-<uuid>`) because a channel
//! *is* a UUID. A project root is a 64-char event id, so project routing keys
//! off the event's own root reference instead, mapped through a deterministic
//! UUIDv5 so every downstream mechanism (session isolation, queueing, dedup,
//! turn counts, backpressure, cancellation) keeps working untouched.
//!
//! **No longer inert.** An earlier version of this note said nothing here
//! opened a subscription or fired a turn, and that only the tests called into
//! the module. Production now uses it: `ProjectRequests` owns the relay's
//! project request lifecycle, `VerifiedProjectEvent` and `VerifiedAnnouncement`
//! gate inbound frames, and the run loop ingests discovery through
//! `DiscoveredRepositories`. What is still inert is the *authority* vocabulary —
//! enrolment, lifecycle and invocation classification — which has no production
//! caller yet.
//!
//! It remains the shared vocabulary that the Hermes Buzz adapter must
//! reimplement byte-for-byte. Where a rule is a cross-runtime invariant it is
//! called out as such.

// Narrow this as the driver consumes each primitive. A module-wide allowance
// hides exactly the question worth asking — whether a piece said to have landed
// has any production caller — so it is a debt, not a decision. Removing it
// entirely needs the authority and reconstruction drivers, which do not exist.
#![allow(dead_code)]

use std::collections::{BTreeMap, BTreeSet};

use serde_json::{json, Value};
use uuid::Uuid;

use buzz_core::kind::{
    KIND_GIT_ISSUE, KIND_GIT_PR_UPDATE, KIND_GIT_PULL_REQUEST, KIND_GIT_REPO_ANNOUNCEMENT,
    KIND_GIT_STATUS_CLOSED, KIND_GIT_STATUS_DRAFT, KIND_GIT_STATUS_MERGED, KIND_GIT_STATUS_OPEN,
    KIND_TEXT_NOTE,
};
use buzz_core::peer_call::{KIND_PEER_CALL, KIND_PEER_CALL_RESULT};

// ── Route key ─────────────────────────────────────────────────────────────────

/// UUIDv5 namespace for project route keys.
///
/// **Cross-runtime invariant.** This constant is copied verbatim into the
/// Hermes Buzz adapter. If the two ever diverge, the same issue maps to two
/// different sessions in Rust and Python and each runtime silently believes it
/// owns the conversation.
pub(crate) const PROJECT_ROUTE_NAMESPACE: Uuid =
    Uuid::from_u128(0x0a01_70ea_22c2_5606_8679_6c72_e92c_1942);

/// Prefix for every project subscription id, mirroring `ch-` for channels.
pub(crate) const PROJECT_SUB_ID_PREFIX: &str = "proj-";

/// Subscription id for the enrolment REQ (`#a` + `#p`): events that tag this
/// agent on a known project.
pub(crate) const PROJECT_ENROL_SUB_ID: &str = "proj-enrol";

/// Subscription id prefix for watched-root REQ generations. The generation
/// suffix is what lets a replacement run overlapping with its predecessor.
pub(crate) const PROJECT_ROOTS_SUB_ID: &str = "proj-roots-0";

/// Which project subscription an id names.
///
/// A parsed class rather than a `starts_with("proj-")` test. The prefix check
/// was fine with two subscriptions and stops being fine the moment discovery,
/// enrolment, watched generations and root catch-up share it: an unrecognised
/// `proj-…` would slide into whichever branch happened to be first. Unknown ids
/// are refused rather than guessed.
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize)]
pub(crate) enum ProjectSubscription {
    /// `kind:30617` announcements. Produces discovery state, **not** a root
    /// route — an announcement has no root and must never be pushed through
    /// [`ProjectRoute::derive`] to be quietly dropped.
    Discovery,
    /// `#a` + `#p`: events that tag this agent on a known project.
    ///
    /// **The live tail.** Its filter floors at startup and asks for no history,
    /// so a frame arriving under this class is news by construction — which is
    /// why nothing reclassifies it after the fact. The class of the request
    /// that delivered a frame *is* the answer to "replay or live", and there is
    /// exactly one producer of each answer.
    Enrolment,
    /// `#e` / `#E` over enrolled roots, one generation per REQ replacement.
    Watched { generation: u64 },
    /// The NIP-PC peer-call REQ, carrying a project-routed envelope.
    ///
    /// A transport source, not a grant. It exists so a call that arrives before
    /// its root is enrolled — or during a watched-REQ replacement — reaches the
    /// project gate at all, rather than being dropped by both paths. Every
    /// authority question is still asked downstream, and the answer does not
    /// depend on which subscription carried the event.
    ///
    /// Deliberately distinct from `Watched`: naming a generation this frame
    /// never had would put a false provenance into the record that decides
    /// which REQ to retire.
    PeerCall,
    /// A generated, exhaustible **historical page of the enrolment question**.
    ///
    /// Frames on it are always replay: the request exists only to reconstruct
    /// roots that predate this process, and it retires when its own page proves
    /// exhausted. One generation per page, so a predecessor's boundary names a
    /// retired id and can neither certify nor retire its successor.
    ///
    /// This is what replaces classifying enrolment frames by the author's
    /// clock. Author time is evidence, not provenance: the relay admits ±15
    /// minutes of drift, so a genuinely live root could carry a `created_at`
    /// before startup and be enrolled without ever being answered. A request
    /// that only ever carries history cannot make that mistake, because the
    /// question it asked was a historical one.
    EnrolmentHistory { generation: u64 },
    /// Historical reconstruction for one newly enrolled root.
    ///
    /// The full root id is carried in the id, not a prefix, so the arriving
    /// route can be bound to it exactly. That makes the id 77 characters;
    /// this relay advertises `MAX_SUB_ID_LENGTH = 256`
    /// (`buzz-relay/src/protocol.rs:9`), so it is accepted. NIP-01's
    /// conventional cap is 64, so a stricter relay would reject it.
    ///
    /// Truncating the root is **not** the answer if that day comes — it would
    /// turn an exact binding into a prefix comparison. Carry a short opaque
    /// token (or a UUIDv5) in the id and keep an internal exact
    /// `token -> full root` map, so `route.root() == expected_full_root`
    /// still holds.
    RootCatchUp {
        root: String,
        /// Which exhaustible stream this catch-up is for.
        ///
        /// A pull request requires two, and they are different questions with
        /// different filters. Without this the registry sees one class per
        /// root, so opening the second stream conflicts with the first — the
        /// owner could enumerate both and the registry could open neither.
        stream: HistoryStream,
    },
}

/// Which project subscription a replacement command targets.
///
/// Deliberately **not** [`ProjectSubscription`]. That type carries the watched
/// generation, and a caller able to supply a generation is a caller able to
/// supply a stale one — which is exactly how a generation the registry had
/// never installed came to be named as a predecessor. Callers name the class;
/// the registry stamps the generation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ProjectReplacement {
    /// `#a` + `#p` over the discovered set. Fixed id.
    Enrolment,
    /// `#e` / `#E` over the enrolled roots. One generation per replacement.
    Watched,
}

pub(crate) use requests::{
    AuthorityVerdict, CatchUpFrame, CatchUpOutcome, EndOfStoredEvents, FrameAdmission,
    IntentAdmission, OpenOutcome, OpenedHistoryPage, ProjectRequests, ReplayableRequest,
};

/// The pure validator the durable-record proofs go through, and the allocator
/// whose ceiling they exercise. Neither can install anything: the validator
/// returns a description and the counter has no route into a record.
#[cfg(test)]
use requests::{plan, validate_persisted_document, AllocatorState, CheckedCounter, CurrentIntent};

/// Named by the replacement wire tests today; the run loop consumes it when
/// dispatch moves behind the narrow replacement capability. Scoped to this one
/// export rather than allowed module-wide, so a genuinely dead export still
/// surfaces.
#[allow(unused_imports)]
pub(crate) use requests::ReplaceOutcome;

/// Named by the tests that open pages, and by the reconstruction driver when it
/// lands — nothing in production opens one yet. Scoped to this one export
/// rather than allowed module-wide, so a genuinely dead export still surfaces.
#[allow(unused_imports)]
pub(crate) use requests::PageOpen;

/// The socket a project REQ can actually be written to.
///
/// **Sealed on purpose.** The previous version took a caller-supplied
/// `FnOnce(String) -> Future<Output = Result<(), E>>`, which is `confirm_sent`
/// with a callback wrapped around it: any crate caller could pass
/// `|_| async { Ok(()) }` and manufacture send authority without a socket
/// existing. A generic success-returning callback is not provenance.
///
/// `Sealed` is private to this module and implemented for exactly one foreign
/// type — the live WebSocket sink — so there is no way for sibling code, a test
/// helper or a future refactor to introduce a second implementation. Injecting
/// a fake at *this* boundary would be injecting "the write succeeded" at the
/// authority boundary, which is the thing being prevented; a test that wants a
/// controllable socket injects a real paired one at the transport layer.
pub(crate) trait ProjectReqSink: sealed::Sealed {
    /// Write one already-serialised REQ frame.
    ///
    /// Takes finished text, never the filter: the registry serialises the REQ
    /// from the registration's own identity, so a caller cannot register one
    /// question and transmit another.
    fn write_project_req(
        &mut self,
        text: String,
    ) -> impl std::future::Future<Output = Result<(), String>>;

    /// Write one already-serialised CLOSE frame.
    ///
    /// Separate from [`Self::write_project_req`] so the two cannot be confused
    /// at a call site: a CLOSE that went out as a REQ would open an unbounded
    /// subscription, and a REQ that went out as a CLOSE would silently retire
    /// the request it was meant to install.
    fn write_project_close(
        &mut self,
        text: String,
    ) -> impl std::future::Future<Output = Result<(), String>>;
}

mod sealed {
    /// Private supertrait. Nothing outside this module can name it, so nothing
    /// outside can implement [`super::ProjectReqSink`].
    pub trait Sealed {}
}

impl sealed::Sealed
    for tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>
{
}

impl ProjectReqSink
    for tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>
{
    async fn write_project_req(&mut self, text: String) -> Result<(), String> {
        use futures_util::SinkExt;
        tokio::time::timeout(
            std::time::Duration::from_secs(PROJECT_REQ_SEND_TIMEOUT_SECS),
            self.send(tokio_tungstenite::tungstenite::Message::Text(text.into())),
        )
        .await
        .map_err(|_| "timed out writing project REQ".to_string())?
        .map_err(|e| format!("failed to write project REQ: {e}"))
    }

    async fn write_project_close(&mut self, text: String) -> Result<(), String> {
        use futures_util::SinkExt;
        tokio::time::timeout(
            std::time::Duration::from_secs(PROJECT_REQ_SEND_TIMEOUT_SECS),
            self.send(tokio_tungstenite::tungstenite::Message::Text(text.into())),
        )
        .await
        .map_err(|_| "timed out writing project CLOSE".to_string())?
        .map_err(|e| format!("failed to write project CLOSE: {e}"))
    }
}

/// Matches `WS_SEND_TIMEOUT_SECS` in `relay.rs`; duplicated rather than
/// exported because this module now owns the write and should not depend on
/// the relay module for a bound on its own operation.
const PROJECT_REQ_SEND_TIMEOUT_SECS: u64 = 10;

/// The registry of project REQs this agent has actually sent and not closed.
///
/// In a private module so `live` cannot be reached around: the whole value of
/// this type is that the only way a subscription id becomes acceptable is
/// `open_request` writing its REQ and *then* installing it.
mod requests {
    use super::{
        HistoryPageCollector, HistoryStream, ProjectReqSink, ProjectSubscription, ProposalDomain,
        VerifiedProjectEvent,
    };
    use serde_json::{json, Value};
    use std::collections::hash_map::Entry;
    use std::collections::HashMap;
    use std::sync::Arc;

    /// The complete identity of one project request: what class, and what
    /// question — all of it.
    ///
    /// The filters belong here, not only in the caller's bookkeeping. When the
    /// live registry stored `ProjectSubscription` alone it could not tell
    /// `Discovery` with filter A from `Discovery` with filter B, so a command
    /// carrying a different filter came back `AlreadyOpen` — no REQ sent, and
    /// filter B left behind as what the next connection would ask for.
    ///
    /// **A NIP-01 REQ carries one *or more* filters, ORed.** This held a single
    /// `Value`, which could not represent the request this crate's own builder
    /// produces: [`super::watched_roots_filters`] returns two filters, because
    /// a comment points at its root with lowercase `e` and a pull-request
    /// revision with uppercase `E`, and a single lowercase filter silently drops
    /// every PR revision. Handing that pair over as a JSON array would have
    /// serialised `["REQ", id, [a, b]]` — not the `["REQ", id, a, b]` the
    /// protocol asks for — and would have made [`Self::admits`] refuse every
    /// event, since the stored value was no longer an object. Only the fixture's
    /// hand-rebuilt single filter kept that hidden.
    ///
    /// **Every filter it holds constrains something.** An earlier revision made
    /// only the *collection* non-empty — the head filter is a field of its own
    /// rather than `filters[0]` — and called that a structural invariant. It was
    /// not: `["REQ", id, {}]` has a filter and is exactly as unbounded as
    /// `["REQ", id]`, because a filter's constraints are ANDed and an empty
    /// conjunction is satisfied by everything. `{"limit": 500}` is the same
    /// shape with one key, since a limit bounds how many rows the relay returns
    /// rather than which events qualify. Both would have installed live
    /// authority that admitted every event on the relay, and both are refused
    /// at construction — see [`constrains_events`].
    ///
    /// So construction is fallible and *every* route through it is. There is no
    /// second door: an infallible `new` for the single-filter case would be a
    /// constructor that skips the check, which is the shape this type exists to
    /// remove.
    #[derive(Debug, Clone, PartialEq)]
    pub(crate) struct ProjectRequestIdentity {
        subscription: ProjectSubscription,
        filter: Value,
        /// Further OR branches, in the order they go on the wire. Empty for
        /// every request that asks one question.
        alternatives: Vec<Value>,
    }

    impl ProjectRequestIdentity {
        /// A request that asks exactly one question.
        ///
        /// `None` when that question constrains nothing; see
        /// [`Self::from_filters`], which this is.
        fn new(subscription: ProjectSubscription, filter: Value) -> Option<Self> {
            Self::from_filters(subscription, vec![filter])
        }

        /// A request built from a filter list, in wire order.
        ///
        /// `None` for an empty list, and `None` for any filter that constrains
        /// no event. Both are the same failure: `["REQ", id]`, `["REQ", id, {}]`
        /// and `["REQ", id, {"limit": 500}]` are not narrower requests than a
        /// filtered one, they are unbounded ones. A caller whose builder produced
        /// nothing must send no REQ, not a REQ that matches everything — see
        /// [`super::watched_roots_filters`], which returns an empty vector when
        /// nothing is enrolled.
        ///
        /// The whole list is checked before anything is built, so a single bad
        /// branch refuses the request rather than silently widening it: one
        /// unbounded filter among several would admit everything through the OR
        /// regardless of how narrow its siblings were.
        /// **Private to this module, and that is the point.** The class is
        /// half of a request's identity; a caller able to name it is a caller
        /// able to name a watched generation, a discovery under the enrolment
        /// id, or a catch-up that would be recorded durably. Every route into
        /// this type now goes through a semantic [`ProjectRequests`] operation
        /// that stamps the class and the wire id from what the registry itself
        /// knows — see [`ProjectRequests::open_discovery`],
        /// [`ProjectRequests::record_discovery_intent`],
        /// [`ProjectRequests::replace_enrolment`],
        /// [`ProjectRequests::replace_watched`] and
        /// [`ProjectRequests::open_history_page`].
        fn from_filters(subscription: ProjectSubscription, filters: Vec<Value>) -> Option<Self> {
            if !bounded_filters(&filters) {
                return None;
            }
            let mut filters = filters.into_iter();
            let filter = filters.next()?;
            Some(Self {
                subscription,
                filter,
                alternatives: filters.collect(),
            })
        }

        pub(crate) fn subscription(&self) -> &ProjectSubscription {
            &self.subscription
        }

        /// Every filter this request carries, in wire order. Never empty.
        pub(crate) fn filters(&self) -> impl Iterator<Item = &Value> {
            std::iter::once(&self.filter).chain(self.alternatives.iter())
        }

        /// The REQ frame for this request under `sub_id`.
        ///
        /// The one place REQ bytes are shaped, so the frame that goes on the
        /// wire and the filters that admit its answers cannot describe different
        /// requests. Both openers serialise this; neither builds an array of
        /// its own.
        pub(crate) fn req_frame(&self, sub_id: &str) -> Value {
            let mut frame = vec![json!("REQ"), json!(sub_id)];
            frame.extend(self.filters().cloned());
            Value::Array(frame)
        }

        /// Does this event match the question this request actually asked?
        ///
        /// A relay chooses what to send under a subscription id; the filters are
        /// the only statement of what was *requested*. Without this check the
        /// live path admitted anything correctly signed that resolved to some
        /// root — so a relay could deliver an event for a root this agent never
        /// watched, on the watched subscription, and it would be promoted to a
        /// routed event and spend the project dedup slot on the way. "The class
        /// we recorded decides the handling" was already true; what the class
        /// did not decide was whether the event belongs to the request at all.
        ///
        /// **OR across filters, AND within one**, which is NIP-01's rule and
        /// therefore the relay's: an event satisfying *any one* filter is one
        /// the relay was entitled to send, and satisfying a filter means
        /// satisfying all of its constraints. Anything looser would admit an
        /// event that matched the kinds of one branch and the tags of another —
        /// a request this agent never sent.
        ///
        /// **Fail closed on anything unrecognised.** A key this does not
        /// understand returns `false` rather than being skipped: the filters
        /// here are built by this crate, so an unknown key means the two have
        /// drifted, and a matcher that ignores what it cannot check would
        /// silently widen every request that grew a constraint.
        pub(crate) fn admits(&self, event: &nostr::Event) -> bool {
            self.filters().any(|filter| filter_admits(filter, event))
        }
    }

    /// Would this filter list build a bounded request?
    ///
    /// **The same rule [`ProjectRequestIdentity::from_filters`] applies, named
    /// once.** It exists because the watched replacement has to decide whether
    /// filters are acceptable *before* it allocates the generation that would
    /// go into the class — and construction needs the class. Validating by
    /// constructing therefore meant burning first and refusing second, which
    /// spent a wire identity on a request that was never going to be sent.
    ///
    /// This is a predicate, not a second constructor. It builds nothing, so it
    /// is not the alternative door the fallible constructor exists to close:
    /// the only way to obtain a `ProjectRequestIdentity` is still
    /// `from_filters`, and `from_filters` asks this same question.
    pub(crate) fn bounded_filters(filters: &[Value]) -> bool {
        !filters.is_empty() && filters.iter().all(constrains_events)
    }

    /// Does this filter narrow the set of events a relay may return under it?
    ///
    /// The question is not "is it non-empty" but "does anything in it *select*".
    /// A filter's constraints are ANDed, so an empty object is an empty
    /// conjunction — satisfied by every event, which is why `["REQ", id, {}]`
    /// and `["REQ", id]` ask the relay for exactly the same thing.
    ///
    /// `limit` is excluded for the same reason it is accepted by
    /// [`constraint_admits`] without inspecting the event: it bounds how many
    /// rows the relay returns, not which events qualify. A filter holding only a
    /// limit is a request for the most recent `n` events on the relay.
    ///
    /// Anything that is not a JSON object refuses too. A `Value::Array` here
    /// would be the two-filters-in-one-element mistake, and a string or a number
    /// is not a filter at all.
    fn constrains_events(filter: &Value) -> bool {
        filter
            .as_object()
            .is_some_and(|constraints| constraints.keys().any(|key| key != "limit"))
    }

    /// One whole filter against one event: every constraint in it, or nothing.
    fn filter_admits(filter: &Value, event: &nostr::Event) -> bool {
        let Some(constraints) = filter.as_object() else {
            return false;
        };
        constraints
            .iter()
            .all(|(key, value)| constraint_admits(key, value, event))
    }

    /// One filter key against one event. `false` unless it is understood *and*
    /// satisfied.
    fn constraint_admits(key: &str, value: &Value, event: &nostr::Event) -> bool {
        match key {
            "kinds" => value.as_array().is_some_and(|kinds| {
                let kind = u64::from(event.kind.as_u16());
                kinds.iter().any(|k| k.as_u64() == Some(kind))
            }),
            "authors" => {
                let author = event.pubkey.to_hex();
                string_list(value).is_some_and(|authors| authors.contains(&author.as_str()))
            }
            "ids" => {
                let id = event.id.to_hex();
                string_list(value).is_some_and(|ids| ids.contains(&id.as_str()))
            }
            "since" => value
                .as_u64()
                .is_some_and(|since| event.created_at.as_secs() >= since),
            "until" => value
                .as_u64()
                .is_some_and(|until| event.created_at.as_secs() <= until),
            // A bound on how many rows the relay may return, not a statement
            // about any one of them. The page counts what arrives itself.
            "limit" => value.as_u64().is_some(),
            // `#e` and `#E` are different questions — a comment points at its
            // root with lowercase `e`, a pull-request revision with uppercase
            // `E` — so the tag name is compared exactly, never case-folded.
            tag if tag.len() == 2
                && tag.starts_with('#')
                && tag.as_bytes()[1].is_ascii_alphabetic() =>
            {
                let name = &tag[1..];
                let Some(wanted) = string_list(value) else {
                    return false;
                };
                event.tags.iter().any(|t| {
                    let parts = t.as_slice();
                    parts.len() >= 2 && parts[0] == name && wanted.contains(&parts[1].as_str())
                })
            }
            _ => false,
        }
    }

    /// A JSON array of strings, or `None` — including for an array holding
    /// anything else, which is a filter this code did not write.
    fn string_list(value: &Value) -> Option<Vec<&str>> {
        value
            .as_array()?
            .iter()
            .map(|v| v.as_str())
            .collect::<Option<Vec<&str>>>()
    }

    /// Which *instance* of a request this is.
    ///
    /// A persistent request's subscription id is deterministic and its filter
    /// is reused, so the same request re-sent on a new connection is
    /// indistinguishable from its predecessor by description alone. This is the
    /// part that is never reused: a fresh number on every registration,
    /// monotonic for the life of the process. A catch-up's wire id carries one
    /// of these numbers, which is what stops two page attempts sharing a name —
    /// but the number is the identity in both cases, and the id never is.
    ///
    /// It exists so a boundary can be attributed to the request that actually
    /// received it. Without it, an EOSE minted on a connection that then died
    /// is interchangeable with one from the replacement request — and the
    /// replacement is precisely the one that had to recover whatever the relay
    /// held while the connection was down.
    ///
    /// "Never reused" is enforced, not asserted. The counter is `checked_add`
    /// and the space is finite, so exhaustion is a state
    /// (`OpenOutcome::Exhausted`) rather than a wrap. A wrapping counter
    /// would silently hand a future request the authority of an ancient one,
    /// which is the precise failure this type exists to prevent — and it would
    /// do so only in release builds, where a debug panic could not warn
    /// anyone.
    #[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
    pub(crate) struct RequestIncarnation(u64);

    /// A collector proven to belong to one live catch-up registration.
    ///
    /// Fields are private to this module and there is no constructor besides
    /// [`ProjectRequests::open_history_page`]. Holding one is therefore
    /// evidence that a REQ was genuinely sent for exactly these page
    /// parameters, under a wire id minted for that one attempt — which is what
    /// makes a later completion claim checkable rather than merely asserted.
    #[derive(Debug)]
    pub(crate) struct OpenedHistoryPage {
        authority: Arc<RegistrationAuthority>,
        sub_id: String,
        collector: HistoryPageCollector,
    }

    impl OpenedHistoryPage {
        pub(crate) fn sub_id(&self) -> &str {
            &self.sub_id
        }

        pub(crate) fn incarnation(&self) -> RequestIncarnation {
            self.authority.incarnation
        }

        /// The upper time bound this page was opened against.
        pub(crate) fn until(&self) -> u64 {
            self.collector.until()
        }

        /// Which root and stream this page collects for.
        ///
        /// Read-only, and derived from the collector the registry bound. An
        /// owner holding several streams must *derive* which one a page belongs
        /// to rather than be told: an `attach(stream, page)` signature would put
        /// the caller back in the position of asserting a fact about authority
        /// it does not establish.
        pub(crate) fn scope(&self) -> &super::HistoryScope {
            self.collector.scope()
        }

        pub(crate) fn effective_limit(&self) -> usize {
            self.collector.effective_limit()
        }

        pub(crate) fn generation(&self) -> u64 {
            self.collector.generation()
        }

        pub(crate) fn proposal_domain(&self) -> &Arc<ProposalDomain> {
            self.collector.proposal_domain()
        }

        /// Feed one verified row that arrived on this page's subscription.
        ///
        /// Observation lives here, not on the bare collector, because the real
        /// order of events is: write the REQ, install the registration, open a
        /// page against it, *then* receive. A collector
        /// that could be filled first and bound afterwards would let arbitrary
        /// rows be laundered into a registration that had not yet been made
        /// when they arrived.
        pub(crate) fn observe(&mut self, verified: VerifiedProjectEvent) {
            self.collector.observe(verified);
        }

        /// Record a frame that arrived on this page but cannot be one of its
        /// rows.
        ///
        /// It still counts: the relay sent it under this request's limit, so
        /// leaving it out would make a saturated page read as exhausted.
        pub(crate) fn observe_unusable(&mut self, reason: &str) {
            self.collector.observe_malformed(reason);
        }

        /// Consume the binding and yield the collector back.
        ///
        /// Unpacking *destroys* the proof rather than manufacturing one, so
        /// this is not a forgery route: whatever a caller does with the
        /// collector afterwards, completing a page still requires a fresh
        /// `OpenedHistoryPage`, and only the registry mints those — one per
        /// registration.
        pub(crate) fn into_collector(self) -> HistoryPageCollector {
            self.collector
        }

        /// How this page's authority relates to a boundary's.
        pub(crate) fn verdict_for(&self, witness: &EndOfStoredEvents) -> AuthorityVerdict {
            self.authority.verdict_for(
                &witness.authority,
                self.asks_the_same_as(witness.subscription()),
            )
        }

        /// How this page's authority relates to the registration that admitted
        /// a frame.
        ///
        /// The same three-way answer [`Self::verdict_for`] gives a boundary,
        /// because it is the same question: did *this* request produce this, or
        /// an instance of it that has since been replaced?
        pub(crate) fn verdict_for_frame(&self, admission: &FrameAdmission) -> AuthorityVerdict {
            self.authority.verdict_for(
                &admission.authority,
                self.asks_the_same_as(admission.subscription()),
            )
        }

        /// Is that request asking this page's own question — same root, same
        /// stream — whichever attempt it was?
        fn asks_the_same_as(&self, subscription: &ProjectSubscription) -> bool {
            // Compared against what this page's own scope *would* register as,
            // so a scope added later cannot silently match a class it does not
            // belong to. The generation is the page's own, for the same reason.
            &self
                .collector
                .scope()
                .subscription(self.collector.generation())
                == subscription
        }
    }

    /// What one attempt to open a history page did.
    ///
    /// Replaces the binding-error enum. Those variants — not live, already
    /// bound, not a catch-up, wrong root, wrong page parameters — all described
    /// ways a caller-supplied registration and a caller-supplied collector
    /// could disagree. [`ProjectRequests::open_history_page`] derives the
    /// registration *from* the collector under an id it mints itself, so there
    /// are no longer two halves to disagree.
    #[derive(Debug)]
    pub(crate) enum PageOpen {
        /// The REQ reached the socket, the registration is installed under a
        /// wire id no other attempt will ever wear, and the page is bound to it.
        Opened(OpenedHistoryPage),
        /// The collector had already observed something. Rows that arrived
        /// before the registration existed cannot belong to it.
        NotPristine,
        /// The filter this page's own parameters imply constrains no event, so
        /// the REQ would have asked the relay for everything. Nothing burned,
        /// nothing written, nothing installed.
        ///
        /// Unreachable from `catch_up_filter`, which always names kinds and a
        /// root tag — the arm exists because identity construction is fallible
        /// for every caller, and an `expect` here would be this operation
        /// asserting a fact about a function it does not own.
        UnboundedFilter,
        /// The incarnation space is spent. Nothing written, nothing installed.
        Exhausted,
        /// The write failed. Nothing installed; the burned token stays burned.
        WriteFailed(String),
        /// The durable record does not resolve, so this registry acted on
        /// nothing: no intent, no incarnation, no registration, no byte. Carries
        /// the violation [`DurableRecord::derive_current`] reported.
        InvariantViolation(String),
    }

    /// A capability naming exactly one registration.
    ///
    /// **Identity is the allocation, never the contents.** The previous version
    /// compared `(sub_id, incarnation)` — two public-looking numbers — and two
    /// independently constructed registries both start counting at zero, so a
    /// boundary minted by one could complete a page opened by the other. Numbers
    /// drawn from separate domains cannot express "the same request instance".
    ///
    /// `registry` exists so that a *failed* identity check can still be
    /// classified: same registry and an older incarnation is a predecessor,
    /// anything else is a contradiction.
    #[derive(Debug)]
    pub(crate) struct RegistrationAuthority {
        registry: Arc<RegistryEpoch>,
        incarnation: RequestIncarnation,
    }

    impl RegistrationAuthority {
        /// `same_question` is whether the other side asked about the same root
        /// and stream — *not* whether it wore the same subscription id.
        ///
        /// It used to be the id. That was a proxy for "the same request,
        /// re-sent", and it only worked while a catch-up's id was reused by
        /// every page of a stream in turn. Now the id names one transport
        /// attempt, so two instances of the same request never share one and
        /// comparing ids would classify every predecessor as a contradiction —
        /// turning an ordinary late boundary into an abandoned root. The
        /// question the request asked is what survives re-sending.
        fn verdict_for(
            &self,
            other: &Arc<RegistrationAuthority>,
            same_question: bool,
        ) -> AuthorityVerdict {
            if std::ptr::eq(self as *const _, Arc::as_ptr(other)) {
                return AuthorityVerdict::SameRegistration;
            }
            // Only a boundary from this same registry, about this same
            // question, from a strictly *earlier* instance, is an ordinary late
            // predecessor. A later instance offered to an older page is an
            // impossible owner transition, another question is the owner having
            // crossed two pages over, and a foreign registry is not comparable
            // at all.
            if Arc::ptr_eq(&self.registry, &other.registry)
                && same_question
                && other.incarnation < self.incarnation
            {
                AuthorityVerdict::Predecessor
            } else {
                AuthorityVerdict::Contradiction
            }
        }
    }

    /// Distinguishes one registry's allocations from another's.
    #[derive(Debug, Default)]
    pub(crate) struct RegistryEpoch;

    /// How a boundary relates to the page it was offered to.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub(crate) enum AuthorityVerdict {
        /// Same registration instance. The boundary may complete the page.
        SameRegistration,
        /// A strictly earlier instance of the same request. Ordinary reconnect
        /// traffic: refuse, but leave the page usable.
        Predecessor,
        /// Anything else — another request, another registry, or a *later*
        /// instance offered to an older page.
        Contradiction,
    }

    /// One live registration.
    ///
    /// There is no `bound` flag. It used to record whether this registration
    /// had already handed out its page, because binding was a second operation
    /// a caller invoked against an id it chose. [`ProjectRequests::open_history_page`]
    /// mints the id, installs the registration and binds the page in one
    /// operation, so a registration that exists has already handed out its one
    /// page and a flag saying so answers a question nobody can ask. Keeping it
    /// would have been worse than useless: a field named `bound` that nothing
    /// reads reads as a guard that is still enforced.
    #[derive(Debug)]
    struct LiveRegistration {
        identity: ProjectRequestIdentity,
        authority: Arc<RegistrationAuthority>,
    }

    /// What [`ProjectRequests::open_discovery`] or
    /// [`ProjectRequests::open_replayed`] decided.
    ///
    /// `Sent` means the REQ reached the socket and the registration was then
    /// installed. Nothing else in the crate can produce it: there is no
    /// reservation to promote and no flag to set.
    #[derive(Debug, PartialEq)]
    pub(crate) enum OpenOutcome {
        /// Registered **and** written to the socket. The only state in which a
        /// page may be bound or a boundary minted.
        Sent,
        /// This exact request is already live; no second REQ was written.
        AlreadyLive,
        /// Refused — this id belongs to a different request. Nothing recorded.
        Conflict { held: Box<ProjectRequestIdentity> },
        /// The incarnation space is spent. No REQ written, nothing recorded.
        Exhausted,
        /// The write failed, so nothing was registered — there is no
        /// reservation to undo. Durable intent survives, because the intent is
        /// still what we want; the write is what failed.
        WriteFailed(String),
        /// The filters would not build a bounded request, so no identity was
        /// minted. Nothing written, nothing recorded.
        ///
        /// The refusal belongs to the owner rather than to its caller. When the
        /// caller built the identity it also decided this, one call earlier,
        /// and a decision made outside the owner is a decision a second caller
        /// can make differently.
        UnboundedFilters,
        /// The durable record does not resolve. Nothing recorded, nothing
        /// written, no incarnation burned — the refusal is decided before the
        /// preflight, so this id's own state is not even consulted.
        InvariantViolation(String),
    }

    /// One request a registry intends and would re-ask for on a connection.
    ///
    /// **A token, not a description.** Its fields are private to this module
    /// and [`ProjectRequests::replayable`] is its only producer, so the only
    /// way to hold one is to have asked a registry what *it* intends — out of
    /// a durable record that same call has just validated end to end. The
    /// reconnect path can therefore re-open a request without ever naming an
    /// id, a class or a generation, which is the thing that must not be
    /// nameable outside the owner.
    ///
    /// The id is readable because a log line and a retry decision need it. It
    /// is a `&str` out of a struct nobody else can build, which grants nothing:
    /// [`ProjectRequests::open_replayed`] consumes the token, not the string.
    #[derive(Debug)]
    pub(crate) struct ReplayableRequest {
        sub_id: String,
        identity: ProjectRequestIdentity,
    }

    impl ReplayableRequest {
        pub(crate) fn sub_id(&self) -> &str {
            &self.sub_id
        }
    }

    /// What [`ProjectRequests::replace_request`] decided.
    ///
    /// Distinct from [`OpenOutcome`] because replacement and opening differ on
    /// exactly one point: a replacement is *authorised* to change the identity
    /// held under an id, and an open is not. Sharing an outcome type would
    /// invite sharing the check that refuses it.
    #[derive(Debug, PartialEq)]
    pub(crate) enum ReplaceOutcome {
        /// The successor is live and written. `retired` names the predecessor
        /// whose live state and durable intent were removed, when replacement
        /// moved to a new id.
        Replaced { retired: Option<String> },
        /// The successor is byte-identical to what is already live. Nothing was
        /// written, because re-sending an identical REQ churns the relay's
        /// admission budget to arrive where it already is. **No generation is
        /// burned**: a request that did not change did not use up a wire
        /// identity.
        Unchanged,
        /// The filters would not build a bounded request, so no identity was
        /// minted. Nothing written, nothing installed, predecessor intact, and
        /// **no generation burned** — refusal is decided before allocation.
        ///
        /// This decision used to live in the relay command handler, *before*
        /// the registry was called — it warned and returned, so nothing
        /// downstream could observe it and the caller's own bookkeeping had
        /// already advanced. Refusing here makes "installs nothing, leaves the
        /// predecessor current" a fact a test can read.
        InvalidFilters,
        /// The watched generation space is spent. Nothing written, predecessor
        /// intact.
        WatchedGenerationExhausted,
        /// The incarnation space is spent. Nothing written, predecessor intact.
        ///
        /// Distinct from [`Self::WatchedGenerationExhausted`] because they are
        /// different ceilings reached by different allocators, and collapsing
        /// them made the operator-facing log wrong: incarnation exhaustion was
        /// reported as "project generations exhausted", which points an
        /// investigation at the wrong counter.
        RequestIncarnationExhausted,
        /// Durable intent holds more than one watched generation, so there is
        /// no single predecessor to retire.
        ///
        /// **Fail closed rather than choose.** Picking one would retire it and
        /// leave the other durable beside the successor — which is the original
        /// defect, arrived at from the other direction. Reaching this state
        /// means something installed a watched intent outside the semantic
        /// replacement owner; the string names what was found so the report
        /// identifies the intruder rather than merely the symptom.
        InvariantViolation(String),
        /// The successor write failed. **The predecessor is intact** — its live
        /// registration and its durable intent are exactly as they were, so the
        /// agent keeps answering on the subscription it already had.
        WriteFailed(String),
    }

    /// Current project state, as read out of durable intent.
    ///
    /// Not stored anywhere. It is the return of one validating walk, so the two
    /// facts a replacement needs cannot be answered from a tree that was found
    /// consistent for one of them and never checked for the other.
    #[derive(Debug, PartialEq, Eq)]
    pub(super) struct CurrentIntent {
        /// The generation whose durable intent is current, if any.
        pub(super) watched: Option<u64>,
        /// Whether enrolment intent is current under its fixed id.
        pub(super) enrolment: bool,
    }

    /// What [`ProjectRequests::record_discovery_intent`] decided.
    #[derive(Debug, PartialEq)]
    pub(crate) enum IntentAdmission {
        Recorded,
        AlreadyIntended,
        Conflict {
            held: Box<ProjectRequestIdentity>,
        },
        /// The filters would not build a bounded request. Nothing recorded —
        /// intent replays verbatim, so an unbounded filter admitted here is a
        /// REQ for everything on the relay written by the next connection.
        UnboundedFilters,
        /// The durable record does not resolve, so nothing was recorded into
        /// it. Stamping a canonical id makes one entry canonical; it cannot make
        /// the record it would join resolvable.
        InvariantViolation(String),
    }

    /// Proof that the relay reported end-of-stored-events for a request this
    /// agent actually has open on this connection.
    ///
    /// **Not constructible outside this module**, and inside it only by
    /// [`ProjectRequests::witness_end_of_stored_events`], which requires a live
    /// registration. That is the whole point: EOSE is the boundary a
    /// completion claim would rest on, and an EOSE nobody can trace to a
    /// request we sent is a relay assertion rather than evidence.
    ///
    /// What it claims is narrow — *this* request, on *this* connection,
    /// received an EOSE frame. It does not claim the pages that preceded it
    /// were retained, ordered, or complete; the cursor owns that, and a
    /// timeout, `CLOSED`, `NOTICE` or reconnect produces no witness at all
    /// because none of them reach this function.
    #[derive(Debug, Clone)]
    pub(crate) struct EndOfStoredEvents {
        sub_id: String,
        identity: ProjectRequestIdentity,
        /// The capability of the exact registration that received this
        /// boundary. Compared by allocation, so it cannot be reproduced by a
        /// different registry that happens to have reached the same count.
        authority: Arc<RegistrationAuthority>,
    }

    /// Two boundaries are the same boundary only if they name the same
    /// registration.
    ///
    /// Deliberately *not* derived. A structural comparison would call two
    /// witnesses equal whenever their id and filter matched — which is exactly
    /// the request-*description* equality this type exists to replace.
    impl PartialEq for EndOfStoredEvents {
        fn eq(&self, other: &Self) -> bool {
            Arc::ptr_eq(&self.authority, &other.authority)
        }
    }

    impl EndOfStoredEvents {
        pub(crate) fn sub_id(&self) -> &str {
            &self.sub_id
        }

        pub(crate) fn subscription(&self) -> &ProjectSubscription {
            self.identity.subscription()
        }

        /// Which registration received this boundary.
        ///
        /// Deliberately reachable by a consumer rather than kept inside the
        /// producer. A generation the reconstruction owner cannot see would
        /// decorate the problem rather than solve it: the owner is the thing
        /// that has to refuse a boundary belonging to a predecessor request.
        pub(crate) fn incarnation(&self) -> RequestIncarnation {
            self.authority.incarnation
        }
    }

    /// Proof that a frame arrived under a request this agent actually has open
    /// on this connection.
    ///
    /// The EVENT counterpart of [`EndOfStoredEvents`], minted only by
    /// [`ProjectRequests::admit_frame`] and for the same reason: a subscription
    /// id is a string the relay chose to echo back, so admitting a frame on the
    /// strength of its spelling is admitting it on the relay's word.
    ///
    /// What it adds beyond "we opened something under this id" is *which*
    /// registration. Everything else in this crate that routes by id alone was
    /// safe because its ids are one-per-request for the life of a connection;
    /// a catch-up's is not, because pagination re-asks under the same id.
    #[derive(Debug)]
    pub(crate) struct FrameAdmission {
        sub_id: String,
        identity: ProjectRequestIdentity,
        authority: Arc<RegistrationAuthority>,
    }

    impl FrameAdmission {
        pub(crate) fn sub_id(&self) -> &str {
            &self.sub_id
        }

        /// The class **we** recorded for this request, never one inferred from
        /// what arrived.
        pub(crate) fn subscription(&self) -> &ProjectSubscription {
            self.identity.subscription()
        }

        /// Does this event match the filter the admitting request sent?
        ///
        /// See [`ProjectRequestIdentity::admits`]. Reached through the
        /// admission rather than the registry so the filter consulted is the
        /// one belonging to the registration that admitted *this* frame, not
        /// whatever is live under the same id by the time the question is
        /// asked.
        pub(crate) fn admits(&self, event: &nostr::Event) -> bool {
            self.identity.admits(event)
        }

        /// Turn this admission into the frame's disposition on a catch-up page.
        ///
        /// Consuming, so one admitted frame produces exactly one delivery. A
        /// reusable admission would be a licence to feed any number of rows
        /// into a page on the strength of a single genuine frame — and a page's
        /// row count is precisely what decides whether its history is
        /// exhausted.
        pub(crate) fn catch_up(self, outcome: CatchUpOutcome) -> CatchUpFrame {
            CatchUpFrame {
                admission: self,
                outcome,
            }
        }
    }

    /// One frame that arrived on a live root catch-up request.
    ///
    /// **Every** admitted catch-up frame becomes one of these, including the
    /// ones that cannot become rows. A page counts what the relay returned in
    /// order to tell a saturated page from an exhausted one, so a frame the
    /// relay sent and this agent discarded before the page saw it does not lose
    /// one event — it makes the page read short, which ends the reconstruction
    /// early and calls the result complete.
    #[derive(Debug)]
    pub(crate) struct CatchUpFrame {
        admission: FrameAdmission,
        outcome: CatchUpOutcome,
    }

    /// What a catch-up frame turned out to be.
    #[derive(Debug)]
    pub(crate) enum CatchUpOutcome {
        /// A verified event for the root this request asked about.
        ///
        /// Boxed only for size: an event dwarfs the two `&'static str` arms, and
        /// every frame this agent refuses would otherwise carry an event's worth
        /// of unused stack.
        Row(Box<VerifiedProjectEvent>),
        /// The frame arrived on this request but cannot be one of its rows —
        /// it did not verify, resolves to no root, or names a root this request
        /// did not ask about.
        ///
        /// Carried rather than dropped so the page is poisoned instead of left
        /// short. The reason is this agent's own words about its own check; it
        /// never contains relay- or publisher-supplied text.
        Unusable(&'static str),
        /// The request ended without a boundary — `CLOSED`, refused, or torn
        /// down.
        ///
        /// The page is **released**, not poisoned. Nothing is wrong with the
        /// rows it did receive; what is missing is any way to prove there were
        /// no more, and the answer to that is to ask again from the same bound.
        /// Without this the page stays in flight forever: `pages_wanted` skips
        /// a stream that holds one, so a request the relay closed would stall
        /// its stream in silence rather than retry it.
        RequestLost(&'static str),
    }

    impl CatchUpFrame {
        pub(crate) fn sub_id(&self) -> &str {
            self.admission.sub_id()
        }

        pub(crate) fn subscription(&self) -> &ProjectSubscription {
            self.admission.subscription()
        }

        /// Split into the disposition and the authority that admitted it.
        ///
        /// Private to the crate's routing seam: the page owner needs both, and
        /// handing out the parts separately is what lets it check the authority
        /// *before* absorbing the row.
        pub(crate) fn into_parts(self) -> (FrameAdmission, CatchUpOutcome) {
            (self.admission, self.outcome)
        }
    }

    /// The single owner of project request state.
    ///
    /// **One owner, not three maps.** Durable intent, live registrations and
    /// relay suspensions were previously separate fields that callers updated
    /// in sequence, and every gap between those updates was a way for them to
    /// disagree: intent recorded before the registry refused, so a refusal was
    /// undone by the next connection's replay; a registry that could not see
    /// filters, so filter drift entered intent through an `AlreadyOpen`. Two
    /// private maps are perfectly capable of disagreeing with each other.
    ///
    /// The three now move together, behind operations that either fully
    /// succeed or change nothing:
    ///
    /// - `intent` is local policy. It outlives connections and only local
    ///   action removes it.
    /// - `live` is what has actually been asked on *this* connection. It is
    ///   what admits an inbound frame, and it is cleared on disconnect.
    /// - `suspended` records relay refusals for this connection, so a refused
    ///   request is not re-sent on the connection that refused it.
    #[derive(Debug, Default)]
    pub(crate) struct ProjectRequests {
        /// Everything a reconnect replays, and the allocation that entitles it
        /// to. See [`DurableRecord`].
        record: DurableRecord,
        live: HashMap<String, LiveRegistration>,
        suspended: HashMap<String, String>,
        /// This registry's own allocation, stamped into every authority it
        /// mints so another registry's proofs can never be mistaken for its.
        epoch: Arc<RegistryEpoch>,
        /// The enrolment tail's unfinished backlog, if it has one.
        ///
        /// See [`EnrolmentBacklog`]. `None` means every frame the tail delivers
        /// is live — either its backlog has drained, or no registration has
        /// claimed one.
        enrolment_backlog: Option<EnrolmentBacklog>,
    }

    /// The stored-events prefix of one enrolment-tail registration.
    ///
    /// The tail's wire id is fixed, so the id cannot say which instance of the
    /// request a frame or a boundary belongs to. The authority can: it is
    /// allocation identity, compared by pointer, and a replacement mints a new
    /// one. That is the whole reason this holds an `Arc<RegistrationAuthority>`
    /// rather than the id it was opened under, and it is what stops a
    /// predecessor's — or a dead connection's — EOSE from certifying the
    /// successor's backlog as drained.
    ///
    /// **There is no mode field.** One was tried: the idea was that a
    /// reconnect's backlog could be live, since nothing else covers work missed
    /// during an outage. It is wrong in the direction that matters. A backlog
    /// is whatever the relay had already stored when it answered, and this
    /// agent cannot tell a root it has already handled from one it has not by
    /// looking at a stored row — that is precisely what the walk, and the
    /// shared dedup slot, exist to know. A backlog is context. What follows the
    /// boundary is work.
    #[derive(Debug)]
    struct EnrolmentBacklog {
        /// The exact registration whose stored events are still arriving.
        authority: Arc<RegistrationAuthority>,
        /// Boundaries still owed to registrations that came before this one.
        ///
        /// **The fixed wire id makes an arriving EOSE anonymous.** A boundary
        /// carries only the id it answers, and the tail's id never changes, so
        /// the registry mints its witness from whatever is live *now* — the
        /// successor. Authority comparison cannot separate a predecessor's
        /// boundary from the successor's, because by the time either arrives
        /// they name the same registration. That is not a flaw in the
        /// comparison; it is the id carrying no information.
        ///
        /// Order does carry it. One connection is one TCP stream, and a relay
        /// answers `REQ`s in the order it read them, so the boundary owed to a
        /// request opened earlier arrives before the boundary owed to the one
        /// that replaced it — always, and exactly once each. Replacing a tail
        /// that had not yet drained therefore owes one boundary that is not
        /// this registration's, and counting them is exact rather than
        /// heuristic.
        ///
        /// Reset with the connection: a dead socket owes nothing, and a
        /// replayed tail starts its own prefix from scratch.
        pending_predecessors: usize,
    }

    /// The durable record a registry derives its current intent from.
    ///
    /// **A description of persisted state, and nothing else.** It holds no live
    /// registrations, no epoch, no connection and no way to mint an authority;
    /// a caller holding one holds no capability [`ProjectRequests`] has. Its
    /// only operation is [`Self::derive_current`], which reads and refuses and
    /// changes nothing.
    ///
    /// **Nothing outside this module can compose one.** There is no builder,
    /// no loader and no `cfg(test)` constructor: a record begins empty at
    /// [`ProjectRequests::new`] and is advanced only by that registry's own
    /// semantic operations. Three shapes have been removed to arrive here —
    /// five mutators that could renumber durable truth behind a live registry,
    /// two `cfg(test)` builders that composed intent for the real owner out of
    /// an id and a class a test chose, and a `restore`/`over` pair that did the
    /// same thing in production spelling, complete with a caller-chosen
    /// allocator position. Each made "the registry is the sole producer of
    /// installed-current truth" conditional, and the condition is what the
    /// defects lived in.
    ///
    /// So a record production cannot produce is now a record that cannot
    /// exist, and the fail-closed rule is proved where such a thing still can:
    /// [`validate_persisted_document`] judges serialised bytes by
    /// [`validate_members`], the same walk [`Self::derive_current`] makes, and
    /// returns a description rather than a record.
    ///
    /// A record is still **not** trusted for existing — every operation that
    /// could act on one validates the whole of it first, see
    /// [`Self::derive_current`] — but with one producer that gate now guards a
    /// state nothing can reach. It is kept as the rule's last line, not as a
    /// proof obligation.
    #[derive(Debug, Default, Clone)]
    pub(crate) struct DurableRecord {
        /// Local policy. It outlives connections, and only local action removes
        /// it.
        intent: HashMap<String, ProjectRequestIdentity>,
        /// The incarnation allocator. Only ever advances, and never wraps.
        ///
        /// Deliberately survives [`ProjectRequests::clear_connection`]: a
        /// reconnect must not restore the ability to mint authority the process
        /// has already spent.
        incarnations: CheckedCounter,
        /// The watched-generation allocator.
        ///
        /// **Allocator state, not current state.** Which generation is
        /// installed is not stored anywhere: it is read out of `intent` by
        /// [`Self::derive_current`], because durable intent is what a reconnect
        /// actually replays and a field beside it is a second copy that has to
        /// be kept in agreement. Two attempts at that agreement have failed so
        /// far — the run loop's copy, then this struct's.
        ///
        /// This counter is genuinely separate: an *attempt* may burn a
        /// generation without installing one. Reusing the number after a failed
        /// write would put a wire identity this process has already used back
        /// on the wire, which is the reuse hazard the whole checked allocator
        /// exists to prevent.
        watched_generations: CheckedCounter,
    }

    /// A counter that hands out each value once, and stops rather than wraps.
    ///
    /// **Extracted so its ceiling can be proved without a registry.** The
    /// arithmetic at the top of the space is the whole of what is interesting
    /// here — `u64::MAX` is handed out exactly once and every later call
    /// refuses — and reaching it by burning is not something a test can do.
    /// The previous shape let a proof *start* an allocator wherever it liked
    /// and hand that to the real owner, which is provenance a production
    /// operation could never have written. This type has no route into a
    /// [`DurableRecord`]: records are default-constructed and only the owner's
    /// own operations advance them, so [`Self::at`] can compose a saturated
    /// counter for a pure proof and cannot install one anywhere.
    #[derive(Debug, Default, Clone)]
    pub(super) struct CheckedCounter {
        /// The next value to hand out, meaningful only while not `spent`.
        next: u64,
        /// Set once the space is exhausted. Never cleared.
        spent: bool,
    }

    impl CheckedCounter {
        /// A counter positioned at `next`, for proofs about the arithmetic.
        ///
        /// Pure and non-installable: nothing accepts one of these, so a
        /// composed counter can be burned and asked what it did, and can reach
        /// no registry, record, registration or socket.
        #[cfg(test)]
        pub(super) fn at(next: u64) -> Self {
            Self { next, spent: false }
        }

        /// Take the next value, or `None` once the space is spent.
        ///
        /// `checked_add` and a sticky flag, because a wrapping counter would
        /// hand a future request the authority of an ancient one — in release
        /// builds only, where no debug panic could warn anyone. The last value
        /// is genuinely handed out: saturation is what makes `next` stop
        /// meaning anything, which is why the flag exists rather than a
        /// comparison against `u64::MAX`.
        pub(super) fn burn(&mut self) -> Option<u64> {
            if self.spent {
                return None;
            }
            let taken = self.next;
            match self.next.checked_add(1) {
                Some(next) => self.next = next,
                None => self.spent = true,
            }
            Some(taken)
        }

        /// The value this counter would hand out next.
        pub(super) fn next_value(&self) -> u64 {
            self.next
        }

        /// Whether every value has been handed out.
        pub(super) fn is_spent(&self) -> bool {
            self.spent
        }

        /// What this counter can still do, as a description rather than a
        /// counter.
        ///
        /// The preflight decisions take one of these: an operation needs to
        /// know whether an identity can still be minted, and nothing else about
        /// the allocator. A description can be written down in a proof; a
        /// counter handed to an owner is provenance a proof chose.
        pub(super) fn state(&self) -> AllocatorState {
            if self.spent {
                AllocatorState::Spent
            } else {
                AllocatorState::Available
            }
        }

        /// A counter recovered from a persisted pair, or `None` if that pair is
        /// a state no counter could be in.
        ///
        /// **`spent` is a fact about arithmetic, not a flag a writer may set.**
        /// [`Self::burn`] sets it in exactly one circumstance — `next` was
        /// `u64::MAX` and `checked_add` refused — so a spent counter always
        /// reads `u64::MAX`, and any other pairing describes a counter that
        /// cannot exist. Admitting one is not a harmless inconsistency: `spent`
        /// is what tells the provenance rule that `next` has stopped advancing,
        /// so `{ spent: true, next: 0 }` suppressed the check entirely and a
        /// document could then claim any generation it liked as issued. The
        /// pair is refused before any member is judged against it.
        #[cfg(test)]
        pub(super) fn from_persisted(next: u64, spent: bool) -> Option<Self> {
            if spent && next != u64::MAX {
                return None;
            }
            Some(Self { next, spent })
        }
    }

    /// What an allocator can still do — never how far along it is.
    ///
    /// The preflight decisions read this and no more, so a proof can describe
    /// "the space is spent" without composing an allocator, and describing it
    /// hands over nothing: there is no route from an `AllocatorState` back to a
    /// counter, a record or a registry.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub(super) enum AllocatorState {
        /// At least one value remains.
        Available,
        /// Every value has been handed out. Never reversible.
        Spent,
    }

    impl DurableRecord {
        /// Read the whole of current project state out of durable intent, or
        /// refuse.
        ///
        /// **One walk, all the invariants, shared by both replacements.** An
        /// enrolment replacement is blocked by a corrupt *watched* intent and
        /// vice versa, which is deliberate: durable intent is one record, and a
        /// registry willing to write into a record it has just found to be
        /// inconsistent is a registry that reconciles corruption by ignoring
        /// it.
        ///
        /// **Every entry, not merely the ones with a generation in them.** The
        /// rule is one rule: a class has exactly one id it may be recorded
        /// under, and an entry whose key is not that id is not a narrower
        /// record — it is an entry this owner never wrote. An earlier revision
        /// checked only watched and enrolment and said so in a comment, which
        /// read as coverage while leaving `Discovery` and `RootCatchUp` to fall
        /// through a catch-all: a discovery class under an arbitrary key passed
        /// every replacement and was replayed verbatim onto the next
        /// connection, and a catch-up page — which is never durable at all,
        /// because its filter carries a bound the cursor walks past — could sit
        /// in the record and be re-asked forever.
        ///
        /// The invariants, each of which is a way the id and the identity under
        /// it can disagree:
        ///
        /// - discovery intent lives under [`super::discovery_sub_id`] and
        ///   nothing else does;
        /// - enrolment intent lives under [`super::PROJECT_ENROL_SUB_ID`] and
        ///   nothing else does — an enrolment class under some other id would
        ///   be replaced without ever being retired, and a foreign class under
        ///   the enrolment id would be retired by an enrolment replacement that
        ///   never installed it;
        /// - a watched intent's key is `watched_sub_id(generation)` for the
        ///   generation its own class carries — the key is what goes on the
        ///   wire and the class is what admits inbound frames, so a pair that
        ///   disagrees asks one question and answers another;
        /// - a watched generation is one this registry's own allocator issued;
        /// - at most one watched intent — more than one has no single
        ///   predecessor, and choosing between them retires one and leaves the
        ///   other durable beside the successor;
        /// - a root catch-up is never durable, under any id.
        ///
        /// Duplicates need no separate check for the singleton classes: their
        /// canonical id is the map key, and a map holds one value per key. The
        /// only class that can appear twice is watched, under two different
        /// generations, and that is the count below.
        ///
        /// Nothing here mutates and nothing here writes.
        ///
        /// **Asked before every operation that could act on the record, not
        /// merely before the two replacements.** A record that fails is a
        /// record whose registry records no intent, allocates no identity,
        /// installs and retires no authority, replays nothing and writes no
        /// byte — and does all of that having changed nothing, because the
        /// question is asked before the first mutation rather than beside it.
        /// The gate is [`ProjectRequests::checked_current`]; its callers are
        /// [`ProjectRequests::record_discovery_intent`],
        /// [`ProjectRequests::open_request`] (so `open_discovery` and
        /// `open_replayed`), [`ProjectRequests::open_history_page`],
        /// [`ProjectRequests::current_watched`] and
        /// [`ProjectRequests::enrolment_current`] (so both replacements and
        /// both offline halves), and [`ProjectRequests::replayable`].
        /// [`ProjectRequests::replace_request`] is the one writer that does not
        /// ask again: it is private, and the predecessor id it takes can only
        /// have been derived from one of these walks.
        ///
        /// An earlier revision gated only the replacements and replay, which
        /// read as coverage: `record_discovery_intent` walked a record holding
        /// a discovery class under a foreign id, found the canonical id vacant,
        /// and wrote a second discovery entry into it — reconciling corruption
        /// by adding to it.
        ///
        /// What is deliberately **not** gated is the pair that only gives
        /// authority up: [`ProjectRequests::refuse_live`] and
        /// [`ProjectRequests::witness_end_of_stored_events`] remove a live
        /// registration this connection installed while the record was still
        /// valid. Refusing to honour a relay's `CLOSED` because durable intent
        /// is inconsistent would leave that registration admitting frames the
        /// relay has already refused, which is the fail-*open* direction. They
        /// touch no durable state, allocate nothing and write nothing.
        fn derive_current(&self) -> Result<CurrentIntent, String> {
            validate_members(
                self.intent
                    .iter()
                    .map(|(id, identity)| (id.as_str(), identity.subscription())),
                &self.watched_generations,
            )
        }
    }

    /// The rule, over an ordered sequence of members.
    ///
    /// **One implementation, two callers with very different inputs.** The
    /// owner's own record is a map, so its keys are unique by construction and
    /// its order is arbitrary; a persisted document is a list, so it can say
    /// the same id twice and the order it says things in is evidence. Writing
    /// the rule against a sequence rather than against either container is
    /// what keeps a document from being judged by a weaker rule than the record
    /// it wants to become — and the duplicate check below is a no-op for the
    /// map rather than a branch the map is exempt from.
    ///
    /// Nothing here mutates, allocates or writes, and nothing it returns is a
    /// capability: [`CurrentIntent`] is two facts about what is installed.
    fn validate_members<'a, I>(
        members: I,
        watched_allocator: &CheckedCounter,
    ) -> Result<CurrentIntent, String>
    where
        I: IntoIterator<Item = (&'a str, &'a super::ProjectSubscription)>,
    {
        let mut seen: Vec<&str> = Vec::new();
        let mut watched: Vec<(&str, u64)> = Vec::new();
        let mut enrolment = false;

        for (id, subscription) in members {
            // Two members under one id are two claims about the same slot.
            // Whether they agree is not the question: a record that says a
            // thing twice was not written by an owner that holds one value per
            // id, so the disagreement is with the writer, not between the
            // members.
            if seen.contains(&id) {
                return Err(format!(
                    "durable intent holds more than one member under id {id}; \
                     one id names one request"
                ));
            }
            seen.push(id);

            match subscription {
                super::ProjectSubscription::Discovery => {
                    let expected = super::discovery_sub_id();
                    if id != expected {
                        return Err(format!(
                            "discovery intent is held under id {id} rather than {expected}"
                        ));
                    }
                }
                super::ProjectSubscription::RootCatchUp { root, stream } => {
                    // A catch-up filter carries the page bound the cursor is
                    // currently at, and the cursor walks it backwards.
                    // Replaying one re-asks for a page already collected, under
                    // an id minted for a transport attempt that ended with the
                    // connection. There is no id under which this is a correct
                    // durable record, so the class is refused rather than its
                    // key checked.
                    return Err(format!(
                        "durable intent holds a root catch-up under id {id} \
                         (root {root}, {stream:?}); catch-up pages are re-derived from \
                         their own cursor and are never durable"
                    ));
                }
                super::ProjectSubscription::EnrolmentHistory { generation } => {
                    // Exhaustible, like a root catch-up: opened for one
                    // transport attempt and re-derived from its cursor if the
                    // connection ends. There is no id under which it is a
                    // correct durable record.
                    return Err(format!(
                        "durable intent holds an enrolment history page under id {id} \
                         (generation {generation}); history pages are re-derived from \
                         their cursor and are never durable"
                    ));
                }
                super::ProjectSubscription::PeerCall => {
                    // The peer-call REQ is not a project request. It is opened
                    // by the relay task under its own fixed id and carries no
                    // project intent to persist; a durable record claiming this
                    // class was written by something that does not hold the
                    // registry's meaning of the word.
                    return Err(format!(
                        "durable intent holds a peer-call source under id {id};                          the peer subscription is a transport source and is never                          a project request"
                    ));
                }
                super::ProjectSubscription::Watched { generation } => {
                    let expected = super::watched_sub_id(*generation);
                    if id != expected {
                        return Err(format!(
                            "watched intent under id {id} carries generation {generation}, \
                             whose id is {expected}"
                        ));
                    }
                    // Allocator provenance. A generation at or above the
                    // allocator's next value was never handed out by this
                    // registry, so durable intent is claiming an identity the
                    // only thing entitled to mint one has no record of.
                    // Retiring it would CLOSE an id the relay never opened
                    // under this process; treating it as current would let an
                    // outside writer choose the predecessor. Neither is a
                    // reconciliation, so neither is done.
                    //
                    // Once the space is spent the allocator has issued every
                    // generation including `u64::MAX`, and `next` stops
                    // advancing — so the comparison alone would reject the last
                    // legitimately issued generation. The spent flag is what
                    // distinguishes "never reached" from "reached and
                    // saturated".
                    if !watched_allocator.is_spent()
                        && *generation >= watched_allocator.next_value()
                    {
                        return Err(format!(
                            "watched intent {id} carries generation {generation}, which this \
                             allocator has never issued (next is {})",
                            watched_allocator.next_value()
                        ));
                    }
                    watched.push((id, *generation));
                }
                super::ProjectSubscription::Enrolment => {
                    if id != super::PROJECT_ENROL_SUB_ID {
                        return Err(format!(
                            "enrolment intent is held under id {id} rather than {}",
                            super::PROJECT_ENROL_SUB_ID
                        ));
                    }
                    enrolment = true;
                }
            }
        }

        match watched.len() {
            0 => Ok(CurrentIntent {
                watched: None,
                enrolment,
            }),
            1 => Ok(CurrentIntent {
                watched: Some(watched[0].1),
                enrolment,
            }),
            _ => {
                // Sorted so the report is the same on every run — a `HashMap`
                // would otherwise name the intruder in a different order each
                // time and the failure would read as flaky rather than
                // deterministic.
                watched.sort_unstable();
                let ids: Vec<&str> = watched.iter().map(|(id, _)| *id).collect();
                Err(format!(
                    "durable intent holds {} watched generations ({}); \
                     exactly one may be current",
                    ids.len(),
                    ids.join(", ")
                ))
            }
        }
    }

    /// Preflight decisions, as pure functions over descriptions.
    ///
    /// **Every refusal an operation can make is decided here, before the
    /// operation has done anything.** Each function reads descriptions — is the
    /// record valid, is the allocator spent, is this a no-op — and returns
    /// either the exact outcome to return or permission to proceed. Production
    /// consumes the decision and does not re-derive it, so an outcome cannot be
    /// spelled one way in a proof and another way on the path that runs.
    ///
    /// This exists because the refusals it owns are unreachable from any
    /// honest fixture. A spent allocator is 2^64 operations away and a corrupt
    /// record cannot be handed to an owner at all, so the branches that handle
    /// them had no load-bearing proof: a reviewer's mutant turned
    /// `RequestIncarnationExhausted` into `InvalidFilters` and the whole suite
    /// still passed. A description of a state is not authority over it —
    /// nothing here accepts or returns a record, a counter, a generation, a
    /// registration or a socket — so these can be proved exactly without
    /// anything being installed.
    pub(super) mod plan {
        use super::{
            AllocatorState, OpenOutcome, PageOpen, ProjectRequestIdentity, ReplaceOutcome,
        };

        /// What an operation decided before acting.
        #[derive(Debug, PartialEq)]
        pub(in crate::project) enum Decision<T> {
            /// Return this outcome, having done nothing at all: no byte
            /// written, no identity allocated, no registration installed, no
            /// predecessor retired, no intent recorded.
            Refuse(T),
            /// Nothing refuses it. The operation may go on to its effects.
            Proceed,
        }

        /// What the registry already holds under the id an open names.
        #[derive(Debug)]
        pub(in crate::project) enum Held {
            /// Nothing live and nothing intended.
            Nothing,
            /// This exact request is already live.
            SameLive,
            /// A different request is live under this id.
            OtherLive(Box<ProjectRequestIdentity>),
            /// Nothing live, but durable intent under this id asks something
            /// else.
            OtherIntent(Box<ProjectRequestIdentity>),
        }

        /// Opening a request: `open_discovery` and `open_replayed`.
        ///
        /// Order matters and is the order production used: the record first,
        /// because a registry that cannot read its own durable intent may not
        /// act on any part of it; then what is held under the id, because an
        /// occupied id is a disagreement about *this* request; then the
        /// allocator, because exhaustion is terminal and process-wide and
        /// should not mask a local conflict that could still be resolved.
        pub(in crate::project) fn open(
            validity: Result<(), String>,
            held: Held,
            incarnations: AllocatorState,
        ) -> Decision<OpenOutcome> {
            if let Err(violation) = validity {
                return Decision::Refuse(OpenOutcome::InvariantViolation(violation));
            }
            match held {
                Held::SameLive => return Decision::Refuse(OpenOutcome::AlreadyLive),
                Held::OtherLive(held) | Held::OtherIntent(held) => {
                    return Decision::Refuse(OpenOutcome::Conflict { held })
                }
                Held::Nothing => {}
            }
            if incarnations == AllocatorState::Spent {
                return Decision::Refuse(OpenOutcome::Exhausted);
            }
            Decision::Proceed
        }

        /// Replacing the watched subscription, connected or offline.
        ///
        /// `current` is the whole-record walk's answer: `Err` refuses, `Ok`
        /// carries the generation whose intent is current. `unchanged` is the
        /// no-op question, asked by the caller because it needs the
        /// predecessor's id to ask it.
        pub(in crate::project) fn watched_replacement(
            bounded_filters: bool,
            current: &Result<Option<u64>, String>,
            unchanged: bool,
            generations: AllocatorState,
        ) -> Decision<ReplaceOutcome> {
            if !bounded_filters {
                return Decision::Refuse(ReplaceOutcome::InvalidFilters);
            }
            if let Err(violation) = current {
                return Decision::Refuse(ReplaceOutcome::InvariantViolation(violation.clone()));
            }
            if unchanged {
                return Decision::Refuse(ReplaceOutcome::Unchanged);
            }
            if generations == AllocatorState::Spent {
                return Decision::Refuse(ReplaceOutcome::WatchedGenerationExhausted);
            }
            Decision::Proceed
        }

        /// Replacing the enrolment subscription, whose id is fixed and whose
        /// generation is not allocated.
        pub(in crate::project) fn enrolment_replacement(
            bounded_filters: bool,
            current: &Result<bool, String>,
        ) -> Decision<ReplaceOutcome> {
            if !bounded_filters {
                return Decision::Refuse(ReplaceOutcome::InvalidFilters);
            }
            if let Err(violation) = current {
                return Decision::Refuse(ReplaceOutcome::InvariantViolation(violation.clone()));
            }
            Decision::Proceed
        }

        /// Writing a replacement, once its class has decided to make one.
        ///
        /// The incarnation is what stamps the successor's authority, so a spent
        /// space refuses here rather than after the write. `already_live` is
        /// the successor being identical to what is already answering, which is
        /// a no-op for the same reason an unchanged watched replacement is.
        pub(in crate::project) fn replacement_write(
            already_live: bool,
            incarnations: AllocatorState,
        ) -> Decision<ReplaceOutcome> {
            if already_live {
                return Decision::Refuse(ReplaceOutcome::Unchanged);
            }
            if incarnations == AllocatorState::Spent {
                return Decision::Refuse(ReplaceOutcome::RequestIncarnationExhausted);
            }
            Decision::Proceed
        }

        /// Opening one history page.
        ///
        /// A page burns its incarnation *before* the write, because the wire id
        /// carries it — so the allocator is part of the preflight rather than
        /// something the write discovers.
        pub(in crate::project) fn history_page(
            validity: Result<(), String>,
            pristine: bool,
            bounded_filter: bool,
            incarnations: AllocatorState,
        ) -> Decision<PageOpen> {
            if let Err(violation) = validity {
                return Decision::Refuse(PageOpen::InvariantViolation(violation));
            }
            if !pristine {
                return Decision::Refuse(PageOpen::NotPristine);
            }
            if !bounded_filter {
                return Decision::Refuse(PageOpen::UnboundedFilter);
            }
            if incarnations == AllocatorState::Spent {
                return Decision::Refuse(PageOpen::Exhausted);
            }
            Decision::Proceed
        }

        /// Replaying durable intent onto a connection.
        ///
        /// A record that does not resolve replays nothing — not the members
        /// that happen to look canonical, and not "as much as possible". Replay
        /// is where durable intent becomes bytes.
        pub(in crate::project) fn replay(validity: Result<(), String>) -> Decision<String> {
            match validity {
                Err(violation) => Decision::Refuse(violation),
                Ok(()) => Decision::Proceed,
            }
        }
    }

    /// Validate a persisted durable document — bytes in, a description out.
    ///
    /// **The proofs' only route to a non-canonical record, and it is not a
    /// route into anything.** What arrives is the serialised form a store would
    /// hold; what leaves is [`CurrentIntent`], two facts about what such a
    /// document would install. There is no `DurableRecord` on either side of
    /// it, so a document cannot become a registry, a registration, a
    /// replacement, a replay or a byte on a socket, whatever it says.
    ///
    /// The rule it applies is the owner's: [`validate_members`], the same walk
    /// [`DurableRecord::derive_current`] makes. That is the point of it. A
    /// proof about a document only says something about a record if the two
    /// are judged identically.
    ///
    /// **Members are examined in the order the document lists them, and none
    /// is discarded on the way.** An earlier revision inserted entries into a
    /// map first: a document naming one id twice arrived at the rule already
    /// shortened, with the earlier member — the malformed one, in the ordering
    /// that mattered — overwritten by the later one, so a document that should
    /// have been refused entire passed as the record its last writer wanted.
    /// Cardinality and order are evidence, and they survive to the rule here.
    ///
    /// Structure is refused before semantics: a member whose filters constrain
    /// nothing cannot be expressed at all, and a document holding one is
    /// refused entire rather than shortened to the members that could be read.
    ///
    /// `#[cfg(test)]` because nothing in production has a document to validate
    /// — the harness holds its record in memory for the process's lifetime and
    /// there is no store behind it. Its judgement is production's all the same,
    /// because the rule is.
    #[cfg(test)]
    pub(crate) fn validate_persisted_document(json: &str) -> Result<CurrentIntent, String> {
        #[derive(serde::Deserialize)]
        struct Document {
            #[serde(default)]
            intent: Vec<Member>,
            #[serde(default)]
            next_watched_generation: u64,
            #[serde(default)]
            watched_generations_spent: bool,
            #[serde(default)]
            next_incarnation: u64,
            #[serde(default)]
            incarnations_spent: bool,
        }
        #[derive(serde::Deserialize)]
        struct Member {
            sub_id: String,
            class: super::ProjectSubscription,
            filters: Vec<Value>,
        }

        let document: Document =
            serde_json::from_str(json).map_err(|e| format!("undecodable durable document: {e}"))?;

        // Every member, in order, structurally — before any of them is judged
        // against the others, and without dropping one.
        for member in &document.intent {
            if ProjectRequestIdentity::from_filters(member.class.clone(), member.filters.clone())
                .is_none()
            {
                return Err(format!(
                    "persisted intent under id {} carries filters that constrain nothing; \
                     a document holding one is refused entire",
                    member.sub_id
                ));
            }
        }

        // Both allocators, before anything is judged against either. A
        // persisted counter is two numbers a writer chose, and only some
        // pairings are states `burn` can produce — the watched pair decides
        // whether the provenance rule below runs at all, so an impossible one
        // is a way to switch that rule off from inside the document. The
        // incarnation pair reaches no rule here, and is checked anyway: a
        // document that misdescribes one allocator is not a document to trust
        // about the other.
        let Some(allocator) = CheckedCounter::from_persisted(
            document.next_watched_generation,
            document.watched_generations_spent,
        ) else {
            return Err(format!(
                "persisted watched allocator is spent at {}, which no counter reaches: \
                 the space is spent only at {}",
                document.next_watched_generation,
                u64::MAX
            ));
        };
        if CheckedCounter::from_persisted(document.next_incarnation, document.incarnations_spent)
            .is_none()
        {
            return Err(format!(
                "persisted incarnation allocator is spent at {}, which no counter reaches: \
                 the space is spent only at {}",
                document.next_incarnation,
                u64::MAX
            ));
        }

        validate_members(
            document
                .intent
                .iter()
                .map(|member| (member.sub_id.as_str(), &member.class)),
            &allocator,
        )
    }

    impl ProjectRequests {
        /// A registry over an empty record. The only constructor there is.
        ///
        /// **Nothing can hand this type a record.** The `over(record)` that
        /// used to sit here took a composed one, and a composed record is
        /// durable authority chosen by its caller: a generation the allocator
        /// never issued, an allocator positioned wherever the caller liked, a
        /// predecessor that reached a successor `REQ` and a predecessor
        /// `CLOSE` without any operation ever having installed it. Records now
        /// start empty and are advanced only by this registry's own semantic
        /// operations, so durable authority has exactly one producer.
        ///
        /// What that costs is the ability to *place* a registry over a corrupt
        /// record, and the refusals that needed one moved to
        /// [`validate_persisted_document`], which applies this same rule to
        /// bytes and hands back a description rather than a record.
        pub(crate) fn new() -> Self {
            Self::default()
        }

        /// The whole durable record, validated, before this registry acts on
        /// it.
        ///
        /// **One gate, one rule, one walk.** Every operation that could mutate
        /// intent, allocate an identity, install or retire authority, replay or
        /// write bytes begins here and returns the violation unchanged if the
        /// record does not resolve — so a refusal is decided before any of
        /// those have happened rather than partway through them. The
        /// `CurrentIntent` it hands back is the same walk's answer, so an
        /// operation that also needs to know the current watched generation or
        /// whether enrolment is installed reads it from here instead of walking
        /// the record a second time.
        fn checked_current(&self) -> Result<CurrentIntent, String> {
            self.record.derive_current()
        }

        /// Record the discovery subscription's durable intent, with no socket.
        ///
        /// **The semantic entry point.** The caller submits what it wants
        /// discovered; the id and the class are this registry's, stamped here.
        /// A caller that supplied them could record a discovery class under any
        /// key it liked — an entry no replacement would ever retire, replayed
        /// verbatim by the next connection.
        ///
        /// The record is validated first. Stamping the canonical id is what
        /// makes *this* entry canonical; it says nothing about the record the
        /// entry would join, and a second discovery entry recorded beside a
        /// foreign one is a record no replacement can resolve afterwards.
        pub(crate) fn record_discovery_intent(&mut self, filters: Vec<Value>) -> IntentAdmission {
            if let Err(violation) = self.checked_current() {
                return IntentAdmission::InvariantViolation(violation);
            }
            let Some(identity) =
                ProjectRequestIdentity::from_filters(ProjectSubscription::Discovery, filters)
            else {
                return IntentAdmission::UnboundedFilters;
            };
            let sub_id = super::discovery_sub_id();
            self.record_intent(&sub_id, identity)
        }

        /// Record durable intent without registering anything.
        ///
        /// For commands that arrive while disconnected. Fail-closed against
        /// both maps, since intent recorded now is replayed verbatim later.
        ///
        /// Private: the id and the identity arrive here already paired, and the
        /// only things allowed to pair them are the semantic operations above.
        fn record_intent(
            &mut self,
            sub_id: &str,
            identity: ProjectRequestIdentity,
        ) -> IntentAdmission {
            if let Some(live) = self.live.get(sub_id) {
                if live.identity != identity {
                    return IntentAdmission::Conflict {
                        held: Box::new(live.identity.clone()),
                    };
                }
            }
            match self.record.intent.entry(sub_id.to_string()) {
                Entry::Vacant(slot) => {
                    slot.insert(identity);
                    IntentAdmission::Recorded
                }
                Entry::Occupied(slot) if *slot.get() == identity => {
                    IntentAdmission::AlreadyIntended
                }
                Entry::Occupied(slot) => IntentAdmission::Conflict {
                    held: Box::new(slot.get().clone()),
                },
            }
        }

        /// The class we recorded for this id, if it is live **on this
        /// connection**.
        ///
        /// An inspection, not an admission route: it hands back a description,
        /// never a capability, and production admits inbound frames through
        /// [`Self::admit_frame`]. It survives so tests can assert what is live
        /// without minting a proof in order to find out.
        #[cfg(test)]
        pub(crate) fn match_frame(&self, sub_id: &str) -> Option<&ProjectSubscription> {
            self.live
                .get(sub_id)
                .map(|live| live.identity.subscription())
        }

        /// Admit one inbound frame against an exact live registration.
        ///
        /// `None` is the unsolicited-frame check: an id we did not send on this
        /// connection, or have since closed, admits nothing, and the frame must
        /// not be verified, deduplicated or delivered.
        ///
        /// The proof names the **registration**, not the id — two defences,
        /// deliberately, because the id used to be the only one and it was not
        /// enough. A catch-up paginated under one deterministic id, so page two
        /// was a new registration wearing page one's name; this lookup found
        /// page two and stamped page one's straggler with page two's authority
        /// before any comparison could tell them apart, filing events from
        /// outside a page's own bound into the history that page claims to have
        /// proven. [`Self::open_history_page`] now mints an id no second
        /// attempt will ever wear, so a straggler for a retired page finds
        /// nothing live and this returns `None`. The proof still carries the
        /// allocation rather than the string, so everything downstream compares
        /// registrations — and a later change to how ids are derived cannot
        /// quietly reintroduce the aliasing.
        pub(crate) fn admit_frame(&self, sub_id: &str) -> Option<FrameAdmission> {
            let live = self.live.get(sub_id)?;
            Some(FrameAdmission {
                sub_id: sub_id.to_string(),
                identity: live.identity.clone(),
                authority: Arc::clone(&live.authority),
            })
        }

        /// Refuse a **live** request, in one operation.
        ///
        /// `None` means nothing was live under this id — and nothing changed:
        /// no suspension recorded, no intent touched.
        ///
        /// One method rather than a bare close then a suspend, because the
        /// invariant is "only a request we actually sent on this connection can
        /// be refused", and a two-step ceremony makes that true only for as
        /// long as every caller performs both steps, in order, against the same
        /// id. `CLOSED` is authenticated by an exact live registration exactly
        /// as an EVENT is; durable intent says what we want, not what we asked,
        /// so it can authenticate nothing.
        ///
        /// Raw suspension insertion is deliberately not exposed. There is no
        /// way to suspend an id that was not live.
        pub(crate) fn refuse_live(
            &mut self,
            sub_id: &str,
            reason: &str,
        ) -> Option<ProjectRequestIdentity> {
            let refused = self.live.remove(sub_id)?.identity;
            self.suspended
                .insert(sub_id.to_string(), reason.to_string());
            Some(refused)
        }

        /// Mint an [`EndOfStoredEvents`] for a live request, **retiring the
        /// request if its class is one-shot**.
        ///
        /// `None` when nothing is live under this id — an EOSE for a request we
        /// did not send, or have already closed, is not evidence about
        /// anything. Authenticated exactly as `CLOSED` and `EVENT` are: by an
        /// exact live registration, never by the id's spelling.
        ///
        /// **A catch-up asked one question and has now been answered.** Its
        /// registration is removed here, in the same operation that mints the
        /// boundary, because the two facts are one fact: an earlier version
        /// left the entry live and left the registry disagreeing with the page
        /// owner about whether that request was still current. Everything
        /// downstream of that disagreement was reachable — a later EVENT still
        /// admitted into a completed page, a second EOSE minting a second
        /// boundary, and, while catch-up ids were still deterministic, the next
        /// page conflicting with its predecessor's entry unless some caller
        /// remembered to close it by hand. A separate `retire_after_eose` would
        /// have been that caller, and forgetting it is exactly the failure being
        /// closed.
        ///
        /// Persistent classes are untouched. Discovery, enrolment and watched
        /// subscriptions keep delivering after their stored backlog drains, so
        /// their boundary retires nothing.
        pub(crate) fn witness_end_of_stored_events(
            &mut self,
            sub_id: &str,
        ) -> Option<EndOfStoredEvents> {
            // Every live entry is a sent entry: `open_request` inserts only
            // after its write returned, so there is no unsent state to reject.
            let live = self.live.get(sub_id)?;
            // An enrolment history page is one-shot for exactly the same
            // reason a root catch-up is: it asked for one page bound and has
            // now been answered. Leaving it live would let a later EVENT be
            // admitted into a completed page and a second EOSE mint a second
            // boundary for it.
            let one_shot = matches!(
                live.identity.subscription(),
                ProjectSubscription::RootCatchUp { .. }
                    | ProjectSubscription::EnrolmentHistory { .. }
            );
            let witness = EndOfStoredEvents {
                sub_id: sub_id.to_string(),
                identity: live.identity.clone(),
                authority: Arc::clone(&live.authority),
            };
            if one_shot {
                self.live.remove(sub_id);
            }
            Some(witness)
        }

        /// Is `witness` the boundary of whatever is live under its id **now**?
        ///
        /// An inspection: it reads, it never mints, and it answers with a
        /// `bool` rather than a capability. It has to be one. The test helper
        /// it replaces asked this question by minting a second boundary and
        /// comparing — which, now that minting retires a one-shot request,
        /// would change the answer by asking it.
        #[cfg(test)]
        pub(crate) fn is_live_boundary(&self, witness: &EndOfStoredEvents) -> bool {
            self.live
                .get(witness.sub_id())
                .is_some_and(|live| Arc::ptr_eq(&live.authority, &witness.authority))
        }

        /// The incarnation of whatever is live under `sub_id`.
        ///
        /// Test-only, and deliberately never a production accessor: an
        /// incarnation a caller can read is an incarnation a caller can relay,
        /// which is the shape this tranche removed. Reading one to assert that
        /// instances are distinct and increasing is a different act from
        /// carrying one as authority — and the assertions that do it are about
        /// this type's own ordering guarantee.
        #[cfg(test)]
        pub(crate) fn live_incarnation(&self, sub_id: &str) -> Option<RequestIncarnation> {
            self.live.get(sub_id).map(|live| live.authority.incarnation)
        }

        /// Open one history page: mint its wire id, write its REQ, install the
        /// registration, bind the page. One operation, no caller-supplied id.
        ///
        /// **The wire id names this attempt and no other.** A catch-up
        /// paginates, so the deterministic `proj-catchup-{stream}-{root}` was
        /// worn by every page of a stream in turn — and a frame is admitted by
        /// looking up whatever is live under the id it carries. A delayed frame
        /// from page one therefore arrived, found page two, and was stamped
        /// with *page two's* authority before any allocation comparison could
        /// run: the predecessor check compared a registration against itself.
        /// For a boundary that was fatal — page one's late EOSE finished page
        /// two as an empty page, which reads as "this history is exhausted".
        ///
        /// The attempt token is burned before the write, because the id must
        /// carry it. That is a departure from `open_request`, which takes its
        /// incarnation only after the write returns; the reason it is safe is
        /// that a burned token grants nothing. It is not a reservation, nothing
        /// can be promoted, and a cancelled or failed attempt simply leaves a
        /// number nobody will use again — while a retry gets a genuinely
        /// different id, which is the property being bought.
        ///
        /// **No caller-supplied identity either.** The class and filter are
        /// derived from the collector, so the registration cannot describe a
        /// different question from the page bound to it. That replaces the old
        /// binding checks — wrong root, wrong stream, wrong bound, wrong limit —
        /// which existed only because the caller supplied both halves and they
        /// could disagree.
        ///
        /// Durable intent is deliberately **not** recorded. A catch-up filter
        /// carries a page bound that moves: pagination walks `until` backwards,
        /// so the second page of a stream is a different question from the
        /// first. Recording it would make `replayable()` re-ask, after a
        /// reconnect, for a page the cursor has already walked past. A
        /// reconstruction is replayed by its owner re-deriving the bound from
        /// its own advanced cursor — see `RootReconstruction::disconnected` and
        /// `pages_wanted`.
        pub(crate) async fn open_history_page<S: ProjectReqSink>(
            &mut self,
            sink: &mut S,
            collector: HistoryPageCollector,
        ) -> PageOpen {
            // Rows that arrived before this registration existed cannot belong
            // to it. Without the pristine question a collector could be filled
            // first and laundered into the registration opened afterwards.
            //
            // The identity is built here so the plan can be asked whether it
            // could be: building one allocates nothing and installs nothing,
            // and a page whose own filter would ask the relay for everything
            // must cost the incarnation space nothing.
            let identity = ProjectRequestIdentity::new(
                collector.scope().subscription(collector.generation()),
                collector
                    .scope()
                    .filter(collector.until(), collector.effective_limit()),
            );
            if let plan::Decision::Refuse(outcome) = plan::history_page(
                self.checked_current().map(|_| ()),
                collector.is_pristine(),
                identity.is_some(),
                self.record.incarnations.state(),
            ) {
                return outcome;
            }
            // The identity is built — and can be refused — *before* a token is
            // burned, so a page whose own filter would ask the relay for
            // everything costs the incarnation space nothing. `ProjectRequests`
            // has one constructor for identities and it is fallible for every
            // caller, this one included: an infallible route for "we built this
            // filter ourselves, it must be fine" is the check-skipping door the
            // type exists to close, and `catch_up_filter` being correct today is
            // not the same fact as this operation refusing an incorrect one.
            let Some(identity) = identity else {
                // Unreachable: the plan refused an unbuildable identity above.
                return PageOpen::UnboundedFilter;
            };
            let Some(incarnation) = self.burn_incarnation() else {
                // Unreachable: the plan refused a spent space above.
                return PageOpen::Exhausted;
            };
            let sub_id = Self::history_wire_id(collector.scope(), incarnation);
            let text = match serde_json::to_string(&identity.req_frame(&sub_id)) {
                Ok(text) => text,
                Err(e) => return PageOpen::WriteFailed(format!("serialize: {e}")),
            };

            // ---- The only await in this operation. -------------------------
            if let Err(e) = sink.write_project_req(text).await {
                return PageOpen::WriteFailed(e);
            }

            // ---- Install, already sent, and bind in the same breath. -------
            let authority = Arc::new(RegistrationAuthority {
                registry: Arc::clone(&self.epoch),
                incarnation,
            });
            self.live.insert(
                sub_id.clone(),
                LiveRegistration {
                    identity,
                    authority: Arc::clone(&authority),
                },
            );
            PageOpen::Opened(OpenedHistoryPage {
                authority,
                sub_id,
                collector,
            })
        }

        /// The wire id for one catch-up transport attempt.
        ///
        /// **Not deterministic, on purpose.** The id used to be
        /// `proj-catchup-{stream}-{root}` and nothing else, so every page of a
        /// stream wore it in turn — and since a frame is admitted by looking up
        /// whatever is live under the id it carries, a delayed frame from page
        /// one was handed page two's authority before any comparison could tell
        /// them apart. The trailing incarnation is what makes the id name a
        /// *registration* rather than a question.
        ///
        /// Root and stream stay in it because they cost nothing and make relay
        /// logs legible; they are not what the id is trusted for. Nothing parses
        /// this string — the class is read from what this agent recorded when it
        /// sent the REQ. At 79 characters plus the incarnation it stays well
        /// inside the 256 this relay advertises
        /// (`buzz-relay/src/protocol.rs:9`).
        fn history_wire_id(scope: &super::HistoryScope, incarnation: RequestIncarnation) -> String {
            match scope {
                super::HistoryScope::Root { root, stream } => {
                    let marker = match stream {
                        HistoryStream::Comments => "c",
                        HistoryStream::PullRequestUpdates => "u",
                    };
                    format!(
                        "{}catchup-{marker}-{root}-{}",
                        super::PROJECT_SUB_ID_PREFIX,
                        incarnation.0
                    )
                }
                // No coordinate in the id. The set can be long and changes as
                // discovery widens, and an id is a lookup key rather than a
                // description — the incarnation is what makes it unique, and
                // the registry holds the question it was opened with.
                super::HistoryScope::Enrolment { .. } => format!(
                    "{}enrol-history-{}",
                    super::PROJECT_SUB_ID_PREFIX,
                    incarnation.0
                ),
            }
        }

        /// Take the next incarnation, or `None` once the space is spent.
        ///
        /// The one allocator. `checked_add` and a sticky flag, because a
        /// wrapping counter would hand a future request the authority of an
        /// ancient one — in release builds only, where no debug panic could
        /// warn anyone.
        fn burn_incarnation(&mut self) -> Option<RequestIncarnation> {
            self.record.incarnations.burn().map(RequestIncarnation)
        }

        /// Take the next watched generation, or `None` once the space is spent.
        ///
        /// Deliberately the same shape as [`Self::burn_incarnation`] — one
        /// allocator idiom in this file, not two. A generation is burned on
        /// *attempt*: a failed write consumes it and it is never handed out
        /// again, because the number may already have been seen on the wire.
        fn burn_watched_generation(&mut self) -> Option<u64> {
            self.record.watched_generations.burn()
        }

        /// The watched generation whose durable intent is current — derived,
        /// never stored.
        ///
        /// **Durable intent is the single record of what is installed.** It is
        /// what a reconnect replays, so a field claiming to mirror it is a
        /// second answer to a question that already has one, and the two have
        /// disagreed twice: first when the run loop advanced its copy on
        /// *enqueue*, then when this struct advanced its copy without seeing an
        /// intent installed by a neighbouring command.
        ///
        /// `Err` when more than one watched intent is durable. There is no
        /// correct choice to make there — retiring one leaves the other beside
        /// the successor, which is the defect this whole design removes — so
        /// the caller is told the registry cannot answer, and installs nothing.
        pub(crate) fn current_watched(&self) -> Result<Option<u64>, String> {
            self.checked_current().map(|current| current.watched)
        }

        /// Whether an enrolment request's durable intent is current.
        ///
        /// Derived for the same reason as [`Self::current_watched`], and by the
        /// same rule: the id is fixed, so the question is whether *that* id
        /// holds enrolment intent — not whether a boolean somewhere was set.
        fn enrolment_current(&self) -> Result<bool, String> {
            self.checked_current().map(|current| current.enrolment)
        }

        /// Replace the watched-roots subscription with one carrying `filters`.
        ///
        /// **The semantic entry point.** Callers submit what they want watched;
        /// they do not choose the generation, the id, or the predecessor. Those
        /// are derived here, by the only component that knows what is installed.
        ///
        /// A caller that could supply the generation could supply a stale one,
        /// which is precisely the defect this replaces: the run loop advanced
        /// its own copy when the command was enqueued and then named a
        /// generation the registry had never seen.
        /// **Order: validate, compare, then burn.** Refusal and a genuine no-op
        /// both decide before allocation, so neither consumes a generation.
        /// Only an attempt that will actually be made spends one — and once
        /// spent it is never handed out again, whatever the attempt returns.
        pub(crate) async fn replace_watched<S: ProjectReqSink>(
            &mut self,
            sink: &mut S,
            filters: Vec<Value>,
        ) -> ReplaceOutcome {
            let current = self.current_watched();
            let predecessor = current
                .as_ref()
                .ok()
                .and_then(|c| c.map(super::watched_sub_id));
            // A no-op is a predecessor whose durable intent already asks these
            // filters *and* which is live on this connection asking the same
            // ones. Intent alone is not enough: intent recorded while
            // disconnected has never reached a socket, and reporting
            // `Unchanged` for it would leave the relay asked nothing while the
            // registry claimed to be current. Live alone is not enough either —
            // the two are separate maps, and a no-op has to be a no-op in both.
            let unchanged = predecessor.as_deref().is_some_and(|prior_id| {
                self.intent_asks_exactly(prior_id, &filters)
                    && self.live_asks_exactly(prior_id, &filters)
            });
            if let plan::Decision::Refuse(outcome) = plan::watched_replacement(
                bounded_filters(&filters),
                &current,
                unchanged,
                self.record.watched_generations.state(),
            ) {
                return outcome;
            }
            let Some(generation) = self.burn_watched_generation() else {
                // Unreachable: the plan above refused a spent allocator, and
                // nothing between there and here can spend one.
                return ReplaceOutcome::WatchedGenerationExhausted;
            };
            let Some(identity) = ProjectRequestIdentity::from_filters(
                super::ProjectSubscription::Watched { generation },
                filters,
            ) else {
                // Unreachable: `bounded_filters` above asked the same question
                // `from_filters` asks. Reported rather than unwrapped, because
                // a panic here would take down the relay task over a
                // disagreement between two expressions of one rule.
                return ReplaceOutcome::InvalidFilters;
            };
            let sub_id = super::watched_sub_id(generation);
            // Nothing to write back afterwards: `replace_request` moves durable
            // intent, and durable intent *is* the current generation.
            self.replace_request(sink, predecessor.as_deref(), &sub_id, identity)
                .await
        }

        /// Does `sub_id`'s durable intent already ask exactly `filters`?
        ///
        /// The whole authority available while disconnected: reconnect replays
        /// intent verbatim, so intent that already asks this is intent the next
        /// connection will install unchanged.
        fn intent_asks_exactly(&self, sub_id: &str, filters: &[Value]) -> bool {
            self.record
                .intent
                .get(sub_id)
                .is_some_and(|held| held.filters().eq(filters.iter()))
        }

        /// Is `sub_id` live on this connection asking exactly `filters`?
        ///
        /// By identity rather than by id: a registration under the right id
        /// asking a different question is not unchanged, and the only thing
        /// that makes a REQ redundant is that the relay is already answering
        /// this exact one.
        fn live_asks_exactly(&self, sub_id: &str, filters: &[Value]) -> bool {
            self.live
                .get(sub_id)
                .is_some_and(|live| live.identity.filters().eq(filters.iter()))
        }

        /// The offline half of [`Self::replace_watched`].
        ///
        /// No REQ can be written with no socket, but the intent must still move
        /// or the next connection replays the predecessor. The generation is
        /// allocated and the current one advanced here too, because durable
        /// intent *is* what reconnect installs — there is no later moment at
        /// which this becomes true.
        pub(crate) fn replace_watched_intent(&mut self, filters: Vec<Value>) -> ReplaceOutcome {
            let current = self.current_watched();
            let predecessor = current
                .as_ref()
                .ok()
                .and_then(|c| c.map(super::watched_sub_id));
            // Offline, "unchanged" is a question about intent alone — there is
            // no connection for anything to be live on, and re-recording the
            // same intent under a fresh generation would burn one to arrive
            // where the next reconnect was already going.
            let unchanged = predecessor
                .as_deref()
                .is_some_and(|prior_id| self.intent_asks_exactly(prior_id, &filters));
            if let plan::Decision::Refuse(outcome) = plan::watched_replacement(
                bounded_filters(&filters),
                &current,
                unchanged,
                self.record.watched_generations.state(),
            ) {
                return outcome;
            }
            let Some(generation) = self.burn_watched_generation() else {
                // Unreachable, as above.
                return ReplaceOutcome::WatchedGenerationExhausted;
            };
            let Some(identity) = ProjectRequestIdentity::from_filters(
                super::ProjectSubscription::Watched { generation },
                filters,
            ) else {
                return ReplaceOutcome::InvalidFilters;
            };
            let sub_id = super::watched_sub_id(generation);
            self.replace_intent(predecessor.as_deref(), &sub_id, identity);
            ReplaceOutcome::Replaced {
                retired: predecessor,
            }
        }

        /// Replace the enrolment subscription with one carrying `filters`.
        ///
        /// The id is fixed, so the predecessor is this same id once anything
        /// has been installed under it — and `None` before that, because naming
        /// a predecessor that never existed is what made the original defect
        /// survivable.
        pub(crate) async fn replace_enrolment<S: ProjectReqSink>(
            &mut self,
            sink: &mut S,
            filters: Vec<Value>,
        ) -> ReplaceOutcome {
            let current = self.enrolment_current();
            if let plan::Decision::Refuse(outcome) =
                plan::enrolment_replacement(bounded_filters(&filters), &current)
            {
                return outcome;
            }
            let Some(identity) = ProjectRequestIdentity::from_filters(
                super::ProjectSubscription::Enrolment,
                filters,
            ) else {
                // Unreachable: the plan asked the same question of the same
                // filters one call earlier.
                return ReplaceOutcome::InvalidFilters;
            };
            let id = super::PROJECT_ENROL_SUB_ID;
            let predecessor = current.unwrap_or(false).then_some(id);
            self.replace_request(sink, predecessor, id, identity).await
        }

        /// The offline half of [`Self::replace_enrolment`].
        pub(crate) fn replace_enrolment_intent(&mut self, filters: Vec<Value>) -> ReplaceOutcome {
            let current = self.enrolment_current();
            if let plan::Decision::Refuse(outcome) =
                plan::enrolment_replacement(bounded_filters(&filters), &current)
            {
                return outcome;
            }
            let Some(identity) = ProjectRequestIdentity::from_filters(
                super::ProjectSubscription::Enrolment,
                filters,
            ) else {
                // Unreachable, as above.
                return ReplaceOutcome::InvalidFilters;
            };
            let id = super::PROJECT_ENROL_SUB_ID;
            let predecessor = current.unwrap_or(false).then_some(id);
            self.replace_intent(predecessor, id, identity);
            ReplaceOutcome::Replaced { retired: None }
        }

        /// Record a replacement's durable intent while disconnected.
        ///
        /// The offline half of [`Self::replace_request`]: no REQ can be written
        /// with no socket, but the *intent* still has to move, or the next
        /// connection replays the predecessor and the replacement is lost.
        ///
        /// Unlike [`Self::record_intent`] this is permitted to overwrite, for
        /// the same reason replacement may and opening may not. It writes no
        /// live registration, so nothing becomes answerable here.
        ///
        /// Private, for the reason [`Self::record_intent`] is: it takes the id
        /// and the identity already paired, and the only things allowed to pair
        /// them are the two offline halves above — which validate the whole
        /// record before they do.
        fn replace_intent(
            &mut self,
            predecessor: Option<&str>,
            sub_id: &str,
            identity: ProjectRequestIdentity,
        ) {
            self.record.intent.insert(sub_id.to_string(), identity);
            if let Some(prior) = predecessor {
                if prior != sub_id {
                    self.record.intent.remove(prior);
                    self.live.remove(prior);
                    self.suspended.remove(prior);
                }
            }
        }

        /// Replace a live project subscription, transactionally.
        ///
        /// **Why this is not `open_request`.** `open_request` refuses to change
        /// the identity held under an id, and must: reopening a live id does
        /// not cancel the relay's existing request, so reclassifying traffic it
        /// is still producing would be a lie. Widening the enrolment filter is
        /// exactly that change, deliberately made — so it needs an operation
        /// that is *allowed* to make it, rather than a relaxation of the one
        /// that must not.
        ///
        /// **Ordering: install the successor, then retire the predecessor.**
        /// The reverse leaves a window with no live subscription, and a
        /// successor that then failed would leave nothing at all — the agent
        /// would stop hearing about roots it is enrolled on, silently. Held
        /// this way round, a failed successor is a no-op.
        ///
        /// `predecessor` is `None` for a first install, `Some(id)` when the
        /// successor takes a new id (a fresh watched generation). When it names
        /// the same id as `sub_id`, the relay's own REQ-replacement semantics
        /// do the retiring and no CLOSE is sent — a CLOSE there would retire
        /// the successor that just replaced it.
        ///
        /// **Private, and validated by its argument rather than by a check of
        /// its own.** `predecessor` is the id this replacement retires, and the
        /// only thing that can derive it is a walk of the whole record —
        /// [`Self::current_watched`] for a watched generation,
        /// [`Self::enrolment_current`] for the fixed enrolment id. So a caller
        /// that has an argument to pass has already validated the record, and a
        /// caller that has not cannot call this at all. A second walk here
        /// would re-ask a question its own parameter is the answer to, and an
        /// arm no caller can reach is an arm no proof can hold to account.
        async fn replace_request<S: ProjectReqSink>(
            &mut self,
            sink: &mut S,
            predecessor: Option<&str>,
            sub_id: &str,
            identity: ProjectRequestIdentity,
        ) -> ReplaceOutcome {
            if let plan::Decision::Refuse(outcome) = plan::replacement_write(
                self.live
                    .get(sub_id)
                    .is_some_and(|l| l.identity == identity),
                self.record.incarnations.state(),
            ) {
                return outcome;
            }

            let text = match serde_json::to_string(&identity.req_frame(sub_id)) {
                Ok(text) => text,
                Err(e) => return ReplaceOutcome::WriteFailed(format!("serialize: {e}")),
            };

            // Durable intent for the successor is recorded before the write, as
            // in `open_request`: a failed write leaves the intent standing
            // because the intent is still what we want. The difference is that
            // here it may *overwrite* a predecessor's intent under the same id,
            // so the prior value is kept to be put back if the write fails.
            let displaced = self
                .record
                .intent
                .insert(sub_id.to_string(), identity.clone());

            if let Err(e) = sink.write_project_req(text).await {
                match displaced {
                    Some(prior) => {
                        self.record.intent.insert(sub_id.to_string(), prior);
                    }
                    None => {
                        self.record.intent.remove(sub_id);
                    }
                }
                return ReplaceOutcome::WriteFailed(e);
            }

            // Taken after the write returned, exactly as `open_request` does:
            // a write that never landed must not consume an incarnation, so a
            // retry is a genuinely new authority.
            let Some(incarnation) = self.burn_incarnation() else {
                // Unreachable: the plan refused a spent space before the write,
                // and `&mut self` is held across it, so nothing else can have
                // spent the last one meanwhile. The intent is put back anyway,
                // because an arm that undoes its own effect is the one shape
                // that stays correct if the reasoning above ever stops holding.
                match displaced {
                    Some(prior) => {
                        self.record.intent.insert(sub_id.to_string(), prior);
                    }
                    None => {
                        self.record.intent.remove(sub_id);
                    }
                }
                return ReplaceOutcome::RequestIncarnationExhausted;
            };
            self.live.insert(
                sub_id.to_string(),
                LiveRegistration {
                    identity,
                    authority: Arc::new(RegistrationAuthority {
                        registry: Arc::clone(&self.epoch),
                        incarnation,
                    }),
                },
            );

            // ---- Successor is installed. Only now retire the predecessor. ---
            let retired = match predecessor {
                Some(prior_id) if prior_id != sub_id => {
                    self.live.remove(prior_id);
                    self.record.intent.remove(prior_id);
                    self.suspended.remove(prior_id);

                    // Best-effort. The predecessor is already retired locally,
                    // so a failed CLOSE costs a stale relay-side subscription
                    // whose frames are no longer admitted — noise, not
                    // authority. Failing the replacement over it would undo a
                    // successor that is already live and correct.
                    let close = serde_json::json!(["CLOSE", prior_id]).to_string();
                    if let Err(e) = sink.write_project_close(close).await {
                        tracing::debug!(
                            sub_id = prior_id,
                            "predecessor CLOSE failed after successful replacement: {e}"
                        );
                    }
                    Some(prior_id.to_string())
                }
                _ => None,
            };

            ReplaceOutcome::Replaced { retired }
        }

        /// Open the discovery subscription over `filters`.
        ///
        /// **The semantic entry point**, and the connected half of
        /// [`Self::record_discovery_intent`]. The id and the class are stamped
        /// here; the caller submits the question and nothing else.
        pub(crate) async fn open_discovery<S: ProjectReqSink>(
            &mut self,
            sink: &mut S,
            filters: Vec<Value>,
        ) -> OpenOutcome {
            let Some(identity) =
                ProjectRequestIdentity::from_filters(ProjectSubscription::Discovery, filters)
            else {
                return OpenOutcome::UnboundedFilters;
            };
            let sub_id = super::discovery_sub_id();
            self.open_request(sink, &sub_id, identity).await
        }

        /// Re-open one request this registry itself intends.
        ///
        /// The argument is the whole point: a [`ReplayableRequest`] is minted
        /// only by [`Self::replayable`], out of a durable record that has just
        /// been validated end to end. So the reconnect path re-asks exactly
        /// what this owner recorded, and cannot name an id, a class or a
        /// generation of its own — it has none to name.
        pub(crate) async fn open_replayed<S: ProjectReqSink>(
            &mut self,
            sink: &mut S,
            request: ReplayableRequest,
        ) -> OpenOutcome {
            let ReplayableRequest { sub_id, identity } = request;
            self.open_request(sink, &sub_id, identity).await
        }

        /// Decide, write the REQ, then install the registration.
        ///
        /// **The registry writes.** There is no `confirm_sent(&str)`, and no
        /// caller-supplied closure either: a generic
        /// `FnOnce(..) -> Result<(), E>` is the same lever with an `async`
        /// wrapper, because `|_| async { Ok(()) }` manufactures success without
        /// a socket. `sink` is a [`ProjectReqSink`], which is sealed and
        /// implemented only for the live WebSocket, so possessing one *is* the
        /// evidence a socket exists.
        ///
        /// The REQ is serialised here from `identity.filter()`, so the write
        /// cannot carry a different question from the one registered — the
        /// caller no longer supplies the bytes at all.
        ///
        /// **Not for catch-ups**, and no longer by a check. They go through
        /// [`Self::open_history_page`], which mints their wire id; this
        /// function is private and its two callers are
        /// [`Self::open_discovery`], which stamps `Discovery`, and
        /// [`Self::open_replayed`], whose argument can only have come from a
        /// record [`DurableRecord::derive_current`] found valid — and a valid
        /// record holds no catch-up at all. The `NotOpenableHere` outcome that
        /// used to guard this went with the caller that could reach it.
        async fn open_request<S: ProjectReqSink>(
            &mut self,
            sink: &mut S,
            sub_id: &str,
            identity: ProjectRequestIdentity,
        ) -> OpenOutcome {
            // ---- The whole record, before the preflight. --------------------
            //
            // Opening records intent, writes a REQ and burns an incarnation.
            // `open_replayed` arrives from a record `replayable` has just
            // validated, but `open_discovery` does not, and neither may act on
            // a record that no longer resolves.
            // An async operation has three exits, not two: `Ok`, `Err`, and
            // *dropped while suspended*. The previous shape reserved before
            // awaiting, so a cancelled future left a live-but-unsent entry that
            // nothing could ever promote — the id was held hostage and that
            // root would silently never reconstruct. Rather than handle
            // cancellation, this removes the state it could strand: no
            // registration exists until the write has already returned.
            let held = match self.live.get(sub_id) {
                Some(live) if live.identity == identity => plan::Held::SameLive,
                Some(live) => plan::Held::OtherLive(Box::new(live.identity.clone())),
                None => match self.record.intent.get(sub_id) {
                    Some(intended) if *intended != identity => {
                        plan::Held::OtherIntent(Box::new(intended.clone()))
                    }
                    _ => plan::Held::Nothing,
                },
            };
            if let plan::Decision::Refuse(outcome) = plan::open(
                self.checked_current().map(|_| ()),
                held,
                self.record.incarnations.state(),
            ) {
                return outcome;
            }
            let text = match serde_json::to_string(&identity.req_frame(sub_id)) {
                Ok(text) => text,
                Err(e) => return OpenOutcome::WriteFailed(format!("serialize: {e}")),
            };

            // Durable intent is the one thing recorded before the write,
            // because it must outlive a failed one — the intent is still what
            // we want; the write is what failed. It confers no authority.
            self.record
                .intent
                .insert(sub_id.to_string(), identity.clone());

            // ---- The only await in this operation. -------------------------
            if let Err(e) = sink.write_project_req(text).await {
                return OpenOutcome::WriteFailed(e);
            }

            // ---- Install, already sent. ------------------------------------
            //
            // The incarnation is taken *here*, after the write returned, so a
            // cancelled open consumes nothing and a retry is a genuinely new
            // authority — which matters because a cancelled write may have
            // reached the relay anyway. A page cannot do the same: its id has
            // to carry the token, so `open_history_page` burns one first and
            // relies on a burned token conferring nothing.
            let Some(incarnation) = self.burn_incarnation() else {
                // Unreachable: the plan refused a spent space before the write.
                return OpenOutcome::Exhausted;
            };
            self.live.insert(
                sub_id.to_string(),
                LiveRegistration {
                    identity,
                    authority: Arc::new(RegistrationAuthority {
                        registry: Arc::clone(&self.epoch),
                        incarnation,
                    }),
                },
            );
            OpenOutcome::Sent
        }

        pub(crate) fn suspension(&self, sub_id: &str) -> Option<&str> {
            self.suspended.get(sub_id).map(String::as_str)
        }

        /// Forget everything belonging to a connection: registrations and the
        /// refusals that connection issued. Durable intent is untouched.
        ///
        /// A relay may deny service while connected; the denial must not
        /// outlive the connection that issued it, and a registration certainly
        /// must not — `BgState` outlives the socket, so a fresh connection
        /// would otherwise answer ids whose replacement REQs were never sent.
        pub(crate) fn clear_connection(&mut self) {
            self.live.clear();
            self.suspended.clear();
            // The backlog belonged to a registration on the connection that
            // just died. Keeping it would let the replacement tail's frames be
            // measured against an authority nothing can ever close, and the
            // replacement's own `bind_enrolment_backlog` is what re-establishes
            // it — from the registration that actually exists.
            self.enrolment_backlog = None;
        }

        /// Claim the stored-events prefix of whatever enrolment tail is live
        /// **now**.
        ///
        /// Called by the owner immediately after an enrolment REQ reaches the
        /// socket. It reads the live registration rather than accepting one:
        /// a caller able to supply the authority would be a caller able to
        /// supply a stale one, which is the whole failure this type exists to
        /// prevent.
        ///
        /// Returns `false` when no enrolment registration is live, which is the
        /// honest answer to "whose backlog?" when the open did not happen.
        pub(crate) fn bind_enrolment_backlog(&mut self) -> bool {
            let Some(live) = self.live.get(super::PROJECT_ENROL_SUB_ID) else {
                return false;
            };
            // A predecessor that had not finished answering still owes its
            // boundary, and that boundary is indistinguishable from this
            // registration's once it arrives. Carry the debt forward, including
            // anything the predecessor had itself inherited: two replacements
            // before the first EOSE owe two.
            let inherited = self
                .enrolment_backlog
                .as_ref()
                .map_or(0, |prior| prior.pending_predecessors + 1);
            self.enrolment_backlog = Some(EnrolmentBacklog {
                authority: Arc::clone(&live.authority),
                pending_predecessors: inherited,
            });
            true
        }

        /// What a frame admitted on the enrolment tail means.
        ///
        /// **The single producer of an enrolment frame's processing mode.** It
        /// answers from the registration that admitted the frame, so there is
        /// no timestamp, no drain flag and no id spelling anywhere in the
        /// decision — the three things that have each, in turn, misclassified a
        /// live root as history.
        ///
        /// Anything that is not this registration's own unfinished backlog is
        /// live: a frame under a *later* registration (its predecessor's
        /// backlog is over by construction), a frame arriving after this
        /// registration's own boundary, or a frame on a tail that never claimed
        /// a backlog at all.
        pub(crate) fn enrolment_frame_mode(
            &self,
            admission: &FrameAdmission,
        ) -> super::ProcessingMode {
            match &self.enrolment_backlog {
                Some(backlog) if Arc::ptr_eq(&backlog.authority, &admission.authority) => {
                    super::ProcessingMode::Replay
                }
                _ => super::ProcessingMode::Live,
            }
        }

        /// Close the enrolment backlog, if this boundary is the one that can.
        ///
        /// Returns whether it closed anything. A boundary owed to a
        /// predecessor registration, or minted on a connection that has since
        /// died, certifies nothing here. Neither refusal rests on the id: the
        /// fixed enrolment id means a stale EOSE is *always* spelled exactly
        /// like the current one. A dead connection is caught by allocation
        /// identity, which a replacement cannot reproduce; a predecessor is
        /// caught by the debt counted in
        /// [`EnrolmentBacklog::pending_predecessors`], because allocation
        /// identity cannot see it.
        pub(crate) fn close_enrolment_backlog(&mut self, witness: &EndOfStoredEvents) -> bool {
            let Some(backlog) = self.enrolment_backlog.as_mut() else {
                return false;
            };
            if !Arc::ptr_eq(&backlog.authority, &witness.authority) {
                return false;
            }
            // Owed to something earlier. Consumed, so the successor's own
            // boundary is still ahead — and refused, so it certifies nothing.
            if backlog.pending_predecessors > 0 {
                backlog.pending_predecessors -= 1;
                return false;
            }
            self.enrolment_backlog = None;
            true
        }

        /// What to (re-)send, in deterministic order — or why nothing may be.
        ///
        /// **The whole record is validated first, and a record that fails
        /// replays nothing.** Replay is where durable intent becomes bytes, so
        /// it is the last place a non-canonical entry can be caught before a
        /// relay is asked a question this owner never wrote. Handing back the
        /// entries that happen to look canonical would be reconciling an
        /// inconsistent record by ignoring the part that is not — the same
        /// refusal [`Self::replace_watched`] and [`Self::replace_enrolment`]
        /// already make, for the same reason, on the same walk.
        ///
        /// On an existing connection, suspended requests are skipped: the
        /// relay already refused them here, and a proactive resubscribe must
        /// not quietly retry on the connection that said no. A fresh
        /// connection has had its suspensions cleared, so everything intended
        /// is offered once.
        pub(crate) fn replayable(&self) -> Result<Vec<ReplayableRequest>, String> {
            if let plan::Decision::Refuse(violation) =
                plan::replay(self.checked_current().map(|_| ()))
            {
                return Err(violation);
            }
            let mut out: Vec<ReplayableRequest> = self
                .record
                .intent
                .iter()
                .filter(|(id, _)| !self.suspended.contains_key(*id))
                .map(|(id, identity)| ReplayableRequest {
                    sub_id: id.clone(),
                    identity: identity.clone(),
                })
                .collect();
            out.sort_by(|a, b| a.sub_id.cmp(&b.sub_id));
            Ok(out)
        }

        pub(crate) fn intent(&self, sub_id: &str) -> Option<&ProjectRequestIdentity> {
            self.record.intent.get(sub_id)
        }

        pub(crate) fn live_len(&self) -> usize {
            self.live.len()
        }

        #[cfg(test)]
        pub(crate) fn intent_len(&self) -> usize {
            self.record.intent.len()
        }
    }
}

pub(crate) fn discovery_sub_id() -> String {
    format!("{PROJECT_SUB_ID_PREFIX}discovery")
}

/// The startup discovery subscription, or `None` when project routing is off.
///
/// This is the entirety of what `project_routing_enabled` gates. Discovery is
/// the one project subscription that depends on no prior state, and every other
/// project REQ derives its filter from what discovery finds: with discovery
/// closed the discovered set stays empty, so enrolment has nothing to widen to
/// and no root is ever watched. The flag therefore needs exactly one check, and
/// a second one would only be somewhere for the two to disagree.
///
/// It exists as a function so the decision is reachable from a test. Reading
/// the flag inline at the call site left it provable only by running the whole
/// startup path, and the control test that stood in for it exercised
/// [`project_req_frames`] — which no production code calls.
///
/// **Filters only.** It returned the id and the class too, which made them
/// values a caller held and could substitute. They are constants of the
/// discovery subscription, so the background task stamps them at the point of
/// registration and nothing carries them across a channel.
pub(crate) fn discovery_subscription(enabled: bool) -> Option<Vec<serde_json::Value>> {
    if !enabled {
        return None;
    }
    Some(vec![
        serde_json::json!({ "kinds": [buzz_core::kind::KIND_GIT_REPO_ANNOUNCEMENT] }),
    ])
}

pub(crate) fn watched_sub_id(generation: u64) -> String {
    format!("{PROJECT_SUB_ID_PREFIX}roots-{generation}")
}

pub(crate) fn canonical_root_id(raw: &str) -> Option<String> {
    if raw.len() != 64 || !raw.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    Some(raw.to_ascii_lowercase())
}

/// Derive the deterministic route key for a project root.
///
/// Same root → same key, in both runtimes. Different roots never collide.
/// Returns `None` when `root_event_id` is not a 64-char hex event id — an
/// invalid reference must not be coerced into a plausible-looking session.
pub(crate) fn project_route_key(root_event_id: &str) -> Option<Uuid> {
    let canonical = canonical_root_id(root_event_id)?;
    Some(Uuid::new_v5(&PROJECT_ROUTE_NAMESPACE, canonical.as_bytes()))
}

// ── Root extraction ───────────────────────────────────────────────────────────

/// Which root event a project event belongs to. **Strict.**
///
/// This began life as a parser helper where "first plausible match" was a
/// reasonable convenience. [`ProjectRoute`] turned its result into the
/// authoritative session key, which changed what a wrong answer costs: a signed
/// event carrying two conflicting root markers would be routed by tag order,
/// letting an author decide which conversation their event joins by shuffling
/// tags. Ambiguity is now refused rather than resolved.
///
/// | Kind | Root |
/// |---|---|
/// | `1621` / `1618` | its own verified event id |
/// | `1619` | exactly one valid uppercase `E` |
/// | `1`, `1630`-`1633` | exactly one valid `e` marked `root`, or — only when no `root` marker is present at all — exactly one valid unmarked `e` |
///
/// A malformed marked root is **not** rescued by a valid fallback: an event
/// that says "my root is `<garbage>`" is malformed, not a legacy event.
pub(crate) fn root_event_id<T, S>(kind: u32, event_id: &str, tags: &[T]) -> Option<String>
where
    T: AsRef<[S]>,
    S: AsRef<str>,
{
    match kind {
        KIND_GIT_ISSUE | KIND_GIT_PULL_REQUEST => canonical_root_id(event_id),
        KIND_GIT_PR_UPDATE => sole_reference(tags, "E"),
        KIND_TEXT_NOTE
        | KIND_GIT_STATUS_OPEN
        | KIND_GIT_STATUS_MERGED
        | KIND_GIT_STATUS_CLOSED
        | KIND_GIT_STATUS_DRAFT
        // A NIP-PC call or result on a project route carries the same marked
        // `["e", root, "", "root"]` every comment does, so it resolves to a
        // root the same way. Its `a` coordinate is read separately as a claim,
        // exactly as for a comment.
        | KIND_PEER_CALL
        | KIND_PEER_CALL_RESULT => sole_reference(tags, "e"),
        _ => None,
    }
}

/// The one unambiguous root reference named `name`, or `None`.
///
/// A `reply` marker on a *separate* tag is fine and ignored — status events
/// legitimately carry `["e", root, "", "root"]` plus
/// `["e", revision, "", "reply"]` (`builders.rs:1230-1234`). What is refused is
/// not knowing which tag is the root.
fn sole_reference<T, S>(tags: &[T], name: &str) -> Option<String>
where
    T: AsRef<[S]>,
    S: AsRef<str>,
{
    let named: Vec<&[S]> = tags
        .iter()
        .map(|t| t.as_ref())
        .filter(|t| t.first().map(|k| k.as_ref()) == Some(name))
        .collect();

    let marked: Vec<&&[S]> = named
        .iter()
        .filter(|t| t.get(3).map(|m| m.as_ref()) == Some("root"))
        .collect();

    if !marked.is_empty() {
        // An explicit marker is a claim about which tag is the root. Two of
        // them is a contradiction, and a malformed one is malformed — neither
        // is an invitation to go looking for something better.
        if marked.len() > 1 {
            return None;
        }
        return marked[0].get(1).and_then(|v| canonical_root_id(v.as_ref()));
    }

    // No marker anywhere: legacy shape. Tolerated only when there is exactly
    // one unmarked candidate, so there is nothing to choose between.
    let unmarked: Vec<&&[S]> = named
        .iter()
        .filter(|t| match t.get(3) {
            None => true,
            Some(m) => m.as_ref().is_empty(),
        })
        .collect();

    if unmarked.len() != 1 {
        return None;
    }
    unmarked[0]
        .get(1)
        .and_then(|v| canonical_root_id(v.as_ref()))
}

// ── Coordinate claims ─────────────────────────────────────────────────────────

/// What an event says about its repository coordinate.
///
/// Three states, not two, because the authority rules treat them differently:
/// lifecycle events may legitimately omit `a` (`GitStatusMeta.repo` is
/// `Option`), but **no** class may carry a malformed or duplicated one. An
/// `Option<String>` collapsed "said nothing" together with "said something
/// incoherent", which would have let a malformed lifecycle event through on the
/// strength of a rule written for genuine absence.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum CoordinateClaim {
    /// No `a` tag at all.
    Absent,
    /// Exactly one non-empty `a`. An unauthenticated claim, preserved verbatim.
    Unique(String),
    /// Value-less, empty, or more than one. Never acceptable.
    Invalid,
}

/// Classify an event's `a` tags.
pub(crate) fn coordinate_claim<T, S>(tags: &[T]) -> CoordinateClaim
where
    T: AsRef<[S]>,
    S: AsRef<str>,
{
    let named: Vec<&[S]> = tags
        .iter()
        .map(|t| t.as_ref())
        .filter(|t| t.first().map(|k| k.as_ref()) == Some("a"))
        .collect();

    match named.len() {
        0 => CoordinateClaim::Absent,
        1 => match named[0].get(1).map(|v| v.as_ref()) {
            Some(v) if !v.is_empty() => CoordinateClaim::Unique(v.to_string()),
            _ => CoordinateClaim::Invalid,
        },
        _ => CoordinateClaim::Invalid,
    }
}

/// Repo owner pubkey from an `a` coordinate (`<kind>:<owner>:<identifier>`).
///
/// Mirrors `repoOwnerFromAddress` in
/// `desktop/src/features/projects/projectIssues.mjs:28-32`, but parses through
/// the strict coordinate validator rather than reaching for the second
/// colon-separated field. This feeds [`lifecycle_actor_allowed`]: a coordinate
/// of the wrong kind, or one missing its identifier, must not be able to
/// nominate a repository owner who can then close watches.
pub(crate) fn repo_owner_from_coordinate(coordinate: &str) -> Option<String> {
    let normalised = normalise_coordinate(coordinate)?;
    normalised.split(':').nth(1).map(str::to_string)
}

/// A syntactically valid repository coordinate: `30617:<owner-hex>:<identifier>`.
///
/// Returns the normalised coordinate (owner lowercased) or `None`. Fails closed
/// on anything else: wrong kind, malformed owner, missing or empty identifier.
/// An identifier may itself contain `:`, so the split is bounded to three parts
/// rather than requiring exactly three.
pub(crate) fn normalise_coordinate(coordinate: &str) -> Option<String> {
    let mut parts = coordinate.splitn(3, ':');
    let kind = parts.next()?;
    let owner = parts.next()?;
    let identifier = parts.next()?;
    if kind != KIND_GIT_REPO_ANNOUNCEMENT.to_string() || identifier.is_empty() {
        return None;
    }
    let owner = canonical_root_id(owner)?;
    Some(format!("{kind}:{owner}:{identifier}"))
}

// ── Verified repository announcements ────────────────────────────────────────

pub(crate) use announcement::VerifiedAnnouncement;

/// The announcement proof, in a private module so its constructor is genuinely
/// the only one.
///
/// Module-level privacy would not be enough here: `project.rs` is one module,
/// so a private-field struct declared at file scope can still be built by
/// struct literal anywhere in this file, tests included. `mod history` already
/// sets the precedent for putting a proof somewhere its invariants cannot be
/// stepped around by a neighbour.
mod announcement {
    use super::{canonical_root_id, sole_value, VerifiedProjectEvent, KIND_GIT_REPO_ANNOUNCEMENT};

    /// A verified event that really is a well-formed repository announcement.
    ///
    /// Previously the relay checked `kind == 30617` and built
    /// `ProjectEvent::Discovery` from the bare verified event, while the *shape*
    /// of the announcement — exactly one non-empty `d` — was only established
    /// later, inside `DiscoveredRepositories::ingest`. Two things followed from
    /// that gap. A malformed `30617` spent a project dedup slot on its way to
    /// being rejected downstream; and a variant named `Discovery` carried
    /// something its type did not oblige to be an announcement at all, so
    /// source admissibility and state admission could quietly disagree.
    ///
    /// The coordinate is computed once, here, from data the signature covers.
    /// Nothing downstream parses `d` a second time.
    #[derive(Debug, Clone)]
    pub(crate) struct VerifiedAnnouncement {
        event: VerifiedProjectEvent,
        coordinate: String,
    }

    impl VerifiedAnnouncement {
        /// Prove an announcement, or refuse.
        ///
        /// `None` for the wrong kind, a missing `d`, an empty `d`, or more than
        /// one `d` — including two that agree, because "two tags that happen to
        /// match" and "one tag" are different claims and only one of them is
        /// unambiguous.
        ///
        /// The owner is the **signer**, never the announcement's own `a`. A
        /// valid signature attests that the signer wrote the event; it says
        /// nothing about whether its contents are honest, so an `a` naming
        /// someone else's repository is attacker-chosen data inside an
        /// otherwise authentic event. Deriving the owner from the signer is
        /// what makes the coordinate unspoofable: you cannot announce a
        /// repository you do not hold the key for.
        pub(crate) fn prove(event: VerifiedProjectEvent) -> Option<Self> {
            if event.kind() != KIND_GIT_REPO_ANNOUNCEMENT {
                return None;
            }
            let owner = canonical_root_id(&event.author())?;
            let identifier = sole_value(&event.tag_vecs(), "d")?;
            let coordinate = format!("{KIND_GIT_REPO_ANNOUNCEMENT}:{owner}:{identifier}");
            Some(Self { event, coordinate })
        }

        /// The coordinate this announcement establishes, already normalised.
        pub(crate) fn coordinate(&self) -> &str {
            &self.coordinate
        }

        pub(crate) fn event(&self) -> &VerifiedProjectEvent {
            &self.event
        }
    }
}

// ── Discovered repositories ───────────────────────────────────────────────────

/// Repository coordinates this agent has actually discovered.
///
/// **Opaque on purpose.** The backing set is private and the only way to add to
/// it is by ingesting a signature-verified announcement, so no caller can
/// assemble a plausible-looking coordinate and hand it to
/// [`validate_enrolment_candidate`] to get a "validated" candidate back. Private fields on the candidate stop
/// struct-literal forgery; this stops validator-assisted forgery, which is the
/// same hole reached one step earlier.
///
/// The only way in is [`Self::ingest`], which takes a [`VerifiedAnnouncement`]
/// and derives the coordinate from the kind, the **signer's** pubkey, and one
/// non-empty `d` tag. An announcement's own `a` claim is never read.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct DiscoveredRepositories {
    coordinates: BTreeSet<String>,
    /// Bytes of every coordinate currently retained.
    ///
    /// Maintained on insertion only. Duplicates cost nothing and must not be
    /// charged, or ordinary repeat traffic on the live REQ would inflate this
    /// until a ceiling tripped on no new data at all.
    retained_bytes: usize,
    /// Set once any ceiling refuses a coordinate. Never cleared.
    ///
    /// See [`DiscoveredRepositories::has_overflowed`].
    overflowed: bool,
    /// How many announcements have been refused. Bounded state, not a log.
    refused: u64,
}

/// How many distinct repository coordinates this agent will hold.
///
/// Deliberately high: tripping it should mean something is wrong, not that a
/// busy relay is busy. The discovery REQ is global (`kinds: [30617]`), so the
/// bound exists because *anyone* can announce a repository, and every valid
/// announcement is a real allocation regardless of whether the agent has any
/// interest in it.
pub(crate) const DISCOVERY_CEILING: usize = 10_000;

/// Longest single coordinate, in bytes.
///
/// A coordinate is `30617:<64 hex>:<d>` — 71 bytes of fixed structure plus an
/// identifier with no length rule of its own. The relay in this repository
/// accepts 512 KiB frames by default and the ACP's WebSocket client takes
/// `connect_async` defaults, so "non-empty" is the only limit the announcement
/// shape imposes, and that is not a limit.
pub(crate) const DISCOVERY_COORDINATE_BYTES: usize = 512;

/// Total bytes of retained coordinates.
///
/// A cardinality bound is not a resource bound. Ten thousand attacker-chosen
/// identifiers at [`DISCOVERY_COORDINATE_BYTES`] each is ~5 MB under the count
/// ceiling alone, and nothing about the number 10,000 says so. Ordinary
/// coordinates run around 80 bytes, so real traffic reaches the count ceiling
/// first (~13,000 would be needed to reach this one); this exists for the case
/// where the identifiers are chosen to be large.
pub(crate) const DISCOVERY_RETAINED_BYTES: usize = 1024 * 1024;

/// Why a coordinate was refused.
///
/// Refusal is **resource admission**, distinct from validity. Everything
/// reaching [`DiscoveredRepositories::ingest`] has already proved its id,
/// signature, kind and `d` shape — a refused announcement is a genuine one this
/// agent has no room for, and reporting it as malformed would be a lie about
/// the publisher.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RefusedBecause {
    /// The set already holds [`DISCOVERY_CEILING`] coordinates.
    Cardinality,
    /// This one coordinate exceeds [`DISCOVERY_COORDINATE_BYTES`].
    CoordinateTooLarge,
    /// Admitting it would exceed [`DISCOVERY_RETAINED_BYTES`].
    RetainedBytes,
}

/// Whether a refusal was the moment the set became degraded.
///
/// Separated so a caller can speak once. Every announcement after the first
/// refusal is also a refusal, and a caller that logged each would have traded
/// an unbounded heap for an unbounded log — with the publisher choosing the
/// contents of both.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Degradation {
    BecameDegraded,
    AlreadyDegraded,
}

/// What [`DiscoveredRepositories::ingest`] did with an announcement.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Discovered {
    /// A coordinate this agent had not seen before.
    Added(String),
    /// Already known. The set is unchanged.
    AlreadyKnown(String),
    /// Refused. **Nothing was evicted.**
    ///
    /// Carries no coordinate, deliberately: the refused value is
    /// attacker-chosen and unbounded, and handing it to a caller is handing it
    /// to a log line. A caller may report the reason and whether this refusal
    /// was the transition into degradation.
    Refused {
        because: RefusedBecause,
        degradation: Degradation,
    },
}

/// A project event whose id hash and Schnorr signature have been checked.
///
/// **The project trust boundary, as a type.** Every project decision reads
/// `event.pubkey`: which human is authorised, who owns the repository, who
/// authored the root, whether a peer is a trusted sibling. `parse_relay_message`
/// deserialises inbound events **without verifying anything**
/// (`relay.rs:3561`), so an unverified project event means a malicious or
/// compromised relay can forge an authorised human's comment, an owner's close,
/// or a root author's reopen — and can also spend a durable dedup slot under
/// the id of a genuine event that has not arrived yet.
///
/// Scoping verification to repository announcements, as I first did, would have
/// left every one of those open. The relay we happen to talk to does verify at
/// ingestion; making that the project authority boundary would be trusting an
/// operational accident.
///
/// This deliberately does **not** change verification for channel traffic,
/// which is a separate behavioural and performance decision.
#[derive(Debug, Clone)]
pub(crate) struct VerifiedProjectEvent {
    event: nostr::Event,
}

/// Why a project event could not be verified.
#[derive(Debug, thiserror::Error)]
pub(crate) enum VerifyError {
    #[error("event failed verification: {0}")]
    Invalid(#[from] buzz_core::error::VerificationError),
    #[error("verification task failed: {0}")]
    TaskJoin(#[from] tokio::task::JoinError),
}

impl VerifiedProjectEvent {
    /// Verify an event, off the async runtime.
    ///
    /// Owned rather than borrowed, and async rather than sync, both on purpose.
    /// The Schnorr check is CPU-bound and `buzz_core::verify_event` requires a
    /// `spawn_blocking` hand-off; doing that inside the constructor means no
    /// caller can get it wrong by not reading a comment. Owning the event is
    /// what makes the hand-off possible at all — a borrow cannot cross into
    /// `spawn_blocking`.
    pub(crate) async fn verify(event: nostr::Event) -> Result<Self, VerifyError> {
        let event = tokio::task::spawn_blocking(move || match buzz_core::verify_event(&event) {
            Ok(()) => Ok(event),
            Err(e) => Err(e),
        })
        .await??;
        Ok(Self { event })
    }

    pub(crate) fn event(&self) -> &nostr::Event {
        &self.event
    }

    pub(crate) fn kind(&self) -> u32 {
        self.event.kind.as_u16() as u32
    }

    /// Author pubkey, lowercase hex. Safe to use for authority decisions
    /// precisely because this type exists.
    pub(crate) fn author(&self) -> String {
        self.event.pubkey.to_hex()
    }

    pub(crate) fn id(&self) -> String {
        self.event.id.to_hex()
    }

    /// Tags as plain string vectors, for the pure helpers in this module.
    pub(crate) fn tag_vecs(&self) -> Vec<Vec<String>> {
        self.event
            .tags
            .iter()
            .map(|t| t.as_slice().to_vec())
            .collect()
    }
}

impl DiscoveredRepositories {
    /// An agent that has discovered nothing yet.
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Ingest a proven repository announcement.
    ///
    /// Infallible, and that is the point: every way this could have failed is
    /// now a way [`VerifiedAnnouncement::prove`] refuses to exist. It used to
    /// take a bare [`VerifiedProjectEvent`] and re-derive the coordinate — kind
    /// check, signer, sole `d` — which meant the *caller* had already decided
    /// the event was admissible on weaker grounds than this function then
    /// applied. Those two judgements could disagree, and did.
    ///
    /// The coordinate is not recomputed here. It was built once, at the proof
    /// boundary, from data the signature covers: the kind, the **signer's**
    /// pubkey, and exactly one non-empty `d`. The announcement's own `a` is
    /// never read, on either side of the boundary.
    pub(crate) fn ingest(&mut self, announcement: &VerifiedAnnouncement) -> Discovered {
        let coordinate = announcement.coordinate();

        // Duplicates first, and before any accounting: a repeat costs nothing
        // and must not be charged, or ordinary live-REQ traffic would inflate
        // the byte total until a ceiling tripped on no new data at all.
        if self.coordinates.contains(coordinate) {
            return Discovered::AlreadyKnown(coordinate.to_string());
        }

        let bytes = coordinate.len();
        let because = if bytes > DISCOVERY_COORDINATE_BYTES {
            Some(RefusedBecause::CoordinateTooLarge)
        } else if self.coordinates.len() >= DISCOVERY_CEILING {
            Some(RefusedBecause::Cardinality)
        } else if self
            .retained_bytes
            .checked_add(bytes)
            .is_none_or(|total| total > DISCOVERY_RETAINED_BYTES)
        {
            // Checked, so an implausible total wrapping around becomes a
            // refusal rather than a suddenly roomy set.
            Some(RefusedBecause::RetainedBytes)
        } else {
            None
        };

        if let Some(because) = because {
            // Refuse and remember. The alternative — evict something to make
            // room — would trade a resource bound for silent authority-state
            // amnesia: a repository would vanish while the set still looked
            // whole, an enrolment that used to be valid would stop being so,
            // and nothing anywhere would say why.
            let degradation = if self.overflowed {
                Degradation::AlreadyDegraded
            } else {
                Degradation::BecameDegraded
            };
            self.overflowed = true;
            self.refused = self.refused.saturating_add(1);
            return Discovered::Refused {
                because,
                degradation,
            };
        }

        self.retained_bytes += bytes;
        let coordinate = coordinate.to_string();
        self.coordinates.insert(coordinate.clone());
        Discovered::Added(coordinate)
    }

    /// Has any ceiling refused an announcement?
    ///
    /// Deliberately **not** `is_complete`. That name would be read as "this
    /// set holds every repository that exists", which it cannot know: it would
    /// answer `true` on an empty set, mid-page, after a single capped 1,000-row
    /// relay page, and after queue loss. A future enrolment opener reads a
    /// name, not a doc comment, and would have treated "the local ceiling has
    /// not tripped" as "the repository universe is known".
    ///
    /// Reconstruction completeness is a different, unbuilt property: completed
    /// pagination, an exact request incarnation, an immutable cutoff, a genuine
    /// EOSE, no resource refusal, and no unrecovered backpressure loss. When
    /// the driver can derive that, it can expose it. This reports the one fact
    /// this type owns.
    ///
    /// Never returns to `false`: the refused announcements are gone and the set
    /// has no way to learn what it missed.
    pub(crate) fn has_overflowed(&self) -> bool {
        self.overflowed
    }

    /// How many announcements have been refused.
    ///
    /// Bounded state a caller can report periodically, rather than logging
    /// every refusal as it happens.
    pub(crate) fn refused_count(&self) -> u64 {
        self.refused
    }

    /// Bytes of coordinate currently retained.
    pub(crate) fn retained_bytes(&self) -> usize {
        self.retained_bytes
    }

    pub(crate) fn contains(&self, coordinate: &str) -> bool {
        self.coordinates.contains(coordinate)
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.coordinates.is_empty()
    }

    pub(crate) fn len(&self) -> usize {
        self.coordinates.len()
    }

    /// Discovered coordinates in deterministic order, for the `#a` filter.
    pub(crate) fn iter(&self) -> impl Iterator<Item = &String> {
        self.coordinates.iter()
    }

    /// Test-only seeding. Deliberately not available in production builds —
    /// otherwise it would be exactly the arbitrary-insertion hole this type
    /// exists to close.
    #[cfg(test)]
    pub(crate) fn for_test<I, S>(coordinates: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        let coordinates: BTreeSet<String> = coordinates.into_iter().map(Into::into).collect();
        Self {
            retained_bytes: coordinates.iter().map(String::len).sum(),
            coordinates,
            overflowed: false,
            refused: 0,
        }
    }
}

// ── Enrolment candidate validation ────────────────────────────────────────────

/// A root this agent may enrol in, after validation.
///
/// **Existence of this value is the proof.** Two things have to hold for that
/// claim to be honest, and both now do: the fields are private, so no sibling
/// module can assemble a struct literal carrying a malformed root or a
/// fabricated issue/PR class; and the validator takes an opaque
/// [`DiscoveredRepositories`], so no caller can supply a hand-made set of
/// plausible coordinates and have the validator bless one. Private fields alone
/// only closed the first of those. Read access is via the accessors below.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct EnrolmentCandidate {
    /// Lowercase hex root event id.
    root: String,
    /// The discovered coordinate, byte-identical to the announced one.
    coordinate: String,
    /// Repository owner, extracted once at validation.
    ///
    /// Carried rather than reparsed by consumers: a validated `30617`
    /// coordinate necessarily names an owner, and re-deriving it downstream
    /// gives that proof a second chance to come back as `None`.
    owner: String,
    /// Who signed the root event, lowercase hex.
    ///
    /// **The only moment this is knowable.** A `1632` close names the root by
    /// `e` and carries no statement about who opened it, so an agent that did
    /// not keep the root's author when it had the root in hand can never
    /// afterwards decide whether a closer is the author. The root event is the
    /// one place the answer exists, and this is the type that has been past it.
    root_author: String,
    /// `true` for a `1618` pull-request root, `false` for a `1621` issue.
    is_pull_request: bool,
}

impl EnrolmentCandidate {
    pub(crate) fn root(&self) -> &str {
        &self.root
    }

    pub(crate) fn coordinate(&self) -> &str {
        &self.coordinate
    }

    pub(crate) fn owner(&self) -> &str {
        &self.owner
    }

    pub(crate) fn root_author(&self) -> &str {
        &self.root_author
    }

    pub(crate) fn is_pull_request(&self) -> bool {
        self.is_pull_request
    }

    /// Test-only construction. Not available in production builds, where the
    /// validator is the sole route.
    #[cfg(test)]
    pub(crate) fn for_test(
        root: &str,
        coordinate: &str,
        owner: &str,
        root_author: &str,
        is_pull_request: bool,
    ) -> Self {
        Self {
            root: root.to_string(),
            coordinate: coordinate.to_string(),
            owner: owner.to_string(),
            root_author: root_author.to_string(),
            is_pull_request,
        }
    }
}

/// The runtime identity and configuration the authority gate is evaluated
/// against.
///
/// Grouped into one borrow so a caller cannot supply the agent's own key from
/// one place and its owner from another, which is how a self-authored event
/// would end up compared against somebody else's identity. Every field is read
/// from live configuration; none of them is a decision.
#[derive(Debug, Clone, Copy)]
pub(crate) struct ProjectIdentity<'a> {
    pub agent: &'a AgentIdentity,
    pub agent_owner: Option<&'a str>,
    pub approved_humans: &'a BTreeSet<String>,
    pub approved_external_agents: &'a BTreeSet<String>,
}

/// What the process currently knows, as distinct from who it is.
///
/// Grouped because these are the inputs that change as the process runs, and
/// because a decision derived from one event's discovery set and another's
/// enrolment set would be incoherent. Read-only: deciding never mutates state.
#[derive(Debug, Clone, Copy)]
pub(crate) struct ProjectState<'a> {
    pub discovered: &'a DiscoveredRepositories,
    pub enrolments: &'a ProjectEnrolments,
    /// How complete this process's history for the root is. Supplied by the
    /// caller because it is a property of what the process holds, not of the
    /// event. A caller with no reconstruction reports
    /// [`RootHistoryReadiness::Unknown`].
    pub readiness: &'a RootHistoryReadiness,
    /// A completed NIP-OA sibling attestation, when one was performed.
    pub sibling: Option<&'a VerifiedSibling>,
}

/// What the gate decided, and the binding that survives with it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ProjectDecision {
    pub effect: ProjectEffect,
    /// The author class this decision was taken under.
    ///
    /// Returned rather than recomputed by callers that need it. A peer call's
    /// loop controls run after this gate, and deriving the caller's trust a
    /// second time from the same inputs would be a second place for the answer
    /// to differ from the one the effect was actually chosen by.
    pub author: ProjectAuthor,
    /// Present only when the effect is one that queues or enrols. An effect
    /// that never reaches the queue carries no origin, so a caller cannot
    /// queue a refused event by reading a binding off the decision anyway.
    pub origin: Option<ProjectOrigin>,
}

/// Compose the accepted primitives against live runtime state.
///
/// This is the whole authority path in one place, deliberately: the inputs to
/// [`classify_project_event`] are each derived by exactly one accepted
/// primitive, and gathering them here is what stops a caller assembling them
/// from convenient booleans. Nothing in this function decides anything itself —
/// it derives, then delegates.
///
/// `readiness` is supplied by the caller because it is a property of the
/// history the process actually holds, not of this event. A caller with no
/// reconstruction reports [`RootHistoryReadiness::Unknown`], which
/// [`resolve_addressing`] treats conservatively.
pub(crate) fn decide_project_event(
    source: &ProjectSubscription,
    route: &ProjectRoute,
    event: &VerifiedProjectEvent,
    identity: ProjectIdentity<'_>,
    state: ProjectState<'_>,
    resolved_candidate: Option<&EnrolmentCandidate>,
) -> ProjectDecision {
    let ignored = ProjectDecision {
        effect: ProjectEffect::Ignore,
        origin: None,
        author: ProjectAuthor::Untrusted,
    };

    // Addressing first, because a discovery-sourced event has no addressing at
    // all and must not be pushed further — `resolve_addressing` returns `None`
    // for it rather than guessing.
    let kind_effect = classify_kind(event.kind());
    let root_state = state.enrolments.state_of(route.root());
    let evidence = AddressingEvidence::resolve(event, identity.agent);
    let Some(addressing) =
        resolve_addressing(source, &evidence, state.readiness, None, identity.agent)
    else {
        return ignored;
    };
    // **A separately fetched root is a binding, and only a binding.**
    //
    // It used to be addressing as well: on an unknown root, a comment on the
    // enrolment subscription whose exact `p` carried this agent was promoted to
    // `Addressing::ExplicitMention` outright, on the argument that the `p`
    // transport plus a verified matching root was the complete comment-first
    // proof and that Desktop's display-name text could not be read here.
    //
    // Both halves have since stopped being true. `named_self` reads the
    // display name Desktop actually writes, so a real `@Claude` no longer needs
    // rescuing by a promotion — it resolves explicitly on its own. And the `p`
    // is not the exact transport the promotion assumed: Desktop stamps the
    // repository owner onto every root it creates and copies every prior
    // recipient into every later comment, so on an agent-owned project a
    // *first* comment on a root the agent was never addressed in still carries
    // the agent's key. That is the shape the live Phase 3d run failed on —
    // root `d2986fa7…` content `test`, correctly ignored, then comment
    // `74f92354…` addressed to `@hermes-gateway` with both parties `p`-tagged,
    // which the promotion turned into an explicit mention of this agent and a
    // turn.
    //
    // The binding half is kept, and it is the half that was ever load-bearing:
    // a comment that genuinely names this agent on a root this process has
    // never seen has no enrolment candidate of its own, and without the fetched
    // root there is nothing to enrol under.
    let resolved_binding = resolved_candidate.filter(|candidate| {
        candidate.root() == route.root()
            && matches!(route.coordinate_claim(), CoordinateClaim::Unique(value) if value == candidate.coordinate())
    });
    let author = classify_project_author(
        event,
        identity.agent,
        identity.agent_owner,
        identity.approved_humans,
        state.sibling,
        identity.approved_external_agents,
    );

    // A root event carries its own candidate; a comment-first mention carries a
    // separately fetched and verified root candidate. Keep the two witnesses
    // distinct: the directing comment supplies authority/addressing, while the
    // root supplies only the durable repository binding.
    let candidate =
        validate_enrolment_candidate(event, state.discovered).or_else(|| resolved_binding.cloned());
    let stored = state.enrolments.get(route.root());

    // Lifecycle authority is a property of the root's **stored binding**, and
    // of nothing the arriving event says about itself.
    //
    // The version this replaces asked `validate_enrolment_candidate` for it.
    // That validator accepts only `1621` and `1618`, so for a `1630`-`1633` it
    // necessarily returned `None` and the authority was unreachable: every
    // lifecycle event on every root, however impeccably signed by the owner,
    // was `Ignore`. A real owner close on a real relay left the watch active,
    // and the next comment woke a turn on an issue that had been closed.
    //
    // Worse than unreachable, it was also incoherent — it passed the *closing*
    // event's author as the root's author, so the one shape it could ever have
    // admitted was "anyone who signs a close is the author of the root they are
    // closing". The stored binding is the only thing that knows who opened the
    // root, and it learned it from the root's own signature.
    //
    // No binding, no authority: a lifecycle event on a root this agent is not
    // watching has nothing to authorise against and is refused.
    let lifecycle_authorised = stored.is_some_and(|enrolment| {
        lifecycle_actor_allowed(&event.author(), &enrolment.root_author, &enrolment.owner)
    });

    let effect = classify_project_event(
        kind_effect,
        author,
        // NIP-PC: what the event *is*, never whether it is allowed. A call from
        // an untrusted stranger still reports `Invocation` here and is refused
        // by the author arm below, which is what keeps the authority decision
        // in one place instead of half-hidden in a parser.
        project_call_marker(event, identity.agent),
        root_state,
        addressing,
        lifecycle_authorised,
        evidence.directed_at_another_party(),
    );

    let origin = match effect {
        ProjectEffect::Enrol | ProjectEffect::EnrolAndWake => {
            candidate.as_ref().map(ProjectOrigin::from_candidate)
        }
        // `ResumeCall` sits here with `Wake`: a result resumes a call on a root
        // this agent is already enrolled in, so its binding is the stored
        // enrolment. Leaving it originless meant every result was dropped by
        // the origin guard before the loop controls could see it, which made
        // correlation unreachable in production.
        ProjectEffect::Wake | ProjectEffect::RefreshContext | ProjectEffect::ResumeCall => stored
            .map(|enrolment| ProjectOrigin::from_enrolment(route.root(), enrolment))
            .or_else(|| candidate.as_ref().map(ProjectOrigin::from_candidate)),
        ProjectEffect::Ignore | ProjectEffect::UntrustedContext | ProjectEffect::ApplyLifecycle => {
            None
        }
    };

    // An effect that needs a binding and has none cannot proceed. This is the
    // mismatched-`a` refusal: the event named a repository that no discovered
    // announcement authorises, so there is no validated coordinate to enrol
    // under and nothing downstream may be spent on it.
    if matches!(
        effect,
        ProjectEffect::Enrol | ProjectEffect::EnrolAndWake | ProjectEffect::Wake
    ) && origin.is_none()
    {
        return ignored;
    }

    ProjectDecision {
        effect,
        origin,
        author,
    }
}

/// Where a queued project event came from, carried beside the UUIDv5 queue key.
///
/// The queue key is a `Uuid` because project events reuse the channel-keyed
/// queue and session machinery. That reuse is deliberate, but it leaves the key
/// unable to say what it names: a UUIDv5 derived from a root is
/// indistinguishable from a real channel UUID by inspection. This type is the
/// thing that says so, and it travels with the event rather than being looked
/// up later — a lookup would have to consult something channel-shaped to
/// resolve a key that names no channel, which is exactly the confusion being
/// avoided.
///
/// **No string constructor.** Both routes take an already-validated source:
/// [`EnrolmentCandidate`], which only [`validate_enrolment_candidate`]
/// produces, or a stored [`Enrolment`], which only [`ProjectEnrolments::enrol`]
/// writes. A `ProjectOrigin` therefore cannot describe a repository binding
/// that was never validated.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ProjectOrigin {
    coordinate: String,
    root: String,
    is_pull_request: bool,
}

impl ProjectOrigin {
    /// From the candidate that just enrolled or reactivated the root.
    pub(crate) fn from_candidate(candidate: &EnrolmentCandidate) -> Self {
        Self {
            coordinate: candidate.coordinate().to_string(),
            root: candidate.root().to_string(),
            is_pull_request: candidate.is_pull_request(),
        }
    }

    /// From the binding already stored for an enrolled root — the continuation
    /// case, where no fresh candidate exists because the event is a comment
    /// rather than a root.
    pub(crate) fn from_enrolment(root: &str, enrolment: &Enrolment) -> Self {
        Self {
            coordinate: enrolment.coordinate.clone(),
            root: root.to_string(),
            is_pull_request: enrolment.is_pull_request,
        }
    }

    pub(crate) fn coordinate(&self) -> &str {
        &self.coordinate
    }

    pub(crate) fn root(&self) -> &str {
        &self.root
    }

    pub(crate) fn is_pull_request(&self) -> bool {
        self.is_pull_request
    }

    /// Test-only construction. Production has exactly two routes, both from an
    /// already-validated source; this exists so prompt-rendering tests need not
    /// stand up a whole enrolment to check a string.
    #[cfg(test)]
    pub(crate) fn for_test(coordinate: &str, root: &str, is_pull_request: bool) -> Self {
        Self {
            coordinate: coordinate.to_string(),
            root: root.to_string(),
            is_pull_request,
        }
    }

    /// How the class reads in a prompt.
    pub(crate) fn class_noun(&self) -> &'static str {
        if self.is_pull_request {
            "pull request"
        } else {
            "issue"
        }
    }

    /// The CLI surface that answers on this class of root.
    ///
    /// The two commands emit the identical event — `buzz pr comment` delegates
    /// to the issue path, one builder, one shape — so this is not about which
    /// one works. It is about the prompt not contradicting itself: the sentence
    /// above the command says "to reply on this pull request", and following it
    /// with `buzz issues comment` invites the agent to conclude that the harness
    /// has handed it the wrong command and to go looking for a better one. The
    /// class is already known here, so naming the matching surface costs
    /// nothing.
    pub(crate) fn reply_command(&self) -> &'static str {
        if self.is_pull_request {
            "buzz pr comment"
        } else {
            "buzz issues comment"
        }
    }
}

/// Validate a root event as an enrolment candidate. **Fails closed.**
///
/// Every condition below has to hold, and any one of them failing yields
/// `None` rather than a partially-trusted enrolment:
///
/// - the kind is `1621` or `1618` — nothing else is a root;
/// - the root id is a real 64-char hex event id;
/// - the root carries **exactly one** `a` tag. Zero is unroutable; two is
///   ambiguous, and accepting the first would let a forged root smuggle a
///   known coordinate past the gate while a second tag says something else;
/// - that `a` value is **byte-identical** to a coordinate this agent actually
///   discovered from a `kind:30617` announcement, as attested by the opaque
///   [`DiscoveredRepositories`] rather than by a set the caller built.
///
/// The last point is a string equality check on purpose. `a` is an
/// unauthenticated claim, so the discovered set is the only authority — and
/// matching on a *parsed* form would quietly make a non-canonical coordinate
/// equivalent to the canonical discovered one behind the validator's back.
/// Parsing stays available for diagnostics; it does not widen acceptance.
/// Validate a verified root event as an enrolment candidate. **Fails closed.**
///
/// Takes the one witness and reads kind, id and tags from it. The previous
/// signature accepted those three decomposed and caller-selected, which meant a
/// caller could take a genuine verified root, reuse its id and kind, and supply
/// tags naming a *different* discovered repository — private fields on the
/// result do not help when the constructor accepts assembled evidence.
///
/// Every condition must hold:
///
/// - the kind is `1621` or `1618` — nothing else is a root;
/// - the id is a real 64-char hex event id;
/// - the event carries **exactly one** `a` tag. Zero is unroutable; two is
///   ambiguous, and first-wins would let a forged root smuggle a known
///   coordinate past the gate while a second tag says otherwise;
/// - that `a` value is **byte-identical** to a coordinate this agent actually
///   discovered from a `kind:30617` announcement.
///
/// The last point is string equality on purpose. `a` is an unauthenticated
/// claim, so the discovered set is the only authority — and matching a *parsed*
/// form would quietly make a non-canonical coordinate equivalent to the
/// canonical discovered one behind the validator's back.
pub(crate) fn validate_enrolment_candidate(
    event: &VerifiedProjectEvent,
    discovered: &DiscoveredRepositories,
) -> Option<EnrolmentCandidate> {
    let is_pull_request = match event.kind() {
        KIND_GIT_ISSUE => false,
        KIND_GIT_PULL_REQUEST => true,
        _ => return None,
    };
    let root = canonical_root_id(&event.id())?;
    let coordinate = sole_value(&event.tag_vecs(), "a")?;
    if !discovered.contains(&coordinate) {
        return None;
    }
    // Membership is checked first, so this parses a coordinate the discovered
    // set already vouched for; it cannot widen acceptance.
    let owner = repo_owner_from_coordinate(&coordinate)?;
    Some(EnrolmentCandidate {
        root,
        coordinate,
        owner,
        // The signer of this very event. It is a fact about the root, taken
        // from the proof that this process verified the signature — never from
        // a later event's claim about who the author was.
        root_author: event.author(),
        is_pull_request,
    })
}

/// The value of the one and only tag named `name`.
///
/// `None` for zero tags *and* for more than one. Callers use this where a
/// repeated tag is not merely redundant but ambiguous, so "take the first" is
/// the wrong answer rather than a convenient one.
///
/// A value-less `["a"]` or an empty `["a", ""]` is rejected here rather than
/// left for a downstream membership check to fall over. Relying on "the
/// discovered set happens not to contain an empty string" would make this
/// function's safety a property of its caller's data.
fn sole_value<T, S>(tags: &[T], name: &str) -> Option<String>
where
    T: AsRef<[S]>,
    S: AsRef<str>,
{
    let mut found: Option<String> = None;
    for tag in tags {
        let tag = tag.as_ref();
        if tag.first().map(|k| k.as_ref()) != Some(name) {
            continue;
        }
        if found.is_some() {
            return None;
        }
        let value = tag.get(1)?.as_ref();
        if value.is_empty() {
            return None;
        }
        found = Some(value.to_string());
    }
    found
}

/// Does a follow-up event on a watched root carry an acceptable coordinate?
///
/// Consumes a classified [`CoordinateClaim`] rather than re-parsing tags, so
/// the absent/invalid distinction cannot be lost between the two.
///
/// | Kind | `Absent` | `Unique` | `Invalid` |
/// |---|---|---|---|
/// | `1` comment | reject — `projectIssues.mjs` always emits `a` | must match exactly | reject |
/// | `1619` PR update | reject — `builders.rs:1434` always emits `a` | must match exactly | reject |
/// | `1630`-`1633` | accept — root-bound by `e`, and `GitStatusMeta.repo` is `Option` | must match exactly | reject |
///
/// `Invalid` is refused for every class. Two coordinates on one event is
/// ambiguity, not redundancy, and a value-less tag is malformed rather than
/// absent.
pub(crate) fn follow_up_coordinate_allowed(
    kind: u32,
    claim: &CoordinateClaim,
    enrolled: &str,
) -> bool {
    match claim {
        CoordinateClaim::Invalid => false,
        CoordinateClaim::Unique(c) => c == enrolled,
        CoordinateClaim::Absent => matches!(
            kind,
            KIND_GIT_STATUS_OPEN
                | KIND_GIT_STATUS_MERGED
                | KIND_GIT_STATUS_CLOSED
                | KIND_GIT_STATUS_DRAFT
        ),
    }
}

// ── Enrolment sets ────────────────────────────────────────────────────────────

/// One enrolled root.
///
/// **The binding is what later events are authorised against, so it carries
/// everything that authority needs.** A lifecycle event names a root and a
/// signer and nothing else; if the enrolment does not remember who opened the
/// root and who owns the repository, there is no way to decide the closer is
/// one of them, and the only reachable answer becomes "refuse everything". That
/// was the defect: authority was re-derived from the arriving event, which
/// could only ever produce a candidate for a root kind, so a valid owner close
/// was classified `Ignore` and the watch stayed active.
///
/// Both fields are functions of the root's own signed event, which is why
/// carrying them cannot weaken the immutability check below: a root id commits
/// to its author, and a coordinate to its owner, so a candidate that disagrees
/// with a stored binding on either is a forged or confused claim exactly as a
/// coordinate mismatch is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Enrolment {
    pub coordinate: String,
    /// Repository owner, from the validated coordinate.
    pub owner: String,
    /// Who signed the root event, lowercase hex.
    pub root_author: String,
    pub is_pull_request: bool,
}

/// What [`ProjectEnrolments::enrol`] did.
///
/// Distinguished from a bare boolean because the caller has two separate
/// questions — "must I replace the watched-root REQ?" (`Enrolled` or
/// `Reactivated`) and "what should I log?" — and a boolean answered neither
/// unambiguously.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum EnrolOutcome {
    /// A root we were not watching is now active.
    Enrolled,
    /// A dormant root is active again.
    Reactivated,
    /// Already active under this exact binding. No subscription change.
    Unchanged,
}

impl EnrolOutcome {
    /// Does this outcome require the watched-root REQ to be replaced?
    pub(crate) fn changes_subscription(self) -> bool {
        matches!(self, Self::Enrolled | Self::Reactivated)
    }
}

/// A candidate disagreed with a root's existing repository binding.
///
/// Carries both sides so the refusal is diagnosable rather than a silent drop:
/// which root, what it is bound to, and what tried to replace it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct BindingMismatch {
    pub root: String,
    pub existing: Enrolment,
    pub attempted: Enrolment,
}

/// The active and dormant enrolment sets.
///
/// Two sets, not one flag, because they are subscribed identically and treated
/// differently: **both** stay in the watched-root REQ so an authorised reopen
/// is observable, while only `active` delivers comments. Dropping a closed root
/// from the subscription would make reopen unobservable — nothing would be
/// listening for the event that revives the watch.
///
/// `BTreeMap`/`BTreeSet` rather than hash containers so the REQ filter's tag
/// lists are deterministically ordered. A REQ that reshuffles its `#e` list
/// between reconnects is needlessly hard to diff when something goes wrong.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct ProjectEnrolments {
    active: BTreeMap<String, Enrolment>,
    dormant: BTreeMap<String, Enrolment>,
}

impl ProjectEnrolments {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Enrol a validated candidate, or reactivate it if dormant.
    ///
    /// **A root's repository binding is immutable.** Once a root is enrolled
    /// under a coordinate and a class, no later event may move it to another
    /// repository or flip it between issue and pull request. A root id is a
    /// signed event that already committed to its own `a` tag, so a candidate
    /// disagreeing with the stored binding is not a legitimate update — it is a
    /// forged or confused claim, and applying it would silently relocate a live
    /// watch. Both mismatch paths are refused with the existing binding intact.
    ///
    /// The mismatch is boxed because it carries two whole bindings and the
    /// success case carries a discriminant: an unboxed error would make every
    /// ordinary enrolment pay for the diagnosis of a refusal that, in a healthy
    /// process, never happens.
    pub(crate) fn enrol(
        &mut self,
        candidate: &EnrolmentCandidate,
    ) -> Result<EnrolOutcome, Box<BindingMismatch>> {
        let attempted = Enrolment {
            coordinate: candidate.coordinate().to_string(),
            owner: candidate.owner().to_string(),
            root_author: candidate.root_author().to_string(),
            is_pull_request: candidate.is_pull_request(),
        };

        if let Some(existing) = self.get(candidate.root()) {
            if *existing != attempted {
                return Err(Box::new(BindingMismatch {
                    root: candidate.root().to_string(),
                    existing: existing.clone(),
                    attempted,
                }));
            }
        }

        if self.active.contains_key(candidate.root()) {
            // Already watching, same binding: an ordinary re-mention. Reporting
            // no change keeps it from churning the watched-root REQ.
            return Ok(EnrolOutcome::Unchanged);
        }

        if self.dormant.remove(candidate.root()).is_some() {
            self.active.insert(candidate.root().to_string(), attempted);
            return Ok(EnrolOutcome::Reactivated);
        }

        self.active.insert(candidate.root().to_string(), attempted);
        Ok(EnrolOutcome::Enrolled)
    }

    /// Move an active root to dormant. Returns `true` if anything changed.
    ///
    /// The root stays subscribed; only comment delivery stops.
    pub(crate) fn close(&mut self, root: &str) -> bool {
        match self.active.remove(root) {
            Some(enrolment) => {
                self.dormant.insert(root.to_string(), enrolment);
                true
            }
            None => false,
        }
    }

    /// Move a dormant root back to active. Returns `true` if anything changed.
    ///
    /// Only ever called for an *authorised* reopen; authority is decided by
    /// [`lifecycle_actor_allowed`] before this point.
    pub(crate) fn reopen(&mut self, root: &str) -> bool {
        match self.dormant.remove(root) {
            Some(enrolment) => {
                self.active.insert(root.to_string(), enrolment);
                true
            }
            None => false,
        }
    }

    pub(crate) fn state_of(&self, root: &str) -> RootState {
        if self.active.contains_key(root) {
            RootState::Active
        } else if self.dormant.contains_key(root) {
            RootState::Dormant
        } else {
            RootState::Unknown
        }
    }

    pub(crate) fn get(&self, root: &str) -> Option<&Enrolment> {
        self.active.get(root).or_else(|| self.dormant.get(root))
    }

    /// Every enrolled root, active and dormant, for the `#e` filter.
    pub(crate) fn all_roots(&self) -> Vec<String> {
        let mut roots: Vec<String> = self
            .active
            .keys()
            .chain(self.dormant.keys())
            .cloned()
            .collect();
        roots.sort();
        roots.dedup();
        roots
    }

    /// Pull-request roots only, for the uppercase `#E` filter.
    pub(crate) fn pull_request_roots(&self) -> Vec<String> {
        let mut roots: Vec<String> = self
            .active
            .iter()
            .chain(self.dormant.iter())
            .filter(|(_, e)| e.is_pull_request)
            .map(|(root, _)| root.clone())
            .collect();
        roots.sort();
        roots.dedup();
        roots
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.active.is_empty() && self.dormant.is_empty()
    }

    pub(crate) fn active_count(&self) -> usize {
        self.active.len()
    }

    pub(crate) fn dormant_count(&self) -> usize {
        self.dormant.len()
    }
}

// ── Subscription filters ──────────────────────────────────────────────────────

/// NIP-01 filter for the enrolment REQ: events that tag this agent on a project
/// we know about.
///
/// `#a` scopes to discovered repositories at the *relay*, so an ordinary social
/// note that happens to `p`-tag the agent never reaches this subscription. A
/// bare `kind:1 + #p` filter would drag every mention on the relay into the
/// project path and rely on client-side filtering to undo it.
///
/// Returns `None` when there are no known coordinates: a filter with an empty
/// `#a` list matches nothing at some relays and *everything* at others, and
/// "accidentally subscribe to all of kind 1" is not a failure worth risking.
///
/// **A live tail, and nothing else.** It carries no row limit, so it is
/// open-ended forwards and asks for no history of its own. It used to reach
/// back thirty days for up to five hundred rows, which was the restart case
/// being answered by the wrong request: a fixed-identity standing REQ cannot
/// page, so the reach-back could only ever sample. History is
/// [`EnrolmentReconstruction`]'s question now, on its own generation-distinct
/// requests, and this one keeps running unreplaced while that walk paginates.
///
/// **`since` is floored a full accepted-skew interval below the caller's
/// watermark, and that is not an approximation.** A relay evaluates `since`
/// against the event's *signed* `created_at`, not against when it arrived. An
/// event whose author's clock runs slow is accepted for storage — the ingest
/// gate allows [`ACCEPTED_CLOCK_SKEW_SECS`] in either direction — and then
/// silently fails a `since` set at this process's own startup. It is dropped at
/// the relay, so no amount of care about how this agent classifies what it
/// receives can recover it; it never arrives. That was a real miss on a real
/// relay: a `1621` addressed to two agents, accepted with `created_at` 387
/// seconds before their startup, delivered to neither.
///
/// Widening the floor pulls stored rows from the overlap into the tail's
/// backlog. They are not turns: the backlog of a tail registration is
/// classified by [`ProjectRequests::enrolment_frame_mode`] from the
/// registration that admitted it, so what the overlap costs is one prefix of
/// context frames, not a re-answered issue.
pub(crate) fn enrolment_filter(
    discovered: &DiscoveredRepositories,
    agent_pubkey_hex: &str,
    since: u64,
) -> Option<Value> {
    if discovered.is_empty() {
        return None;
    }
    let coords: Vec<&String> = discovered.iter().collect();
    Some(json!({
        "kinds": [KIND_GIT_ISSUE, KIND_GIT_PULL_REQUEST, KIND_TEXT_NOTE],
        "#a": coords,
        "#p": [agent_pubkey_hex],
        "since": since.saturating_sub(ACCEPTED_CLOCK_SKEW_SECS),
    }))
}

/// How far from server time the relay will accept an event's `created_at`.
///
/// Mirrors `MAX_TIMESTAMP_DRIFT_SECS` in
/// `crates/buzz-relay/src/handlers/ingest.rs`, which is applied to every kind
/// after signature verification. It is the exact width of the interval in which
/// "already stored" and "published just now" are indistinguishable from the
/// timestamp alone, so it is the exact amount a live tail must reach back to be
/// gapless.
///
/// Deliberately a named constant equal to the relay's, not a margin chosen to
/// feel safe. Too small silently drops accepted events; too large is a longer
/// context prefix on every reconnect for no coverage the relay would honour.
pub(crate) const ACCEPTED_CLOCK_SKEW_SECS: u64 = 900;

/// NIP-01 filters for the watched-root REQ.
///
/// Two filters, because the two reference styles are not interchangeable:
/// comments and status events point at the root with lowercase `e`, while a PR
/// update points at it with **uppercase `E`** (`buzz-sdk/src/builders.rs:1444`).
/// A single lowercase filter silently drops every PR revision.
///
/// Both active and dormant roots appear in the `#e` list — dormant roots are
/// subscribed precisely so a reopen is observable.
///
/// Returns an empty vector when nothing is enrolled, so the caller sends no REQ
/// at all rather than one that matches everything.
pub(crate) fn watched_roots_filters(enrolments: &ProjectEnrolments, since: u64) -> Vec<Value> {
    let mut filters = Vec::new();

    let roots = enrolments.all_roots();
    if !roots.is_empty() {
        filters.push(json!({
            "kinds": HistoryStream::Comments.kinds(),
            HistoryStream::Comments.root_tag(): roots,
            "since": since,
        }));
    }

    let pr_roots = enrolments.pull_request_roots();
    if !pr_roots.is_empty() {
        filters.push(json!({
            "kinds": HistoryStream::PullRequestUpdates.kinds(),
            HistoryStream::PullRequestUpdates.root_tag(): pr_roots,
            "since": since,
        }));
    }

    filters
}

/// The full set of project REQ frames to send, or empty when project routing is
/// off.
///
/// **This is the R1 gate in its load-bearing position.** With the flag disabled
/// the function returns an empty vector before touching coordinates or
/// enrolments, so no project REQ can be constructed — which is what makes
/// "flag off issues no project REQ" checkable by inspecting frames rather than
/// by observing that nothing happened.
pub(crate) fn project_req_frames(
    enabled: bool,
    discovered: &DiscoveredRepositories,
    enrolments: &ProjectEnrolments,
    agent_pubkey_hex: &str,
    since: u64,
) -> Vec<Value> {
    if !enabled {
        return Vec::new();
    }

    let mut frames = Vec::new();
    if let Some(filter) = enrolment_filter(discovered, agent_pubkey_hex, since) {
        frames.push(json!(["REQ", PROJECT_ENROL_SUB_ID, filter]));
    }
    let watched = watched_roots_filters(enrolments, since);
    if !watched.is_empty() {
        let mut frame = vec![json!("REQ"), json!(PROJECT_ROOTS_SUB_ID)];
        frame.extend(watched);
        frames.push(Value::Array(frame));
    }
    frames
}

// ── Route ─────────────────────────────────────────────────────────────────────

/// Where a project event belongs: the root it is about, and the deterministic
/// session key derived from it.
///
/// Constructed only from a [`VerifiedProjectEvent`], so a route cannot be
/// invented for an event whose author or contents were never checked. The key
/// is the UUIDv5 of the root, which is what lets project events reuse the
/// channel-keyed session, queue and dedup machinery untouched.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ProjectRoute {
    key: Uuid,
    root: String,
    claim: CoordinateClaim,
}

impl ProjectRoute {
    /// Derive the route for a verified project event.
    ///
    /// `None` when the event does not resolve to a project root at all — an
    /// unrelated kind, or a missing/malformed root reference. An event we
    /// cannot place is dropped rather than routed somewhere plausible.
    pub(crate) fn derive(verified: &VerifiedProjectEvent) -> Option<Self> {
        let tags = verified.tag_vecs();
        let root = root_event_id(verified.kind(), &verified.id(), &tags)?;
        let key = project_route_key(&root)?;
        Some(Self {
            key,
            root,
            claim: coordinate_claim(&tags),
        })
    }

    /// The UUIDv5 session/queue key.
    pub(crate) fn key(&self) -> Uuid {
        self.key
    }

    /// Lowercase hex root event id.
    pub(crate) fn root(&self) -> &str {
        &self.root
    }

    /// What this event claims about its repository, classified.
    ///
    /// A claim, not an authority: [`follow_up_coordinate_allowed`] decides
    /// whether it is acceptable against the enrolled coordinate. Returned as a
    /// [`CoordinateClaim`] so absence and incoherence stay distinguishable all
    /// the way to the gate.
    pub(crate) fn coordinate_claim(&self) -> &CoordinateClaim {
        &self.claim
    }
}

/// A verified project event, tagged with where it came from.
///
/// Discovery is a separate variant rather than a `Routed` with an empty route:
/// a `kind:30617` announcement has no root, so pushing it through
/// [`ProjectRoute::derive`] would correctly yield nothing and then be silently
/// dropped — the announcement would vanish through a code path that looks like
/// it handled it.
///
/// **Nothing about reconstruction crosses here.** Neither a catch-up frame nor
/// an end-of-backlog boundary becomes one of these. Both are handled in the
/// relay task, beside the registry that admitted them: carrying a frame would
/// put a queue — with a `Full` arm — between a page and the rows it counts, and
/// a page short by a dropped row reads as the end of history. Carrying a
/// boundary would put a *cloneable capability* on a queue for a consumer that
/// holds no page and can do nothing with it.
#[derive(Debug, Clone)]
pub(crate) enum ProjectEvent {
    /// A repository announcement. Feeds [`DiscoveredRepositories::ingest`].
    ///
    /// Carries a [`VerifiedAnnouncement`], not a bare verified event, so the
    /// variant's name and its type agree. Naming a variant `Discovery` while
    /// its payload was merely "some event that passed a kind check" let a
    /// malformed `30617` cross this boundary and be rejected somewhere further
    /// on, having already spent a dedup slot to get there.
    Discovery { announcement: VerifiedAnnouncement },
    /// An event about a specific root.
    ///
    /// `source` is carried rather than re-inferred downstream. Authority and
    /// effect differ between a live enrolment mention, a watched continuation
    /// and a historical reconstruction, and re-deriving that from the event
    /// shape would be guessing at something already known.
    ///
    /// `mode` is carried for a stronger version of the same reason: it is not
    /// derivable downstream *at all*. A `processing_mode_for(source)` lookup
    /// stood here and was wrong by construction — the enrolment class covers
    /// both a tail's stored-events prefix and everything live that follows it,
    /// and one value cannot answer for both. Only the relay task holds what
    /// separates them, which is the registration that admitted the frame, so
    /// the relay task is the only thing that may say. Everything downstream
    /// reads this field; nothing recomputes it.
    Routed {
        source: ProjectSubscription,
        route: ProjectRoute,
        event: VerifiedProjectEvent,
        mode: ProcessingMode,
    },
}

// ── Event class ───────────────────────────────────────────────────────────────

/// Enrolment state of a root, as tracked by the two enrolment sets.
///
/// Closing a root moves it to `Dormant` rather than dropping it: the root stays
/// in the watched-root REQ so a later authorised reopen is still observed.
/// Unsubscribing entirely would make reopen unobservable, because nothing would
/// be listening for the event that revives the watch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RootState {
    /// Enrolled and receiving comments.
    Active,
    /// Enrolled, closed or resolved: lifecycle only, no comment delivery.
    Dormant,
    /// Not enrolled. Only an enrolment signal can change this.
    Unknown,
}

/// What a delivered project event is allowed to do, based on its kind and the
/// state of the root it lands on. Not every delivered event is a turn.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum KindEffect {
    /// A comment. May become a turn — subject to the author gate.
    Comment,
    /// A lifecycle status change. **Never** a model turn, and honoured only
    /// from an authorised actor.
    Lifecycle,
    /// A PR revision. Refreshes context; **never** a model turn by itself.
    ContextRefresh,
    /// An issue or PR root. May enrol — subject to the author gate.
    Root,
    /// Nothing to do.
    Ignore,
}

/// Classify a delivered event by kind alone.
///
/// Root state deliberately plays no part here. An earlier version dropped
/// comments on dormant roots at this layer, which made the plan's "an explicit
/// re-tag reactivates a dormant enrolment" unreachable — the event never
/// survived to the point where anything could tell a re-tag from an inherited
/// participant tag. Suppressing dormant comments is an *authority* decision, so
/// it lives in [`classify_project_event`] where the addressing is known.
pub(crate) fn classify_kind(kind: u32) -> KindEffect {
    match kind {
        KIND_GIT_ISSUE | KIND_GIT_PULL_REQUEST => KindEffect::Root,
        KIND_TEXT_NOTE => KindEffect::Comment,
        KIND_GIT_STATUS_OPEN
        | KIND_GIT_STATUS_MERGED
        | KIND_GIT_STATUS_CLOSED
        | KIND_GIT_STATUS_DRAFT => KindEffect::Lifecycle,
        KIND_GIT_PR_UPDATE => KindEffect::ContextRefresh,
        // NIP-PC peer calls and results land on a project root as ordinary
        // conversation about it, so they classify as `Comment` and are then
        // decided by [`classify_project_event`] on their [`CallMarker`]. They
        // are deliberately *not* a class of their own: a call from an untrusted
        // author must fall through the same untrusted-context arm every other
        // comment does, and a separate class would be a second place for that
        // rule to be got wrong.
        KIND_PEER_CALL | KIND_PEER_CALL_RESULT => KindEffect::Comment,
        _ => KindEffect::Ignore,
    }
}

/// What an authorised status event does to a watched root.
///
/// Deliberately not `bool`. "Closed" is one of four status kinds and the other
/// three do not agree with each other, so a boolean at this boundary would be a
/// mapping decision hidden in a name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LifecycleTransition {
    /// The root is live work again: watch it and answer on it.
    Activate,
    /// The root is finished: keep watching so a reopen is still observed, but
    /// answer nothing on it.
    Suspend,
}

/// Which transition an authorised status event carries.
///
/// `None` for every kind [`classify_kind`] does not call `Lifecycle`, so this
/// cannot be handed a comment and asked to move a watch.
///
/// Merged sits with closed rather than with open: a merged pull request is
/// finished work, and leaving it active would keep answering on a branch that
/// no longer exists. Draft sits with open — a pull request moved back to draft
/// is unfinished, not concluded, and its author is usually still asking for
/// something.
pub(crate) fn lifecycle_transition(kind: u32) -> Option<LifecycleTransition> {
    match kind {
        KIND_GIT_STATUS_OPEN | KIND_GIT_STATUS_DRAFT => Some(LifecycleTransition::Activate),
        KIND_GIT_STATUS_CLOSED | KIND_GIT_STATUS_MERGED => Some(LifecycleTransition::Suspend),
        _ => None,
    }
}

/// May `author` change this root's lifecycle?
///
/// Root author or repository owner only, matching `allowedActorsForRoot`
/// (`desktop/src/features/projects/projectIssues.mjs:38-45`).
///
/// **Owner authority comes from the root's immutable binding, never from the
/// lifecycle event's own `a` tag.** Deriving it from the event conflated two
/// separate questions and rejected a legitimate case: `GitStatusMeta.repo` is
/// optional, so an owner-signed close that omits `a` is well-formed — and under
/// the old signature it produced no owner and was ignored. Whether the event's
/// own coordinate is acceptable is [`follow_up_coordinate_allowed`]'s job; this
/// function only asks who signed.
pub(crate) fn lifecycle_actor_allowed(
    author: &str,
    root_author: &str,
    repository_owner: &str,
) -> bool {
    let Some(author) = canonical_root_id(author) else {
        return false;
    };
    canonical_root_id(root_author).as_deref() == Some(author.as_str())
        || canonical_root_id(repository_owner).as_deref() == Some(author.as_str())
}

// ── History pagination ────────────────────────────────────────────────────────

/// Which exact filter a pagination stream covers.
///
/// **One cursor per filter, never one per REQ.** NIP-01 applies `limit`
/// per-filter, so a REQ carrying several filters and deduplicating the results
/// into one page produces an aggregate count that proves exhaustion for none of
/// them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, serde::Deserialize)]
pub(crate) enum HistoryStream {
    /// Comments and lifecycle, by lowercase `#e`.
    Comments,
    /// PR updates, by uppercase `#E`.
    PullRequestUpdates,
}

impl HistoryStream {
    /// Which tag style points at the root for this stream.
    ///
    /// Not interchangeable: comments and status events use lowercase `e`, a PR
    /// update uses uppercase `E` (`buzz-sdk/src/builders.rs:1444`).
    pub(crate) fn root_tag(self) -> &'static str {
        match self {
            HistoryStream::Comments => "#e",
            HistoryStream::PullRequestUpdates => "#E",
        }
    }

    /// The event kinds this stream carries.
    ///
    /// Shared with [`watched_roots_filters`] on purpose. Catch-up and the live
    /// watched REQ must ask the **same question** over different time ranges —
    /// if the two lists drifted, reconstruction would silently omit a class of
    /// event that the live subscription goes on delivering, and the root would
    /// look healthy while missing history nobody could point at.
    pub(crate) fn kinds(self) -> &'static [u32] {
        match self {
            HistoryStream::Comments => &[
                KIND_TEXT_NOTE,
                KIND_GIT_STATUS_OPEN,
                KIND_GIT_STATUS_MERGED,
                KIND_GIT_STATUS_CLOSED,
                KIND_GIT_STATUS_DRAFT,
                // NIP-PC traffic rides the `#e` stream because that is where it
                // lives: a project-routed call is an event about the root, keyed
                // on the root, and omitting it here would mean the live watched
                // REQ never delivers a call on an enrolled issue at all.
                //
                // Catch-up therefore replays calls too, which is correct rather
                // than merely consistent: `apply_processing_mode` maps a
                // replayed `ResumeCall` to `Ignore` and a replayed `Wake` to
                // `RefreshContext`, so restoring history cannot re-run a call
                // the agent already answered.
                KIND_PEER_CALL,
                KIND_PEER_CALL_RESULT,
            ],
            HistoryStream::PullRequestUpdates => &[KIND_GIT_PR_UPDATE],
        }
    }

    /// Exhaustible streams required for this root class.
    ///
    /// The root itself is **not** here. It is a required object proven by
    /// [`VerifiedBoundRoot`], not an exhaustible query — "the root query is
    /// exhausted" and "the root exists" are different claims, and an empty
    /// result satisfies the first while failing the second.
    pub(crate) fn required_for(is_pull_request: bool) -> &'static [HistoryStream] {
        if is_pull_request {
            &[HistoryStream::Comments, HistoryStream::PullRequestUpdates]
        } else {
            &[HistoryStream::Comments]
        }
    }

    /// Which stream carried a row of this kind.
    ///
    /// The merge folds both streams into one ordered history, so a row that has
    /// left pagination behind no longer knows which page fetched it — and the
    /// class it is replayed under must still name one. Deriving it from the
    /// row's own kind keeps that honest: the two streams partition the kinds,
    /// so there is exactly one answer, and `None` for a kind neither admits
    /// rather than a plausible default.
    pub(crate) fn carrying(kind: u32) -> Option<Self> {
        [HistoryStream::Comments, HistoryStream::PullRequestUpdates]
            .into_iter()
            .find(|stream| stream.admits(kind))
    }

    /// Does this stream carry rows of `kind`?
    ///
    /// **Answered from [`HistoryStream::kinds`], not from a second list.** The
    /// two used to be written out separately and they drifted: `kinds` asked
    /// the relay for [`KIND_PEER_CALL`] and [`KIND_PEER_CALL_RESULT`] and this
    /// rejected them, so a relay returning exactly what was requested degraded
    /// every real root that had ever carried peer-call traffic — a root whose
    /// history the agent then refused to claim, on the strength of rows it had
    /// asked for itself.
    ///
    /// A request list and an admission list are the same fact stated twice, and
    /// the failure mode of stating it twice is not a rejected row: it is a
    /// *silently narrower* history when the drift runs the other way. So the
    /// request is the one statement, and admission reads it.
    fn admits(self, kind: u32) -> bool {
        self.kinds().contains(&kind)
    }
}

/// What to do after absorbing one page of history.
///
/// Deliberately **not** `PartialEq` any more. Two of these variants now carry
/// event collections, and comparing whole outcomes by value would silently
/// compare those collections — a question no caller has, and one that would let
/// a test claim to check an outcome while really checking rows. Callers
/// destructure.
#[derive(Debug)]
pub(crate) enum PageOutcome {
    /// Request another page.
    Continue { until: u64, limit: usize },
    /// This stream is exhausted through the cutoff, and here are the rows it
    /// retained along the way.
    ///
    /// Success carries the history rather than a count. Before this, pagination
    /// proved progress and produced nothing: the cursor recorded ids in a `seen`
    /// set purely to detect repeats and dropped every event, so a caller that
    /// paginated a root to exhaustion held no rows at the end of it.
    ///
    /// There is no route to a [`RetainedStream`] that does not pass through this
    /// variant, and no route through this variant that does not carry one.
    Complete(RetainedStream),
    /// An authentic boundary from a **previous incarnation** of this same
    /// request arrived. The page is untouched and handed straight back.
    ///
    /// This is not corruption, and treating it as corruption would be a
    /// self-inflicted outage: the adversary model explicitly permits a
    /// predecessor's EOSE to be queued, the connection to be replaced, the page
    /// to be reopened, and the old boundary to be consumed late. It happens on
    /// an ordinary reconnect.
    ///
    /// So the predecessor must not *complete* the replacement — its boundary
    /// says nothing about what the relay held while the connection was down,
    /// and only the replacement could have recovered it — but it must not
    /// poison the replacement either. The page comes back so the replacement's
    /// own witness can still complete it.
    ///
    /// The distinction this variant exists to carry: **stale evidence is
    /// refused; contradictory state is degraded.**
    Stale { page: OpenedHistoryPage },
    /// Completeness could not be proven. The root is degraded, not healthy.
    ///
    /// `rows` are strictly diagnostic — see [`DiagnosticRows`].
    Degraded {
        reason: String,
        rows: DiagnosticRows,
    },
}

/// Which history question a page belongs to.
///
/// One cursor paginates both, because saturation, timestamp ties and the
/// fail-closed exhaustion proof are the same problem whatever is being walked
/// backwards — and a second cursor beside this one would be a second place for
/// those rules to drift. What differs is only the question: which rows the REQ
/// asks for, and which rows the collector will admit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum HistoryScope {
    /// One root's comments or PR revisions.
    Root { root: String, stream: HistoryStream },
    /// The roots on a discovered repository set that address this agent.
    ///
    /// **Roots only.** Comments reach an enrolled root through its watched REQ,
    /// so asking for them here would let a busy repository's chatter consume
    /// the page budget and crowd out the roots the budget exists to find —
    /// while the run reported complete authority over fewer of them.
    Enrolment {
        coordinates: Vec<String>,
        agent: String,
    },
}

impl HistoryScope {
    /// The REQ this scope's page sends.
    ///
    /// Single-sourced for the reason `catch_up_filter` was: the registered
    /// filter and the live admission check must be the same question, or the
    /// check decays into "both sides produced something filter-shaped".
    ///
    /// Neither arm carries `since`. A history page walks backwards until an
    /// unsaturated page proves exhaustion; a floor would make the proof a
    /// statement about the floor instead.
    pub(crate) fn filter(&self, until: u64, effective_limit: usize) -> Value {
        match self {
            HistoryScope::Root { root, stream } => {
                catch_up_filter(root, *stream, until, effective_limit)
            }
            HistoryScope::Enrolment { coordinates, agent } => json!({
                "kinds": [KIND_GIT_ISSUE, KIND_GIT_PULL_REQUEST],
                "#a": coordinates,
                "#p": [agent],
                "until": until,
                "limit": effective_limit,
            }),
        }
    }

    /// Which request class a page of this scope is registered as.
    ///
    /// Both are replay-only and neither is ever durable intent: a page is
    /// re-derived from its own cursor, so a record claiming one was written by
    /// something that mistook a transport attempt for a standing question.
    pub(crate) fn subscription(&self, generation: u64) -> ProjectSubscription {
        match self {
            HistoryScope::Root { root, stream } => ProjectSubscription::RootCatchUp {
                root: root.clone(),
                stream: *stream,
            },
            HistoryScope::Enrolment { .. } => ProjectSubscription::EnrolmentHistory { generation },
        }
    }

    /// Which stream of one root this scope walks, if it walks a root at all.
    ///
    /// `None` is not "unknown" — it is the enrolment scope, which walks roots
    /// across a coordinate set and belongs to no single one of them.
    pub(crate) fn stream(&self) -> Option<HistoryStream> {
        match self {
            HistoryScope::Root { stream, .. } => Some(*stream),
            HistoryScope::Enrolment { .. } => None,
        }
    }

    /// Admit one verified row, or say why it does not belong to this page.
    fn admit(&self, verified: &VerifiedProjectEvent) -> Result<(), String> {
        match self {
            HistoryScope::Root { root, stream } => {
                if !stream.admits(verified.kind()) {
                    let kind = verified.kind();
                    return Err(format!("kind {kind} does not belong to {stream:?}"));
                }
                match root_event_id(verified.kind(), &verified.id(), &verified.tag_vecs()) {
                    Some(found) if &found == root => Ok(()),
                    Some(other) => Err(format!("event belongs to root {other}, not this one")),
                    None => Err("event resolves to no root".to_string()),
                }
            }
            HistoryScope::Enrolment { coordinates, agent } => {
                let kind = verified.kind();
                if !matches!(kind, KIND_GIT_ISSUE | KIND_GIT_PULL_REQUEST) {
                    return Err(format!("kind {kind} is not a project root"));
                }
                // The root's *own* `a`, against the discovered set. A root that
                // claims a repository this agent never discovered is not a row
                // of this question, whoever delivered it.
                //
                // Read here rather than through `sole_reference`, which
                // canonicalises a 64-hex *event id* — a coordinate is
                // `30617:<owner>:<name>` and never satisfies it, so every root
                // would be refused and the page would read as an unusable
                // cohort. Exactly one, for the same reason `sole_reference`
                // insists on one: a root naming two repositories belongs to
                // neither, and picking either is choosing on its behalf.
                let tags = verified.tag_vecs();
                let mut named = tags
                    .iter()
                    .filter(|t| t.len() > 1 && t[0] == "a")
                    .map(|t| t[1].as_str());
                let (Some(coordinate), None) = (named.next(), named.next()) else {
                    return Err("root names no single repository coordinate".to_string());
                };
                if !coordinates.iter().any(|known| known == coordinate) {
                    return Err(format!("root names undiscovered repository {coordinate}"));
                }
                if !verified
                    .tag_vecs()
                    .iter()
                    .any(|t| t.len() > 1 && t[0] == "p" && &t[1] == agent)
                {
                    return Err("root does not address this agent".to_string());
                }
                Ok(())
            }
        }
    }
}

/// The one shape a root catch-up REQ filter may take.
///
/// Single-sourced because [`ProjectRequests::open_history_page`] builds the
/// registered filter from the collector's own page parameters, and the live
/// admission check compares arriving events against that same JSON. If the
/// sender and the admitter each built it themselves they would drift, and the
/// check would decay into "both sides produced something filter-shaped".
pub(crate) fn catch_up_filter(
    root: &str,
    stream: HistoryStream,
    until: u64,
    effective_limit: usize,
) -> Value {
    json!({
        "kinds": stream.kinds(),
        stream.root_tag(): [root],
        "until": until,
        "limit": effective_limit,
    })
}

// Consumed by the reconstruction driver in `relay.rs`. The blanket
// `allow(unused_imports)` this carried while the driver was still unwritten is
// gone with it: every name below has a caller, so a future one that loses its
// last caller is a warning rather than a silence.
pub(crate) use history::{
    merge::{merge_completed_streams, OrderedRetainedRows},
    DiagnosticRows, HistoryCursor, HistoryPageCollector, ProposalDomain, RetainedStream,
};

/// Only tests name a single row. Production reads them through
/// [`DiagnosticRows`], which is the whole point of that type: a failure is
/// describable without the witnesses being reachable.
#[cfg(test)]
pub(crate) use history::DiagnosticRow;

/// Pagination internals, in a private module so the proof types are genuinely
/// exclusive rather than merely documented as such.
///
/// The previous version's collector was `pub(crate)` with a method named
/// `finish_at_eose` and a comment saying it was only called after EOSE. A name
/// is not an enforcement mechanism: any caller could build an empty collector
/// and get a page whose zero `raw_count` read as `Complete`. Here the
/// constructor, the collector-to-page transition and the page's fields are all
/// private to this module, and the only route in is
/// [`HistoryCursor::propose_request`] — whose collector, in turn, only counts
/// once [`HistoryCursor::commit_request`] accepts it from the cursor that
/// stamped it.
mod history {
    use super::{
        AuthorityVerdict, EndOfStoredEvents, HistoryStream, OpenedHistoryPage, PageOutcome,
        VerifiedProjectEvent,
    };
    use std::collections::BTreeMap;
    use std::sync::Arc;

    /// Immutable identity of one page request.
    ///
    /// Binds root, stream, cutoff, effective limit and generation, so a page
    /// cannot be absorbed by a cursor that did not ask for it — a Comments page
    /// answering a PR-update cursor, a page for root A answering root B, or a
    /// page built under a different limit.
    #[derive(Debug, Clone, PartialEq, Eq)]
    struct HistoryPageRequest {
        generation: u64,
        scope: super::HistoryScope,
        until: u64,
        effective_limit: usize,
    }

    /// A page collected through a genuine EOSE with its integrity intact.
    #[derive(Debug)]
    pub(crate) struct EoseHistoryPage {
        request: HistoryPageRequest,
        raw_count: usize,
        events: Vec<VerifiedProjectEvent>,
    }

    impl EoseHistoryPage {
        pub(crate) fn events(&self) -> &[VerifiedProjectEvent] {
            &self.events
        }
    }

    /// A page that reached the end of its request with integrity already lost.
    ///
    /// Carries the rows that looked valid before the poisoning, because they are
    /// genuinely useful when diagnosing *why* a root degraded. They travel in a
    /// plain `Vec` and end up in [`DiagnosticRows`]; there is no function from
    /// either into [`RetainedStream`], so the diagnostic path cannot become a
    /// history path by anyone's convenience.
    struct PoisonedPage {
        reason: String,
        rows: Vec<VerifiedProjectEvent>,
    }

    /// Verified rows retained across every page of one stream.
    ///
    /// Deliberately **not** named `CompleteHistory` or `VerifiedHistory`. EOSE
    /// provenance is still forgeable, so this type carries no claim that the
    /// pages it accumulated were bounded by a genuine, request-bound end of
    /// stored events. It claims exactly this much: every row was verified,
    /// belongs to this root and this stream, was no newer than the page that
    /// asked for it, was paginated from `cutoff`, and the cursor's own
    /// exhaustion rule was satisfied. Readiness stays the driver's to declare,
    /// and remains blocked.
    ///
    /// Not `Clone`, on purpose: a stream is yielded once, and a duplicate would
    /// be a second copy of history that could be merged twice.
    ///
    /// It has no production method that reads its rows. The only code that
    /// touches `events` is [`merge`], a child module, which reaches the private
    /// field directly. A crate caller therefore cannot lift rows out of a
    /// completed stream and route them past the merge's checks.
    #[derive(Debug)]
    pub(crate) struct RetainedStream {
        scope: super::HistoryScope,
        /// The immutable snapshot boundary this stream was paginated from.
        ///
        /// Separate from the cursor's moving `until`, which is where pagination
        /// has reached. Two streams of one root that completed from different
        /// cutoffs describe two different snapshots, and merging them yields a
        /// coherent-looking history missing everything between the two.
        cutoff: u64,
        events: Vec<VerifiedProjectEvent>,
    }

    impl RetainedStream {
        /// The root this stream paginated, for the per-root merge.
        ///
        /// Empty for an enrolment scope, which paginates a repository set
        /// rather than a root — the merge that reads this only ever sees
        /// root-scoped streams, because it is reached from a bound root.
        pub(crate) fn root(&self) -> &str {
            match &self.scope {
                super::HistoryScope::Root { root, .. } => root,
                super::HistoryScope::Enrolment { .. } => "",
            }
        }

        pub(crate) fn stream(&self) -> HistoryStream {
            match &self.scope {
                super::HistoryScope::Root { stream, .. } => *stream,
                // Unreachable from the per-root merge, which is the only
                // caller: it is reached from a `VerifiedBoundRoot`, and an
                // enrolment page has no root to be bound to.
                super::HistoryScope::Enrolment { .. } => HistoryStream::Comments,
            }
        }

        pub(crate) fn scope(&self) -> &super::HistoryScope {
            &self.scope
        }

        pub(crate) fn cutoff(&self) -> u64 {
            self.cutoff
        }

        pub(crate) fn len(&self) -> usize {
            self.events.len()
        }

        pub(crate) fn is_empty(&self) -> bool {
            self.events.is_empty()
        }

        /// Take the retained rows, oldest first.
        ///
        /// **Consuming.** A retained stream is a completed proof, and a reader
        /// that could take it twice would let two owners each believe they hold
        /// the reconstruction. The enrolment walk is the one production reader;
        /// the per-root merge reads its own streams by reference.
        pub(crate) fn into_events(self) -> Vec<VerifiedProjectEvent> {
            self.events
        }

        /// Test-only row access, for asserting retention and ordering.
        ///
        /// Not available in production builds, where the only readers are the
        /// merge and the enrolment walk — which is the whole point of the type.
        #[cfg(test)]
        pub(crate) fn events(&self) -> &[VerifiedProjectEvent] {
            &self.events
        }
    }

    /// One row's identifying metadata, with no witness attached.
    ///
    /// What a degraded cursor may say about what it saw. Enough to diagnose a
    /// failure — which ids, of which kinds, at which times — and not enough to
    /// replay anything, because it is not a [`VerifiedProjectEvent`] and no
    /// authority function will accept it.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub(crate) struct DiagnosticRow {
        pub(crate) id: String,
        pub(crate) kind: u32,
        pub(crate) created_at: u64,
    }

    /// What a cursor that failed may report. **Diagnostics only.**
    ///
    /// A distinct type from [`RetainedStream`] so the difference is enforced
    /// rather than remembered, and — since the previous version of this type was
    /// only *named* diagnostic — it no longer holds anything replayable to hand
    /// out. It exposed `rows(&self) -> &[VerifiedProjectEvent]` crate-wide, and
    /// `VerifiedProjectEvent` is `Clone`, so any caller could take the witnesses
    /// and feed them to a fold directly. Being unable to build a
    /// `RetainedStream` never stopped that; it only stopped one route.
    ///
    /// Production sees [`DiagnosticRow`] metadata. Raw witnesses are dropped at
    /// the boundary rather than gated behind `#[cfg(test)]`, because nothing —
    /// test or otherwise — has a use for them that is not replay.
    ///
    /// These rows are not "most of the history". A poisoned page means some row
    /// occupied a slot under the relay's limit and then failed a check, which is
    /// exactly the situation in which what is missing cannot be enumerated.
    #[derive(Debug)]
    pub(crate) struct DiagnosticRows {
        rows: Vec<DiagnosticRow>,
    }

    impl DiagnosticRows {
        fn empty() -> Self {
            Self { rows: Vec::new() }
        }

        fn describe(events: impl IntoIterator<Item = VerifiedProjectEvent>) -> Self {
            Self {
                rows: events
                    .into_iter()
                    .map(|e| DiagnosticRow {
                        id: e.id(),
                        kind: e.kind(),
                        created_at: e.event().created_at.as_secs(),
                    })
                    .collect(),
            }
        }

        pub(crate) fn len(&self) -> usize {
            self.rows.len()
        }

        pub(crate) fn is_empty(&self) -> bool {
            self.rows.is_empty()
        }

        pub(crate) fn rows(&self) -> &[DiagnosticRow] {
            &self.rows
        }
    }

    /// Identifies the cursor that stamped a proposal.
    ///
    /// Empty on purpose: identity is the **allocation**, compared with
    /// `Arc::ptr_eq`. The previous version authenticated a proposal by its
    /// generation number, and an independently constructed cursor with the
    /// same root, stream, cutoff, bound and limit stamps the same number — so
    /// a throwaway cursor could commit the reconstruction's. This is the same
    /// correction as `RegistrationAuthority` in Piece 1, one layer down.
    #[derive(Debug)]
    pub(crate) struct ProposalDomain;

    /// Collects one request's frames and decides whether a witness may exist.
    ///
    /// The collector owns integrity rather than trusting its caller: it sees
    /// every frame on its subscription, counts it toward `raw_count`, and
    /// verifies and shape-checks it here. A bad row that vanished before being
    /// counted would leave a short page that falsely reads as end-of-history,
    /// because that row still consumed a slot under the relay's limit.
    #[derive(Debug)]
    pub(crate) struct HistoryPageCollector {
        request: HistoryPageRequest,
        /// The cursor this collector was stamped by.
        domain: Arc<ProposalDomain>,
        raw_count: usize,
        events: Vec<VerifiedProjectEvent>,
        integrity_lost: Option<String>,
    }

    impl HistoryPageCollector {
        /// Read-only view of the page parameters this collector was opened for.
        ///
        /// Exposed so `ProjectRequests::open_history_page` can build the REQ
        /// from them. Reading a request's own parameters is not the forgery
        /// risk — *constructing* authority is, and that stays inside the
        /// registry.
        pub(crate) fn scope(&self) -> &super::HistoryScope {
            &self.request.scope
        }

        pub(crate) fn until(&self) -> u64 {
            self.request.until
        }

        pub(crate) fn effective_limit(&self) -> usize {
            self.request.effective_limit
        }

        /// Which proposal this collector was stamped for.
        ///
        /// Sequence data only. On its own it proves nothing — an unrelated
        /// cursor with the same descriptive state stamps the same number — so
        /// [`HistoryCursor::commit_request`] checks the proposal *domain*
        /// alongside it.
        pub(crate) fn generation(&self) -> u64 {
            self.request.generation
        }

        /// Which cursor stamped this collector.
        pub(crate) fn proposal_domain(&self) -> &Arc<ProposalDomain> {
            &self.domain
        }

        /// Has this collector observed nothing at all yet?
        ///
        /// Checked at binding. A collector that already holds rows was filled
        /// before the registration it is being attached to existed, so those
        /// rows cannot have arrived on that subscription.
        pub(crate) fn is_pristine(&self) -> bool {
            self.raw_count == 0 && self.events.is_empty() && self.integrity_lost.is_none()
        }

        /// Feed one verified row from this request's subscription.
        ///
        /// Takes a [`VerifiedProjectEvent`] rather than a bare event because
        /// the relay dispatch has already verified it, and a second verifier
        /// here would be a second place that has to agree with the first. What
        /// remains are the *page* checks — stream, bound and root — which are
        /// about the question this request asked, not about the signature.
        pub(crate) fn observe(&mut self, verified: VerifiedProjectEvent) {
            self.raw_count += 1;

            if verified.event().created_at.as_secs() > self.request.until {
                let until = self.request.until;
                return self.poison(format!("event newer than the requested until={until}"));
            }
            // Scope-specific: which rows this question asked for. Both arms
            // poison rather than skip, because a row the page did not ask for
            // means the relay answered a different question and the count this
            // page's exhaustion proof rests on is no longer trustworthy.
            match self.request.scope.admit(&verified) {
                Ok(()) => self.events.push(verified),
                Err(reason) => self.poison(reason),
            }
        }

        /// A frame that could not even be parsed as an event.
        pub(crate) fn observe_malformed(&mut self, reason: impl Into<String>) {
            self.raw_count += 1;
            self.poison(reason);
        }

        /// The request ended without EOSE — `CLOSED`, timeout, reconnect,
        /// NOTICE. Consumes the collector so no witness can follow.
        pub(crate) fn abandon(self) {}

        fn poison(&mut self, reason: impl Into<String>) {
            if self.integrity_lost.is_none() {
                self.integrity_lost = Some(reason.into());
            }
        }

        /// Private: only [`HistoryCursor::complete`] calls it, on a genuine
        /// EOSE.
        fn finish_at_eose(self) -> Result<EoseHistoryPage, PoisonedPage> {
            if let Some(reason) = self.integrity_lost {
                // The rows that passed their checks before the poisoning still
                // exist, and throwing them away made every degradation equally
                // uninformative. They come back as diagnostics, never as page
                // events: this arm cannot construct an `EoseHistoryPage`.
                return Err(PoisonedPage {
                    reason,
                    rows: self.events,
                });
            }
            Ok(EoseHistoryPage {
                request: self.request,
                raw_count: self.raw_count,
                events: self.events,
            })
        }
    }

    /// What a cursor is allowed to do next.
    #[derive(Debug)]
    enum CursorState {
        /// Still paginating.
        Open,
        /// Already yielded its [`RetainedStream`]. The accumulator is empty and
        /// no further page may be absorbed — a second `Complete` would be a
        /// second copy of the same history.
        Finished,
        /// Permanently degraded, holding the first reason.
        ///
        /// Sticky by design. A short page arriving after integrity was lost
        /// looks exactly like exhaustion, so allowing one to be absorbed would
        /// let any stream rehabilitate itself simply by being asked once more.
        Degraded(String),
    }

    /// Paginates one stream backwards through a fixed cutoff, retaining what it
    /// accepts.
    ///
    /// `until` moves to the oldest timestamp **inclusively** — `oldest - 1`
    /// would drop the rest of that second — so the boundary cohort is delivered
    /// again on the next page and folds by id, and a page consisting entirely of
    /// one timestamp grows the page rather than moving the cursor, because such
    /// a cohort may be truncated.
    ///
    /// Not `Clone`: a copy would share a generation with the original and could
    /// absorb the same page a second time, which is the movable-proof hazard in
    /// a different costume.
    #[derive(Debug)]
    pub(crate) struct HistoryCursor {
        scope: super::HistoryScope,
        /// The snapshot boundary this cursor was opened against. Immutable.
        ///
        /// Kept apart from `until` because they answer different questions:
        /// `cutoff` is which snapshot this stream belongs to, `until` is how far
        /// back it has paginated. Storing only the latter left nothing for a
        /// merge to check, so two streams of one root could complete from two
        /// different snapshots and merge without complaint.
        cutoff: u64,
        until: u64,
        limit: usize,
        relay_max_limit: usize,
        generation: u64,
        /// This cursor's proposal domain, stamped into every collector it
        /// issues so another cursor's cannot be committed here.
        domain: Arc<ProposalDomain>,
        /// Set once the generation space is spent. Never cleared: a wrapped
        /// generation would let a superseded page match a later request, which
        /// is the failure the generation exists to prevent.
        generations_exhausted: bool,
        /// Accepted rows, keyed by `(created_at, event_id)`.
        ///
        /// That key is injective with `event_id` alone: a verified event's id is
        /// the hash of its canonical contents, `created_at` among them
        /// (`buzz-core/src/verification.rs:11-25`), so two entries cannot share
        /// an id and differ in key. Duplicates therefore still fold exactly
        /// once, while the ordered map hands back `(created_at, event_id)` order
        /// for free rather than by a sort someone has to remember to run.
        retained: BTreeMap<(u64, String), VerifiedProjectEvent>,
        state: CursorState,
    }

    impl HistoryCursor {
        pub(crate) fn new(
            scope: super::HistoryScope,
            cutoff: u64,
            initial_limit: usize,
            relay_max_limit: usize,
        ) -> Self {
            let relay_max_limit = relay_max_limit.max(1);
            Self {
                scope,
                cutoff,
                until: cutoff,
                limit: initial_limit.clamp(1, relay_max_limit),
                relay_max_limit,
                generation: 0,
                domain: Arc::new(ProposalDomain),
                generations_exhausted: false,
                retained: BTreeMap::new(),
                state: CursorState::Open,
            }
        }

        /// The question this cursor walks. Every page it opens asks it again,
        /// one `until` further back.
        pub(crate) fn scope(&self) -> &super::HistoryScope {
            &self.scope
        }

        pub(crate) fn cutoff(&self) -> u64 {
            self.cutoff
        }

        pub(crate) fn until(&self) -> u64 {
            self.until
        }

        /// The limit to request, already clamped to the relay ceiling so the
        /// page witness can be compared against a number the relay will apply.
        pub(crate) fn limit(&self) -> usize {
            self.limit
        }

        /// How many distinct rows are currently held.
        ///
        /// Zero once the stream has been yielded or drained into diagnostics —
        /// this counts what the cursor still holds, not what it ever saw.
        pub(crate) fn retained_count(&self) -> usize {
            self.retained.len()
        }

        /// The reason this cursor is permanently degraded, if it is.
        pub(crate) fn degraded_reason(&self) -> Option<&str> {
            match &self.state {
                CursorState::Degraded(reason) => Some(reason),
                _ => None,
            }
        }

        /// Open a collector bound to this cursor's current request. The only
        /// route to a page witness.
        ///
        /// A closed cursor may still hand one out; [`Self::complete`] is the
        /// single gate, so there is one place where "may this page count?" is
        /// decided rather than two that could disagree.
        /// Stamp and immediately commit. Test-only convenience.
        ///
        /// Production issues through [`Self::propose_request`] so that a second
        /// issue before transport cannot supersede the first.
        #[cfg(test)]
        pub(crate) fn begin_request(&mut self) -> HistoryPageCollector {
            let collector = self
                .propose_request()
                .expect("generation space is not exhausted in tests");
            assert!(self.commit_request(collector.generation(), collector.proposal_domain()));
            collector
        }

        /// Stamp a collector for the *next* request **without advancing**.
        ///
        /// Idempotent: calling it twice yields two collectors carrying the same
        /// generation and domain, so issuing a second cannot invalidate a first
        /// still on its way to the socket. The generation moves only when a
        /// page is accepted — see [`Self::commit_request`].
        ///
        /// The alternative shape, remembering "one is outstanding", would be a
        /// flag living across the transport `await`; a cancelled or
        /// indeterminate write would then strand the stream with an
        /// outstanding proposal nothing could clear.
        ///
        /// `None` once the generation space is spent. Fails closed rather than
        /// wrapping, for the same reason registration incarnations do: a reused
        /// generation lets a superseded page match a later request.
        pub(crate) fn propose_request(&self) -> Option<HistoryPageCollector> {
            if self.generations_exhausted {
                return None;
            }
            let next = self.generation.checked_add(1)?;
            Some(self.collector_for(next))
        }

        /// Advance to the proposed generation, if this cursor stamped it.
        ///
        /// Checks the proposal **domain** as well as the number. An unrelated
        /// cursor with identical root, stream, cutoff, bound and limit stamps
        /// the same generation, so the number alone authenticates nothing.
        pub(crate) fn commit_request(
            &mut self,
            generation: u64,
            domain: &Arc<ProposalDomain>,
        ) -> bool {
            if !Arc::ptr_eq(&self.domain, domain) {
                return false;
            }
            if self.generations_exhausted || generation != self.generation + 1 {
                return false;
            }
            self.generation = generation;
            if self.generation == u64::MAX {
                self.generations_exhausted = true;
            }
            true
        }

        fn collector_for(&self, generation: u64) -> HistoryPageCollector {
            HistoryPageCollector {
                request: HistoryPageRequest {
                    generation,
                    scope: self.scope.clone(),
                    until: self.until,
                    effective_limit: self.limit,
                },
                domain: Arc::clone(&self.domain),
                raw_count: 0,
                events: Vec::new(),
                integrity_lost: None,
            }
        }

        #[cfg(test)]
        pub(crate) fn force_generation(&mut self, generation: u64) {
            self.generation = generation;
        }

        /// Close a request at EOSE and absorb the resulting page.
        ///
        /// Both arguments are proofs, not descriptions. `witness` is minted only
        /// by `ProjectRequests::witness_end_of_stored_events` against a live
        /// registration, and `opened` only by
        /// `ProjectRequests::open_history_page` — so a collector that never
        /// rode a REQ, a `CLOSED`, a refusal and a timeout are all unable to
        /// reach this function at all, rather than being rejected once inside
        /// it.
        ///
        /// What remains checkable here is whether the two proofs name the
        /// *same registration*, which [`OpenedHistoryPage::verdict_for`]
        /// decides by comparing capabilities rather than numbers:
        ///
        /// - [`AuthorityVerdict::Predecessor`] — a strictly earlier instance of
        ///   this same request in this same registry. Ordinary reconnect
        ///   traffic: [`PageOutcome::Stale`], page returned, cursor untouched.
        /// - [`AuthorityVerdict::Contradiction`] — another request, another
        ///   registry, or a *later* instance offered to an older page. None of
        ///   those can happen without the owner being confused, so the cursor
        ///   degrades permanently.
        pub(crate) fn complete(
            &mut self,
            witness: &EndOfStoredEvents,
            opened: OpenedHistoryPage,
        ) -> PageOutcome {
            let verdict = opened.verdict_for(witness);

            // A predecessor's boundary must leave the cursor exactly as it
            // found it, whatever state that is — so this is settled before
            // anything below can mutate.
            if verdict == AuthorityVerdict::Predecessor {
                return PageOutcome::Stale { page: opened };
            }

            if let Some(reason) = self.closed_reason() {
                // The collector's rows are dropped rather than reported. A
                // cursor that has already finished or already failed must gain
                // nothing at all from a later page — including a set of rows
                // that would make the failure look partially recovered.
                //
                // This sits above the contradiction check for the same reason.
                // Degrading here would flip a `Finished` cursor whose rows were
                // already handed to the merge, and would replace an existing
                // degradation's reason with a later symptom — the first
                // recorded cause is the true one.
                return PageOutcome::Degraded {
                    reason,
                    rows: DiagnosticRows::empty(),
                };
            }

            if verdict == AuthorityVerdict::Contradiction {
                return self.degrade(
                    format!(
                        "end-of-stored-events for `{}` offered to a page opened as `{}`, \
                         and it is not an earlier instance of that request",
                        witness.sub_id(),
                        opened.sub_id()
                    ),
                    Vec::new(),
                );
            }

            let collector = opened.into_collector();
            match collector.finish_at_eose() {
                Ok(page) => self.absorb(page),
                Err(poisoned) => self.degrade(poisoned.reason, poisoned.rows),
            }
        }

        fn closed_reason(&self) -> Option<String> {
            match &self.state {
                CursorState::Open => None,
                CursorState::Finished => Some(
                    "stream already yielded its retained rows; it cannot absorb another page"
                        .to_string(),
                ),
                CursorState::Degraded(reason) => {
                    Some(format!("cursor is permanently degraded: {reason}"))
                }
            }
        }

        fn absorb(&mut self, page: EoseHistoryPage) -> PageOutcome {
            let EoseHistoryPage {
                request: r,
                raw_count,
                events,
            } = page;

            if r.generation != self.generation
                || r.scope != self.scope
                || r.until != self.until
                || r.effective_limit != self.limit
            {
                // Permanent, not a soft rejection. A cursor cannot tell its own
                // driver's superseded request apart from a page arriving from
                // somewhere else, and the correct way to drop a request that was
                // superseded is `HistoryPageCollector::abandon`, which never
                // reaches this function.
                return self.degrade(
                    "history page does not match this cursor's outstanding request".to_string(),
                    events,
                );
            }

            let mut fresh = 0usize;
            let mut oldest: Option<u64> = None;
            let mut newest: Option<u64> = None;
            for event in events {
                let created_at = event.event().created_at.as_secs();
                oldest = Some(oldest.map_or(created_at, |o| o.min(created_at)));
                newest = Some(newest.map_or(created_at, |n| n.max(created_at)));
                let id = event.id();
                if self.retained.insert((created_at, id), event).is_none() {
                    fresh += 1;
                }
            }

            // Judged on rows the relay returned against the limit it applied —
            // never on what we asked for, never on the post-filter count, and
            // never on how many of them were new. An inclusive boundary cohort
            // arrives again on the next page and folds to nothing fresh while
            // still consuming the relay's slots.
            if raw_count < r.effective_limit {
                return self.finish();
            }

            let Some(oldest) = oldest else {
                return self.degrade(
                    "saturated page contained no usable events".to_string(),
                    Vec::new(),
                );
            };
            let newest = newest.unwrap_or(oldest);

            if oldest == newest {
                if self.limit >= self.relay_max_limit {
                    let ceiling = self.relay_max_limit;
                    return self.degrade(
                        format!(
                            "timestamp {oldest} saturated the effective page ceiling {ceiling}; cannot prove the cohort is complete"
                        ),
                        Vec::new(),
                    );
                }
                self.limit = self.limit.saturating_mul(4).min(self.relay_max_limit);
                return PageOutcome::Continue {
                    until: self.until,
                    limit: self.limit,
                };
            }

            // Unreached by any page this API can produce, and deliberately kept
            // rather than tested. Two invariants exclude it together: the
            // collector refuses rows newer than the request's `until`, and
            // `until` only ever moves to a page's oldest stamp, so the only
            // already-retained rows a later page may legally repeat are those
            // sitting exactly at `until` — a single-timestamp cohort, handled
            // above. It guards the invariants, not an observed case; a fixture
            // for it would have to violate one of them and would then be
            // testing a shape the collector cannot deliver.
            if fresh == 0 {
                let until = self.until;
                return self.degrade(
                    format!(
                        "saturated page at until={until} returned no new events; pagination cannot advance"
                    ),
                    Vec::new(),
                );
            }

            self.until = oldest;
            PageOutcome::Continue {
                until: self.until,
                limit: self.limit,
            }
        }

        /// Close the stream and hand over what it retained.
        fn finish(&mut self) -> PageOutcome {
            self.state = CursorState::Finished;
            PageOutcome::Complete(RetainedStream {
                scope: self.scope.clone(),
                cutoff: self.cutoff,
                events: std::mem::take(&mut self.retained).into_values().collect(),
            })
        }

        /// Fail permanently, draining the accumulator into diagnostics.
        ///
        /// Draining is the point. If a degraded cursor kept its rows, the rows
        /// and the failure would sit side by side and some later caller would
        /// reach for the former; emptying it means the only thing that survives
        /// a failure is a [`DiagnosticRows`], which nothing can turn into
        /// history.
        ///
        /// `extra` carries rows that never made it into the accumulator —
        /// the valid-looking part of a poisoned page, or a page rejected for
        /// answering a request this cursor did not make.
        fn degrade(&mut self, reason: String, extra: Vec<VerifiedProjectEvent>) -> PageOutcome {
            let mut rows = std::mem::take(&mut self.retained);
            for event in extra {
                let key = (event.event().created_at.as_secs(), event.id());
                rows.entry(key).or_insert(event);
            }
            self.state = CursorState::Degraded(reason.clone());
            PageOutcome::Degraded {
                reason,
                // The witnesses are described and dropped here. Nothing beyond
                // this line ever holds a verified event that failed its page.
                rows: DiagnosticRows::describe(rows.into_values()),
            }
        }
    }

    /// The merge, in a child module so [`merge::OrderedRetainedRows`] has
    /// exactly one constructor and [`RetainedStream`] needs no row accessor at
    /// all.
    ///
    /// A child module can read its ancestors' private fields, so this reaches
    /// `RetainedStream.events` directly. That is the point: a crate-visible
    /// `into_events` would have let any caller lift rows out of a completed
    /// stream and hand them onward without the checks below ever running.
    pub(crate) mod merge {
        use super::super::{HistoryStream, VerifiedBoundRoot, VerifiedProjectEvent};
        use super::RetainedStream;

        /// Rows from every required stream of one snapshot, in reconstruction
        /// order.
        ///
        /// The narrow claim, and no wider: exactly the required streams, one
        /// root, one cutoff, root first, deterministic order thereafter. It does
        /// **not** claim EOSE completeness and does **not** claim the root is
        /// ready — matching cutoffs prove snapshot coherence, not that either
        /// stream saw everything within it.
        ///
        /// It exists so the eventual reconstruction fold can demand a value that
        /// has been through [`merge_completed_streams`], rather than a
        /// `Vec<VerifiedProjectEvent>` that anything at all can produce. There is
        /// deliberately no `into_rows`: adding one would restore the very
        /// laundering path this type is here to close.
        #[derive(Debug)]
        pub(crate) struct OrderedRetainedRows {
            root: String,
            cutoff: u64,
            rows: Vec<VerifiedProjectEvent>,
        }

        impl OrderedRetainedRows {
            pub(crate) fn root(&self) -> &str {
                &self.root
            }

            pub(crate) fn cutoff(&self) -> u64 {
                self.cutoff
            }

            pub(crate) fn rows(&self) -> &[VerifiedProjectEvent] {
                &self.rows
            }

            pub(crate) fn len(&self) -> usize {
                self.rows.len()
            }

            pub(crate) fn is_empty(&self) -> bool {
                self.rows.is_empty()
            }
        }

        /// Merge independently completed streams of one snapshot.
        ///
        /// **Completion is gated by the type system rather than by a comment.** A
        /// [`RetainedStream`] exists only inside `PageOutcome::Complete`, so
        /// there is no way to pass this function a stream that is still
        /// paginating, one that degraded, or one a caller assembled.
        ///
        /// Refuses outright rather than merging partially when:
        ///
        /// - the streams present are not exactly those
        ///   [`HistoryStream::required_for`] names for this root's class. A
        ///   `1618` root merged from its comments alone would otherwise yield a
        ///   well-ordered, entirely plausible history with every revision
        ///   missing;
        /// - a stream appears twice, which would duplicate each of its rows;
        /// - a stream was paginated for some other root;
        /// - a stream was paginated from a different cutoff than the
        ///   reconstruction selected. Two streams that each completed, from
        ///   `1_000` and from `500`, describe two snapshots; merging them loses
        ///   everything one of them never asked for and says nothing about it.
        ///   Checked for single-stream issue roots too, so the driver can prove
        ///   the stream belongs to the snapshot it chose rather than to an
        ///   earlier one it has forgotten.
        ///
        /// Ordering is root first by rule, then `(created_at, event_id)`. The
        /// root leads because it is the root, not because it is oldest — a relay
        /// may hand back a comment bearing an earlier `created_at` than the issue
        /// it answers, and a reconstruction opening with that comment would fold
        /// participants and lifecycle in an order that never happened. Ties break
        /// on id so two events in the same second have one order rather than
        /// whichever the merge happened to visit first.
        pub(crate) fn merge_completed_streams(
            root: &VerifiedBoundRoot,
            expected_cutoff: u64,
            streams: Vec<RetainedStream>,
        ) -> Result<OrderedRetainedRows, String> {
            let mut expected =
                HistoryStream::required_for(root.binding().is_pull_request()).to_vec();
            expected.sort();
            let mut present: Vec<HistoryStream> =
                streams.iter().map(RetainedStream::stream).collect();
            present.sort();
            if present != expected {
                return Err(format!(
                    "this root requires exactly {expected:?}; got {present:?}"
                ));
            }

            let root_id = root.binding().root();
            for stream in &streams {
                if stream.root() != root_id {
                    return Err(format!(
                        "{:?} was paginated for root {}, not {root_id}",
                        stream.stream(),
                        stream.root()
                    ));
                }
                if stream.cutoff != expected_cutoff {
                    return Err(format!(
                        "{:?} was paginated from cutoff {}, not the reconstruction's {expected_cutoff}",
                        stream.stream(),
                        stream.cutoff
                    ));
                }
            }

            // No cross-stream duplicate is possible to fold here:
            // `HistoryStream::admits` partitions the kinds, so a row admitted by
            // Comments is refused by PullRequestUpdates and the reverse.
            let mut rows: Vec<VerifiedProjectEvent> =
                streams.into_iter().flat_map(|s| s.events).collect();
            rows.sort_by(|a, b| {
                a.event()
                    .created_at
                    .as_secs()
                    .cmp(&b.event().created_at.as_secs())
                    .then_with(|| a.id().cmp(&b.id()))
            });

            let mut ordered = Vec::with_capacity(rows.len() + 1);
            ordered.push(root.event().clone());
            ordered.extend(rows);
            Ok(OrderedRetainedRows {
                root: root_id.to_string(),
                cutoff: expected_cutoff,
                rows: ordered,
            })
        }
    }
}

/// One root's historical reconstruction: the cutoff, the cursors, and at most
/// one page in flight per stream.
///
/// **Owns the cutoff.** Every stream of one reconstruction must be exhausted
/// against the *same* upper bound, or the merge is comparing histories that end
/// at different moments. The cutoff is taken once at construction and there is
/// no method that changes it.
///
/// **Derives, does not accept.** Which stream a page belongs to comes from the
/// page itself ([`OpenedHistoryPage::stream`]), never from a caller argument.
/// The same applies to the root. This is the lesson of Piece 1 applied one
/// layer out: a signature that lets the caller state a fact about authority is
/// a signature that will eventually be handed the wrong one.
///
/// **Claims nothing about readiness.** There is deliberately no
/// `is_complete()`. Completeness depends on backpressure recovery that does not
/// exist yet, and an API that could not honestly answer would be answered
/// optimistically. Callers can see which streams have finished; nothing here
/// says the reconstruction as a whole is trustworthy.
#[derive(Debug)]
pub(crate) struct RootReconstruction {
    /// The proof this reconstruction is *of*, kept rather than reduced to a
    /// root id.
    ///
    /// [`merge_completed_streams`] needs it to check that the streams present
    /// are the ones this root's class requires, and it must be the same proof
    /// the streams were opened under. Reducing it to a string at construction
    /// and asking a caller for a root again at completion would be the
    /// caller-states-a-fact hole the rest of this type closes, arriving at the
    /// one moment where the fact decides what a whole history means.
    root: VerifiedBoundRoot,
    /// Immutable for the life of the reconstruction.
    cutoff: u64,
    streams: Vec<StreamProgress>,
    /// Set once, terminal. A reconstruction that has given up says so rather
    /// than continuing to accept pages.
    abandoned: Option<String>,
}

#[derive(Debug)]
struct StreamProgress {
    stream: HistoryStream,
    cursor: HistoryCursor,
    /// At most one page may be in flight per stream. A second concurrent page
    /// on one stream would produce two boundaries the cursor could not order.
    open: Option<OpenedHistoryPage>,
    /// Set when this stream exhausted its history through the cutoff.
    retained: Option<RetainedStream>,
}

/// A refused attachment, **carrying the page back**.
///
/// `attach` consumes an [`OpenedHistoryPage`], and binding is one-shot: if a
/// rejection dropped the page, its registration would stay bound with nothing
/// able to reach it, and the caller would have no authority to clean up. The
/// page comes back with the reason so the rejection is recoverable rather than
/// merely reported.
#[derive(Debug)]
pub(crate) struct AttachRejected {
    pub(crate) error: AttachError,
    pub(crate) page: OpenedHistoryPage,
}

/// Why a page could not be attached to this reconstruction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum AttachError {
    /// The page collects for a different root.
    WrongRoot,
    /// The page collects for a different question entirely — a root page
    /// offered to an enrolment walk, or an enrolment page opened over a
    /// coordinate set this walk is not proving exhaustion over.
    WrongScope,
    /// This root does not require that stream — a PR update page offered to an
    /// issue reconstruction, for instance.
    StreamNotRequired,
    /// That stream already has a page in flight.
    AlreadyInFlight,
    /// The page was opened against a different upper bound than this
    /// reconstruction's current position for that stream.
    WrongUntil { expected: u64, found: u64 },
    /// The page was stamped for a proposal this stream has moved past, or one
    /// it never issued.
    Superseded,
    /// The page was opened with a different page limit than the cursor will
    /// accept at completion.
    ///
    /// The cursor compares the effective limit exactly. Checking only root,
    /// stream and `until` let a genuine page be held all the way to EOSE and
    /// *then* degrade the reconstruction — a rejection deferred until the
    /// point where it costs the most.
    WrongLimit { expected: usize, found: usize },
    /// That stream has already finished, or the whole reconstruction was
    /// abandoned.
    Closed,
}

/// What attaching a boundary to a reconstruction produced.
#[derive(Debug)]
pub(crate) enum StreamAdvance {
    /// This stream wants another page, from `until` with `limit`.
    Continue {
        stream: HistoryStream,
        until: u64,
        limit: usize,
    },
    /// This stream is exhausted through the cutoff.
    Finished { stream: HistoryStream },
    /// An authentic boundary from an earlier instance of the same request.
    ///
    /// Ordinary reconnect traffic. The page stays in flight and the
    /// reconstruction is untouched — there is nothing for the caller to do and
    /// nothing handed back, because only this page's own boundary can finish
    /// it.
    Stale { stream: HistoryStream },
    /// This stream failed permanently, and with it the reconstruction.
    Degraded {
        stream: HistoryStream,
        reason: String,
    },
}

/// Where an admitted catch-up frame went.
///
/// Deliberately not a `bool`. The previous signature answered "did some page
/// take this?", which cannot distinguish a frame that belongs to nobody here
/// from one that belongs to a page this reconstruction has already replaced —
/// and those need opposite handling: the first is somebody else's, the second
/// must be dropped precisely *because* it looks like ours.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum FrameRouting {
    /// Absorbed by the page that exact registration opened.
    Absorbed { stream: HistoryStream },
    /// That registration is gone, so its page was dropped. The stream wants a
    /// page again, from the bound it already had.
    Released { stream: HistoryStream },
    /// From a strictly earlier instance of the same request. Dropped; the page
    /// in flight is untouched and still completable by its own boundary.
    Predecessor { stream: HistoryStream },
    /// No page this reconstruction holds was opened by that registration.
    NotOurs,
    /// The frame names an id this reconstruction holds a page under, but comes
    /// from a registration that is neither that page's nor a predecessor of it.
    /// Terminal.
    Contradiction {
        stream: HistoryStream,
        reason: String,
    },
}

impl RootReconstruction {
    /// Begin a reconstruction for a proven root.
    ///
    /// Takes [`VerifiedBoundRoot`] rather than a root string so the caller
    /// cannot start one for a root it has not proven, and so the PR/issue class
    /// — which decides *which* streams are required — is read from the proof
    /// rather than passed alongside it.
    pub(crate) fn begin(
        root: &VerifiedBoundRoot,
        cutoff: u64,
        page_limit: usize,
        relay_max: usize,
    ) -> Self {
        let root_id = root.binding().root().to_string();
        let streams = HistoryStream::required_for(root.binding().is_pull_request())
            .iter()
            .map(|stream| StreamProgress {
                stream: *stream,
                cursor: HistoryCursor::new(
                    crate::project::HistoryScope::Root {
                        root: root_id.clone(),
                        stream: *stream,
                    },
                    cutoff,
                    page_limit,
                    relay_max,
                ),
                open: None,
                retained: None,
            })
            .collect();
        Self {
            root: root.clone(),
            cutoff,
            streams,
            abandoned: None,
        }
    }

    pub(crate) fn root(&self) -> &str {
        self.root.binding().root()
    }

    pub(crate) fn cutoff(&self) -> u64 {
        self.cutoff
    }

    pub(crate) fn is_pull_request(&self) -> bool {
        self.root.binding().is_pull_request()
    }

    /// Has every stream this root's class requires reached proven exhaustion?
    ///
    /// Deliberately narrower than "is this trustworthy": an abandoned
    /// reconstruction answers `false` here and stays abandoned, so completeness
    /// and degradation cannot both be claimed of one root.
    pub(crate) fn all_streams_finished(&self) -> bool {
        if self.abandoned.is_some() {
            return false;
        }
        let required = HistoryStream::required_for(self.is_pull_request()).len();
        self.streams.iter().filter(|s| s.retained.is_some()).count() == required
    }

    /// Consume the reconstruction into its merged history.
    ///
    /// Consuming rather than borrowing, because a reconstruction that has given
    /// up its retained streams has nothing left to page and must not be
    /// advertised for one: `pages_wanted` selects on `retained.is_none()`, so a
    /// version that took the streams out in place would re-open every page it
    /// had just finished, forever. The caller drops it from the live set by
    /// calling this at all.
    ///
    /// `Err` when the merge refuses — a stream missing, duplicated, or opened
    /// against a different cutoff. That is a degraded root, not a partial one.
    pub(crate) fn into_completed(mut self) -> Result<OrderedRetainedRows, String> {
        let streams: Vec<RetainedStream> = self
            .streams
            .iter_mut()
            .filter_map(|s| s.retained.take())
            .collect();
        merge_completed_streams(&self.root, self.cutoff, streams)
    }

    pub(crate) fn abandoned_reason(&self) -> Option<&str> {
        self.abandoned.as_deref()
    }

    /// The streams still wanting a page, and the bound each must be opened
    /// against.
    ///
    /// A stream appears here only when it has no page in flight, has not
    /// finished, and the reconstruction is live. The `until` is the cursor's,
    /// not a caller's choice — pagination walks backwards from the immutable
    /// cutoff and only the cursor knows how far it has got.
    pub(crate) fn pages_wanted(&self) -> Vec<(HistoryStream, u64, usize)> {
        if self.abandoned.is_some() {
            return Vec::new();
        }
        self.streams
            .iter()
            .filter(|s| {
                s.open.is_none() && s.retained.is_none() && s.cursor.degraded_reason().is_none()
            })
            .map(|s| (s.stream, s.cursor.until(), s.cursor.limit()))
            .collect()
    }

    /// Issue a collector for `stream`, from **this reconstruction's own
    /// cursor**.
    ///
    /// The only route on this type. `pages_wanted` says which streams want a
    /// page and on what bound; the collector comes from the owned cursor — the
    /// one whose `complete()` will judge it. The first version of this piece
    /// advertised requests it could never accept back, and a test helper that
    /// built a throwaway cursor hid it.
    ///
    /// It is **not** the only way a collector can come into existence, and the
    /// text here used to claim it was. `HistoryCursor::propose_request` is
    /// reachable, and a cursor built with the same root, stream, cutoff and
    /// limit stamps the *same* generation — so the number proves nothing. What
    /// the owned cursor recognises is the private proposal domain it stamps
    /// into its own collectors, checked by allocation at `attach`.
    ///
    /// `None` when that stream cannot take a page: abandoned, not required,
    /// already in flight, finished, or degraded.
    pub(crate) fn begin_page(&mut self, stream: HistoryStream) -> Option<HistoryPageCollector> {
        if self.abandoned.is_some() {
            return None;
        }
        let progress = self.streams.iter_mut().find(|s| s.stream == stream)?;
        if progress.open.is_some()
            || progress.retained.is_some()
            || progress.cursor.degraded_reason().is_some()
        {
            return None;
        }
        // Proposes without advancing, so issuing twice before either reaches
        // the socket cannot invalidate the first. The cursor moves only when
        // `attach` commits the proposal that actually arrived.
        match progress.cursor.propose_request() {
            Some(collector) => Some(collector),
            None => {
                // Generation space spent. Abandon rather than return `None`
                // forever: a stream that can never issue another page while
                // `pages_wanted` keeps advertising it is a spin, which is only
                // a quieter failure than wrapping.
                self.abandon(format!(
                    "{stream:?}: page generation space exhausted; no further page can be \
                     distinguished from a superseded one"
                ));
                None
            }
        }
    }

    /// Take ownership of a page opened for one of this root's streams.
    pub(crate) fn attach(&mut self, page: OpenedHistoryPage) -> Result<(), Box<AttachRejected>> {
        macro_rules! refuse {
            ($e:expr) => {
                return Err(Box::new(AttachRejected { error: $e, page }))
            };
        }
        if self.abandoned.is_some() {
            refuse!(AttachError::Closed);
        }
        let crate::project::HistoryScope::Root { root, stream } = page.scope().clone() else {
            // An enrolment page has no root to attach to a root's progress.
            refuse!(AttachError::WrongRoot);
        };
        if root != self.root() {
            refuse!(AttachError::WrongRoot);
        }
        let Some(progress) = self.streams.iter_mut().find(|s| s.stream == stream) else {
            refuse!(AttachError::StreamNotRequired);
        };
        if progress.retained.is_some() || progress.cursor.degraded_reason().is_some() {
            refuse!(AttachError::Closed);
        }
        if progress.open.is_some() {
            refuse!(AttachError::AlreadyInFlight);
        }
        // The *whole* expected request, not a hand-picked subset: the cursor
        // compares generation, root, stream, `until` and effective limit at
        // completion, and anything it will reject then should be rejected now.
        let expected_until = progress.cursor.until();
        if page.until() != expected_until {
            refuse!(AttachError::WrongUntil {
                expected: expected_until,
                found: page.until(),
            });
        }
        let expected_limit = progress.cursor.limit();
        if page.effective_limit() != expected_limit {
            refuse!(AttachError::WrongLimit {
                expected: expected_limit,
                found: page.effective_limit(),
            });
        }
        // Commit the exact proposal that arrived. A page stamped for a
        // proposal this cursor has already moved past — or one it never
        // stamped — is refused here rather than at EOSE.
        if !progress
            .cursor
            .commit_request(page.generation(), page.proposal_domain())
        {
            refuse!(AttachError::Superseded);
        }
        progress.open = Some(page);
        Ok(())
    }

    /// Which in-flight page was opened under this subscription id, if any.
    ///
    /// Destination by **provenance**: matched against the page's own
    /// `sub_id()`, so neither `observe` nor `complete` takes a caller-supplied
    /// stream. An untyped stream argument kept aligned by convention is the
    /// same shape as the authority claims Piece 1 removed — and here it is
    /// worse, because misdirecting a boundary turns a routing slip into
    /// terminal contradiction.
    fn stream_awaiting(&self, sub_id: &str) -> Option<HistoryStream> {
        self.streams
            .iter()
            .find(|s| s.open.as_ref().is_some_and(|p| p.sub_id() == sub_id))
            .map(|s| s.stream)
    }

    /// Route one admitted catch-up frame to the page **that request** opened.
    ///
    /// The id is not the destination. Under the old deterministic catch-up id
    /// `sub_id` named a *sequence* of registrations and the page in flight
    /// belonged to exactly one of them: the relay may still be delivering page
    /// one's rows when page two's REQ goes out, and matching on the id absorbed
    /// those stragglers into page two, where they were rows from outside its own
    /// bound, counted against its own limit. The page then completed and
    /// asserted a history it never received. An id now names one attempt, so the
    /// straggler is refused a registration upstream of here — but this stays
    /// registration-compared rather than id-compared, because a routing rule
    /// that is only correct while ids happen to be unique is a rule that depends
    /// on something it does not state.
    ///
    /// So the destination is the registration, compared by allocation, and the
    /// three answers are the same three a boundary gets.
    pub(crate) fn observe(&mut self, frame: CatchUpFrame) -> FrameRouting {
        let (admission, outcome) = frame.into_parts();
        if self.abandoned.is_some() {
            return FrameRouting::NotOurs;
        }
        let Some(stream) = self.stream_awaiting(admission.sub_id()) else {
            return FrameRouting::NotOurs;
        };
        let Some(progress) = self.streams.iter_mut().find(|s| s.stream == stream) else {
            return FrameRouting::NotOurs;
        };
        let Some(page) = progress.open.as_mut() else {
            return FrameRouting::NotOurs;
        };
        match page.verdict_for_frame(&admission) {
            AuthorityVerdict::SameRegistration => match outcome {
                CatchUpOutcome::Row(verified) => {
                    page.observe(*verified);
                    FrameRouting::Absorbed { stream }
                }
                // Counted, and the page loses its integrity claim. Dropping it
                // silently would leave the page short by exactly the number of
                // frames this agent refused, which reads as end-of-history.
                CatchUpOutcome::Unusable(reason) => {
                    page.observe_unusable(reason);
                    FrameRouting::Absorbed { stream }
                }
                // The page goes away and the stream re-advertises itself. It is
                // dropped rather than completed: only a genuine boundary from
                // this same registration may finish a page, and this request no
                // longer has one to give.
                CatchUpOutcome::RequestLost(_) => {
                    progress.open = None;
                    FrameRouting::Released { stream }
                }
            },
            // Ordinary reconnect traffic: a row of the page this one replaced.
            // Refused, and the page in flight is untouched — the same treatment
            // a predecessor's boundary gets.
            AuthorityVerdict::Predecessor => FrameRouting::Predecessor { stream },
            // A frame claiming an id this reconstruction holds a page under,
            // from a registration that is neither that page's nor an earlier
            // instance of it. Our model of who owns this id is wrong, and a
            // page whose arrivals cannot be accounted for cannot claim
            // retention integrity — so this is terminal, exactly as the same
            // verdict is for a boundary.
            AuthorityVerdict::Contradiction => {
                let reason = format!("{stream:?}: frame from a registration this page is not");
                self.abandon(reason.clone());
                FrameRouting::Contradiction { stream, reason }
            }
        }
    }

    /// Complete whichever page this boundary belongs to.
    ///
    /// The stream is derived from `witness.sub_id()`, so a boundary cannot be
    /// offered to the wrong stream's page. `None` when no page in flight was
    /// opened under that id.
    pub(crate) fn complete(&mut self, witness: &EndOfStoredEvents) -> Option<StreamAdvance> {
        if self.abandoned.is_some() {
            return None;
        }
        let stream = self.stream_awaiting(witness.sub_id())?;
        let progress = self.streams.iter_mut().find(|s| s.stream == stream)?;
        let page = progress.open.take()?;
        match progress.cursor.complete(witness, page) {
            PageOutcome::Continue { until, limit } => Some(StreamAdvance::Continue {
                stream,
                until,
                limit,
            }),
            PageOutcome::Complete(retained) => {
                progress.retained = Some(retained);
                Some(StreamAdvance::Finished { stream })
            }
            PageOutcome::Stale { page } => {
                // Untouched. The page goes back in flight because only this
                // instance's own boundary may finish it, and the reconstruction
                // — not the caller — is what holds it meanwhile.
                progress.open = Some(page);
                Some(StreamAdvance::Stale { stream })
            }
            PageOutcome::Degraded { reason, .. } => {
                // Terminal for the *reconstruction*, not just this stream. A
                // root whose history cannot be proven complete on one stream is
                // not partially trustworthy, and leaving another stream's page
                // in flight would let it keep absorbing events after the answer
                // had already become "we do not know".
                let reason = format!("{stream:?}: {reason}");
                self.abandon(reason.clone());
                Some(StreamAdvance::Degraded { stream, reason })
            }
        }
    }

    /// The connection died: every page in flight belonged to it.
    ///
    /// Cursors and the cutoff survive, so the reconstruction resumes from where
    /// it got to — under fresh registrations, since the old ones are gone.
    pub(crate) fn disconnected(&mut self) {
        for progress in &mut self.streams {
            progress.open = None;
        }
    }

    /// Give up permanently, with a reason.
    pub(crate) fn abandon(&mut self, reason: impl Into<String>) {
        if self.abandoned.is_none() {
            self.abandoned = Some(reason.into());
        }
        self.disconnected();
    }

    /// Streams that have exhausted their history, for the merge to consume.
    ///
    /// Deliberately *not* a readiness check: this says which streams finished,
    /// not that the reconstruction is trustworthy.
    pub(crate) fn finished_streams(&self) -> Vec<&RetainedStream> {
        self.streams
            .iter()
            .filter_map(|s| s.retained.as_ref())
            .collect()
    }

    /// Test-only: wind one stream's page generation near its ceiling.
    ///
    /// Exhaustion needs 2^64 pages to reach honestly, which is not a test
    /// anyone can run.
    #[cfg(test)]
    pub(crate) fn force_stream_generation(&mut self, stream: HistoryStream, generation: u64) {
        if let Some(progress) = self.streams.iter_mut().find(|s| s.stream == stream) {
            progress.cursor.force_generation(generation);
        }
    }

    #[cfg(test)]
    pub(crate) fn in_flight(&self, stream: HistoryStream) -> bool {
        self.streams
            .iter()
            .any(|s| s.stream == stream && s.open.is_some())
    }

    /// Test-only: the wire id and bound of the page currently in flight.
    ///
    /// Reading the page the *production* driver opened, rather than opening one
    /// and reading that back. A fixture that opens its own page cannot tell
    /// whether the driver would have.
    #[cfg(test)]
    pub(crate) fn in_flight_page(&self, stream: HistoryStream) -> Option<(String, u64)> {
        self.streams
            .iter()
            .find(|s| s.stream == stream)
            .and_then(|s| s.open.as_ref())
            .map(|page| (page.sub_id().to_string(), page.until()))
    }
}

/// The reconstructions in progress, and the only thing that puts an admitted
/// catch-up frame in front of a page.
///
/// **Dispatch is by provenance, not by content.** Which reconstruction a frame
/// belongs to comes from the class this agent recorded when it sent the REQ —
/// `RootCatchUp { root, .. }` — never from the root the arriving event names.
/// The event's own root is checked too, but as a *disagreement* check: a relay
/// answering a different question than the one asked poisons the page rather
/// than redirecting it.
///
/// **Holds no scheduling.** It does not decide which roots to reconstruct, does
/// not open pages, and answers no readiness question. A `Continue` handed back
/// by a stream is reported to the caller, which is where the next page will be
/// issued once there is something that issues pages.
///
/// **Lives beside the registry**, in the relay task's `BgState`, because a page
/// is bound by the registry the moment its REQ reaches the socket — an owner
/// anywhere else could only issue a collector and wait for the bound page to be
/// sent back. Nothing it holds is reachable from the run loop, and nothing it
/// routes crosses a queue.
#[derive(Debug, Default)]
pub(crate) struct ProjectReconstructions {
    live: Vec<RootReconstruction>,
}

impl ProjectReconstructions {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Take ownership of a reconstruction, so frames for its root can reach it.
    ///
    /// Refuses a second reconstruction of the same root: two owners for one
    /// root would both be offered every frame, and the one that did not open
    /// the page would report `NotOurs` while the other absorbed it — a
    /// coin-flip that depends on vector order.
    ///
    /// Refusing is also what makes a repeated restore idempotent: a root
    /// rediscovered by a second enrolment walk asks for its history again, and
    /// starting a second reconstruction of a root already being rebuilt would
    /// double every page.
    pub(crate) fn insert(&mut self, reconstruction: RootReconstruction) -> bool {
        if self.find(reconstruction.root()).is_some() {
            return false;
        }
        self.live.push(reconstruction);
        true
    }

    fn find(&mut self, root: &str) -> Option<&mut RootReconstruction> {
        self.live.iter_mut().find(|r| r.root() == root)
    }

    /// Every page any live reconstruction currently wants, as `(root, stream)`.
    ///
    /// The bound and the limit are deliberately **not** here. They are the
    /// cursor's, and a driver that carried them from this call to `begin_page`
    /// would be a second place for them to be stated — the collector already
    /// carries the only copy that matters.
    pub(crate) fn pages_wanted(&self) -> Vec<(String, HistoryStream)> {
        self.live
            .iter()
            .flat_map(|r| {
                r.pages_wanted()
                    .into_iter()
                    .map(|(stream, _, _)| (r.root().to_string(), stream))
            })
            .collect()
    }

    /// Issue a collector for one root's stream, from that reconstruction's own
    /// cursor.
    pub(crate) fn begin_page(
        &mut self,
        root: &str,
        stream: HistoryStream,
    ) -> Option<HistoryPageCollector> {
        self.find(root)?.begin_page(stream)
    }

    /// Hand a bound page to the reconstruction it was opened for.
    ///
    /// The root comes from the page's own scope, never from an argument, so a
    /// page cannot be attached to a reconstruction it does not describe.
    /// `WrongRoot` when no reconstruction holds that root — the same refusal a
    /// mismatched page gets, because "nobody is rebuilding this" and "you
    /// offered this to the wrong rebuild" are the same failure for the caller:
    /// close the registration.
    pub(crate) fn attach(&mut self, page: OpenedHistoryPage) -> Result<(), Box<AttachRejected>> {
        let HistoryScope::Root { root, .. } = page.scope().clone() else {
            return Err(Box::new(AttachRejected {
                error: AttachError::WrongRoot,
                page,
            }));
        };
        match self.find(&root) {
            Some(reconstruction) => reconstruction.attach(page),
            None => Err(Box::new(AttachRejected {
                error: AttachError::WrongRoot,
                page,
            })),
        }
    }

    /// Give up on one root permanently.
    pub(crate) fn abandon(&mut self, root: &str, reason: impl Into<String>) {
        if let Some(reconstruction) = self.find(root) {
            reconstruction.abandon(reason);
        }
    }

    /// The roots that have given up, and why.
    ///
    /// A degraded reconstruction stays in the live set rather than being
    /// dropped: dropping it would let the next restore of the same root start a
    /// fresh one and quietly re-enter the healthy path, so the agent would
    /// oscillate between "cannot prove this root's history" and "complete"
    /// without either being true.
    pub(crate) fn abandoned(&self) -> Vec<(String, String)> {
        self.live
            .iter()
            .filter_map(|r| {
                r.abandoned_reason()
                    .map(|reason| (r.root().to_string(), reason.to_string()))
            })
            .collect()
    }

    /// Take the merged history of `root`, if every stream it requires has
    /// reached proven exhaustion.
    ///
    /// **Removes the reconstruction.** A root whose history has been handed on
    /// is finished, and leaving a spent reconstruction in the live set would
    /// keep offering it pages it has no cursor left to fill.
    pub(crate) fn take_completed(
        &mut self,
        root: &str,
    ) -> Option<Result<OrderedRetainedRows, String>> {
        let index = self
            .live
            .iter()
            .position(|r| r.root() == root && r.all_streams_finished())?;
        Some(self.live.swap_remove(index).into_completed())
    }

    /// Route one admitted catch-up frame.
    ///
    /// `NotOurs` when no reconstruction is rebuilding that root, or when the
    /// one that is holds no page from that registration. The caller decides
    /// what an unowned frame means; absorbing it into whatever page is nearest
    /// is the thing this type exists to prevent.
    pub(crate) fn observe(&mut self, frame: CatchUpFrame) -> FrameRouting {
        let ProjectSubscription::RootCatchUp { root, .. } = frame.subscription() else {
            return FrameRouting::NotOurs;
        };
        let root = root.clone();
        match self.find(&root) {
            Some(reconstruction) => reconstruction.observe(frame),
            None => FrameRouting::NotOurs,
        }
    }

    /// Route one end-of-stored-events boundary.
    ///
    /// Same provenance rule: the root comes from the class recorded for the
    /// request the boundary names, so a boundary cannot be steered by anything
    /// the relay chose.
    pub(crate) fn complete(&mut self, witness: &EndOfStoredEvents) -> Option<StreamAdvance> {
        let ProjectSubscription::RootCatchUp { root, .. } = witness.subscription() else {
            return None;
        };
        let root = root.clone();
        self.find(&root)?.complete(witness)
    }

    /// The connection died. Every page in flight belonged to it.
    pub(crate) fn disconnected(&mut self) {
        for reconstruction in &mut self.live {
            reconstruction.disconnected();
        }
    }

    #[cfg(test)]
    pub(crate) fn get(&mut self, root: &str) -> Option<&mut RootReconstruction> {
        self.find(root)
    }
}

// ── Enrolment reconstruction ──────────────────────────────────────────────────

/// Walking the roots this agent is addressed on, backwards, to exhaustion.
///
/// The question a restarted agent has to answer is "which conversations am I
/// already responsible for?", and until this existed the enrolment REQ answered
/// it with a live tail plus a 30-day, 500-row reach-back. Both halves of that
/// reach-back were false completeness: the window silently excluded older
/// roots, and the row cap silently truncated the newer ones — and *neither*
/// announced itself, so the agent reported full authority over a set it had
/// only sampled.
///
/// So this is not a bigger window. It is a cursor: page backwards from a fixed
/// snapshot boundary until an unsaturated page proves there is nothing older,
/// and if that proof cannot be reached, say so — see [`EnrolmentAdvance`].
///
/// **Roots only, and a strict replay.** The rows it retains reconstruct
/// authority and lifecycle; they never become turns, because every page is
/// registered as [`ProjectSubscription::EnrolmentHistory`], which
/// [`processing_mode_for`] maps to [`ProcessingMode::Replay`].
///
/// Structurally the same as [`RootReconstruction`] with one stream, and it
/// shares that stream's [`HistoryCursor`] rather than reimplementing it —
/// saturation escalation, same-timestamp cohorts, the predecessor/contradiction
/// verdict and the exhaustion proof are the same rules whatever is being walked
/// backwards, and a second copy of them is a second place for them to drift.
#[derive(Debug)]
pub(crate) struct EnrolmentReconstruction {
    scope: HistoryScope,
    /// Immutable for the life of the reconstruction.
    cutoff: u64,
    cursor: HistoryCursor,
    /// At most one page in flight. A second would produce two boundaries the
    /// cursor could not order.
    open: Option<OpenedHistoryPage>,
    /// Set once, terminal.
    abandoned: Option<String>,
    /// Set when the walk reached proven exhaustion. Pages stop being wanted.
    complete: bool,
}

/// What attaching a boundary to an enrolment reconstruction produced.
#[derive(Debug)]
pub(crate) enum EnrolmentAdvance {
    /// Another page is wanted, from `until` with `limit`.
    Continue { until: u64, limit: usize },
    /// Exhaustion is proven. These are every root the walk found, oldest first.
    Finished { roots: Vec<VerifiedProjectEvent> },
    /// An authentic boundary from an earlier instance of the same request. The
    /// page stays in flight; only its own boundary may finish it.
    Stale,
    /// Completeness could not be established. **This is the fail-closed state**
    /// the plan requires be visible: the caller reports it rather than
    /// continuing as though the walk had succeeded.
    Degraded { reason: String },
}

/// Where an admitted enrolment-history frame went.
///
/// The same five answers [`FrameRouting`] gives, without the stream — an
/// enrolment walk has one.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum EnrolmentRouting {
    Absorbed,
    Released,
    Predecessor,
    NotOurs,
    Contradiction { reason: String },
}

impl EnrolmentReconstruction {
    /// Begin a walk over the discovered coordinate set.
    ///
    /// `None` when nothing has been discovered: the filter would carry an empty
    /// `#a`, which matches nothing at some relays and everything at others.
    pub(crate) fn begin(
        coordinates: Vec<String>,
        agent_pubkey_hex: &str,
        cutoff: u64,
        page_limit: usize,
        relay_max: usize,
    ) -> Option<Self> {
        if coordinates.is_empty() {
            return None;
        }
        let scope = HistoryScope::Enrolment {
            coordinates,
            agent: agent_pubkey_hex.to_string(),
        };
        Some(Self {
            cursor: HistoryCursor::new(scope.clone(), cutoff, page_limit, relay_max),
            scope,
            cutoff,
            open: None,
            abandoned: None,
            complete: false,
        })
    }

    pub(crate) fn scope(&self) -> &HistoryScope {
        &self.scope
    }

    pub(crate) fn cutoff(&self) -> u64 {
        self.cutoff
    }

    pub(crate) fn abandoned_reason(&self) -> Option<&str> {
        self.abandoned.as_deref()
    }

    /// Has this walk **proven** it reached the end of history?
    ///
    /// Named for the proof rather than for a readiness state, because that is
    /// all it is: an unsaturated page came back, so there is nothing older.
    /// Whether the agent is *ready* additionally depends on what happened to
    /// the rows, which is the caller's question and not this type's.
    pub(crate) fn has_proven_exhaustion(&self) -> bool {
        self.complete
    }

    /// Does this walk want a page, and on what bound?
    ///
    /// The `until` is the cursor's, never a caller's choice.
    pub(crate) fn page_wanted(&self) -> Option<(u64, usize)> {
        if self.abandoned.is_some()
            || self.complete
            || self.open.is_some()
            || self.cursor.degraded_reason().is_some()
        {
            return None;
        }
        Some((self.cursor.until(), self.cursor.limit()))
    }

    /// Issue a collector from **this reconstruction's own cursor**.
    pub(crate) fn begin_page(&mut self) -> Option<HistoryPageCollector> {
        self.page_wanted()?;
        match self.cursor.propose_request() {
            Some(collector) => Some(collector),
            None => {
                // Same reasoning as the per-root walk: a walk that can never
                // issue another page while it keeps advertising one is a spin.
                self.abandon(
                    "enrolment history: page generation space exhausted; no further page \
                     can be distinguished from a superseded one",
                );
                None
            }
        }
    }

    /// Take ownership of a page opened for this walk.
    pub(crate) fn attach(&mut self, page: OpenedHistoryPage) -> Result<(), Box<AttachRejected>> {
        macro_rules! refuse {
            ($e:expr) => {
                return Err(Box::new(AttachRejected { error: $e, page }))
            };
        }
        if self.abandoned.is_some() || self.complete || self.cursor.degraded_reason().is_some() {
            refuse!(AttachError::Closed);
        }
        // The whole question, not the variant: a page opened for a narrower
        // coordinate set than the one this walk is proving exhaustion over
        // would finish it early over a set it never asked about.
        if page.scope() != &self.scope {
            refuse!(AttachError::WrongScope);
        }
        if self.open.is_some() {
            refuse!(AttachError::AlreadyInFlight);
        }
        let expected_until = self.cursor.until();
        if page.until() != expected_until {
            refuse!(AttachError::WrongUntil {
                expected: expected_until,
                found: page.until(),
            });
        }
        let expected_limit = self.cursor.limit();
        if page.effective_limit() != expected_limit {
            refuse!(AttachError::WrongLimit {
                expected: expected_limit,
                found: page.effective_limit(),
            });
        }
        if !self
            .cursor
            .commit_request(page.generation(), page.proposal_domain())
        {
            refuse!(AttachError::Superseded);
        }
        self.open = Some(page);
        Ok(())
    }

    /// Is the page in flight the one **that registration** opened?
    fn awaits(&self, sub_id: &str) -> bool {
        self.open.as_ref().is_some_and(|p| p.sub_id() == sub_id)
    }

    /// Route one admitted enrolment-history frame to the page that opened it.
    pub(crate) fn observe(&mut self, frame: CatchUpFrame) -> EnrolmentRouting {
        let (admission, outcome) = frame.into_parts();
        if self.abandoned.is_some() || !self.awaits(admission.sub_id()) {
            return EnrolmentRouting::NotOurs;
        }
        let Some(page) = self.open.as_mut() else {
            return EnrolmentRouting::NotOurs;
        };
        match page.verdict_for_frame(&admission) {
            AuthorityVerdict::SameRegistration => match outcome {
                CatchUpOutcome::Row(verified) => {
                    page.observe(*verified);
                    EnrolmentRouting::Absorbed
                }
                CatchUpOutcome::Unusable(reason) => {
                    page.observe_unusable(reason);
                    EnrolmentRouting::Absorbed
                }
                CatchUpOutcome::RequestLost(_) => {
                    self.open = None;
                    EnrolmentRouting::Released
                }
            },
            AuthorityVerdict::Predecessor => EnrolmentRouting::Predecessor,
            AuthorityVerdict::Contradiction => {
                let reason =
                    "enrolment history: frame from a registration this page is not".to_string();
                self.abandon(reason.clone());
                EnrolmentRouting::Contradiction { reason }
            }
        }
    }

    /// Complete the page this boundary belongs to.
    ///
    /// `None` when no page in flight was opened under that id — which is the
    /// ordinary answer for the live enrolment tail's own boundary, and the
    /// reason a predecessor's EOSE can no longer certify anything here.
    pub(crate) fn complete(&mut self, witness: &EndOfStoredEvents) -> Option<EnrolmentAdvance> {
        if self.abandoned.is_some() || !self.awaits(witness.sub_id()) {
            return None;
        }
        let page = self.open.take()?;
        match self.cursor.complete(witness, page) {
            PageOutcome::Continue { until, limit } => {
                Some(EnrolmentAdvance::Continue { until, limit })
            }
            PageOutcome::Complete(retained) => {
                self.complete = true;
                Some(EnrolmentAdvance::Finished {
                    roots: retained.into_events(),
                })
            }
            PageOutcome::Stale { page } => {
                self.open = Some(page);
                Some(EnrolmentAdvance::Stale)
            }
            PageOutcome::Degraded { reason, .. } => {
                let reason = format!("enrolment history: {reason}");
                self.abandon(reason.clone());
                Some(EnrolmentAdvance::Degraded { reason })
            }
        }
    }

    /// The connection died: the page in flight belonged to it. The cursor and
    /// the cutoff survive, so the walk resumes from where it got to.
    pub(crate) fn disconnected(&mut self) {
        self.open = None;
    }

    pub(crate) fn abandon(&mut self, reason: impl Into<String>) {
        if self.abandoned.is_none() {
            self.abandoned = Some(reason.into());
        }
        self.disconnected();
    }

    #[cfg(test)]
    pub(crate) fn in_flight(&self) -> bool {
        self.open.is_some()
    }

    #[cfg(test)]
    pub(crate) fn force_generation(&mut self, generation: u64) {
        self.cursor.force_generation(generation);
    }
}

/// Proof that the required root event exists, is bound to a discovered
/// repository, and is of the expected class.
///
/// **The root is a required object, not an exhaustible stream.** Treating it as
/// one let an exact-id query returning zero rows satisfy "complete", so
/// readiness could be reached with no root at all — and therefore no root
/// author, no repository binding, and no class from which to derive prior
/// facts.
#[derive(Debug, Clone)]
pub(crate) struct VerifiedBoundRoot {
    event: VerifiedProjectEvent,
    binding: EnrolmentCandidate,
}

impl VerifiedBoundRoot {
    /// Prove a root from exactly one verified candidate event.
    ///
    /// Derives the binding internally rather than accepting one, so there is no
    /// API path that pairs a verified root with an independently selected
    /// candidate. Fewer movable proofs, fewer opportunities for creative
    /// assembly.
    ///
    /// `None` for zero events, more than one, or a root whose own signed `a`
    /// does not name a discovered repository.
    pub(crate) fn prove(
        candidates: &[VerifiedProjectEvent],
        discovered: &DiscoveredRepositories,
    ) -> Option<Self> {
        let [event] = candidates else {
            return None;
        };
        let binding = validate_enrolment_candidate(event, discovered)?;
        Some(Self {
            event: event.clone(),
            binding,
        })
    }

    pub(crate) fn event(&self) -> &VerifiedProjectEvent {
        &self.event
    }

    pub(crate) fn binding(&self) -> &EnrolmentCandidate {
        &self.binding
    }
}

// ── Root history ──────────────────────────────────────────────────────────────

/// Whether an event is being processed as reconstructed history or live
/// traffic.
///
/// Answers "may processing this now create a model turn?", **not** "what did
/// this event mean?". Conflating them was a real defect: suppressing every
/// replayed bare `p` as inherited meant a root enrolled by an authorised
/// human's structural `p` was silently forgotten across a restart.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ProcessingMode {
    Replay,
    Live,
}

/// How much of a root's history has been fetched.
///
/// A property of the *snapshot*, separate from [`PriorRootFacts`], which is
/// relative to one event's position within it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum RootHistoryReadiness {
    Unknown,
    Reconstructing,
    Complete,
    /// Reconstruction failed or tripped its breaker. Degraded, not healthy.
    Degraded(String),
}

/// The authority facts holding immediately *before* one event.
///
/// Private fields, seeded only from a proven root and folded only through
/// [`Self::observe`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PriorRootFacts {
    agent_was_participant: bool,
    root_author: String,
    /// Always present: a validated `30617` coordinate necessarily names an
    /// owner, so an `Option` here would only be a place for that proof to leak
    /// away.
    repository_owner: String,
    lifecycle: RootState,
}

impl PriorRootFacts {
    /// Seed the fold from a proven, bound root.
    ///
    /// Takes [`VerifiedBoundRoot`] rather than an event plus a coordinate
    /// string: the witness already establishes the root id, class, exact
    /// coordinate and discovered binding together.
    pub(crate) fn seed(root: &VerifiedBoundRoot) -> Self {
        Self {
            agent_was_participant: false,
            root_author: root.event().author(),
            repository_owner: root.binding().owner().to_string(),
            lifecycle: RootState::Active,
        }
    }

    /// Fold one verified event's participants in, **after** it has been
    /// evaluated.
    ///
    /// Order matters: incorporating first would let the first genuine mention
    /// see itself already present and classify as inherited.
    pub(crate) fn observe(&mut self, event: &VerifiedProjectEvent, agent: &AgentIdentity) {
        if self.agent_was_participant {
            return;
        }
        let hex = agent.hex().to_ascii_lowercase();
        self.agent_was_participant = event.tag_vecs().iter().any(|t| {
            t.first().map(String::as_str) == Some("p")
                && t.get(1).is_some_and(|v| v.eq_ignore_ascii_case(&hex))
        });
    }

    /// Apply a lifecycle transition.
    ///
    /// Only after the event has passed verification, route and coordinate
    /// checks, exact root binding, signer authority, and classification
    /// producing `ApplyLifecycle`. A verified lifecycle event is not
    /// necessarily an authorised one.
    pub(crate) fn set_lifecycle(&mut self, lifecycle: RootState) {
        self.lifecycle = lifecycle;
    }

    pub(crate) fn lifecycle(&self) -> RootState {
        self.lifecycle
    }

    pub(crate) fn root_author(&self) -> &str {
        &self.root_author
    }

    pub(crate) fn repository_owner(&self) -> &str {
        &self.repository_owner
    }

    pub(crate) fn agent_was_participant(&self) -> bool {
        self.agent_was_participant
    }

    #[cfg(test)]
    pub(crate) fn for_test(
        agent_was_participant: bool,
        root_author: &str,
        repository_owner: &str,
        lifecycle: RootState,
    ) -> Self {
        Self {
            agent_was_participant,
            root_author: root_author.to_string(),
            repository_owner: repository_owner.to_string(),
            lifecycle,
        }
    }
}

/// Deterministic history order: the root first, then `(created_at, event_id)`.
///
/// **Cross-runtime invariant.** Relay arrival order is not history — it is the
/// order the network happened to hand things over. Rust and the Hermes adapter
/// must fold in the same order or reconstruct different facts from identical
/// events. `created_at` alone is not a total order, hence the id tie-break.
pub(crate) fn history_order_key(root: &str, event_id: &str, created_at: u64) -> (u8, u64, String) {
    let is_root = if event_id == root { 0 } else { 1 };
    (is_root, created_at, event_id.to_string())
}

// ── Mention syntax ────────────────────────────────────────────────────────────

/// Characters that are *unconditionally* part of a mention token.
///
/// Unicode alphanumeric plus `_`. Deliberately not tied to either key alphabet:
/// a token ends where the lexer says, not where the key's alphabet runs out.
/// "Is the next character another hex digit" accepted `@<64-hex>garbage`.
///
/// The hyphen is **not** here, even though display handles are written with it,
/// because whether a hyphen belongs to the token depends on what surrounds it.
/// That judgement lives in [`mention_char_at`], which is what the boundary
/// checks call. This predicate is the context-free half; anything holding a
/// position should ask the contextual one.
fn is_mention_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// The character a hyphenated handle is written with.
const MENTION_JOINER: char = '-';

/// Is the character at byte offset `at` part of a mention token, *here*?
///
/// Everything [`is_mention_char`] accepts always is. The hyphen is admitted
/// only when it **joins** — when the run of hyphens it belongs to has an
/// ordinary mention character on both sides. That is what makes
/// `@hermes-gateway` one address rather than `@hermes` followed by noise, and
/// it matters in both directions: an agent called `Claude` must not answer
/// `@claude-bot`, and an agent called `hermes-gateway` must still be found by
/// its whole name.
///
/// A hyphen that joins nothing is prose, not part of a name: `@Claude - one
/// more thing`, `@Claude--`, `@claude-` and a `- @Claude` bullet all still name
/// `Claude`. The two halves of the rule fail in opposite directions, which is
/// why the split is where it is. Swallowing a dangling dash into the token
/// loses a mention somebody plainly typed — the agent stays silent when it was
/// asked. Splitting a joining dash reads somebody else's handle as ours, which
/// does not merely fail to suppress a turn: it *manufactures* one, on the exact
/// comment that named the agent the work was for. So the dash is given to the
/// token only when there is a name on the far side of it to join to.
///
/// Whole *runs* are judged, not single characters, so `@hermes--gateway` is one
/// token as well. Judging one character at a time would let a doubled dash
/// smuggle a prefix match past the rule a single dash is subject to.
///
/// `at` is a byte offset into `text` and must be a char boundary; every caller
/// derives one from a `find` on an ASCII prefix or from a decoded char. An
/// offset at the end of `text` is not a token character, which is what makes
/// end-of-content a boundary.
fn mention_char_at(text: &str, at: usize) -> bool {
    let Some(c) = text[at..].chars().next() else {
        return false;
    };
    if c != MENTION_JOINER {
        return is_mention_char(c);
    }
    // Skip the rest of the run in both directions: what decides a joiner is the
    // first ordinary character on each side, not the next dash along.
    let after = text[at..].chars().find(|c| *c != MENTION_JOINER);
    let before = text[..at].chars().rev().find(|c| *c != MENTION_JOINER);
    before.is_some_and(is_mention_char) && after.is_some_and(is_mention_char)
}

/// Does a mention token run right up to byte offset `at`?
///
/// The leading-boundary half of [`mention_char_at`]: the same question, asked
/// of the character immediately *before* `at` and judged in that character's
/// own context. Start of content answers no, which is what makes it a boundary.
fn mention_char_before(text: &str, at: usize) -> bool {
    text[..at]
        .chars()
        .next_back()
        .is_some_and(|c| mention_char_at(text, at - c.len_utf8()))
}

/// Characters a mention token may *begin* with.
///
/// A token has to name something. A hyphen only ever joins two names
/// ([`mention_char_at`]) and there is nothing to its left inside the token to
/// join, so a stray `@-` in prose is punctuation rather than an address. Spelt
/// out rather than delegated to [`is_mention_char`]: excluding the hyphen from
/// token *starts* is its own decision and must not quietly follow whoever edits
/// the interior alphabet next.
fn opens_mention_token(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// Accepted explicit-mention prefixes. `nostr:` is the NIP-27 form.
const MENTION_PREFIXES: [&str; 2] = ["nostr:", "@"];

/// Does `content` contain `identity` as a complete explicit mention token?
///
/// The whole prefixed candidate must stand alone lexically: a boundary before
/// the prefix, the exact identity, and a boundary after it. Every occurrence is
/// scanned, so prose mentioning an identity followed by a genuine mention still
/// resolves.
///
/// Boundaries are [`mention_char_at`], so a display name that is a *prefix* of
/// somebody else's handle does not match it: an agent called `Claude` reads
/// `@claude-bot` as a mention of `claude-bot`, which is not its name. Before
/// the hyphen could hold a token together, the trailing-boundary check landed
/// on the `-`, found no reason to stop there, and that comment named this agent
/// and woke it.
///
/// The identity is matched literally, so the same rule serves both alphabets:
/// a display handle carries its own hyphens into the needle, and hex and bech32
/// contain none, so for a key the hyphen only ever decides an *edge* —
/// `@<key>-2` is a different token, `nostr:<key> - please look` is a dash in
/// prose.
fn explicit_mention_present(content: &str, identity: &str) -> bool {
    if identity.is_empty() {
        return false;
    }
    let lower = content.to_ascii_lowercase();
    let ident = identity.to_ascii_lowercase();

    for prefix in MENTION_PREFIXES {
        let needle = format!("{prefix}{ident}");
        let mut from = 0usize;
        while let Some(offset) = lower[from..].find(&needle) {
            let start = from + offset;
            let end = start + needle.len();
            let leading_ok = !mention_char_before(&lower, start);
            let trailing_ok = !mention_char_at(&lower, end);
            if leading_ok && trailing_ok {
                return true;
            }
            from = start + 1;
        }
    }
    false
}

/// Does `content` carry at least one explicit mention token, whoever it names?
///
/// Deliberately identity-blind: it answers "did the author address somebody by
/// name here" and nothing else. Pairing it with the identity-aware checks is
/// what separates *this comment names someone else* from *this comment names
/// nobody* — the second is an ordinary continuation and must keep waking an
/// active root.
///
/// A token is a prefix at a lexical boundary followed by a character that can
/// open a handle ([`opens_mention_token`]), so an email address, a lone `@`, or
/// a decorative `nostr:` with nothing after it is not a mention. What follows
/// that first character is not inspected: the token's *extent* only matters
/// when it is being compared against an identity, which is
/// [`explicit_mention_present`]'s job.
fn mention_token_present(content: &str) -> bool {
    let lower = content.to_ascii_lowercase();

    for prefix in MENTION_PREFIXES {
        let mut from = 0usize;
        while let Some(offset) = lower[from..].find(prefix) {
            let start = from + offset;
            let end = start + prefix.len();
            let leading_ok = !mention_char_before(&lower, start);
            let names_something = lower[end..].chars().next().is_some_and(opens_mention_token);
            if leading_ok && names_something {
                return true;
            }
            from = start + prefix.len();
        }
    }
    false
}

/// This agent's identity, in the forms mention detection needs.
///
/// Constructed once from a `PublicKey` so hex and bech32 cannot disagree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AgentIdentity {
    hex: String,
    npub: String,
    display_name: Option<String>,
}

impl AgentIdentity {
    pub(crate) fn new(pubkey: &nostr::PublicKey) -> Result<Self, nostr::nips::nip19::Error> {
        use nostr::ToBech32;
        Ok(Self {
            hex: pubkey.to_hex(),
            npub: pubkey.to_bech32()?,
            display_name: None,
        })
    }

    /// Attach the display name this agent is known by in Desktop.
    ///
    /// A display name is **never** on its own proof of address — it is neither
    /// unique nor owned, which is why it does not feed `visible_mention`. It is
    /// admitted for one narrow negative purpose: telling "this comment names
    /// somebody, and that somebody is not me" apart from "this comment names
    /// nobody". Blank and whitespace-only names are dropped rather than stored,
    /// so an unset `BUZZ_ACP_DISPLAY_NAME` cannot match an empty needle.
    pub(crate) fn with_display_name(mut self, name: &str) -> Self {
        let trimmed = name.trim();
        self.display_name = (!trimmed.is_empty()).then(|| trimmed.to_string());
        self
    }

    pub(crate) fn hex(&self) -> &str {
        &self.hex
    }

    pub(crate) fn npub(&self) -> &str {
        &self.npub
    }

    pub(crate) fn display_name(&self) -> Option<&str> {
        self.display_name.as_deref()
    }
}

// ── Addressing resolution ─────────────────────────────────────────────────────

/// What the event itself says about addressing this agent.
///
/// Private fields, derived only from a [`VerifiedProjectEvent`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct AddressingEvidence {
    p_tag_present: bool,
    visible_mention: bool,
    /// This agent's own display name, used as explicit mention syntax.
    named_self: bool,
    /// Somebody was named here, by any mention syntax.
    named_anyone: bool,
    /// The event `p`-tags at least one key that is not this agent.
    p_tags_another: bool,
    /// This agent knows the name it is called by, so absence of that name is
    /// evidence rather than ignorance.
    knows_own_name: bool,
}

impl AddressingEvidence {
    /// Derive addressing evidence from a verified event.
    ///
    /// Both identity forms come from the one typed [`AgentIdentity`], so the
    /// `p` check and mention detection cannot be pointed at different keys.
    ///
    /// A visible mention is explicit mention *syntax*, not an identity
    /// occurrence: substring matching would let an authorised human pasting a
    /// payload or quoting a log line reactivate a dormant agent.
    pub(crate) fn resolve(event: &VerifiedProjectEvent, agent: &AgentIdentity) -> Self {
        let hex = agent.hex().to_ascii_lowercase();

        let mut p_tag_present = false;
        let mut p_tags_another = false;
        for tag in event.tag_vecs() {
            if tag.first().map(String::as_str) != Some("p") {
                continue;
            }
            match tag.get(1) {
                Some(value) if value.eq_ignore_ascii_case(&hex) => p_tag_present = true,
                Some(value) if !value.is_empty() => p_tags_another = true,
                _ => {}
            }
        }

        let content = &event.event().content;
        let visible_mention = explicit_mention_present(content, agent.hex())
            || explicit_mention_present(content, agent.npub());
        let named_self = visible_mention
            || agent
                .display_name()
                .is_some_and(|name| explicit_mention_present(content, name));

        Self {
            p_tag_present,
            visible_mention,
            named_self,
            named_anyone: mention_token_present(content),
            p_tags_another,
            knows_own_name: agent.display_name().is_some(),
        }
    }

    /// Is this comment addressed to a named party that is not this agent?
    ///
    /// All three conditions are required, and each rules out a different way of
    /// being wrong:
    ///
    /// - somebody is named — otherwise this is a bare follow-up, which this
    ///   predicate has nothing to say about;
    /// - that somebody is not us, by key *or* by display name — a comment that
    ///   names us is ours no matter who else it also names;
    /// - the event actually `p`-tags another key — this is what makes the name
    ///   an address rather than prose. A display name with no matching `p`
    ///   behind it names nobody the relay agreed to deliver to, so it must not
    ///   silence a turn.
    ///
    /// Under the target-only rule a bare follow-up does not wake an active root
    /// either, so this predicate no longer decides that case — but it still
    /// answers a different question, and conflating "names somebody else" with
    /// "names nobody" would lose the distinction the enrolment paths rely on.
    ///
    /// An agent with no configured display name cannot satisfy the second
    /// condition honestly: `@Its Own Name` would read as somebody else's, and
    /// it would fall silent on exactly the comments meant for it. Not knowing
    /// its own name means it has no opinion here — it never suppresses on that
    /// basis, and key syntax still addresses it.
    pub(crate) fn directed_at_another_party(&self) -> bool {
        self.knows_own_name && self.named_anyone && !self.named_self && self.p_tags_another
    }

    #[cfg(test)]
    pub(crate) fn for_test(p_tag_present: bool, visible_mention: bool) -> Self {
        Self {
            p_tag_present,
            visible_mention,
            named_self: visible_mention,
            named_anyone: visible_mention,
            p_tags_another: false,
            knows_own_name: false,
        }
    }
}

/// Resolve how an event addresses this agent, or refuse it.
///
/// `None` means the event must not be processed — not that it defaults to
/// something harmless. An event on the enrolment subscription **without** a
/// matching `p` did not match the filter that selected it, so the relay is
/// broken or lying; treating that as `WatchedRoot` would invent a route.
///
/// A bare `p` is weak evidence: Desktop's `p` set unions repository owner, root
/// author, prior recipients and actual mentions, so "not in the prior set" is
/// negative evidence about propagation, not proof of intent. It is explicit
/// only when the snapshot is `Complete`, prior facts exist, the agent was not
/// already a participant, and the agent is not present merely as repository
/// owner or root author.
///
/// The event's **kind** is deliberately absent too, and for the same reason:
/// an issue root, a pull-request root and a comment are addressed to this agent
/// in exactly one way — the agent is named, with its own `p` behind the name.
/// A root used to be exempt on the grounds that its `p` could not have been
/// copied forward from a predecessor it does not have. True, and not enough:
/// Desktop stamps the repository owner onto every root it creates, so on an
/// agent-owned project that exemption made every issue anybody opened an
/// address to the agent.
///
/// Processing mode is deliberately absent: whether this is replay or live has
/// no bearing on what the event meant when it was written.
pub(crate) fn resolve_addressing(
    source: &ProjectSubscription,
    evidence: &AddressingEvidence,
    readiness: &RootHistoryReadiness,
    facts: Option<&PriorRootFacts>,
    agent: &AgentIdentity,
) -> Option<Addressing> {
    if matches!(source, ProjectSubscription::Discovery) {
        return None;
    }

    if !evidence.p_tag_present {
        if matches!(
            source,
            ProjectSubscription::Enrolment | ProjectSubscription::EnrolmentHistory { .. }
        ) {
            return None;
        }
        return Some(Addressing::WatchedRoot);
    }

    // Naming this agent, with this agent's own `p` behind it, is explicit —
    // whichever spelling the name took.
    //
    // `named_self` widens `visible_mention` by one case: the configured display
    // name in `@`-mention syntax. That case is the *ordinary* one, and treating
    // it as weak evidence was the other half of the addressing failure. Desktop
    // writes a mention as the visible name plus a `p` tag; it does not put hex
    // or an npub in the body. So `visible_mention` is almost never true for a
    // mention a person actually typed, and a genuine `@Claude` on a dormant
    // root fell through to `InheritedParticipant` and stayed dormant.
    //
    // Note where this sits: past the `p_tag_present` gate above. The name alone
    // never reaches here, which is what keeps a display name — neither unique
    // nor owned — from being an address on its own. It is the conjunction the
    // contract names: `@Display Name` *plus that agent's matching `p` tag*. A
    // bare name in prose carries no `@` and is not a mention token at all.
    if evidence.named_self {
        return Some(Addressing::ExplicitMention);
    }

    // **A root's bare `p` is structure, not intent — and nothing below it can
    // tell the difference, so it is left to fall through as weak evidence.**
    //
    // A `1621`/`1618` root really does have no predecessor, so its `p` cannot
    // have been *copied forward* from an earlier participant list. That was the
    // whole argument for reading it as explicit, and it is true and beside the
    // point: propagation is not the only way a `p` arrives without anybody
    // deciding to address this agent. Desktop stamps the **repository owner**
    // onto every root it creates — unconditionally, before any mention is
    // considered (`desktop/src/features/projects/projectIssues.mjs:175-177` for
    // `1621`, `desktop/src/features/projects/pullRequestMutations.ts:37-42` for
    // `1618`) — so on a project the agent owns, *every* root, every issue anyone
    // opens about anything, carries the agent's key. Reading that as an address
    // woke a turn on roots whose entire content was `test`, before their real
    // addressee had said a word.
    //
    // So the root shortcut is gone and roots take the same path comments do:
    // the `named_self` check above is the enrolment route, and it is the one
    // Desktop actually produces for a mention — the visible `@Name`, plus that
    // agent's own `p` behind it. What is left here is a root whose only claim
    // on this agent is a tag the client wrote by itself, which is the same
    // weak evidence a copied-forward comment tag is and gets the same answer.
    //
    // The event's kind is consequently not an input here at all any more, and
    // the parameter is gone rather than left unread: a kind still in the
    // signature would read as a route this function takes, and there isn't one.
    // Kind still decides plenty — `classify_project_event` branches on it for
    // every effect — but not *whether the agent was addressed*, which is the
    // one question this function answers.
    if !matches!(readiness, RootHistoryReadiness::Complete) {
        return Some(Addressing::InheritedParticipant);
    }

    let Some(facts) = facts else {
        return Some(Addressing::InheritedParticipant);
    };

    if facts.agent_was_participant {
        return Some(Addressing::InheritedParticipant);
    }

    let hex = agent.hex().to_ascii_lowercase();
    if facts.root_author.eq_ignore_ascii_case(&hex)
        || facts.repository_owner.eq_ignore_ascii_case(&hex)
    {
        return Some(Addressing::InheritedParticipant);
    }

    Some(Addressing::ExplicitMention)
}

/// Constrain an effect by processing mode.
///
/// Replay restores state and context but never wakes the model.
pub(crate) fn apply_processing_mode(effect: ProjectEffect, mode: ProcessingMode) -> ProjectEffect {
    match mode {
        ProcessingMode::Live => effect,
        ProcessingMode::Replay => match effect {
            ProjectEffect::EnrolAndWake => ProjectEffect::Enrol,
            ProjectEffect::Wake => ProjectEffect::RefreshContext,
            // No call can be resumed from history *yet*: Phase 1b has not
            // frozen the envelope, so there is no durable outstanding-call
            // state for a replayed result to correlate against.
            ProjectEffect::ResumeCall => ProjectEffect::Ignore,
            other => other,
        },
    }
}

/// The call marker for a project event.
///
/// Delegates to the NIP-PC parser rather than inspecting tags here. A visible
/// `@Agent` is deliberately still **not** normalised into an invocation: doing
/// so would route invocation through the human addressing heuristic, in the
/// exact place the reply loop lives, and Desktop copies every prior
/// participant into every later comment's `p` tags. An agent that wants to
/// invoke another publishes an envelope.
///
/// Verification is not repeated. A [`VerifiedProjectEvent`] is already proof
/// that this process checked the signature and id, so the peer-call wrapper is
/// built from that proof instead of re-running a Schnorr check on the hot path.
pub(crate) fn project_call_marker(
    event: &VerifiedProjectEvent,
    agent: &AgentIdentity,
) -> CallMarker {
    crate::peer_call::call_marker(
        &crate::peer_call::VerifiedPeerEvent::from_project(event),
        agent.hex(),
    )
}

// ── Author gate ───────────────────────────────────────────────────────────────

/// Who authored a project event, after trust resolution.
///
/// "Trusted" means a cryptographically verified same-owner NIP-OA sibling or an
/// owner-approved external pubkey. It never means every relay identity.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ProjectAuthor {
    /// This agent. Its own replies must neither enrol nor wake it.
    SelfAuthored,
    /// The owner, or a human the owner has approved.
    AuthorisedHuman,
    /// A verified same-owner sibling or owner-approved external agent.
    TrustedAgent,
    /// Anyone else on the relay.
    Untrusted,
}

/// Whether the event carries an explicit peer-call marker.
///
/// Desktop puts **every** participant into every comment's `p` tags — project
/// owner, root author, all prior recipients, plus mentions
/// (`desktop/src/features/projects/hooks.ts:474-483`, `541-550`). So a bare
/// structural `p` usually means "you are on this thread", not "do something".
/// For an agent author that distinction is the difference between coordination
/// and an unbounded reply loop.
///
/// **Until Phase 1b freezes the call envelope, every trusted-agent project
/// event must resolve to [`CallMarker::None`].** There is no wire format yet to
/// recognise, so inferring an invocation from structural `p` tags would be
/// inventing one — and inventing it in the exact place the reply loop lives.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CallMarker {
    /// No call envelope — a bare structural `p`.
    None,
    /// An explicit call envelope, or a literal visible `@Agent` from a trusted
    /// agent normalised into one.
    Invocation,
    /// A correlated result for a call this agent made.
    Result,
}

/// How — and whether — this event names the agent.
///
/// **The caller resolves this; the classifier cannot.** Desktop copies every
/// prior participant into every subsequent comment's `p` tags
/// (`desktop/src/features/projects/hooks.ts:474-483`, `541-550`), so the mere
/// presence of the agent's pubkey carries no intent. Distinguishing a fresh
/// mention from an inherited one is exactly the judgement this classifier is
/// built to contain, which is why it is a required input rather than something
/// inferred from tag presence.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Addressing {
    /// A `p` tag naming this agent that is *not* carried forward from the
    /// root's existing participant set, or a literal visible `@Agent` in the
    /// content. This is the only form that enrols or reactivates.
    ExplicitMention,
    /// The agent's pubkey is present as client-written structure rather than as
    /// an address: copied forward from an earlier participant list, or stamped
    /// on by Desktop because the agent is the repository's owner or the root's
    /// author. Never an enrolment signal.
    ///
    /// The name is about the *strength* of the evidence, not about one way of
    /// acquiring it — a root has no participant list to inherit and still lands
    /// here, because "the client put my key on this by itself" is the same
    /// nothing whichever rule the client applied.
    InheritedParticipant,
    /// The agent is not named at all — the event reached us through the
    /// watched-root REQ because we are already enrolled.
    WatchedRoot,
}

/// What an event is permitted to do, after both gates.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ProjectEffect {
    /// Drop it. Not context, not a turn.
    Ignore,
    /// Include as clearly-labelled untrusted context. Cannot enrol, wake,
    /// steer, close, reopen, or assign.
    UntrustedContext,
    /// Ensure the root is in the active set without running a turn.
    ///
    /// What [`ProjectEffect::EnrolAndWake`] becomes under replay: the watch is
    /// restored, the model is not woken.
    Enrol,
    /// Ensure the root is in the active set — enrolling it, or reactivating a
    /// dormant enrolment — then run a turn.
    ///
    /// Enrolment and reactivation are one variant on purpose. The route key is
    /// the UUIDv5 of the root, so a reactivated root resolves to the very same
    /// session it had before; "reactivate" and "enrol" are the same write
    /// against the active set. Splitting them would let a caller handle one and
    /// silently forget the other. The distinction the reviewer asked for lives
    /// in the *inputs* ([`Addressing`] plus [`RootState`]), which is where it is
    /// load-bearing, and both paths are asserted separately in the tests.
    EnrolAndWake,
    /// Continue an already-active root. Does not create an enrolment.
    Wake,
    /// Apply a lifecycle change. Never a model turn.
    ApplyLifecycle,
    /// Refresh stored context. Never a model turn.
    RefreshContext,
    /// Resume the caller's outstanding call. Never a fresh invocation.
    ResumeCall,
}

// `SiblingResolver` is consumed by the async authority path in the driver.
#[allow(unused_imports)]
pub(crate) use sibling::{SiblingResolver, VerifiedSibling};

/// Sibling attestation, in a private module so the proof cannot be assembled
/// from strings.
///
/// The previous version had a `pub(crate) fn attested(author, owner)` carrying
/// a doc comment claiming it was "callable only where the lookup actually
/// happened". That comment was simply false — any caller could pass the current
/// event's author and the binding's owner and manufacture the result. This
/// module makes the claim true by construction: the only production route to a
/// `VerifiedSibling` runs through a [`SiblingResolver`] implementation.
mod sibling {
    /// Proof that an authenticated NIP-OA lookup found `author` to be a
    /// same-owner sibling under `owner`.
    ///
    /// Binds both, so a proof is about one pair rather than a general grant.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub(crate) struct VerifiedSibling {
        author: String,
        owner: String,
    }

    impl VerifiedSibling {
        pub(crate) fn matches(&self, author: &str, owner: &str) -> bool {
            self.author.eq_ignore_ascii_case(author) && self.owner.eq_ignore_ascii_case(owner)
        }
    }

    /// Performs the authenticated NIP-OA sibling lookup.
    ///
    /// `attest` is the sole constructor of [`VerifiedSibling`] and is private
    /// to this module, so an implementation can only mint a proof by returning
    /// `true` from a lookup it actually performed.
    pub(crate) trait SiblingResolver {
        /// Does the authenticated NIP-OA path show `author` and `owner` share
        /// an owner?
        fn is_same_owner_sibling(&self, author: &str, owner: &str) -> bool;

        /// Resolve to a proof.
        ///
        /// An implementor *can* override this — Rust trait defaults are not
        /// sealed — but overriding buys nothing: `attest` is private to this
        /// module, so no implementation outside it can mint a `VerifiedSibling`
        /// at all. The worst an override achieves is returning `None`, which
        /// fails closed.
        fn resolve(&self, author: &str, owner: &str) -> Option<VerifiedSibling> {
            if !self.is_same_owner_sibling(author, owner) {
                return None;
            }
            attest(author, owner)
        }
    }

    fn attest(author: &str, owner: &str) -> Option<VerifiedSibling> {
        Some(VerifiedSibling {
            author: super::canonical_root_id(author)?,
            owner: super::canonical_root_id(owner)?,
        })
    }
}

/// Classify a verified event's author for project purposes.
///
/// **Repository ownership is not invocation authority.** These are two
/// different powers and collapsing them was a privilege escalation: anyone can
/// sign a `kind:30617` announcement for a repository they invent, so treating
/// "author is the repository owner" as `AuthorisedHuman` let any relay user
/// announce a repo, open an issue under it, tag the agent, and thereby enrol
/// and wake somebody else's agent. Discovery is candidate selection, not
/// permission.
///
/// The split:
///
/// - **`agent_owner` / `approved_humans`** — may enrol and wake. This is the
///   agent owner's decision and nobody else's;
/// - **repository owner** — may perform *lifecycle* actions on their own root,
///   via [`lifecycle_actor_allowed`], and anchors the immutable binding. It
///   does not appear here at all.
///
/// Also deliberately ignores channel policy: `RespondTo::Anyone` exists
/// (`crates/buzz-acp/src/config.rs:99`) and an empty Hermes allow-list means
/// allow-all. Project routing inherits neither.
///
/// The sibling proof binds against `agent_owner`, because a NIP-OA sibling is
/// an agent sharing *this agent's* owner — not an agent belonging to whoever
/// happens to own the repository being discussed.
///
/// Order matters: self first, then trusted agents before generic human lists.
/// An agent identity must not gain human comment authority merely because it
/// was also placed in a broad allowed-user list.
pub(crate) fn classify_project_author(
    event: &VerifiedProjectEvent,
    agent: &AgentIdentity,
    agent_owner: Option<&str>,
    approved_humans: &BTreeSet<String>,
    sibling: Option<&VerifiedSibling>,
    approved_external_agents: &BTreeSet<String>,
) -> ProjectAuthor {
    let Some(author) = canonical_root_id(&event.author()) else {
        return ProjectAuthor::Untrusted;
    };

    if author.eq_ignore_ascii_case(agent.hex()) {
        return ProjectAuthor::SelfAuthored;
    }

    let attested = sibling.is_some_and(|s| agent_owner.is_some_and(|o| s.matches(&author, o)));
    if attested || approved_external_agents.contains(&author) {
        return ProjectAuthor::TrustedAgent;
    }

    // Invocation authority comes from the agent's owner, never from the
    // repository's. Explicit agent classification above wins over this generic
    // human allow-list.
    if agent_owner.is_some_and(|o| o.eq_ignore_ascii_case(&author))
        || approved_humans.contains(&author)
    {
        return ProjectAuthor::AuthorisedHuman;
    }

    ProjectAuthor::Untrusted
}

/// The project authority gate.
///
/// **This gate is project-specific and fails closed.** It must hold even where
/// ordinary channel config is permissive: `RespondTo::Anyone` exists
/// (`crates/buzz-acp/src/config.rs:99`) and an empty Hermes allow-list
/// currently means allow-all. Project routing inherits neither — a `#p` or `#e`
/// match is *candidate selection*, not permission.
///
/// `kind_effect` is the outcome of [`classify_kind`]; `lifecycle_authorised`
/// is [`lifecycle_actor_allowed`] for lifecycle events and ignored otherwise.
///
/// `directed_elsewhere` is [`AddressingEvidence::directed_at_another_party`].
/// It only ever subtracts: it can stop a comment aimed at a different agent
/// waking or enrolling this one, whatever state the root is in, and it can
/// never create a turn or an enrolment.
pub(crate) fn classify_project_event(
    kind_effect: KindEffect,
    author: ProjectAuthor,
    call: CallMarker,
    root_state: RootState,
    addressing: Addressing,
    lifecycle_authorised: bool,
    directed_elsewhere: bool,
) -> ProjectEffect {
    // Self-authorship is suppressed per event class, in the `Root` and
    // `Comment` arms below — deliberately *not* as an early return.
    //
    // Suppressing it up front also discarded the agent's own authorised state
    // events: an agent that opened an issue and later closed it would ignore
    // its own valid `1632` and leave the watch active forever. Self-authorship
    // must stop a *turn*, not a state update, and lifecycle here is still gated
    // on `lifecycle_authorised`, so this widens nothing an unauthorised signer
    // could reach.
    match kind_effect {
        KindEffect::Ignore => ProjectEffect::Ignore,

        // Lifecycle is decided by signer authority and nothing else — including
        // when the signer is this agent, which is how an agent that closes its
        // own issue moves its own watch to dormant. A result marker must not
        // convert a status event into a call resumption: `1630`-`1633` are
        // lifecycle-only, and an unauthorised one is dropped rather than
        // deprioritised.
        KindEffect::Lifecycle => {
            if lifecycle_authorised {
                ProjectEffect::ApplyLifecycle
            } else {
                ProjectEffect::Ignore
            }
        }

        // A PR revision refreshes context and never becomes a turn, so neither
        // a result marker nor self-authorship changes the outcome: the agent's
        // own push still has to land in its context.
        KindEffect::ContextRefresh => match author {
            ProjectAuthor::Untrusted => ProjectEffect::UntrustedContext,
            _ => ProjectEffect::RefreshContext,
        },

        // A root announces a new issue or PR. It is never a call result, so a
        // result marker here is malformed and falls through to `Ignore` below.
        KindEffect::Root => match author {
            ProjectAuthor::SelfAuthored => ProjectEffect::Ignore,
            ProjectAuthor::Untrusted => ProjectEffect::UntrustedContext,
            ProjectAuthor::AuthorisedHuman => match addressing {
                Addressing::ExplicitMention => ProjectEffect::EnrolAndWake,
                Addressing::InheritedParticipant | Addressing::WatchedRoot => ProjectEffect::Ignore,
            },
            ProjectAuthor::TrustedAgent => match call {
                CallMarker::Invocation => ProjectEffect::EnrolAndWake,
                CallMarker::None | CallMarker::Result => ProjectEffect::Ignore,
            },
        },

        // A comment is the only surface a call result can currently land on,
        // and the only one that can wake a turn.
        KindEffect::Comment => match author {
            ProjectAuthor::SelfAuthored => ProjectEffect::Ignore,

            // Untrusted identities may comment; they cannot direct the agent,
            // and cannot forge a correlation either.
            ProjectAuthor::Untrusted => ProjectEffect::UntrustedContext,

            ProjectAuthor::AuthorisedHuman => match call {
                // Only a trusted agent can return a result. From anyone else
                // this is a forged correlation attempt.
                CallMarker::Result => ProjectEffect::Ignore,
                _ => wake_or_enrol(root_state, addressing, directed_elsewhere),
            },

            // A trusted agent's bare `p` is never an invocation — that is the
            // reply loop. It needs an explicit call envelope.
            ProjectAuthor::TrustedAgent => match call {
                CallMarker::Result => ProjectEffect::ResumeCall,
                // An invocation envelope names its callee, so it is explicit
                // addressing by construction and needs no separate re-tag.
                // A NIP-PC envelope names *this agent* as its callee and was
                // admitted as such upstream, so it is addressed to us by
                // construction. Whoever else the prose happens to name cannot
                // retract that, and `directed_elsewhere` is false here for the
                // same reason the addressing is `ExplicitMention`.
                CallMarker::Invocation => {
                    wake_or_enrol(root_state, Addressing::ExplicitMention, false)
                }
                CallMarker::None => ProjectEffect::Ignore,
            },
        },
    }
}

/// Resolve an authorised comment against the enrolment sets.
///
/// The dormant row is the one that matters. A closed root keeps receiving
/// events so a reopen stays observable, but only a *genuine* re-tag brings it
/// back: an inherited participant tag must leave it dormant, because Desktop
/// copies prior participants into every later comment and treating that as a
/// re-tag would reanimate every closed issue the agent ever touched.
///
/// | Root state | Explicit mention | Inherited / watched |
/// |---|---|---|
/// | `Unknown` | enrol and wake | ignore — nothing enrolled us |
/// | `Active` | wake | ignore — not addressed to us |
/// | `Dormant` | reactivate and wake | ignore — stays dormant |
///
/// …and every row is void when the comment names somebody else.
///
/// **Target-only.** Every row now requires the comment to address *this* agent.
/// The active row used to wake on anything an approved human wrote, on the
/// theory that a continuation needs no re-tag. That is true of a two-party
/// conversation and false of these roots, which are shared: Desktop copies prior
/// participants into every later comment's `p` set, so "no re-tag required"
/// meant every agent ever enrolled woke on every comment, including
/// `@Other Agent please take this`. An inherited `p` is propagation, not intent,
/// and it is the only thing a bare follow-up has to offer.
///
/// The cost is deliberate and worth naming: a bare `yes, go ahead` no longer
/// wakes anybody. On a root with one agent that reads as a regression; on a root
/// with three it is the difference between one answer and three. Addressing is
/// how a shared thread picks an addressee, and there is no silent default that
/// is right for both.
///
/// `directed_elsewhere` is kept as a separate guard rather than folded into the
/// addressing check. It is not redundant here: [`Addressing::ExplicitMention`]
/// is also reachable via a *fresh* `p` tag with complete history, so a comment
/// can name another agent in prose while still carrying explicit-`p` evidence
/// for us. Naming somebody else wins.
///
/// It guards **every** row, not only the active one. A root this process has
/// never seen is exactly where an unaddressed comment is most expensive: the
/// active row at least means somebody once addressed this agent here, whereas
/// enrolling from `@another-agent, please take this` starts a watch on a
/// conversation nobody invited the agent into, and answers it. Restricting the
/// guard to `Active` was the live Phase 3d failure — on a fresh root the very
/// first comment, handed to another party, still enrolled and woke.
fn wake_or_enrol(
    root_state: RootState,
    addressing: Addressing,
    directed_elsewhere: bool,
) -> ProjectEffect {
    // A comment handed to a different agent is never ours, whatever else it
    // carries or whatever state the root is in — checked before addressing for
    // exactly that reason.
    if directed_elsewhere {
        return ProjectEffect::Ignore;
    }
    match root_state {
        RootState::Active => match addressing {
            Addressing::ExplicitMention => ProjectEffect::Wake,
            Addressing::InheritedParticipant | Addressing::WatchedRoot => ProjectEffect::Ignore,
        },
        RootState::Unknown | RootState::Dormant => match addressing {
            Addressing::ExplicitMention => ProjectEffect::EnrolAndWake,
            Addressing::InheritedParticipant | Addressing::WatchedRoot => ProjectEffect::Ignore,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nostr::{EventBuilder, JsonUtil, Keys, Kind};

    const ROOT: &str = "48be1cc2000000000000000000000000000000000000000000000000000000ab";
    const OTHER_ROOT: &str = "48be1cc2000000000000000000000000000000000000000000000000000000ac";
    const OWNER: &str = "93941e544971f89d581a19acd4570572f4d5f7bb0783a9ac1febfa1dc0deaebf";
    const STRANGER: &str = "222b9658e0e4945cbca51ffa8d364a178a02e349d79847e9282e6ee1306a00ce";

    // ── Route key ────────────────────────────────────────────────────────────

    #[test]
    fn namespace_matches_the_agreed_literal() {
        // Cross-runtime invariant: the Hermes adapter hard-codes this string.
        assert_eq!(
            PROJECT_ROUTE_NAMESPACE.to_string(),
            "0a0170ea-22c2-5606-8679-6c72e92c1942"
        );
    }

    #[test]
    fn route_key_is_deterministic() {
        assert_eq!(project_route_key(ROOT), project_route_key(ROOT));
    }

    #[test]
    fn route_key_is_case_insensitive_on_input() {
        // Hashing the uppercase form as-is would mint a second session for the
        // same issue.
        assert_eq!(
            project_route_key(&ROOT.to_ascii_uppercase()),
            project_route_key(ROOT)
        );
    }

    #[test]
    fn different_roots_get_different_keys() {
        assert_ne!(project_route_key(ROOT), project_route_key(OTHER_ROOT));
    }

    #[test]
    fn route_key_is_a_v5_uuid() {
        let key = project_route_key(ROOT).unwrap();
        assert_eq!(key.get_version_num(), 5);
    }

    #[test]
    fn route_key_rejects_non_event_ids() {
        assert!(project_route_key("").is_none());
        assert!(project_route_key("not-hex").is_none());
        assert!(project_route_key(&ROOT[..63]).is_none());
        assert!(project_route_key(&format!("{ROOT}0")).is_none());
        assert!(project_route_key(&ROOT.replace('a', "z")).is_none());
    }

    #[test]
    fn route_key_rejects_whitespace_padding() {
        // "64 hex characters, nothing else" is the contract. Trimming would
        // coerce malformed input into a plausible-looking session, which is
        // exactly the fail-open this key derivation must not have.
        assert!(project_route_key(&format!(" {ROOT}")).is_none());
        assert!(project_route_key(&format!("{ROOT} ")).is_none());
        assert!(project_route_key(&format!(" {ROOT} ")).is_none());
        assert!(project_route_key(&format!("\t{ROOT}")).is_none());
        assert!(project_route_key(&format!("{ROOT}\n")).is_none());
        // Padding that keeps the length at 64 is rejected on the hex check.
        assert!(project_route_key(&format!(" {}", &ROOT[..63])).is_none());
    }

    #[test]
    fn root_extraction_rejects_whitespace_padded_tag_values() {
        assert_eq!(
            root_event_id(KIND_TEXT_NOTE, ROOT, &tags(&[&["e", &format!(" {ROOT}")]])),
            None
        );
        assert_eq!(
            root_event_id(KIND_GIT_ISSUE, &format!("{ROOT} "), &tags(&[])),
            None
        );
    }

    #[test]
    fn repo_owner_rejects_whitespace_padding() {
        assert_eq!(
            repo_owner_from_coordinate(&format!("30617: {OWNER}:r")),
            None
        );
    }

    #[test]
    fn lifecycle_authority_rejects_whitespace_padded_pubkeys() {
        assert!(!lifecycle_actor_allowed(
            &format!(" {OWNER}"),
            STRANGER,
            OWNER
        ));
    }

    #[test]
    fn owner_authority_survives_a_lifecycle_event_with_no_coordinate() {
        // `GitStatusMeta.repo` is optional, so an owner-signed close that omits
        // `a` is well-formed. Owner authority comes from the root's immutable
        // binding, not from whether the event repeated the coordinate — the old
        // signature derived it from the event and ignored this case.
        assert!(
            lifecycle_actor_allowed(OWNER, THIRD_PARTY, OWNER),
            "repository-owner-signed lifecycle with no event `a` is authorised"
        );
        assert!(
            !lifecycle_actor_allowed(STRANGER, THIRD_PARTY, OWNER),
            "an unrelated signer with no event `a` is still ignored"
        );
    }

    /// Cross-runtime golden vectors, generated independently with CPython's
    /// `uuid.uuid5` over the same namespace and canonical input:
    ///
    /// ```python
    /// import uuid
    /// ns = uuid.UUID("0a0170ea-22c2-5606-8679-6c72e92c1942")
    /// uuid.uuid5(ns, "0000…0000")  # -> e2971ac5-a240-5c5d-94d9-ab837dd74a3c
    /// ```
    ///
    /// These are the numbers the Hermes Buzz adapter must reproduce. Asserting
    /// against a second `Uuid::new_v5` call here would only prove Rust agrees
    /// with itself, which is not the invariant at risk. If a value changes,
    /// every enrolled session is silently re-keyed — regenerate deliberately,
    /// never to make a test pass.
    #[test]
    fn route_key_matches_python_uuid5_vectors() {
        assert_eq!(
            project_route_key("0000000000000000000000000000000000000000000000000000000000000000")
                .unwrap()
                .to_string(),
            "e2971ac5-a240-5c5d-94d9-ab837dd74a3c"
        );
        assert_eq!(
            project_route_key(ROOT).unwrap().to_string(),
            "a10a99e1-abbe-5111-9405-4ab8e245d93d"
        );
    }

    // ── Sub ids ──────────────────────────────────────────────────────────────

    /// The filters production's discovery subscription carries.
    ///
    /// Read from the production builder rather than spelled out again: the
    /// class and the id are no longer nameable from out here, so the only thing
    /// a test may supply is the same question production supplies.
    fn discovery_filters() -> Vec<Value> {
        discovery_subscription(true).expect("enabled")
    }

    /// A second, narrower discovery question — the same class, a different
    /// filter, which is what a conflict is made of.
    fn other_discovery_filters() -> Vec<Value> {
        vec![json!({ "kinds": [30617], "authors": [AGENT] })]
    }

    /// Does durable intent under `sub_id` ask exactly `filters`?
    ///
    /// The identity itself cannot be built out here to compare against, which
    /// is the point of this tranche. What a test may read is what the request
    /// asks — through the same accessor the registry's own comparisons use.
    fn intent_asks(requests: &ProjectRequests, sub_id: &str, filters: &[Value]) -> bool {
        requests
            .intent(sub_id)
            .is_some_and(|held| held.filters().eq(filters.iter()))
    }

    /// The ids a reconnect would re-ask for, in the order it would ask them.
    fn replay_ids(requests: &ProjectRequests) -> Vec<String> {
        requests
            .replayable()
            .expect("a canonical durable record")
            .iter()
            .map(|request| request.sub_id().to_string())
            .collect()
    }

    #[test]
    fn an_id_this_agent_never_opened_has_no_class() {
        // These all used to classify — the parser read the class out of the
        // relay's own string. Now the question is not "is this id well-formed"
        // but "did we send this", and none of these was sent. A well-formed id
        // we never asked for is exactly the case a parser could not tell apart.
        let requests = ProjectRequests::new();
        for id in [
            PROJECT_ENROL_SUB_ID,
            PROJECT_ROOTS_SUB_ID,
            "proj-",
            "proj-unknown",
            "proj-roots-7",
            "proj-catchup-garbage",
            "ch-550e8400-e29b-41d4-a716-446655440000",
            "membership-notif",
            "agent-observer-control",
            "",
        ] {
            assert!(requests.match_frame(id).is_none(), "{id}");
        }
    }

    #[tokio::test]
    async fn a_frames_class_comes_from_the_registry_not_from_the_id_it_carries() {
        // The substantive inversion, now made twice over. The class used to be
        // the caller's to supply beside an id of its choosing, so this test
        // registered a catch-up-shaped id as watched generation 7 and read the
        // watched class back. That pairing no longer exists: `open_discovery`
        // stamps both halves, so the only class a well-formed id can carry is
        // the one this registry minted for it — and an id spelled like a
        // catch-up for ROOT, never opened, still has no class at all.
        let mut requests = ProjectRequests::new();
        assert_eq!(
            open_discovery_on_test_socket(&mut requests, discovery_filters()).await,
            OpenOutcome::Sent
        );

        assert_eq!(
            requests.match_frame(&discovery_sub_id()),
            Some(&ProjectSubscription::Discovery)
        );
        assert!(
            requests
                .match_frame(&format!("proj-catchup-c-{ROOT}-1"))
                .is_none(),
            "an id nobody opened classifies as nothing, however it is spelled"
        );
    }

    #[tokio::test]
    async fn a_conflicting_open_records_absolutely_nothing() {
        // The trapdoor this closes: an earlier arrangement admitted intent
        // first and consulted the live registry second, so a refused identity
        // was still left sitting in intent — and the next reconnect installed
        // it. A conflict must leave no residue in *either* map, or the refusal
        // is only a delay.
        let mut requests = ProjectRequests::new();
        let sub_id = discovery_sub_id();
        assert_eq!(
            open_discovery_on_test_socket(&mut requests, discovery_filters()).await,
            OpenOutcome::Sent
        );

        assert!(matches!(
            open_discovery_on_test_socket(&mut requests, other_discovery_filters()).await,
            OpenOutcome::Conflict { .. }
        ));

        assert!(intent_asks(&requests, &sub_id, &discovery_filters()));
        assert_eq!(
            requests.match_frame(&sub_id),
            Some(&ProjectSubscription::Discovery)
        );
        assert_eq!(
            replay_ids(&requests),
            vec![sub_id.clone()],
            "and the refused identity is not what a reconnect would re-ask"
        );
        assert!(
            intent_asks(&requests, &sub_id, &discovery_filters()),
            "the reconnect re-asks the original question, not the refused one"
        );
    }

    #[tokio::test]
    async fn an_open_differing_only_in_filter_is_a_conflict_not_a_no_op() {
        // Same class, different question. When the live registry stored only
        // the class it answered "already live", emitted no REQ, and let the
        // other filter through as what the next connection would ask for. The
        // class is now the registry's, so *only* the filter can differ — which
        // makes this the whole of what the comparison has left to catch.
        let mut requests = ProjectRequests::new();
        let sub_id = discovery_sub_id();
        assert_eq!(
            open_discovery_on_test_socket(&mut requests, discovery_filters()).await,
            OpenOutcome::Sent
        );
        assert!(matches!(
            open_discovery_on_test_socket(&mut requests, other_discovery_filters()).await,
            OpenOutcome::Conflict { .. }
        ));
        assert!(intent_asks(&requests, &sub_id, &discovery_filters()));
    }

    #[tokio::test]
    async fn an_identical_open_is_already_live_and_emits_no_second_request() {
        let mut requests = ProjectRequests::new();
        assert_eq!(
            open_discovery_on_test_socket(&mut requests, discovery_filters()).await,
            OpenOutcome::Sent
        );
        assert_eq!(
            open_discovery_on_test_socket(&mut requests, discovery_filters()).await,
            OpenOutcome::AlreadyLive,
            "this exact request is live; asking again must not re-send"
        );
        assert_eq!(requests.live_len(), 1);
        assert_eq!(requests.intent_len(), 1);
    }

    #[tokio::test]
    async fn an_open_whose_filters_constrain_nothing_is_refused_and_records_nothing() {
        // The refusal belongs to the owner. When the caller built the identity
        // it also made this decision, one call earlier, and a decision made
        // outside the owner is one a second caller can make differently — the
        // shape that let an unbounded REQ be recorded as intent and replayed
        // onto the next connection.
        let mut requests = ProjectRequests::new();
        let narrow = json!({ "kinds": [30617] });
        for unbounded in [
            Vec::new(),
            vec![json!({})],
            vec![json!({ "limit": 500 })],
            // Not an object at all — including the two-filters-in-one-element
            // mistake, which is refused rather than merely unmatchable.
            vec![json!([{ "kinds": [30617] }])],
            vec![json!("everything")],
            vec![Value::Null],
            // Narrow branch, unbounded branch. The OR makes it unbounded.
            vec![narrow.clone(), json!({})],
            vec![json!({}), narrow.clone()],
        ] {
            assert_eq!(
                open_discovery_on_test_socket(&mut requests, unbounded.clone()).await,
                OpenOutcome::UnboundedFilters,
                "must refuse: {unbounded:?}"
            );
            assert_eq!(
                requests.record_discovery_intent(unbounded.clone()),
                IntentAdmission::UnboundedFilters,
                "and the offline half refuses the same shapes: {unbounded:?}"
            );
        }
        assert_eq!(requests.live_len(), 0, "nothing became answerable");
        assert_eq!(requests.intent_len(), 0, "and nothing is replayed");

        // The positive control: the gate is not achieved by refusing
        // everything, and a limit is fine *alongside* something selective.
        assert_eq!(
            open_discovery_on_test_socket(
                &mut requests,
                vec![json!({ "kinds": [30617], "limit": 500 })]
            )
            .await,
            OpenOutcome::Sent
        );
    }

    #[tokio::test]
    async fn a_failed_send_keeps_the_intent_it_failed_to_send() {
        // The write failed; the intent did not. Dropping intent here would let
        // one bad write permanently stop asking.
        //
        // There is no `roll_back` to call any more: nothing is registered until
        // the write returns, so a failure has nothing to undo. The failure has
        // to come from a real dead socket.
        let mut requests = ProjectRequests::new();
        let sub_id = discovery_sub_id();
        let mut socket = test_socket().await;
        socket.client.close(None).await.expect("close");
        assert!(matches!(
            requests
                .open_discovery(&mut socket.client, discovery_filters())
                .await,
            OpenOutcome::WriteFailed(_)
        ));

        assert!(
            requests.match_frame(&sub_id).is_none(),
            "nothing answerable"
        );
        assert!(intent_asks(&requests, &sub_id, &discovery_filters()));
        assert_eq!(replay_ids(&requests), vec![sub_id]);
    }

    #[tokio::test]
    async fn a_suspended_request_is_not_offered_again_until_the_connection_changes() {
        // A relay refusal is scoped to the connection that issued it. It must
        // stop a proactive resubscribe on that same connection, and must not
        // survive it.
        let mut requests = ProjectRequests::new();
        let sub_id = discovery_sub_id();
        open_discovery_on_test_socket(&mut requests, discovery_filters()).await;

        assert!(requests.refuse_live(&sub_id, "restricted: nope").is_some());

        assert_eq!(requests.suspension(&sub_id), Some("restricted: nope"));
        assert!(
            replay_ids(&requests).is_empty(),
            "a proactive resubscribe on this connection must skip it"
        );
        assert!(
            intent_asks(&requests, &sub_id, &discovery_filters()),
            "but local policy survives the relay's opinion"
        );

        requests.clear_connection();
        assert_eq!(requests.suspension(&sub_id), None);
        assert_eq!(
            replay_ids(&requests),
            vec![sub_id],
            "and a new connection asks once more"
        );
    }

    /// A real paired WebSocket.
    ///
    /// [`ProjectReqSink`] is sealed and implemented only for the live socket,
    /// so a test cannot substitute "the write succeeded" at the authority
    /// boundary. It injects a genuine socket at the transport layer instead —
    /// which is the point: the previous helper passed
    /// `|_| async { Ok(()) }` and manufactured send authority with nothing
    /// listening.
    struct TestSocket {
        client: tokio_tungstenite::WebSocketStream<
            tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
        >,
        _server: tokio_tungstenite::WebSocketStream<tokio::net::TcpStream>,
    }

    async fn test_socket() -> TestSocket {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind test socket");
        let address = listener.local_addr().expect("read test address");
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.expect("accept");
            tokio_tungstenite::accept_async(stream)
                .await
                .expect("server handshake")
        });
        let (client, _) = tokio_tungstenite::connect_async(format!("ws://{address}"))
            .await
            .expect("client handshake");
        TestSocket {
            client,
            _server: server.await.expect("join server"),
        }
    }

    /// A paired socket whose **server side is kept and readable**.
    ///
    /// `TestSocket` drops its server, which is fine for "did the write
    /// succeed" but proves nothing about what the relay received. Replacement
    /// is about ordering and content on the wire, so these tests read frames.
    struct WireSocket {
        client: tokio_tungstenite::WebSocketStream<
            tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
        >,
        server: tokio_tungstenite::WebSocketStream<tokio::net::TcpStream>,
    }

    async fn wire_socket() -> WireSocket {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind wire socket");
        let address = listener.local_addr().expect("read address");
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.expect("accept");
            tokio_tungstenite::accept_async(stream)
                .await
                .expect("server handshake")
        });
        let (client, _) = tokio_tungstenite::connect_async(format!("ws://{address}"))
            .await
            .expect("client handshake");
        WireSocket {
            client,
            server: server.await.expect("join server"),
        }
    }

    impl WireSocket {
        /// Next frame the relay actually received, as JSON.
        async fn next_frame(&mut self) -> serde_json::Value {
            use futures_util::StreamExt;
            let msg = tokio::time::timeout(std::time::Duration::from_secs(2), self.server.next())
                .await
                .expect("timed out waiting for a frame")
                .expect("socket closed")
                .expect("read frame");
            serde_json::from_str(msg.to_text().expect("text frame")).expect("parse frame")
        }

        /// Every frame currently readable without blocking.
        async fn drain_frames(&mut self) -> Vec<serde_json::Value> {
            use futures_util::StreamExt;
            let mut out = Vec::new();
            while let Ok(Some(Ok(msg))) =
                tokio::time::timeout(std::time::Duration::from_millis(200), self.server.next())
                    .await
            {
                if let Ok(text) = msg.to_text() {
                    if let Ok(v) = serde_json::from_str(text) {
                        out.push(v);
                    }
                }
            }
            out
        }
    }

    /// Watched-root filters over the given roots, built the production way.
    ///
    /// Filters, not an identity: the class and the generation are the
    /// registry's to stamp, and `replace_watched` is the only thing that
    /// stamps them.
    fn watched_filters_for(roots: &[&str]) -> Vec<Value> {
        let mut enrolments = ProjectEnrolments::new();
        for root in roots {
            enrolments
                .enrol(&EnrolmentCandidate::for_test(
                    root,
                    &coord(),
                    OWNER,
                    STRANGER,
                    false,
                ))
                .expect("enrol");
        }
        let filters = watched_roots_filters(&enrolments, 0);
        assert!(!filters.is_empty(), "watched filters must not be empty");
        filters
    }

    /// Enrolment filters over the given discovered coordinates.
    fn enrolment_filters_for(coords: &[&str]) -> Vec<Value> {
        let discovered = known(coords);
        vec![enrolment_filter(&discovered, AGENT, 0).expect("filter")]
    }

    /// **A second discovered repository widens the enrolment filter.**
    ///
    /// This is the defect that shipped: the enrolment id is fixed, so the
    /// second identity differed under the same id and `open_request` refused
    /// it as `Conflict` — which the caller read as handled. The filter could
    /// never grow past the first repository.
    #[tokio::test]
    async fn a_second_discovery_widens_the_enrolment_filter_on_the_wire() {
        let mut requests = ProjectRequests::new();
        let mut socket = wire_socket().await;
        let id = PROJECT_ENROL_SUB_ID;

        assert_eq!(
            requests
                .replace_enrolment(&mut socket.client, enrolment_filters_for(&[&coord()]))
                .await,
            ReplaceOutcome::Replaced { retired: None }
        );
        let frame = socket.next_frame().await;
        assert_eq!(frame[0], "REQ");
        assert_eq!(frame[1], id);
        let first_text = frame.to_string();

        let second_coord = format!("30617:{OWNER}:second-repo");
        assert_eq!(
            requests
                .replace_enrolment(
                    &mut socket.client,
                    enrolment_filters_for(&[&coord(), &second_coord])
                )
                .await,
            ReplaceOutcome::Replaced { retired: None },
            "same-id replacement retires nothing: the relay's REQ replacement does it"
        );

        let frame = socket.next_frame().await;
        assert_eq!(frame[0], "REQ", "the widened filter is a REQ, not a CLOSE");
        assert_eq!(frame[1], id, "replacement reuses the enrolment id");
        let widened_text = frame.to_string();
        assert_ne!(first_text, widened_text, "the filter did not change");
        assert!(
            widened_text.contains("second-repo"),
            "the second repository is absent from the widened filter: {widened_text}"
        );
    }

    /// **The successor REQ is written before the predecessor CLOSE.**
    ///
    /// Reversed, there is a window with no live subscription, and a successor
    /// that then failed would leave nothing at all.
    #[tokio::test]
    async fn the_successor_req_precedes_the_predecessor_close() {
        let mut requests = ProjectRequests::new();
        let mut socket = wire_socket().await;

        let gen0 = watched_sub_id(0);
        let gen1 = watched_sub_id(1);

        requests
            .replace_watched(&mut socket.client, watched_filters_for(&[ROOT]))
            .await;
        let _ = socket.next_frame().await;

        assert_eq!(
            requests
                .replace_watched(&mut socket.client, watched_filters_for(&[ROOT, OTHER_ROOT]))
                .await,
            ReplaceOutcome::Replaced {
                retired: Some(gen0.clone())
            },
            "the registry allocates generation 1 and names generation 0 as its own predecessor"
        );

        let frames = socket.drain_frames().await;
        assert_eq!(
            frames.len(),
            2,
            "expected successor REQ then predecessor CLOSE"
        );
        assert_eq!(frames[0][0], "REQ");
        assert_eq!(frames[0][1], gen1, "successor first");
        assert_eq!(frames[1][0], "CLOSE");
        assert_eq!(frames[1][1], gen0, "predecessor closed second");
    }

    /// The successor carries the complete new root set, not a delta.
    #[tokio::test]
    async fn the_watched_successor_carries_the_whole_root_set() {
        let mut requests = ProjectRequests::new();
        let mut socket = wire_socket().await;

        requests
            .replace_watched(&mut socket.client, watched_filters_for(&[ROOT]))
            .await;
        let _ = socket.next_frame().await;

        requests
            .replace_watched(&mut socket.client, watched_filters_for(&[ROOT, OTHER_ROOT]))
            .await;

        let req = socket.next_frame().await;
        let text = req.to_string();
        assert!(text.contains(ROOT), "successor lost the original root");
        assert!(text.contains(OTHER_ROOT), "successor lacks the new root");
    }

    /// **A failed successor leaves the predecessor intact.**
    ///
    /// Live registration and durable intent both. Otherwise a transient write
    /// error silently unsubscribes the agent from roots it is enrolled on.
    #[tokio::test]
    async fn a_failed_successor_preserves_the_predecessor() {
        let mut requests = ProjectRequests::new();
        let mut socket = wire_socket().await;

        let gen0 = watched_sub_id(0);
        let gen1 = watched_sub_id(1);
        requests
            .replace_watched(&mut socket.client, watched_filters_for(&[ROOT]))
            .await;
        let _ = socket.next_frame().await;

        // A genuinely closed transport, not a simulated one. Dropping the peer
        // is not enough: tungstenite buffers into the OS socket and the write
        // still returns `Ok`. Closing the client's own WebSocket makes the
        // next send fail for real, which is the condition under test.
        <_ as futures_util::SinkExt<tokio_tungstenite::tungstenite::Message>>::close(
            &mut socket.client,
        )
        .await
        .expect("close the client transport");
        drop(socket.server);
        let outcome = requests
            .replace_watched(&mut socket.client, watched_filters_for(&[ROOT, OTHER_ROOT]))
            .await;

        assert!(
            matches!(outcome, ReplaceOutcome::WriteFailed(_)),
            "expected a write failure, got {outcome:?}"
        );
        assert!(
            intent_asks(&requests, &gen0, &watched_filters_for(&[ROOT])),
            "the predecessor's durable intent was discarded on a failed successor"
        );
        assert!(
            requests.intent(&gen1).is_none(),
            "a successor that never reached the socket left durable intent behind"
        );
    }

    /// Reconnect replays the successor only — retired generations do not
    /// accumulate in durable intent.
    #[tokio::test]
    async fn reconnect_replays_only_the_latest_generation() {
        let mut requests = ProjectRequests::new();
        let mut socket = wire_socket().await;

        // Three genuinely different root sets, so each replacement is a real
        // change and burns its own generation — repeating a set would report
        // `Unchanged` and allocate nothing, which is a different property with
        // its own test.
        for roots in [vec![ROOT], vec![ROOT, OTHER_ROOT], vec![OTHER_ROOT]] {
            requests
                .replace_watched(&mut socket.client, watched_filters_for(&roots))
                .await;
            let _ = socket.drain_frames().await;
        }

        requests.clear_connection();
        let replay = replay_ids(&requests);
        assert_eq!(
            replay,
            vec![watched_sub_id(2)],
            "every retired generation must be gone from durable intent"
        );
    }

    /// The authority distinction is preserved: opening still refuses to change
    /// an id's identity, and replacement is the only operation permitted to
    /// make that change.
    ///
    /// A metadata conflict therefore cannot masquerade as a successful
    /// replacement — the two outcomes come from different operations, and the
    /// one that refuses has not been loosened.
    ///
    /// The two halves are shown on the two subscriptions that have them.
    /// Opening is discovery's only route and it has no replacement; enrolment
    /// has only a replacement. Neither is reachable from the other, which is
    /// itself the distinction: an id cannot be reclassified by asking again.
    #[tokio::test]
    async fn a_metadata_conflict_cannot_masquerade_as_replacement() {
        let mut requests = ProjectRequests::new();

        assert_eq!(
            open_discovery_on_test_socket(&mut requests, discovery_filters()).await,
            OpenOutcome::Sent
        );
        let refused = open_discovery_on_test_socket(&mut requests, other_discovery_filters()).await;
        assert!(
            matches!(refused, OpenOutcome::Conflict { .. }),
            "opening must still refuse to change an id's identity, got {refused:?}"
        );

        let mut socket = wire_socket().await;
        assert_eq!(
            requests
                .replace_enrolment(&mut socket.client, enrolment_filters_for(&[&coord()]))
                .await,
            ReplaceOutcome::Replaced { retired: None }
        );
        let _ = socket.next_frame().await;

        let second = format!("30617:{OWNER}:second-repo");
        assert_eq!(
            requests
                .replace_enrolment(
                    &mut socket.client,
                    enrolment_filters_for(&[&coord(), &second])
                )
                .await,
            ReplaceOutcome::Replaced { retired: None },
            "replacement is the operation permitted to make that change"
        );
    }

    /// Open the discovery subscription the only way one can be opened.
    ///
    /// Goes through `ProjectRequests::open_discovery` against a **real paired
    /// socket**. It is async for the same reason production is: the registry
    /// performs the write, and there is no synchronous shortcut for a test to
    /// take because there is none for anyone.
    ///
    /// It takes filters and nothing else, because that is all the operation
    /// takes. The id and the class used to be arguments here too, and being the
    /// helper most tests copy, it was the widest route to a caller-selected
    /// identity in the file.
    async fn open_discovery_on_test_socket(
        requests: &mut ProjectRequests,
        filters: Vec<Value>,
    ) -> OpenOutcome {
        let mut socket = test_socket().await;
        requests.open_discovery(&mut socket.client, filters).await
    }

    #[tokio::test]
    async fn an_eose_witness_requires_a_live_request() {
        // EOSE is the boundary any completion claim would rest on, so it is
        // authenticated exactly as `CLOSED` and `EVENT` are: by an exact live
        // registration, never by the id's spelling. An EOSE for a request we
        // never sent is the relay's word, not evidence about our backlog.
        let mut requests = ProjectRequests::new();
        let sub_id = discovery_sub_id();

        assert_eq!(
            requests.witness_end_of_stored_events(&sub_id),
            None,
            "an id that was never opened yields no witness"
        );

        open_discovery_on_test_socket(&mut requests, discovery_filters()).await;
        let witness = requests
            .witness_end_of_stored_events(&sub_id)
            .expect("a live request yields one");
        assert_eq!(witness.sub_id(), sub_id);
        assert_eq!(witness.subscription(), &ProjectSubscription::Discovery);

        // Intent alone is not evidence that we asked on this connection.
        requests.clear_connection();
        assert!(requests.intent(&sub_id).is_some(), "policy survives");
        assert_eq!(
            requests.witness_end_of_stored_events(&sub_id),
            None,
            "but a dead connection's request cannot witness anything"
        );
    }

    #[tokio::test]
    async fn a_witness_from_a_replaced_request_is_not_the_current_one() {
        // The seam hermes-gateway found: id, class and filter are all reused on
        // reconnect, so a witness described only by those is interchangeable
        // with its predecessor's. The predecessor's boundary proves nothing
        // about the gap the replacement had to recover.
        let mut requests = ProjectRequests::new();
        let sub_id = discovery_sub_id();

        open_discovery_on_test_socket(&mut requests, discovery_filters()).await;
        let before = requests
            .witness_end_of_stored_events(&sub_id)
            .expect("live");
        assert!(requests.is_live_boundary(&before));

        // Connection dies; the identical request is re-sent on the next one.
        requests.clear_connection();
        open_discovery_on_test_socket(&mut requests, discovery_filters()).await;
        let after = requests
            .witness_end_of_stored_events(&sub_id)
            .expect("live again");

        assert_ne!(
            before, after,
            "identical description, different instance — they must differ"
        );
        assert_ne!(before.incarnation(), after.incarnation());
        assert_eq!(
            before.sub_id(),
            after.sub_id(),
            "and the thing that differs is not the id"
        );
        assert_eq!(before.subscription(), after.subscription());

        assert!(
            !requests.is_live_boundary(&before),
            "a queued predecessor witness cannot complete the replacement"
        );
        assert!(
            requests.is_live_boundary(&after),
            "the replacement's own boundary can"
        );
    }

    // `an_exhausted_incarnation_space_refuses_rather_than_wrapping` and
    // `exhaustion_does_not_disturb_requests_already_live` stood here. Both
    // needed a registry whose allocator was already at `u64::MAX`, and the only
    // way to produce one was to hand the real owner a composed allocator
    // position — provenance no production operation could have written, which
    // is what a caller-chosen predecessor was made of. The arithmetic they were
    // really about is proved below against the allocator itself, which nothing
    // can install; what is no longer proved at the registry is its *handling*
    // of a spent allocator — `OpenOutcome::Exhausted`, an untouched live
    // registration beside a refused one — because reaching that state honestly
    // costs 2^64 registrations.

    /// **The last value is handed out exactly once, and then never again.**
    ///
    /// `+= 1` on a `u64` panics in debug and wraps in release. Wrapping is the
    /// dangerous half: incarnation 0 would come round again and authenticate a
    /// boundary minted an eternity earlier — the exact substitution the
    /// incarnation exists to prevent — and it would happen only in the build
    /// where no panic could warn anyone.
    #[test]
    fn a_spent_allocator_refuses_rather_than_wrapping() {
        let mut counter = CheckedCounter::at(u64::MAX);
        assert_eq!(
            counter.burn(),
            Some(u64::MAX),
            "the last value is legitimately issued"
        );
        assert!(counter.is_spent(), "and the space is now spent");
        for _ in 0..3 {
            assert_eq!(counter.burn(), None, "no wrap, no reuse — a refusal");
            assert!(
                counter.is_spent(),
                "spent is never cleared — no later call revives it"
            );
            assert_eq!(
                counter.state(),
                AllocatorState::Spent,
                "and it describes itself as spent to every preflight that asks"
            );
        }
    }

    /// The saturating step, from one below the ceiling: two values, then stop.
    #[test]
    fn an_allocator_at_the_ceiling_issues_both_remaining_values() {
        let mut counter = CheckedCounter::at(u64::MAX - 1);
        assert_eq!(counter.burn(), Some(u64::MAX - 1));
        assert!(
            !counter.is_spent(),
            "one value remains, so the space is not yet spent"
        );
        assert_eq!(counter.next_value(), u64::MAX);
        assert_eq!(counter.burn(), Some(u64::MAX));
        assert_eq!(counter.burn(), None);
    }

    /// A fresh allocator hands out `0`, then `1`, and only ever increases.
    #[test]
    fn an_allocator_only_ever_increases() {
        let mut counter = CheckedCounter::default();
        let taken: Vec<u64> = (0..8).filter_map(|_| counter.burn()).collect();
        assert_eq!(taken, (0..8).collect::<Vec<u64>>());
        assert!(!counter.is_spent());
    }

    #[tokio::test]
    async fn an_incarnation_is_never_reused() {
        // Monotonic, not merely different from its immediate predecessor. A
        // counter that recycled values would let an old witness match a much
        // later request.
        let mut requests = ProjectRequests::new();
        let sub_id = discovery_sub_id();
        let mut seen = Vec::new();

        for _ in 0..8 {
            assert_eq!(
                open_discovery_on_test_socket(&mut requests, discovery_filters()).await,
                OpenOutcome::Sent
            );
            seen.push(requests.live_incarnation(&sub_id).expect("live"));
            requests.clear_connection();
        }

        let mut sorted = seen.clone();
        sorted.sort();
        sorted.dedup();
        assert_eq!(sorted.len(), seen.len(), "every instance is distinct");
        assert_eq!(sorted, seen, "and they only ever increase");
    }

    #[tokio::test]
    async fn distinct_requests_get_distinct_incarnations() {
        let mut requests = ProjectRequests::new();
        let discovery = discovery_sub_id();
        let watched = watched_sub_id(0);
        let mut socket = test_socket().await;
        open_discovery_on_test_socket(&mut requests, discovery_filters()).await;
        requests
            .replace_watched(&mut socket.client, watched_filters_for(&[ROOT]))
            .await;

        assert_ne!(
            requests.live_incarnation(&discovery),
            requests.live_incarnation(&watched)
        );

        // And a witness from one is not current for the other.
        let w = requests
            .witness_end_of_stored_events(&discovery)
            .expect("live");
        assert!(requests.is_live_boundary(&w));
        assert_eq!(w.sub_id(), discovery);
    }

    #[tokio::test]
    async fn a_witness_is_not_current_once_its_request_is_refused() {
        let mut requests = ProjectRequests::new();
        let sub_id = discovery_sub_id();
        open_discovery_on_test_socket(&mut requests, discovery_filters()).await;
        let witness = requests
            .witness_end_of_stored_events(&sub_id)
            .expect("live");

        requests.refuse_live(&sub_id, "restricted: nope");
        assert!(!requests.is_live_boundary(&witness));
    }

    #[tokio::test]
    async fn witnessing_an_eose_does_not_retire_the_request() {
        // EOSE means end of *stored* events. Discovery, enrolment and watched
        // subscriptions keep delivering live traffic afterwards, so closing on
        // EOSE would silently stop routing the moment a backlog drained. Only
        // a one-shot catch-up retires here, and the class is read from what
        // this agent recorded when it sent the REQ.
        let mut requests = ProjectRequests::new();
        let sub_id = discovery_sub_id();
        open_discovery_on_test_socket(&mut requests, discovery_filters()).await;

        for _ in 0..3 {
            assert!(requests.witness_end_of_stored_events(&sub_id).is_some());
        }
        assert!(
            requests.match_frame(&sub_id).is_some(),
            "still answerable after its backlog drained"
        );
        assert_eq!(requests.live_len(), 1);
    }

    #[tokio::test]
    async fn a_refused_request_can_no_longer_witness_an_eose() {
        // A relay that refuses a request and then announces its backlog is
        // complete must not be believed about the request it just declined.
        let mut requests = ProjectRequests::new();
        let sub_id = discovery_sub_id();
        open_discovery_on_test_socket(&mut requests, discovery_filters()).await;
        requests.refuse_live(&sub_id, "restricted: nope");

        assert_eq!(requests.witness_end_of_stored_events(&sub_id), None);
    }

    #[test]
    fn refusing_an_id_that_is_not_live_changes_nothing() {
        // The invariant made structural. `refuse_live` is the only way to
        // record a suspension, and it requires an exact live registration
        // first — so an unsolicited `CLOSED` for an id we merely *intend*
        // cannot suspend anything, regardless of what the caller does.
        let mut requests = ProjectRequests::new();
        let sub_id = discovery_sub_id();
        assert_eq!(
            requests.record_discovery_intent(discovery_filters()),
            IntentAdmission::Recorded
        );

        assert_eq!(
            requests.refuse_live(&sub_id, "restricted: nope"),
            None,
            "intent is not evidence that we asked on this connection"
        );
        assert_eq!(requests.suspension(&sub_id), None);
        assert!(intent_asks(&requests, &sub_id, &discovery_filters()));
        assert_eq!(
            replay_ids(&requests),
            vec![sub_id],
            "and the next connection still asks"
        );
    }

    #[tokio::test]
    async fn clearing_a_connection_keeps_policy_and_drops_everything_answerable() {
        let mut requests = ProjectRequests::new();
        let sub_id = watched_sub_id(0);
        let mut socket = test_socket().await;
        requests
            .replace_watched(&mut socket.client, watched_filters_for(&[ROOT]))
            .await;

        requests.clear_connection();

        assert!(requests.match_frame(&sub_id).is_none());
        assert!(intent_asks(
            &requests,
            &sub_id,
            &watched_filters_for(&[ROOT])
        ));
    }

    // Deleted 2026-08-02: `overlapping_watched_generations_are_both_live_until_the_old_one_closes`.
    //
    // It opened two watched generations side by side through
    // `open_request_on_test_socket`, asserted both answered, then closed the
    // older with `close_active`. Neither call exists for any caller now: a
    // watched generation is allocated and installed only by `replace_watched`,
    // which retires its own predecessor as part of the same operation, and the
    // bare close it used went with it. So there is no longer a way to leave two
    // generations live and then retire one by hand.
    //
    // Its subject survives in the two assertions that were doing the work.
    // That the successor is live before the predecessor stops being so is
    // `the_successor_req_precedes_the_predecessor_close`, on the wire rather
    // than in the map — the REQ is written before the CLOSE, which is the
    // ordering the overlap existed for. That the retired one then answers
    // nothing is `a_retired_project_request_stops_being_answerable`.

    #[test]
    fn recording_intent_while_disconnected_is_fail_closed_too() {
        // Intent recorded while disconnected is replayed verbatim by the next
        // connection, so admitting a conflict here admits it everywhere.
        //
        // The conflicting submission is a second *question*, because the class
        // is no longer the caller's to vary. That is the narrower and harder
        // case: two requests that differ only in what they ask are exactly
        // what a registry storing the class alone could not tell apart.
        let mut requests = ProjectRequests::new();
        let sub_id = discovery_sub_id();
        assert_eq!(
            requests.record_discovery_intent(discovery_filters()),
            IntentAdmission::Recorded
        );
        assert_eq!(
            requests.record_discovery_intent(discovery_filters()),
            IntentAdmission::AlreadyIntended
        );
        assert!(matches!(
            requests.record_discovery_intent(other_discovery_filters()),
            IntentAdmission::Conflict { .. }
        ));
        assert!(intent_asks(&requests, &sub_id, &discovery_filters()));
        assert_eq!(requests.live_len(), 0, "recording intent registers nothing");
    }

    #[tokio::test]
    async fn every_subscription_class_reaches_the_wire_through_its_own_operation() {
        // Positive control: the gate is not achieved by refusing everything.
        //
        // One operation per class, which is what replaced the single
        // `open_request(sub_id, identity)` this test used to walk a class list
        // through. The id and the generation below are *assertions* about what
        // the registry stamped, not arguments handed to it.
        let mut requests = ProjectRequests::new();
        let mut socket = wire_socket().await;

        assert_eq!(
            requests
                .open_discovery(&mut socket.client, discovery_filters())
                .await,
            OpenOutcome::Sent
        );
        assert_eq!(
            requests
                .replace_enrolment(&mut socket.client, enrolment_filters_for(&[&coord()]))
                .await,
            ReplaceOutcome::Replaced { retired: None }
        );
        assert_eq!(
            requests
                .replace_watched(&mut socket.client, watched_filters_for(&[ROOT]))
                .await,
            ReplaceOutcome::Replaced { retired: None }
        );

        assert_eq!(
            requests.match_frame(&discovery_sub_id()),
            Some(&ProjectSubscription::Discovery)
        );
        assert_eq!(
            requests.match_frame(PROJECT_ENROL_SUB_ID),
            Some(&ProjectSubscription::Enrolment)
        );
        assert_eq!(
            requests.match_frame(&watched_sub_id(0)),
            Some(&ProjectSubscription::Watched { generation: 0 }),
            "the first watched replacement is generation 0, allocated by the registry"
        );
        assert_eq!(requests.live_len(), 3);

        // The fourth class is absent on purpose, and no longer by a refusal a
        // caller could reach: a catch-up's wire id has to name one transport
        // attempt, only `open_history_page` mints those, and no operation on
        // this registry accepts a catch-up class from outside at all.
        let ids: Vec<String> = replay_ids(&requests);
        assert_eq!(
            ids,
            vec![
                discovery_sub_id(),
                PROJECT_ENROL_SUB_ID.to_string(),
                watched_sub_id(0)
            ],
            "and the durable record holds exactly the three replayable classes"
        );
    }

    // ── Root extraction ──────────────────────────────────────────────────────

    /// Build a candidate the only way production can: through the validator.
    ///
    /// Deliberately not a struct literal. `mod tests` is a child module and
    /// *could* reach the private fields, but then the tests would exercise a
    /// construction path no caller has.
    /// Sign an event of `kind` carrying `tags`, and verify it.
    ///
    /// Validation tests go through a real witness now that the validator reads
    /// kind, id and tags from one event rather than accepting them separately.
    async fn verified_with(kind: u32, tags: &[Vec<String>]) -> VerifiedProjectEvent {
        let keys = Keys::generate();
        let event = signed(&keys, kind, tags.to_vec());
        VerifiedProjectEvent::verify(event).await.expect("valid")
    }

    /// Enrolment-set fixtures need specific root ids, which a real signed event
    /// cannot be made to have. These tests exercise `ProjectEnrolments`, not
    /// validation; validation has its own witness-driven tests below.
    fn candidate_at(root: &str, coordinate: &str, pr: bool) -> EnrolmentCandidate {
        candidate_authored_at(root, coordinate, STRANGER, pr)
    }

    /// The same fixture with the root's author named.
    ///
    /// Separate because the author is exactly what lifecycle authority turns
    /// on, and a default that happened to equal the owner would make an
    /// authority test pass for the wrong reason. `candidate_at` therefore
    /// defaults to a third party who is neither the owner nor this agent.
    fn candidate_authored_at(
        root: &str,
        coordinate: &str,
        root_author: &str,
        pr: bool,
    ) -> EnrolmentCandidate {
        let owner = repo_owner_from_coordinate(coordinate).expect("well-formed coordinate");
        EnrolmentCandidate::for_test(root, coordinate, &owner, root_author, pr)
    }

    fn candidate(root: &str, pr: bool) -> EnrolmentCandidate {
        candidate_at(root, &coord(), pr)
    }

    /// A kind-`1` comment, which is the surface that can wake a turn.
    fn comment(
        author: ProjectAuthor,
        call: CallMarker,
        state: RootState,
        addressing: Addressing,
    ) -> ProjectEffect {
        classify_project_event(
            classify_kind(KIND_TEXT_NOTE),
            author,
            call,
            state,
            addressing,
            false,
            false,
        )
    }

    const ALL_ADDRESSING: [Addressing; 3] = [
        Addressing::ExplicitMention,
        Addressing::InheritedParticipant,
        Addressing::WatchedRoot,
    ];

    /// Does the live registration installed for `filters` admit `event`?
    ///
    /// Through the production admission boundary, which is the only route
    /// left: the identity is minted inside the registry, so what a test can
    /// hold is the admission a real inbound frame would be checked against.
    /// `replace_watched` is the operation that installs one over arbitrary
    /// filters; the generation it stamps is its own.
    async fn watched_admits(filters: Vec<Value>, event: &nostr::Event) -> bool {
        let mut requests = ProjectRequests::new();
        let mut socket = test_socket().await;
        assert!(
            matches!(
                requests.replace_watched(&mut socket.client, filters).await,
                ReplaceOutcome::Replaced { .. }
            ),
            "the filters under test must install, or the admission proves nothing"
        );
        requests
            .admit_frame(&watched_sub_id(0))
            .expect("the replacement is live on this connection")
            .admits(event)
    }

    /// A filter key this code does not understand admits nothing.
    ///
    /// The direction matters. A matcher that skipped what it could not check
    /// would keep passing while a request grew a constraint it silently stopped
    /// enforcing — the failure would be invisible precisely because the filter
    /// looked more specific than the check. Refusing instead turns that into a
    /// visible outage rather than a silent widening.
    #[tokio::test]
    async fn a_filter_constraint_this_code_cannot_check_admits_nothing() {
        let keys = Keys::generate();
        let event = signed(&keys, KIND_TEXT_NOTE, vec![tag(&["e", ROOT, "", "root"])]);

        assert!(
            watched_admits(
                vec![json!({ "kinds": [KIND_TEXT_NOTE], "#e": [ROOT] })],
                &event
            )
            .await,
            "the positive control"
        );

        // All still installable: each names at least one selective constraint,
        // so it is a *narrow* request this code cannot evaluate — not an
        // unbounded one. The distinction matters, and the two failures are
        // opposite: an unreadable filter admits nothing, an unbounded one
        // admits everything. Only the second is refused at installation, and
        // `no_route_installs_a_request_that_constrains_nothing` is where that
        // is asserted.
        for unreadable in [
            json!({ "kinds": [KIND_TEXT_NOTE], "#e": [ROOT], "search": "anything" }),
            json!({ "kinds": [KIND_TEXT_NOTE], "#e": [ROOT], "unknown": 1 }),
            // Shapes this crate never writes: a tag list holding non-strings,
            // a `since` that is not a number.
            json!({ "#e": [7] }),
            json!({ "since": "soon" }),
        ] {
            assert!(
                !watched_admits(vec![unreadable.clone()], &event).await,
                "must refuse rather than skip: {unreadable}"
            );
        }
    }

    /// OR across a REQ's filters, AND within each one.
    ///
    /// NIP-01's rule, and therefore the relay's: an event the relay was entitled
    /// to send under this id satisfies *some one* filter completely. The
    /// dangerous looseness is the one in between — matching the kinds of one
    /// branch and the tags of another describes a request nobody sent, and a
    /// matcher that merged the branches into one constraint set would admit it.
    #[tokio::test]
    async fn a_request_admits_an_event_matching_any_one_of_its_filters_entirely() {
        const OTHER: &str = "48be1cc2000000000000000000000000000000000000000000000000000000ac";
        let keys = Keys::generate();
        let both = vec![
            json!({ "kinds": [KIND_TEXT_NOTE], "#e": [ROOT] }),
            json!({ "kinds": [KIND_GIT_PULL_REQUEST], "#E": [OTHER] }),
        ];

        // Either branch, satisfied whole.
        assert!(
            watched_admits(
                both.clone(),
                &signed(&keys, KIND_TEXT_NOTE, vec![tag(&["e", ROOT, "", "root"])])
            )
            .await
        );
        assert!(
            watched_admits(
                both.clone(),
                &signed(&keys, KIND_GIT_PULL_REQUEST, vec![tag(&["E", OTHER])])
            )
            .await
        );

        // Half of each. Neither filter is satisfied, so neither admits.
        for crossed in [
            signed(&keys, KIND_TEXT_NOTE, vec![tag(&["E", OTHER])]),
            signed(
                &keys,
                KIND_GIT_PULL_REQUEST,
                vec![tag(&["e", ROOT, "", "root"])],
            ),
        ] {
            assert!(
                !watched_admits(both.clone(), &crossed).await,
                "a filter is satisfied whole or not at all"
            );
        }

        // And one unreadable branch does not become a licence for the other:
        // an event matching only the branch this code cannot check is refused.
        assert!(
            !watched_admits(
                vec![
                    json!({ "kinds": [KIND_TEXT_NOTE], "#e": [ROOT] }),
                    json!({ "kinds": [KIND_GIT_PULL_REQUEST], "unknown": 1 }),
                ],
                &signed(&keys, KIND_GIT_PULL_REQUEST, Vec::new())
            )
            .await
        );
    }

    // Renamed 2026-08-02: `a_request_that_constrains_nothing_cannot_be_built`
    // is now `no_route_installs_a_request_that_constrains_nothing`, below.
    //
    // Not a deletion — same shapes, same table, one more axis. It asserted
    // against `ProjectRequestIdentity::from_filters`, which this tranche made
    // private to the registry, so the assertion had to move to the operations.
    // Moving it was an improvement rather than a concession: a constructor that
    // refuses is only as good as the routes that go through it, and the new
    // name says what is now being claimed.

    // ── Preflight decisions ──────────────────────────────────────────────────
    //
    // Every refusal these operations can make, proved exactly, against
    // descriptions of states no fixture can honestly reach. `plan::Decision`
    // carries no effect: `Refuse` *is* "returned this outcome and did nothing",
    // and production's only use of a refusal is to return it immediately, so
    // these assertions are assertions about zero bytes, zero allocation, no
    // registration, no retirement and no recorded intent.
    //
    // Nothing here takes or returns a record, a counter, a generation, a
    // registration or a socket. A test can say "the space is spent" and cannot
    // make an owner think so.

    /// A violation string standing for "the whole-record walk refused".
    fn refused_record() -> Result<(), String> {
        Err("durable intent holds two watched generations".to_string())
    }

    /// **A spent watched allocator refuses the replacement, exactly.**
    ///
    /// The outcome is the whole assertion. Reporting exhaustion as
    /// `InvalidFilters` — the reviewer's mutant, which the suite could not see
    /// — says the caller's filters were unusable when the truth is that this
    /// process can never name another generation, and the difference is
    /// whether anyone retries.
    #[test]
    fn a_spent_watched_allocator_refuses_and_does_nothing() {
        assert_eq!(
            plan::watched_replacement(true, &Ok(Some(3)), false, AllocatorState::Spent),
            plan::Decision::Refuse(ReplaceOutcome::WatchedGenerationExhausted),
            "a spent generation space is not an invalid filter, a no-op or a conflict"
        );
        // The predecessor is untouched because there is no effect to touch it
        // with: a refusal is the whole of what happens.
        assert_eq!(
            plan::watched_replacement(true, &Ok(Some(3)), false, AllocatorState::Available),
            plan::Decision::Proceed,
            "and with a value left, the same inputs proceed"
        );
    }

    /// **A spent incarnation space refuses the write, exactly.**
    ///
    /// This is the arm the reviewer mutated to `InvalidFilters` without a
    /// single test failing.
    #[test]
    fn a_spent_incarnation_space_refuses_the_replacement_write() {
        assert_eq!(
            plan::replacement_write(false, AllocatorState::Spent),
            plan::Decision::Refuse(ReplaceOutcome::RequestIncarnationExhausted),
        );
        assert_eq!(
            plan::replacement_write(false, AllocatorState::Available),
            plan::Decision::Proceed,
        );
        // A no-op decides before the allocator is consulted at all, so an
        // identical successor over a spent space is still `Unchanged` — the
        // request that is already answering keeps answering.
        assert_eq!(
            plan::replacement_write(true, AllocatorState::Spent),
            plan::Decision::Refuse(ReplaceOutcome::Unchanged),
        );
    }

    /// **A spent incarnation space refuses an open, and does not mask a
    /// conflict.**
    ///
    /// Order is part of the decision: an occupied id is a disagreement about
    /// this request and could still be resolved, while exhaustion is terminal
    /// and process-wide. Reporting the terminal one first would hide a fixable
    /// fault behind an unfixable one.
    #[test]
    fn a_spent_incarnation_space_refuses_the_open() {
        assert_eq!(
            plan::open(Ok(()), plan::Held::Nothing, AllocatorState::Spent),
            plan::Decision::Refuse(OpenOutcome::Exhausted),
        );
        assert_eq!(
            plan::open(Ok(()), plan::Held::Nothing, AllocatorState::Available),
            plan::Decision::Proceed,
        );
        assert_eq!(
            plan::open(Ok(()), plan::Held::SameLive, AllocatorState::Spent),
            plan::Decision::Refuse(OpenOutcome::AlreadyLive),
            "an exact request already live is not affected by a spent space"
        );
    }

    /// **Exhaustion refuses a new request without disturbing a live one.**
    ///
    /// A spent space is a refusal to mint *further* authority, never a
    /// withdrawal of authority already minted. The id that is already
    /// answering reports `AlreadyLive` over a spent allocator exactly as it
    /// does over a fresh one, and a `Refuse` carries no effect that could reach
    /// the registration anyway.
    #[test]
    fn exhaustion_does_not_disturb_a_request_that_is_already_live() {
        for allocator in [AllocatorState::Available, AllocatorState::Spent] {
            assert_eq!(
                plan::open(Ok(()), plan::Held::SameLive, allocator),
                plan::Decision::Refuse(OpenOutcome::AlreadyLive),
                "{allocator:?}: a live request is unaffected by the allocator"
            );
        }
        assert_eq!(
            plan::replacement_write(true, AllocatorState::Spent),
            plan::Decision::Refuse(ReplaceOutcome::Unchanged),
            "and an identical successor is a no-op rather than an exhaustion report"
        );
    }

    /// **A page over a spent space is refused, and so is one whose own filter
    /// asks for everything.**
    ///
    /// A page burns before it writes, because its wire id carries the
    /// incarnation — so both are preflight questions here rather than
    /// discoveries made afterwards.
    #[test]
    fn a_page_refuses_before_it_burns_anything() {
        // `matches!` rather than equality: a page's success variant carries a
        // registration authority, and a type that can be compared for equality
        // is a type whose authority can be compared to another's.
        assert!(matches!(
            plan::history_page(Ok(()), true, true, AllocatorState::Spent),
            plan::Decision::Refuse(PageOpen::Exhausted)
        ));
        assert!(matches!(
            plan::history_page(Ok(()), false, true, AllocatorState::Available),
            plan::Decision::Refuse(PageOpen::NotPristine)
        ));
        assert!(matches!(
            plan::history_page(Ok(()), true, false, AllocatorState::Available),
            plan::Decision::Refuse(PageOpen::UnboundedFilter)
        ));
        assert!(matches!(
            plan::history_page(Ok(()), true, true, AllocatorState::Available),
            plan::Decision::Proceed
        ));
    }

    /// **A record that does not resolve refuses every operation class, and
    /// refuses it first.**
    ///
    /// Every class, because durable intent is one record: an enrolment
    /// replacement over a corrupt *watched* entry is a registry writing into a
    /// record it has just found inconsistent. And first, because a refusal
    /// decided after an allocator check is a refusal that has already spent
    /// something.
    #[test]
    fn a_record_that_does_not_resolve_refuses_every_operation() {
        let violation = match refused_record() {
            Err(v) => v,
            Ok(()) => unreachable!(),
        };

        assert_eq!(
            plan::open(
                refused_record(),
                plan::Held::Nothing,
                AllocatorState::Available
            ),
            plan::Decision::Refuse(OpenOutcome::InvariantViolation(violation.clone())),
        );
        assert_eq!(
            plan::watched_replacement(
                true,
                &Err(violation.clone()),
                false,
                AllocatorState::Available
            ),
            plan::Decision::Refuse(ReplaceOutcome::InvariantViolation(violation.clone())),
        );
        assert_eq!(
            plan::enrolment_replacement(true, &Err(violation.clone())),
            plan::Decision::Refuse(ReplaceOutcome::InvariantViolation(violation.clone())),
        );
        assert!(
            matches!(
                plan::history_page(refused_record(), true, true, AllocatorState::Available),
                plan::Decision::Refuse(PageOpen::InvariantViolation(ref v)) if *v == violation
            ),
            "a page over a record that does not resolve is refused with the violation"
        );
        assert_eq!(
            plan::replay(refused_record()),
            plan::Decision::Refuse(violation.clone()),
            "replay is where durable intent becomes bytes, so it refuses too"
        );

        // Even with a spent allocator beside it, the record is what refuses:
        // the violation is the thing a reader has to fix.
        assert_eq!(
            plan::open(refused_record(), plan::Held::Nothing, AllocatorState::Spent),
            plan::Decision::Refuse(OpenOutcome::InvariantViolation(violation)),
        );
    }

    /// **A replayable record replays; a refused one replays nothing.**
    #[test]
    fn replay_proceeds_only_over_a_record_that_resolves() {
        assert_eq!(plan::replay(Ok(())), plan::Decision::Proceed);
        assert!(matches!(
            plan::replay(refused_record()),
            plan::Decision::Refuse(_)
        ));
    }

    // ── The durable document ─────────────────────────────────────────────────
    //
    // A persisted record is the one thing about this subsystem that does not
    // come from the owner. These proofs give the rule documents the owner would
    // never have written and assert that it refuses them — against
    // `validate_persisted_document`, which decodes bytes, keeps every member in
    // the order it was given, applies the owner's own walk and hands back a
    // description. There is no `DurableRecord` on either side of it, so nothing
    // proved here can be installed, replayed, registered or written.

    /// A bounded filter, so a member's refusal is never about its filters.
    fn document_filter() -> Value {
        json!({ "#e": [ROOT] })
    }

    /// One member of a persisted document.
    fn member(sub_id: &str, class: Value) -> Value {
        json!({ "sub_id": sub_id, "class": class, "filters": [document_filter()] })
    }

    /// A document holding `members`, with the watched allocator at `next`.
    fn document(members: Vec<Value>, next_watched_generation: u64) -> String {
        json!({
            "intent": members,
            "next_watched_generation": next_watched_generation,
        })
        .to_string()
    }

    /// **Discovery intent lives under the discovery id, and nothing else
    /// does.**
    ///
    /// The class the rule used to let fall through. Discovery has no
    /// generation, so it read as "not the disagreement this walk exists to
    /// catch" — but the disagreement is the same one: the key is what goes on
    /// the wire, and a discovery class under a key no replacement names is an
    /// entry nothing will ever retire and every reconnect would re-ask.
    #[test]
    fn a_discovery_class_under_a_foreign_id_is_refused() {
        for id in [
            "proj-discovery-elsewhere".to_string(),
            watched_sub_id(0),
            PROJECT_ENROL_SUB_ID.to_string(),
        ] {
            let violation =
                validate_persisted_document(&document(vec![member(&id, json!("Discovery"))], 1))
                    .expect_err("a discovery class under a foreign id must not validate");
            assert!(
                violation.contains(&id) && violation.contains(&discovery_sub_id()),
                "the report must name the id it found and the id the class implies: {violation}"
            );
        }

        // The positive control: under its own id it resolves.
        assert_eq!(
            validate_persisted_document(&document(
                vec![member(&discovery_sub_id(), json!("Discovery"))],
                0
            ))
            .expect("the canonical id resolves"),
            CurrentIntent {
                watched: None,
                enrolment: false,
            }
        );
    }

    /// **Enrolment intent lives under the enrolment id, and nothing else
    /// does.**
    ///
    /// Both directions are refused. An enrolment class under a foreign id would
    /// never be retired, because the enrolment replacement only ever names its
    /// own fixed id; a foreign class under the enrolment id would be retired by
    /// an enrolment replacement that never installed it.
    #[test]
    fn the_enrolment_id_and_the_enrolment_class_imply_each_other() {
        for (id, class) in [
            ("proj-enrol-elsewhere".to_string(), json!("Enrolment")),
            (PROJECT_ENROL_SUB_ID.to_string(), json!("Discovery")),
        ] {
            assert!(
                validate_persisted_document(&document(vec![member(&id, class.clone())], 1))
                    .is_err(),
                "{id}/{class}: the pair must not validate"
            );
        }

        assert_eq!(
            validate_persisted_document(&document(
                vec![member(PROJECT_ENROL_SUB_ID, json!("Enrolment"))],
                0
            ))
            .expect("the canonical pair resolves"),
            CurrentIntent {
                watched: None,
                enrolment: true,
            }
        );
    }

    /// **A watched id and the generation its identity carries must agree.**
    ///
    /// The key is what goes on the wire; the class is what admits inbound
    /// frames. A pair that disagrees asks the relay one question and admits the
    /// answers to another — and it would resolve as a predecessor under the
    /// wrong id, so the `CLOSE` would retire a subscription the relay never
    /// opened while the real one stayed live.
    #[test]
    fn a_watched_id_that_disagrees_with_its_generation_is_refused() {
        let violation = validate_persisted_document(&document(
            vec![member(
                &watched_sub_id(3),
                json!({ "Watched": { "generation": 7 } }),
            )],
            8,
        ))
        .expect_err("a disagreeing id and generation must not validate");
        assert!(
            violation.contains(&watched_sub_id(3)) && violation.contains(&watched_sub_id(7)),
            "the report must name both the id it found and the id the class implies: {violation}"
        );
    }

    /// **A watched generation must be one this allocator issued.**
    ///
    /// A generation at or above the allocator's next value was never handed
    /// out here. Retiring it would `CLOSE` an id the relay never opened under
    /// this process; treating it as current would let an outside writer choose
    /// the predecessor.
    #[test]
    fn a_watched_generation_the_allocator_never_issued_is_refused() {
        let unissued = document(
            vec![member(
                &watched_sub_id(9),
                json!({ "Watched": { "generation": 9 } }),
            )],
            5,
        );
        let violation = validate_persisted_document(&unissued)
            .expect_err("an unissued generation must not validate");
        assert!(
            violation.contains(&watched_sub_id(9)) && violation.contains('5'),
            "the report must name the generation and the allocator's position: {violation}"
        );

        // The same member under an allocator that did issue it resolves — so
        // the refusal is about provenance and not about the member's shape.
        assert_eq!(
            validate_persisted_document(&document(
                vec![member(
                    &watched_sub_id(9),
                    json!({ "Watched": { "generation": 9 } })
                )],
                10,
            ))
            .expect("an issued generation resolves")
            .watched,
            Some(9)
        );
    }

    /// **At most one watched generation may be current.**
    ///
    /// More than one has no single predecessor, and choosing between them
    /// retires one and leaves the other durable beside the successor — which is
    /// the defect the whole design removes, arrived at from the other
    /// direction. Both generations here were issued by the allocator, so this
    /// is the ambiguity refusal and not the provenance one.
    #[test]
    fn two_watched_generations_have_no_predecessor_and_are_refused() {
        let violation = validate_persisted_document(&document(
            vec![
                member(
                    &watched_sub_id(0),
                    json!({ "Watched": { "generation": 0 } }),
                ),
                member(
                    &watched_sub_id(99),
                    json!({ "Watched": { "generation": 99 } }),
                ),
            ],
            100,
        ))
        .expect_err("two watched generations must not validate");
        assert!(
            violation.contains(&watched_sub_id(0)) && violation.contains(&watched_sub_id(99)),
            "the report must name both: {violation}"
        );
    }

    /// **A root catch-up is never durable, under any id.**
    ///
    /// Its filter carries the page bound its cursor is currently at, and the
    /// cursor walks that bound backwards; its wire id names one transport
    /// attempt that ended with the connection. There is no id under which a
    /// durable catch-up is correct, so the class is refused rather than its key
    /// checked.
    #[test]
    fn a_durable_root_catch_up_is_refused_under_every_id() {
        for id in [
            format!("proj-catchup-c-{ROOT}-0"),
            discovery_sub_id(),
            "proj-anything".to_string(),
        ] {
            let violation = validate_persisted_document(&document(
                vec![member(
                    &id,
                    json!({ "RootCatchUp": { "root": ROOT, "stream": "Comments" } }),
                )],
                1,
            ))
            .expect_err("a durable catch-up must not validate");
            assert!(
                violation.contains(&id),
                "{id}: the report must name it: {violation}"
            );
        }
    }

    /// **One id names one member, and a document that says otherwise is
    /// refused whole.**
    ///
    /// The shipped defect in the previous iteration: members were inserted into
    /// a map on the way to the rule, so a document naming an id twice arrived
    /// already shortened — the later member silently overwriting the earlier
    /// one — and whatever the last writer wanted was what got judged. Both
    /// duplicates are covered, because "they happen to agree" is not the
    /// question: a record that says a thing twice was not written by an owner
    /// that holds one value per id.
    #[test]
    fn a_document_naming_one_id_twice_is_refused() {
        let identical = document(
            vec![
                member(&discovery_sub_id(), json!("Discovery")),
                member(&discovery_sub_id(), json!("Discovery")),
            ],
            0,
        );
        let violation = validate_persisted_document(&identical)
            .expect_err("identical duplicates must not validate");
        assert!(
            violation.contains(&discovery_sub_id()),
            "the report must name the id said twice: {violation}"
        );

        // Conflicting: a second member under the same id carrying a different
        // class. Collapsing this one kept whichever class came last.
        let conflicting = json!({
            "intent": [
                { "sub_id": discovery_sub_id(), "class": "Discovery",
                  "filters": [document_filter()] },
                { "sub_id": discovery_sub_id(), "class": "Enrolment",
                  "filters": [document_filter()] },
            ],
        })
        .to_string();
        assert!(
            validate_persisted_document(&conflicting).is_err(),
            "conflicting duplicates must not validate"
        );

        // Two watched members under one id, agreeing on their generation. The
        // watched count alone would see one member and resolve.
        let duplicate_watched = document(
            vec![
                member(
                    &watched_sub_id(0),
                    json!({ "Watched": { "generation": 0 } }),
                ),
                member(
                    &watched_sub_id(0),
                    json!({ "Watched": { "generation": 0 } }),
                ),
            ],
            1,
        );
        assert!(
            validate_persisted_document(&duplicate_watched).is_err(),
            "a repeated watched member must not validate"
        );
    }

    /// **A malformed member is refused wherever it sits in the document.**
    ///
    /// Ordering was the other half of the collapse: a malformed member followed
    /// by a canonical one under the same id was erased by it, and the document
    /// validated as the canonical member alone. Structure is now checked across
    /// every member before any of them is judged, so neither ordering survives.
    #[test]
    fn a_malformed_member_refuses_the_document_in_either_order() {
        let unbounded = json!({
            "sub_id": discovery_sub_id(), "class": "Discovery", "filters": [{}]
        });
        let canonical = member(&discovery_sub_id(), json!("Discovery"));

        for (why, members) in [
            (
                "malformed first",
                vec![unbounded.clone(), canonical.clone()],
            ),
            ("malformed second", vec![canonical, unbounded]),
        ] {
            let violation = validate_persisted_document(&document(members, 0))
                .expect_err("a document holding a malformed member must not validate");
            assert!(
                violation.contains("constrain nothing"),
                "{why}: the malformed member must be what refuses it, not the \
                 member that happened to survive a collapse: {violation}"
            );
        }
    }

    /// **A persisted allocator must be in a state a counter could reach.**
    ///
    /// `spent` is a fact about arithmetic, not a flag a writer may set:
    /// `burn` sets it in exactly one circumstance, when `next` was `u64::MAX`
    /// and `checked_add` refused. A document claiming `spent` at any other
    /// position describes a counter that cannot exist — and it is not a
    /// harmless inconsistency, because `spent` is what tells the provenance
    /// rule that `next` has stopped advancing. `{ spent: true, next: 0 }`
    /// switched that rule off from inside the document, and generation 99 then
    /// resolved as current against an allocator that had issued nothing.
    #[test]
    fn a_persisted_allocator_in_an_impossible_state_is_refused() {
        let watched_99 = || {
            member(
                &watched_sub_id(99),
                json!({ "Watched": { "generation": 99 } }),
            )
        };

        // The reported defect, exactly.
        let violation = validate_persisted_document(
            &json!({
                "intent": [watched_99()],
                "next_watched_generation": 0,
                "watched_generations_spent": true,
            })
            .to_string(),
        )
        .expect_err("a spent allocator at 0 is a state no counter reaches");
        assert!(
            violation.contains("spent"),
            "the refusal must be about the allocator, not the member: {violation}"
        );

        // Any non-terminal position, with or without a member to protect.
        for next in [0u64, 1, 100, u64::MAX - 1] {
            assert!(
                validate_persisted_document(
                    &json!({
                        "intent": [],
                        "next_watched_generation": next,
                        "watched_generations_spent": true,
                    })
                    .to_string(),
                )
                .is_err(),
                "spent at {next} must be refused"
            );
        }

        // The incarnation allocator is described too, and judged by the same
        // rule — a document that misdescribes one allocator is not a document
        // to trust about the other.
        assert!(
            validate_persisted_document(
                &json!({
                    "intent": [],
                    "next_incarnation": 7,
                    "incarnations_spent": true,
                })
                .to_string(),
            )
            .is_err(),
            "a spent incarnation space at 7 must be refused"
        );

        // The genuinely saturated positive control: every generation has been
        // issued, including `u64::MAX`, so the last one it issued is current.
        assert_eq!(
            validate_persisted_document(
                &json!({
                    "intent": [member(
                        &watched_sub_id(u64::MAX),
                        json!({ "Watched": { "generation": u64::MAX } })
                    )],
                    "next_watched_generation": u64::MAX,
                    "watched_generations_spent": true,
                    "next_incarnation": u64::MAX,
                    "incarnations_spent": true,
                })
                .to_string(),
            )
            .expect("a truly saturated allocator resolves")
            .watched,
            Some(u64::MAX),
            "the last generation a spent allocator issued is still current"
        );

        // And the same member without the spent flag is refused, because at
        // `next == u64::MAX` the generation `u64::MAX` has not been issued yet.
        assert!(
            validate_persisted_document(
                &json!({
                    "intent": [member(
                        &watched_sub_id(u64::MAX),
                        json!({ "Watched": { "generation": u64::MAX } })
                    )],
                    "next_watched_generation": u64::MAX,
                    "watched_generations_spent": false,
                })
                .to_string(),
            )
            .is_err(),
            "the flag is what distinguishes 'reached and saturated' from 'never reached'"
        );
    }

    /// A request that constrains nothing cannot be built — by any route.
    ///
    /// Every shape here asks the relay for its whole store, and each is a
    /// different way of arriving there. The empty *vector* was refused before;
    /// the rest were not, and "the collection is non-empty" was being called a
    /// structural invariant while `["REQ", id, {}]` sat one `json!` away.
    ///
    /// `limit` is in the list because it is the same failure wearing a key: a
    /// limit says how many rows the relay may return, not which events qualify,
    /// so `{"limit": 500}` is a request for the five hundred most recent events
    /// on the relay. `constraint_admits` already accepts it without looking at
    /// the event, which is precisely why it cannot be the only thing in a
    /// filter.
    ///
    /// The mixed case is the one that would have survived a per-filter check
    /// applied lazily: one unbounded branch among narrow ones admits everything
    /// through the OR, so the *whole list* has to be refused, not just that
    /// branch.
    #[tokio::test]
    async fn no_route_installs_a_request_that_constrains_nothing() {
        let narrow = json!({ "kinds": [KIND_TEXT_NOTE], "#e": [ROOT] });
        let unbounded_shapes = [
            Vec::new(),
            vec![json!({})],
            vec![json!({ "limit": 500 })],
            // Not an object at all — including the two-filters-in-one-element
            // mistake, which is now refused rather than merely unmatchable.
            vec![json!([{ "kinds": [KIND_TEXT_NOTE] }])],
            vec![json!("everything")],
            vec![Value::Null],
            // Narrow branch, unbounded branch. The OR makes it unbounded.
            vec![narrow.clone(), json!({})],
            vec![json!({}), narrow.clone()],
        ];

        // Every replacement route, connected and offline. The refusal used to
        // be provable only against the constructor, which is now private —
        // and the constructor was never the thing at risk. What matters is
        // that no *operation* installs one, and that refusing costs nothing:
        // a watched refusal decides before allocation, so the generation the
        // next legitimate replacement takes is still 0.
        for unbounded in &unbounded_shapes {
            let mut requests = ProjectRequests::new();
            let mut socket = test_socket().await;
            assert_eq!(
                requests
                    .replace_watched(&mut socket.client, unbounded.clone())
                    .await,
                ReplaceOutcome::InvalidFilters,
                "connected watched: {unbounded:?}"
            );
            assert_eq!(
                requests.replace_watched_intent(unbounded.clone()),
                ReplaceOutcome::InvalidFilters,
                "offline watched: {unbounded:?}"
            );
            assert_eq!(
                requests
                    .replace_enrolment(&mut socket.client, unbounded.clone())
                    .await,
                ReplaceOutcome::InvalidFilters,
                "connected enrolment: {unbounded:?}"
            );
            assert_eq!(
                requests.replace_enrolment_intent(unbounded.clone()),
                ReplaceOutcome::InvalidFilters,
                "offline enrolment: {unbounded:?}"
            );
            assert_eq!(requests.intent_len(), 0, "nothing recorded: {unbounded:?}");
            assert_eq!(requests.live_len(), 0, "nothing live: {unbounded:?}");

            assert_eq!(
                requests
                    .replace_watched(&mut socket.client, watched_filters_for(&[ROOT]))
                    .await,
                ReplaceOutcome::Replaced { retired: None }
            );
            assert!(
                requests.match_frame(&watched_sub_id(0)).is_some(),
                "four refusals must not have burned a generation: {unbounded:?}"
            );

            // And a persisted document, which is the fifth route in: one
            // holding a member like this is refused entire, so there is no
            // record to be born over and nothing is silently shortened to the
            // members that could be read.
            let document = serde_json::json!({
                "intent": [
                    { "sub_id": discovery_sub_id(), "class": "Discovery",
                      "filters": unbounded },
                    { "sub_id": PROJECT_ENROL_SUB_ID, "class": "Enrolment",
                      "filters": watched_filters_for(&[ROOT]) },
                ],
            })
            .to_string();
            let violation = validate_persisted_document(&document)
                .expect_err("a document holding an unbounded filter must not validate");
            assert!(
                violation.contains(&discovery_sub_id()),
                "the refusal must name the member it refused: {violation}"
            );
        }
    }

    /// One filter rides as the third REQ element, not as an array.
    ///
    /// Asserted off the wire rather than against `req_frame`, because the
    /// claim is about the bytes a relay receives. `["REQ", id, [f]]` is not
    /// the frame NIP-01 describes, and it would make the request unmatchable
    /// as well as unparseable.
    #[tokio::test]
    async fn a_single_filter_rides_as_the_third_req_element() {
        let mut requests = ProjectRequests::new();
        let mut socket = wire_socket().await;
        assert_eq!(
            requests
                .open_discovery(&mut socket.client, discovery_filters())
                .await,
            OpenOutcome::Sent
        );
        assert_eq!(
            socket.next_frame().await,
            json!(["REQ", discovery_sub_id(), { "kinds": [30617] }])
        );

        // A limit is fine *alongside* something selective — which is what every
        // catch-up page carries — and it reaches the wire whole.
        let mut requests = ProjectRequests::new();
        let mut socket = wire_socket().await;
        let with_limit = json!({ "kinds": [KIND_TEXT_NOTE], "#e": [ROOT], "limit": 500 });
        assert_eq!(
            requests
                .replace_watched(&mut socket.client, vec![with_limit.clone()])
                .await,
            ReplaceOutcome::Replaced { retired: None }
        );
        assert_eq!(
            socket.next_frame().await,
            json!(["REQ", watched_sub_id(0), with_limit])
        );
    }

    fn tag(parts: &[&str]) -> Vec<String> {
        parts.iter().map(|p| p.to_string()).collect()
    }

    fn signed(keys: &Keys, kind: u32, tags: Vec<Vec<String>>) -> nostr::Event {
        let tags: Vec<nostr::Tag> = tags
            .into_iter()
            .map(|t| nostr::Tag::parse(t).expect("tag"))
            .collect();
        EventBuilder::new(Kind::Custom(kind as u16), "")
            .tags(tags)
            .sign_with_keys(keys)
            .expect("sign")
    }

    /// Re-serialise a signed event with mutated tags: id and signature no
    /// longer match the contents. What a malicious relay would send.
    fn tampered(event: &nostr::Event, tags: serde_json::Value) -> nostr::Event {
        let mut json: serde_json::Value = serde_json::from_str(&event.as_json()).expect("parse");
        json["tags"] = tags;
        nostr::Event::from_json(json.to_string()).expect("parse")
    }

    /// Swap the author pubkey while keeping the original signature — the
    /// forged-identity attack the witness exists to stop.
    fn forged_author(event: &nostr::Event, pubkey_hex: &str) -> nostr::Event {
        let mut json: serde_json::Value = serde_json::from_str(&event.as_json()).expect("parse");
        json["pubkey"] = serde_json::Value::String(pubkey_hex.to_string());
        nostr::Event::from_json(json.to_string()).expect("parse")
    }

    fn known(coords: &[&str]) -> DiscoveredRepositories {
        DiscoveredRepositories::for_test(coords.iter().map(|c| c.to_string()))
    }

    fn coord() -> String {
        format!("30617:{OWNER}:repo")
    }

    fn tags(raw: &[&[&str]]) -> Vec<Vec<String>> {
        raw.iter()
            .map(|t| t.iter().map(|s| s.to_string()).collect())
            .collect()
    }

    #[test]
    fn issue_and_pr_roots_are_their_own_root() {
        assert_eq!(
            root_event_id(KIND_GIT_ISSUE, ROOT, &tags(&[&["a", "30617:x:y"]])),
            Some(ROOT.to_string())
        );
        assert_eq!(
            root_event_id(KIND_GIT_PULL_REQUEST, ROOT, &tags(&[])),
            Some(ROOT.to_string())
        );
    }

    #[test]
    fn comment_uses_lowercase_e() {
        assert_eq!(
            root_event_id(
                KIND_TEXT_NOTE,
                OTHER_ROOT,
                &tags(&[&["e", ROOT, "", "root"], &["a", "30617:x:y"]])
            ),
            Some(ROOT.to_string())
        );
    }

    #[test]
    fn pr_update_uses_uppercase_e() {
        // The bug this function exists to prevent: a lowercase-only filter
        // silently misses every PR revision.
        assert_eq!(
            root_event_id(KIND_GIT_PR_UPDATE, OTHER_ROOT, &tags(&[&["E", ROOT]])),
            Some(ROOT.to_string())
        );
        assert_eq!(
            root_event_id(KIND_GIT_PR_UPDATE, OTHER_ROOT, &tags(&[&["e", ROOT]])),
            None
        );
    }

    #[test]
    fn root_marker_wins_over_tag_order() {
        // A status event carries the root plus an accepted-revision `reply`
        // (builders.rs:1230-1234). Order must not decide which one we key on.
        assert_eq!(
            root_event_id(
                KIND_GIT_STATUS_CLOSED,
                STRANGER,
                &tags(&[&["e", OTHER_ROOT, "", "reply"], &["e", ROOT, "", "root"]])
            ),
            Some(ROOT.to_string())
        );
    }

    #[test]
    fn unmarked_e_tag_falls_back_to_first() {
        assert_eq!(
            root_event_id(KIND_GIT_STATUS_OPEN, STRANGER, &tags(&[&["e", ROOT]])),
            Some(ROOT.to_string())
        );
    }

    #[test]
    fn missing_or_malformed_root_reference_is_none() {
        assert_eq!(root_event_id(KIND_TEXT_NOTE, ROOT, &tags(&[])), None);
        assert_eq!(
            root_event_id(KIND_TEXT_NOTE, ROOT, &tags(&[&["e", "nope"]])),
            None
        );
        // Unrelated kinds never resolve a project root.
        assert_eq!(root_event_id(9, ROOT, &tags(&[&["e", OTHER_ROOT]])), None);
    }

    #[test]
    fn issue_with_no_p_tag_still_resolves_its_root() {
        // Real case `48be1cc2…`: an issue carrying only `a` and `subject`.
        // Absence of `p` is ordinary, not malformed — it must not error.
        assert_eq!(
            root_event_id(
                KIND_GIT_ISSUE,
                ROOT,
                &tags(&[&["a", "30617:x:y"], &["subject", "hi"]])
            ),
            Some(ROOT.to_string())
        );
    }

    // ── Repo owner ───────────────────────────────────────────────────────────

    #[test]
    fn repo_owner_parses_from_coordinate() {
        assert_eq!(
            repo_owner_from_coordinate(&format!("30617:{OWNER}:my-repo")),
            Some(OWNER.to_string())
        );
    }

    #[test]
    fn repo_owner_rejects_malformed_coordinates() {
        assert_eq!(repo_owner_from_coordinate("30617"), None);
        assert_eq!(repo_owner_from_coordinate("30617:short:my-repo"), None);
        assert_eq!(repo_owner_from_coordinate(""), None);
    }

    #[test]
    fn repo_owner_extraction_is_strict_about_kind_and_identifier() {
        // This feeds lifecycle authority. A coordinate of the wrong kind, or
        // one with no identifier, must not be able to nominate an owner who
        // can then close watches.
        assert_eq!(
            repo_owner_from_coordinate(&format!("30618:{OWNER}:repo")),
            None
        );
        assert_eq!(repo_owner_from_coordinate(&format!("1:{OWNER}:repo")), None);
        assert_eq!(repo_owner_from_coordinate(&format!("30617:{OWNER}")), None);
        assert_eq!(repo_owner_from_coordinate(&format!("30617:{OWNER}:")), None);
        assert_eq!(
            repo_owner_from_coordinate(&format!("30617:{OWNER}:repo")),
            Some(OWNER.to_string())
        );
    }

    #[test]
    fn owner_authority_comes_from_the_binding_not_the_events_coordinate() {
        // Replaces `lifecycle_authority_ignores_a_wrong_kind_coordinate`, whose
        // premise disappeared when the coordinate left this signature. The two
        // questions are now separate: whether the event's own `a` is acceptable
        // is `follow_up_coordinate_allowed`'s job; this asks only who signed.
        //
        // `GitStatusMeta.repo` is optional, so an owner-signed close carrying no
        // `a` at all is well-formed — and the old coordinate-derived version
        // rejected exactly that.
        assert!(
            lifecycle_actor_allowed(OWNER, THIRD_PARTY, OWNER),
            "repository-owner-signed lifecycle with no event `a` is authorised"
        );
        assert!(
            !lifecycle_actor_allowed(STRANGER, THIRD_PARTY, OWNER),
            "an unrelated signer with no event `a` is still ignored"
        );
        // And the coordinate question, asked separately, still admits absence
        // for lifecycle and refuses it for comments.
        assert!(follow_up_coordinate_allowed(
            KIND_GIT_STATUS_CLOSED,
            &CoordinateClaim::Absent,
            &coord()
        ));
        assert!(!follow_up_coordinate_allowed(
            KIND_TEXT_NOTE,
            &CoordinateClaim::Absent,
            &coord()
        ));
    }

    // ── Kind classification ──────────────────────────────────────────────────

    #[test]
    fn comments_are_classified_by_kind_alone() {
        // Root state deliberately does not appear here. Suppressing a dormant
        // comment is an authority decision, and doing it at this layer made an
        // explicit re-tag unable to reactivate a closed root.
        assert_eq!(classify_kind(KIND_TEXT_NOTE), KindEffect::Comment);
    }

    #[test]
    fn roots_are_classified_as_roots() {
        assert_eq!(classify_kind(KIND_GIT_ISSUE), KindEffect::Root);
        assert_eq!(classify_kind(KIND_GIT_PULL_REQUEST), KindEffect::Root);
    }

    #[test]
    fn status_events_are_lifecycle() {
        for kind in [
            KIND_GIT_STATUS_OPEN,
            KIND_GIT_STATUS_MERGED,
            KIND_GIT_STATUS_CLOSED,
            KIND_GIT_STATUS_DRAFT,
        ] {
            assert_eq!(classify_kind(kind), KindEffect::Lifecycle);
        }
    }

    #[test]
    fn pr_update_is_context_not_a_turn() {
        assert_eq!(
            classify_kind(KIND_GIT_PR_UPDATE),
            KindEffect::ContextRefresh
        );
    }

    #[test]
    fn unrelated_kinds_are_ignored() {
        for kind in [9u32, 1, 0, 30617, 1617] {
            if kind == KIND_TEXT_NOTE {
                continue;
            }
            assert_eq!(classify_kind(kind), KindEffect::Ignore, "kind {kind}");
        }
    }

    // ── Lifecycle authority ──────────────────────────────────────────────────

    #[test]
    fn root_author_and_repo_owner_may_change_lifecycle() {
        assert!(lifecycle_actor_allowed(STRANGER, STRANGER, OWNER));
        assert!(lifecycle_actor_allowed(OWNER, STRANGER, OWNER));
    }

    #[test]
    fn a_third_party_may_not_change_lifecycle() {
        assert!(!lifecycle_actor_allowed(THIRD_PARTY, STRANGER, OWNER));
        // Neither root author nor repository owner.
        assert!(!lifecycle_actor_allowed(OWNER, STRANGER, THIRD_PARTY));
    }

    #[test]
    fn lifecycle_authority_is_case_insensitive() {
        // The adapted version briefly compared OWNER against OWNER, which is
        // not a case difference and asserted nothing its name claims.
        let shouty = OWNER.to_ascii_uppercase();
        assert_ne!(shouty, OWNER, "the fixture must actually differ in case");
        assert!(lifecycle_actor_allowed(&shouty, STRANGER, OWNER));
        assert!(lifecycle_actor_allowed(OWNER, &shouty, THIRD_PARTY));
    }

    // ── History pagination ───────────────────────────────────────────────────

    fn cursor(cutoff: u64, limit: usize, relay_max: usize) -> HistoryCursor {
        HistoryCursor::new(
            HistoryScope::Root {
                root: ROOT.to_string(),
                stream: HistoryStream::Comments,
            },
            cutoff,
            limit,
            relay_max,
        )
    }

    fn comment_on(root: &str, ts: u64, marker: &str) -> nostr::Event {
        let keys = Keys::generate();
        EventBuilder::new(Kind::Custom(KIND_TEXT_NOTE as u16), marker)
            .custom_created_at(nostr::Timestamp::from(ts))
            .tags([nostr::Tag::parse(tag(&["e", root, "", "root"])).unwrap()])
            .sign_with_keys(&keys)
            .expect("sign")
    }

    fn pr_update_on(root: &str, ts: u64, marker: &str) -> nostr::Event {
        let keys = Keys::generate();
        EventBuilder::new(Kind::Custom(KIND_GIT_PR_UPDATE as u16), marker)
            .custom_created_at(nostr::Timestamp::from(ts))
            .tags([nostr::Tag::parse(tag(&["E", root])).unwrap()])
            .sign_with_keys(&keys)
            .expect("sign")
    }

    fn comment_at(ts: u64, marker: &str) -> nostr::Event {
        comment_on(ROOT, ts, marker)
    }

    // ---- Piece 1 falsifiers: the witness-authorised page contract ----------
    //
    // Each one names the thing that must NOT be possible. They are grouped so a
    // later reader can tell at a glance which property a failure has broken.

    /// 1. A predecessor's boundary cannot complete the replacement's page.
    #[tokio::test]
    async fn predecessor_witness_cannot_complete_replacement() {
        let mut h = PageHarness::new();
        let mut c = cursor(1_000, 4, 1_000);

        let first = h.open(&mut c).await;
        let stale_witness = h.witness(first.sub_id());
        drop(first); // connection dies; the boundary is still in flight

        let replacement = h.open(&mut c).await;
        assert_ne!(
            stale_witness.incarnation(),
            replacement.incarnation(),
            "reopening must mint a new incarnation, or nothing below is testable"
        );

        match c.complete(&stale_witness, replacement) {
            PageOutcome::Stale { .. } => {}
            other => panic!("predecessor boundary must not complete: {other:?}"),
        }
    }

    /// 2. …and it must not poison the replacement either.
    ///
    /// The reconnect sequence in the adversary model is *ordinary*, so
    /// degrading here would turn every reconnect into a permanently broken
    /// reconstruction — strictness that is really a self-inflicted outage.
    #[tokio::test]
    async fn predecessor_witness_does_not_degrade_replacement() {
        let mut h = PageHarness::new();
        let mut c = cursor(1_000, 4, 1_000);

        let first = h.open(&mut c).await;
        let stale_witness = h.witness(first.sub_id());
        drop(first);

        let replacement = h.open(&mut c).await;
        let returned = match c.complete(&stale_witness, replacement) {
            PageOutcome::Stale { page } => page,
            other => panic!("expected Stale, got {other:?}"),
        };

        assert!(
            c.degraded_reason().is_none(),
            "a late predecessor is expected traffic, not corruption"
        );
        assert_eq!(
            returned.incarnation(),
            h.requests
                .live_incarnation(returned.sub_id())
                .expect("replacement still live"),
            "the page must come back intact and still be the live one"
        );
    }

    /// 3. The replacement's own boundary still completes it afterwards.
    ///
    /// This is the half that makes falsifier 2 meaningful: "not degraded" is
    /// worthless if the page can no longer be finished.
    #[tokio::test]
    async fn replacement_witness_completes_after_stale_attempt() {
        let mut h = PageHarness::new();
        let mut c = cursor(1_000, 4, 1_000);

        let first = h.open(&mut c).await;
        let stale_witness = h.witness(first.sub_id());
        drop(first);

        let mut replacement = h.open(&mut c).await;
        replacement.observe(row(comment_at(900, "a")).await);

        let returned = match c.complete(&stale_witness, replacement) {
            PageOutcome::Stale { page } => page,
            other => panic!("expected Stale, got {other:?}"),
        };

        let good_witness = h.witness(returned.sub_id());
        match c.complete(&good_witness, returned) {
            PageOutcome::Complete(stream) => {
                assert_eq!(stream.len(), 1, "the page's row must survive");
            }
            other => panic!("replacement's own boundary must complete: {other:?}"),
        }
    }

    /// 3b. A *successor* boundary offered to an older page is a contradiction,
    ///     not an ordinary late predecessor.
    ///
    /// The rule is "an earlier instance is stale", and the comparison that
    /// implements it is `other.incarnation < self.incarnation`. A `!=` there
    /// would read the same in the reconnect direction and swallow this one: a
    /// page would be quietly left in flight by a boundary that cannot exist,
    /// instead of the reconstruction being abandoned. Both directions are tested
    /// because only one of them is the ordinary case, and the ordinary case is
    /// the one that keeps passing.
    #[tokio::test]
    async fn a_successor_witness_is_not_treated_as_stale() {
        let mut h = PageHarness::new();
        let mut c = cursor(1_000, 4, 1_000);

        let older = h.open(&mut c).await;
        let newer = h.open(&mut c).await;
        assert!(
            newer.incarnation() > older.incarnation(),
            "the second attempt must be a later instance, or nothing below is \
             testable"
        );
        assert_ne!(
            older.sub_id(),
            newer.sub_id(),
            "and they are two attempts, so they do not share a name"
        );

        let successor_witness = h.witness(newer.sub_id());
        match c.complete(&successor_witness, older) {
            PageOutcome::Degraded { .. } => {}
            other => panic!("a later instance offered to an older page must degrade: {other:?}"),
        }
        assert!(c.degraded_reason().is_some());
    }

    /// 3c. Another registry's boundary cannot complete this page — even when
    ///     every number in it says predecessor.
    ///
    /// Every `ProjectRequests` starts counting incarnations at zero, so
    /// `(sub_id, incarnation)` — two public-looking numbers — collided across
    /// independently constructed registries. Identity is the *allocation*, and
    /// `registry` is the half of it that this test isolates: the foreign
    /// boundary is deliberately given a lower incarnation than the page and the
    /// same root and stream, so every comparison except the epoch says "ordinary
    /// late predecessor". Without the epoch check this returns `Stale` and the
    /// page stays quietly in flight.
    #[tokio::test]
    async fn a_foreign_registrys_witness_cannot_complete_this_page() {
        let mut mine = PageHarness::new();
        let mut theirs = PageHarness::new();
        let mut c = cursor(1_000, 4, 1_000);
        let mut same_question = cursor(1_000, 4, 1_000);

        // Theirs is opened first and only once, so it holds incarnation 0.
        let foreign = theirs.open(&mut same_question).await;
        // Mine burns one and keeps the second, so its incarnation is strictly
        // greater — by number alone, the foreign boundary is an earlier
        // instance of the very same request.
        drop(mine.open(&mut c).await);
        let page = mine.open(&mut c).await;
        assert!(
            foreign.incarnation() < page.incarnation(),
            "the numbers must say predecessor, or the epoch is not what refuses it"
        );

        let foreign_witness = theirs.witness(foreign.sub_id());
        match c.complete(&foreign_witness, page) {
            PageOutcome::Degraded { .. } => {}
            other => panic!("a foreign registry's boundary must not complete: {other:?}"),
        }
        assert!(c.degraded_reason().is_some());
    }

    /// 4. A boundary belonging to another request cannot complete this page.
    ///
    /// Unlike a stale predecessor this is an internal contradiction — the owner
    /// has crossed two pages over — so it degrades permanently.
    #[tokio::test]
    async fn witness_for_another_request_degrades() {
        let mut h = PageHarness::new();
        let mut c = cursor(1_000, 4, 1_000);
        let mut other = HistoryCursor::new(
            HistoryScope::Root {
                root: OTHER_ROOT.to_string(),
                stream: HistoryStream::Comments,
            },
            1_000,
            4,
            1_000,
        );

        let other_page = h.open(&mut other).await;
        let other_witness = h.witness(other_page.sub_id());

        let mine = h.open(&mut c).await;
        assert_ne!(other_witness.sub_id(), mine.sub_id());

        match c.complete(&other_witness, mine) {
            PageOutcome::Degraded { .. } => {}
            other => panic!("a boundary for another request must degrade: {other:?}"),
        }
        assert!(c.degraded_reason().is_some());
    }

    /// 4b. A cross-request boundary cannot resurrect a cursor that already
    ///     finished.
    ///
    /// Found reviewing my own tranche: the contradiction check originally ran
    /// *above* the closed-cursor check, so a straggler could flip a `Finished`
    /// cursor — one whose rows the merge may already hold — to `Degraded`. A
    /// completed stream must not be retroactively marked failed by a later
    /// arrival.
    #[tokio::test]
    async fn cross_request_witness_cannot_unfinish_a_cursor() {
        let mut h = PageHarness::new();
        let mut c = cursor(1_000, 4, 1_000);
        let mut other = HistoryCursor::new(
            HistoryScope::Root {
                root: OTHER_ROOT.to_string(),
                stream: HistoryStream::Comments,
            },
            1_000,
            4,
            1_000,
        );

        // Short page -> the cursor finishes.
        expect_complete(run_page(&mut c, &[(900, "a")]).await);
        assert!(c.degraded_reason().is_none());

        let other_page = h.open(&mut other).await;
        let other_witness = h.witness(other_page.sub_id());
        let mine = h.open(&mut c).await;

        let (reason, rows) = expect_degraded(c.complete(&other_witness, mine));
        assert!(
            reason.contains("already yielded"),
            "a finished cursor reports why it is closed, not the straggler: {reason}"
        );
        assert!(rows.is_empty());
        assert!(
            c.degraded_reason().is_none(),
            "a finished cursor must stay finished"
        );
    }

    /// 4c. …nor overwrite an existing degradation's cause.
    ///
    /// The first recorded cause is the true one; a later contradiction is a
    /// symptom of it. Reporting the symptom instead loses the diagnosis.
    #[tokio::test]
    async fn cross_request_witness_does_not_overwrite_degradation_cause() {
        let mut h = PageHarness::new();
        let mut c = cursor(1_000, 4, 1_000);
        let mut other = HistoryCursor::new(
            HistoryScope::Root {
                root: OTHER_ROOT.to_string(),
                stream: HistoryStream::Comments,
            },
            1_000,
            4,
            1_000,
        );

        let mut poisoned = h.open(&mut c).await;
        poisoned.observe_unusable("frame was not parseable as an event");
        let (first, _) = expect_degraded(complete_opened(&mut c, &mut h, poisoned));
        assert!(first.contains("parseable"), "{first}");

        let other_page = h.open(&mut other).await;
        let other_witness = h.witness(other_page.sub_id());
        let mine = h.open(&mut c).await;
        let (reason, _) = expect_degraded(c.complete(&other_witness, mine));

        assert!(
            reason.contains("parseable"),
            "the original cause must survive, got: {reason}"
        );
        assert!(
            c.degraded_reason().is_some_and(|r| r.contains("parseable")),
            "recorded cause must still be the first one"
        );
    }

    /// A catch-up filter must name its kinds, or the relay refuses it outright.
    ///
    /// A REQ with no `kinds` reads as a wildcard that could match p-gated
    /// kinds, so the relay demands `#p` = self and otherwise answers
    /// `CLOSED restricted: p-gated events require #p matching your pubkey`
    /// (`handlers/req.rs:181`). Not an HTTP 403 — that is the HTTP surface's
    /// framing, and an earlier version of this comment said so wrongly. The
    /// refusal is explicit and carries a reason, so it is diagnosable; what it
    /// is not is a subscription that ever opens.
    #[test]
    fn catch_up_filter_names_its_kinds_and_root_tag() {
        for (stream, tag) in [
            (HistoryStream::Comments, "#e"),
            (HistoryStream::PullRequestUpdates, "#E"),
        ] {
            let filter = catch_up_filter(ROOT, stream, 1_000, 25);
            let kinds = filter["kinds"]
                .as_array()
                .unwrap_or_else(|| panic!("{stream:?} filter must carry kinds"));
            assert!(!kinds.is_empty(), "{stream:?} kinds must not be empty");
            assert_eq!(filter[tag], json!([ROOT]), "{stream:?} root tag");
            assert_eq!(filter["until"], json!(1_000));
            assert_eq!(filter["limit"], json!(25));
        }
    }

    /// Every catch-up filter a page can build is a bounded request.
    ///
    /// This is what makes `PageOpen::UnboundedFilter` unreachable rather than
    /// merely unreached. `open_history_page` builds its identity through the
    /// same fallible constructor as everything else — there is no
    /// "we wrote this filter ourselves" door — so the arm has to exist; what
    /// this pins is the reason it never fires. Bounds and limits are varied
    /// because they are the parts that move as a reconstruction paginates, and
    /// a limit alone is exactly the shape that constrains nothing.
    #[test]
    fn every_catch_up_filter_a_page_can_build_constrains_events() {
        for stream in [HistoryStream::Comments, HistoryStream::PullRequestUpdates] {
            for until in [0, 1, u64::MAX] {
                for limit in [0, 1, 4, usize::MAX] {
                    let filter = catch_up_filter(ROOT, stream, until, limit);
                    assert!(
                        requests::bounded_filters(std::slice::from_ref(&filter)),
                        "{stream:?} until={until} limit={limit}: {filter}"
                    );
                }
            }
        }
    }

    /// The catch-up filter satisfies this relay's REQ gate *predicates*.
    ///
    /// Distinct from the shape assertion above, which passes for *any*
    /// non-empty kind list — including one made entirely of gated kinds that
    /// would be refused on sight. This checks the three conditions
    /// `handlers/req.rs` applies to a global REQ (one with no `#h`), reading
    /// the same constants the relay reads:
    ///
    /// - `p_gated_filters_authorized` (req.rs:1051) — a missing or p-gated
    ///   kind list demands `#p` = self, which catch-up does not carry;
    /// - `engram_filters_authorized` (req.rs:1108) — likewise for engrams;
    /// - `author_only_filters_authorized` (req.rs:1264) — a list consisting
    ///   *only* of author-only kinds demands `authors` = self.
    ///
    /// Note all three use `is_none_or`: an absent `kinds` is a wildcard that
    /// trips the first two. That is why "non-empty" is load-bearing rather
    /// than tidiness.
    ///
    /// **What this does not do:** it re-states the relay's conditions here
    /// rather than calling `p_gated_filters_authorized` and friends, which are
    /// `pub(crate)` to `buzz-relay`. So it catches a bad *kind list* but would
    /// not notice the relay changing its rules. A shared predicate or a
    /// relay-side integration test would be strictly stronger; this is the
    /// version available without widening another crate's API, and the name
    /// says "predicates" rather than "the relay accepts it" for that reason.
    #[test]
    fn catch_up_filter_satisfies_the_req_gate_predicates() {
        use buzz_core::kind::{AUTHOR_ONLY_KINDS, KIND_AGENT_ENGRAM, P_GATED_KINDS};

        for stream in [HistoryStream::Comments, HistoryStream::PullRequestUpdates] {
            let kinds = stream.kinds();
            assert!(
                !kinds.is_empty(),
                "{stream:?}: an absent or empty kind list reads as a wildcard and is refused"
            );
            for kind in kinds {
                assert!(
                    !P_GATED_KINDS.contains(kind),
                    "{stream:?}: kind {kind} is p-gated and would demand #p=self"
                );
                assert_ne!(
                    *kind, KIND_AGENT_ENGRAM,
                    "{stream:?}: engram reads demand authors=[self] or #p=[self]"
                );
            }
            assert!(
                !kinds.iter().all(|k| AUTHOR_ONLY_KINDS.contains(k)),
                "{stream:?}: an all-author-only list would demand authors=self"
            );
        }
    }

    /// Catch-up and the live watched REQ must ask the same question.
    ///
    /// Different time ranges, identical event classes. If these drift, a
    /// reconstruction silently omits a class the live subscription keeps
    /// delivering — the root reads as healthy while missing history that
    /// nothing in the system can point at.
    #[test]
    fn catch_up_kinds_match_the_watched_subscription() {
        let mut enrolments = ProjectEnrolments::new();
        enrolments.enrol(&candidate(ROOT, true)).unwrap();
        let watched = watched_roots_filters(&enrolments, 0);

        for (stream, tag) in [
            (HistoryStream::Comments, "#e"),
            (HistoryStream::PullRequestUpdates, "#E"),
        ] {
            let live = watched
                .iter()
                .find(|f| f.get(tag).is_some())
                .unwrap_or_else(|| panic!("watched REQ must have a {tag} filter"));
            let catch_up = catch_up_filter(ROOT, stream, 1_000, 25);
            assert_eq!(
                catch_up["kinds"], live["kinds"],
                "{stream:?}: catch-up and watched kinds have drifted"
            );
        }
    }

    // ---- Reviewer counterexamples (hermes-gateway, 2026-08-01) -------------
    //
    // Four probes that passed against the previous design. Each names a way the
    // authority claim was convention rather than structure.

    /// R1. A cancelled open strands nothing.
    ///
    /// Replaces "a reserved-but-unsent request cannot bind or witness". That
    /// state is no longer constructible: `open_request` registers nothing until
    /// its write has already returned, so there is no pending entry for a
    /// dropped future to leave behind and no `reserve` to manufacture one with.
    ///
    /// An async operation has three exits — `Ok`, `Err`, and *dropped while
    /// suspended*. The previous shape handled two of them and left the third
    /// holding the subscription id hostage forever.
    #[tokio::test]
    async fn a_cancelled_open_strands_nothing() {
        let mut requests = ProjectRequests::new();
        let mut socket = test_socket().await;
        let sub_id = discovery_sub_id();

        // Build the operation and drop it without driving it to completion.
        drop(requests.open_discovery(&mut socket.client, discovery_filters()));

        assert!(
            requests.match_frame(&sub_id).is_none(),
            "a cancelled open registers nothing answerable"
        );
        assert!(
            requests.witness_end_of_stored_events(&sub_id).is_none(),
            "and nothing that could mint a boundary"
        );
    }

    /// Reconnect restarts the interrupted page at the *same* immutable cutoff,
    /// under a new registration and a new name.
    ///
    /// Three facts that have to hold together, which is why they are one test:
    /// the cutoff must not move (a page reopened against a later snapshot would
    /// leave a hole between the two), the reopened page must not inherit the
    /// dead connection's identity, and it must still be able to complete. A
    /// reconnect is the ordinary case, so a rule that merely refuses everything
    /// after one would pass the first two and break the third.
    #[tokio::test]
    async fn a_reconnect_restarts_the_page_at_the_same_cutoff() {
        let mut h = PageHarness::new();
        let mut c = cursor(1_000, 4, 1_000);

        let interrupted = h.open(&mut c).await;
        let dead_incarnation = interrupted.incarnation();
        let dead_sub_id = interrupted.sub_id().to_string();
        drop(interrupted);
        h.requests.clear_connection();

        let mut reopened = h.open(&mut c).await;
        assert_eq!(
            reopened.until(),
            1_000,
            "the cutoff is immutable across the reconnect"
        );
        assert_ne!(
            reopened.incarnation(),
            dead_incarnation,
            "a reopened page must not inherit the dead connection's identity"
        );
        assert_ne!(
            reopened.sub_id(),
            dead_sub_id,
            "nor its name — a frame still in flight for the dead page must not \
             find this one"
        );

        reopened.observe(row(comment_at(900, "a")).await);
        let witness = h.witness(reopened.sub_id());
        match c.complete(&witness, reopened) {
            PageOutcome::Complete(stream) => assert_eq!(stream.len(), 1),
            other => panic!("the reopened page must still complete: {other:?}"),
        }
    }

    /// Rows cannot predate the registration they are attributed to.
    ///
    /// Observation belongs to the opened page, so the old order — fill a
    /// collector, then register and bind it — is no longer expressible. What
    /// remains reachable is handing the open a collector that has already seen
    /// something, and that is refused before anything is written or burned: a
    /// prepopulated collector laundered into a fresh registration would file
    /// rows that arrived before the request existed into the history that
    /// request claims to have proven.
    #[tokio::test]
    async fn a_prepopulated_collector_cannot_be_laundered_into_a_registration() {
        let mut h = PageHarness::new();
        let mut c = cursor(1_000, 4, 1_000);

        let mut collector = c.begin_request();
        collector.observe(row(comment_at(900, "arrived before any request")).await);

        assert!(
            matches!(h.try_open_with(collector).await, PageOpen::NotPristine),
            "a collector that has already observed cannot open a page"
        );
        assert_eq!(
            h.requests.live_len(),
            0,
            "and the refusal registers nothing"
        );

        // The refusal is upstream of the token, so it costs the next attempt
        // nothing — a check that silently burned an incarnation would be a way
        // to exhaust the space by replaying one bad open. Compared against a
        // registry that never saw the bad open, because a `RequestIncarnation`
        // a test can spell is a `RequestIncarnation` anything can spell.
        let page = h.open(&mut c).await;
        let mut fresh = PageHarness::new();
        let mut fresh_cursor = cursor(1_000, 4, 1_000);
        let untouched = fresh.open(&mut fresh_cursor).await;
        assert_eq!(
            page.incarnation(),
            untouched.incarnation(),
            "the rejected attempt burned no token"
        );
    }

    /// A page open that never lands leaves nothing behind, and never lends its
    /// name to the next attempt.
    ///
    /// A page cannot take its incarnation after the write the way
    /// `open_request` does — the wire id has to carry it — so the token is
    /// burned first. That is only safe if a burned token confers nothing and is
    /// never handed out again. Both halves are here: a future dropped before it
    /// is ever polled does nothing at all, and a write that genuinely fails
    /// leaves no registration while consuming the token it had already spent on
    /// the id it wrote.
    #[tokio::test]
    async fn a_page_open_that_never_lands_leaves_nothing_behind() {
        let mut h = PageHarness::new();
        let mut c = cursor(1_000, 4, 1_000);

        // Dropped before its first poll: nothing ran, so nothing was taken.
        let mut socket = test_socket().await;
        drop(
            h.requests
                .open_history_page(&mut socket.client, c.begin_request()),
        );
        assert_eq!(
            h.requests.live_len(),
            0,
            "a cancelled page open registers nothing"
        );

        let first = h.open(&mut c).await;

        // A write that genuinely fails: still nothing registered, and the token
        // it burned is gone for good.
        let mut dead = test_socket().await;
        dead.client.close(None).await.expect("close");
        assert!(matches!(
            h.requests
                .open_history_page(&mut dead.client, c.begin_request())
                .await,
            PageOpen::WriteFailed(_)
        ));
        assert_eq!(
            h.requests.live_len(),
            1,
            "a failed write installs nothing of its own"
        );

        let second = h.open(&mut c).await;
        assert_ne!(first.sub_id(), second.sub_id());
        assert!(
            second.incarnation() > first.incarnation(),
            "and the failed attempt's token was not handed out again"
        );
    }

    /// R1d. A failed write leaves nothing that could later be promoted.
    ///
    /// The replacement for "receipt A cannot confirm reservation B": there are
    /// no receipts, so the question becomes whether a failed open can leave a
    /// registration behind for a second attempt to inherit. It cannot — the
    /// rollback is inside the same operation as the write.
    #[tokio::test]
    async fn a_failed_write_leaves_no_registration_to_inherit() {
        let mut requests = ProjectRequests::new();
        let sub_id = discovery_sub_id();

        // A genuinely dead socket, not a closure that returns `Err`. The
        // failure has to come from the transport for the same reason success
        // does.
        let mut socket = test_socket().await;
        socket.client.close(None).await.expect("close the socket");

        let outcome = requests
            .open_discovery(&mut socket.client, discovery_filters())
            .await;
        assert!(
            matches!(outcome, OpenOutcome::WriteFailed(_)),
            "a closed socket must fail the write, got {outcome:?}"
        );

        assert!(requests.match_frame(&sub_id).is_none());
        assert!(requests.witness_end_of_stored_events(&sub_id).is_none());
        assert!(
            requests.intent(&sub_id).is_some(),
            "durable intent outlives a failed write — the intent is still what \
             we want; the write is what failed"
        );

        // A later successful open is a *new* registration, not a resurrection.
        assert_eq!(
            open_discovery_on_test_socket(&mut requests, discovery_filters()).await,
            OpenOutcome::Sent
        );
        assert!(requests.match_frame(&sub_id).is_some());
    }

    /// A registry that opens pages the way the driver will have to.
    ///
    /// Deliberately *not* a `bind(sub_id, incarnation)` helper, and not one
    /// that names a subscription id either. A test helper taking the authority
    /// fields would rebuild the exact forgery route this piece removes, behind
    /// a friendlier name; one choosing the id would rebuild the reused-name
    /// defect the same way. Everything here goes through
    /// `ProjectRequests::open_history_page`, against a real socket.
    struct PageHarness {
        requests: ProjectRequests,
    }

    impl PageHarness {
        fn new() -> Self {
            Self {
                requests: ProjectRequests::new(),
            }
        }

        /// Open the catch-up REQ this collector's parameters imply.
        ///
        /// One operation now: the registry mints the wire id, writes, installs
        /// and binds. There is no shortcut available to it — the sink is
        /// sealed, so a harness cannot substitute a writer — and no id for a
        /// caller to choose, which is what stops two successive pages sharing
        /// one name.
        async fn open_with(&mut self, collector: HistoryPageCollector) -> OpenedHistoryPage {
            match self.try_open_with(collector).await {
                PageOpen::Opened(page) => page,
                other => panic!("a freshly written REQ must open its page: {other:?}"),
            }
        }

        async fn try_open_with(&mut self, collector: HistoryPageCollector) -> PageOpen {
            let mut socket = test_socket().await;
            self.requests
                .open_history_page(&mut socket.client, collector)
                .await
        }

        async fn open(&mut self, c: &mut HistoryCursor) -> OpenedHistoryPage {
            let collector = c.begin_request();
            self.open_with(collector).await
        }

        /// Mint the boundary for a live registration.
        ///
        /// `&mut` because minting one is not a read: a catch-up registration is
        /// retired by its own boundary, in production and here. A second call
        /// for the same id therefore panics, which is the point — there is only
        /// ever one end of one request's stored events.
        fn witness(&mut self, sub_id: &str) -> EndOfStoredEvents {
            self.requests
                .witness_end_of_stored_events(sub_id)
                .expect("registration must be live to mint a boundary")
        }
    }

    /// Run one page through a fresh registration.
    ///
    /// The harness is per-page here because these callers test pagination,
    /// retention and merging — not request identity. They still mint authority
    /// through the real path; what they do not exercise is incarnation
    /// continuity across pages, which the falsifiers below do explicitly.
    async fn run_page(c: &mut HistoryCursor, entries: &[(u64, &str)]) -> PageOutcome {
        let mut h = PageHarness::new();
        let mut page = h.open(c).await;
        for (ts, marker) in entries {
            page.observe(row(comment_at(*ts, marker)).await);
        }
        let witness = h.witness(page.sub_id());
        c.complete(&witness, page)
    }

    /// Complete a page whose rows arrived through the page itself.
    ///
    /// There is deliberately no helper that takes a pre-filled collector any
    /// more. Rows can only enter via [`OpenedHistoryPage::observe`], and no
    /// page exists until the REQ has been written and its registration
    /// installed — so the test helpers can no longer express the order that let
    /// rows predate their registration.
    fn complete_opened(
        c: &mut HistoryCursor,
        h: &mut PageHarness,
        page: OpenedHistoryPage,
    ) -> PageOutcome {
        let witness = h.witness(page.sub_id());
        c.complete(&witness, page)
    }

    /// Feed already-built events, so a page can re-deliver the *same* event a
    /// second time. `run_page` mints a fresh key per row and cannot.
    async fn run_page_with(c: &mut HistoryCursor, events: &[nostr::Event]) -> PageOutcome {
        let mut h = PageHarness::new();
        let mut page = h.open(c).await;
        for event in events {
            page.observe(row(event.clone()).await);
        }
        complete_opened(c, &mut h, page)
    }

    // `PageOutcome` is no longer `PartialEq` — two of its variants carry event
    // collections now. These destructure instead, which also keeps each
    // assertion pointed at the one field it means to check.
    fn expect_continue(outcome: PageOutcome) -> (u64, usize) {
        match outcome {
            PageOutcome::Continue { until, limit } => (until, limit),
            other => panic!("expected Continue, got {other:?}"),
        }
    }

    fn expect_complete(outcome: PageOutcome) -> RetainedStream {
        match outcome {
            PageOutcome::Complete(retained) => retained,
            other => panic!("expected Complete, got {other:?}"),
        }
    }

    fn expect_degraded(outcome: PageOutcome) -> (String, DiagnosticRows) {
        match outcome {
            PageOutcome::Degraded { reason, rows } => (reason, rows),
            other => panic!("expected Degraded, got {other:?}"),
        }
    }

    fn stamps_of(events: &[VerifiedProjectEvent]) -> Vec<u64> {
        events
            .iter()
            .map(|e| e.event().created_at.as_secs())
            .collect()
    }

    fn ids_of(events: &[VerifiedProjectEvent]) -> Vec<String> {
        events.iter().map(VerifiedProjectEvent::id).collect()
    }

    #[tokio::test]
    async fn a_short_page_means_this_stream_is_complete() {
        let mut c = cursor(1_000, 4, 1_000);
        let retained = expect_complete(run_page(&mut c, &[(900, "a"), (890, "b")]).await);
        assert_eq!(retained.len(), 2);
        assert_eq!(retained.stream(), HistoryStream::Comments);
        assert_eq!(retained.root(), ROOT);
        // Handed over, not copied.
        assert_eq!(c.retained_count(), 0);
    }

    #[tokio::test]
    async fn completion_is_judged_against_the_relay_applied_limit() {
        // The concrete false-complete: ask for 2,000, the relay clamps to
        // 1,000 (`buzz-db/src/event.rs:25`) and returns exactly that with EOSE.
        // Comparing against what we *asked* for would report Complete while a
        // page may remain.
        let mut c = cursor(1_000, 2_000, 1_000);
        assert_eq!(
            c.limit(),
            1_000,
            "the request is clamped to the relay ceiling"
        );

        let entries: Vec<(u64, String)> = (0..1_000)
            .map(|i| (900 - (i % 7) as u64, format!("e{i}")))
            .collect();
        let borrowed: Vec<(u64, &str)> = entries.iter().map(|(t, m)| (*t, m.as_str())).collect();

        assert!(
            !matches!(run_page(&mut c, &borrowed).await, PageOutcome::Complete(_)),
            "a page saturating the effective limit is not proof of exhaustion"
        );
    }

    #[tokio::test]
    async fn a_page_witness_cannot_be_built_without_a_cursor_request() {
        // Structural, not documentary: `HistoryPageCollector` has no public
        // constructor, and `EoseHistoryPage` has no constructor at all outside
        // the private module. `begin_request` is the only route in, so an empty
        // collector cannot be conjured and asked for a `Complete`.
        let mut c = cursor(1_000, 4, 1_000);
        let mut h = PageHarness::new();
        let page = h.open(&mut c).await;
        // An empty page from a *real* request is still legitimate: zero rows
        // under the limit genuinely means exhausted. It yields an empty
        // collection rather than nothing at all.
        let retained = expect_complete(complete_opened(&mut c, &mut h, page));
        assert!(retained.is_empty());
    }

    #[tokio::test]
    async fn a_superseded_request_cannot_be_absorbed() {
        // Generation binding: a page answering an earlier request must not be
        // applied to the cursor's current one.
        let mut c = cursor(1_000, 4, 1_000);
        let mut h = PageHarness::new();
        let stale = h.open(&mut c).await;
        let _current = c.begin_request();
        let (reason, _rows) = expect_degraded(complete_opened(&mut c, &mut h, stale));
        assert!(reason.contains("outstanding request"), "{reason}");
        // And permanently. `abandon` is the route for a request the driver
        // superseded, so a page reaching absorption is either a driver defect or
        // something the cursor never asked for; neither is recoverable by
        // asking again.
        assert!(c.degraded_reason().is_some());
    }

    #[tokio::test]
    async fn the_collector_poisons_a_page_containing_a_foreign_root() {
        // Integrity is the collector's job, not the caller's. A row for another
        // root occupied a slot under the relay limit, so the short page can no
        // longer distinguish exhaustion from displacement.
        let mut c = cursor(1_000, 4, 1_000);
        let mut h = PageHarness::new();
        let mut page = h.open(&mut c).await;
        page.observe(row(comment_at(900, "mine")).await);

        let keys = Keys::generate();
        let foreign = EventBuilder::new(Kind::Custom(KIND_TEXT_NOTE as u16), "elsewhere")
            .custom_created_at(nostr::Timestamp::from(890))
            .tags([nostr::Tag::parse(tag(&["e", OTHER_ROOT, "", "root"])).unwrap()])
            .sign_with_keys(&keys)
            .expect("sign");
        page.observe(row(foreign).await);

        let (reason, rows) = expect_degraded(complete_opened(&mut c, &mut h, page));
        assert!(reason.contains("root"), "{reason}");
        // The row that passed its own checks is diagnosable. It is not history:
        // it arrives as `DiagnosticRows`, which has no route to a
        // `RetainedStream`.
        assert_eq!(rows.len(), 1);
    }

    #[tokio::test]
    async fn the_collector_poisons_a_page_containing_a_wrong_kind() {
        let mut c = HistoryCursor::new(
            HistoryScope::Root {
                root: ROOT.to_string(),
                stream: HistoryStream::PullRequestUpdates,
            },
            1_000,
            4,
            1_000,
        );
        let mut h = PageHarness::new();
        let mut page = h.open(&mut c).await;
        // A comment on a PR-update stream: wrong filter class.
        page.observe(row(comment_at(900, "a")).await);
        let (reason, _rows) = expect_degraded(complete_opened(&mut c, &mut h, page));
        assert!(reason.contains("does not belong"), "{reason}");
    }

    #[tokio::test]
    async fn the_collector_poisons_a_page_containing_a_too_new_event() {
        let mut c = cursor(1_000, 4, 1_000);
        let mut h = PageHarness::new();
        let mut page = h.open(&mut c).await;
        page.observe(row(comment_at(2_000, "future")).await);
        let (reason, _rows) = expect_degraded(complete_opened(&mut c, &mut h, page));
        assert!(reason.contains("newer than"), "{reason}");
    }

    #[tokio::test]
    async fn a_malformed_frame_poisons_the_page_rather_than_vanishing() {
        let mut c = cursor(1_000, 4, 1_000);
        let mut h = PageHarness::new();
        let mut page = h.open(&mut c).await;
        page.observe_unusable("frame was not parseable as an event");
        let (reason, _rows) = expect_degraded(complete_opened(&mut c, &mut h, page));
        assert!(reason.contains("parseable"), "{reason}");
    }

    #[tokio::test]
    async fn the_cursor_moves_inclusively_not_one_second_earlier() {
        let mut c = cursor(1_000, 3, 1_000);
        assert_eq!(
            expect_continue(run_page(&mut c, &[(900, "a"), (880, "b"), (870, "c")]).await),
            (870, 3)
        );
    }

    #[tokio::test]
    async fn a_saturated_single_timestamp_page_grows_rather_than_skipping() {
        let mut c = cursor(1_000, 2, 1_000);
        assert_eq!(
            expect_continue(run_page(&mut c, &[(900, "a"), (900, "b")]).await),
            (1_000, 8),
            "same `until`, larger page"
        );
    }

    #[tokio::test]
    async fn an_unprovable_cohort_degrades_and_reports_only_what_is_known() {
        let mut c = cursor(1_000, 4, 4);
        let (reason, rows) = expect_degraded(
            run_page(&mut c, &[(900, "a"), (900, "b"), (900, "c"), (900, "d")]).await,
        );
        assert!(reason.contains("900"), "names the timestamp: {reason}");
        assert!(
            reason.contains('4'),
            "names the effective ceiling: {reason}"
        );
        assert!(
            !reason.contains("has at least"),
            "diagnostic must not overstate its evidence: {reason}"
        );
        assert_eq!(rows.len(), 4, "the cohort is diagnosable, not replayable");
    }

    // ── Pagination retains what it accepts ───────────────────────────────────

    #[tokio::test]
    async fn a_completed_stream_yields_the_rows_it_retained_not_a_count() {
        // The defect this closes: pagination proved progress and produced no
        // history. The cursor recorded ids in a `seen` set purely to detect
        // repeats and dropped every event, so a caller that paginated a root to
        // exhaustion finished holding nothing to replay.
        let mut c = cursor(1_000, 2, 1_000);
        assert_eq!(
            expect_continue(run_page(&mut c, &[(900, "a"), (880, "b")]).await),
            (880, 2)
        );
        let retained = expect_complete(run_page(&mut c, &[(870, "c")]).await);

        assert_eq!(retained.len(), 3, "rows from both pages, not just the last");
        assert_eq!(
            stamps_of(retained.events()),
            vec![870, 880, 900],
            "`(created_at, event_id)` order"
        );
    }

    #[tokio::test]
    async fn an_inclusive_boundary_duplicate_folds_exactly_once() {
        // `until` moves to the oldest timestamp inclusively, so the boundary row
        // is delivered a second time by design. It must be retained once.
        let mut c = cursor(1_000, 2, 1_000);
        let newer = comment_at(900, "newer");
        let boundary = comment_at(880, "boundary");
        let older = comment_at(870, "older");

        assert_eq!(
            expect_continue(run_page_with(&mut c, &[newer, boundary.clone()]).await),
            (880, 2)
        );
        assert_eq!(c.retained_count(), 2);

        assert_eq!(
            expect_continue(run_page_with(&mut c, &[boundary, older]).await),
            (870, 2)
        );
        assert_eq!(c.retained_count(), 3, "the repeat added nothing");

        let retained = expect_complete(run_page_with(&mut c, &[]).await);
        assert_eq!(retained.len(), 3);
        let mut unique = ids_of(retained.events());
        unique.sort();
        unique.dedup();
        assert_eq!(unique.len(), 3, "no id appears twice in the retained rows");
    }

    #[tokio::test]
    async fn duplicate_rows_still_count_towards_the_relay_page_limit() {
        // A page the relay filled with rows already held is still a *full* page:
        // it stopped at its limit, so older events may remain behind it.
        // Judging saturation on fresh rows rather than raw rows would read a
        // page of repeats as the end of history.
        let mut c = cursor(1_000, 2, 1_000);
        let newer = comment_at(900, "newer");
        let boundary = comment_at(880, "boundary");
        assert_eq!(
            expect_continue(run_page_with(&mut c, &[newer, boundary.clone()]).await),
            (880, 2)
        );

        // Both slots filled by the boundary row already retained.
        assert_eq!(
            expect_continue(run_page_with(&mut c, &[boundary.clone(), boundary]).await),
            (880, 8),
            "saturated, so the cursor grows its page rather than declaring exhaustion"
        );
        assert_eq!(
            c.retained_count(),
            2,
            "and both repeats folded into nothing"
        );
    }

    #[tokio::test]
    async fn a_degraded_cursor_surrenders_its_rows_as_diagnostics_only() {
        let mut c = cursor(1_000, 2, 1_000);
        assert_eq!(
            expect_continue(run_page(&mut c, &[(900, "a"), (880, "b")]).await),
            (880, 2)
        );
        assert_eq!(c.retained_count(), 2);

        let mut h = PageHarness::new();
        let mut page = h.open(&mut c).await;
        page.observe_unusable("frame was not parseable as an event");
        let (reason, rows) = expect_degraded(complete_opened(&mut c, &mut h, page));
        assert!(reason.contains("parseable"), "{reason}");
        assert_eq!(rows.len(), 2, "what was held is diagnosable");
        assert_eq!(c.retained_count(), 0, "and is gone from the cursor");

        // No later short page rehabilitates it. A short page after integrity was
        // lost looks exactly like exhaustion, which is precisely why it must not
        // be allowed to mean it.
        let (reason, rows) = expect_degraded(run_page(&mut c, &[(870, "c")]).await);
        assert!(reason.contains("permanently degraded"), "{reason}");
        assert!(
            reason.contains("parseable"),
            "the original reason is kept, not replaced: {reason}"
        );
        assert!(
            rows.is_empty(),
            "a dead cursor gains nothing from a new page"
        );
    }

    #[tokio::test]
    async fn a_finished_stream_cannot_yield_its_history_twice() {
        let mut c = cursor(1_000, 4, 1_000);
        assert_eq!(
            expect_complete(run_page(&mut c, &[(900, "a")]).await).len(),
            1
        );
        let (reason, rows) = expect_degraded(run_page(&mut c, &[(890, "b")]).await);
        assert!(reason.contains("already yielded"), "{reason}");
        assert!(rows.is_empty());
    }

    #[tokio::test]
    async fn an_abandoned_request_leaves_the_cursor_open() {
        // The superseded-page rule is permanent, so `abandon` has to be a real
        // escape: a request dropped on CLOSED, timeout or reconnect must not
        // degrade a cursor that never absorbed anything.
        let mut c = cursor(1_000, 4, 1_000);
        c.begin_request().abandon();
        assert!(c.degraded_reason().is_none());
        assert_eq!(
            expect_complete(run_page(&mut c, &[(900, "a")]).await).len(),
            1
        );
    }

    #[tokio::test]
    async fn comments_and_pull_request_updates_retain_independently() {
        // One cursor per exact filter means one accumulator per exact filter.
        // Sharing them would let a complete Comments stream carry rows the PR
        // stream never proved exhausted, and vice versa.
        let mut comments = HistoryCursor::new(
            HistoryScope::Root {
                root: ROOT.to_string(),
                stream: HistoryStream::Comments,
            },
            1_000,
            4,
            1_000,
        );
        let mut updates = HistoryCursor::new(
            HistoryScope::Root {
                root: ROOT.to_string(),
                stream: HistoryStream::PullRequestUpdates,
            },
            1_000,
            4,
            1_000,
        );

        let comment_rows =
            expect_complete(run_page(&mut comments, &[(900, "c1"), (890, "c2")]).await);

        let mut h = PageHarness::new();
        let mut page = h.open(&mut updates).await;
        page.observe(row(pr_update_on(ROOT, 895, "r1")).await);
        let update_rows = expect_complete(complete_opened(&mut updates, &mut h, page));

        assert_eq!(comment_rows.stream(), HistoryStream::Comments);
        assert_eq!(update_rows.stream(), HistoryStream::PullRequestUpdates);
        assert_eq!(comment_rows.len(), 2);
        assert_eq!(update_rows.len(), 1);
        for id in ids_of(update_rows.events()) {
            assert!(!ids_of(comment_rows.events()).contains(&id));
        }
    }

    // ── Merging completed streams ────────────────────────────────────────────

    /// A proven root plus its id, for the merge tests.
    async fn bound_root(kind: u32) -> (VerifiedBoundRoot, String) {
        let keys = Keys::generate();
        let event = signed(&keys, kind, vec![tag(&["a", &coord()])]);
        let event = VerifiedProjectEvent::verify(event).await.expect("valid");
        let id = event.id();
        let bound = VerifiedBoundRoot::prove(std::slice::from_ref(&event), &known(&[&coord()]))
            .expect("proves");
        (bound, id)
    }

    /// A verified row, produced the way the relay dispatch produces one.
    ///
    /// A page takes a [`VerifiedProjectEvent`] rather than verifying for itself,
    /// so a test that feeds a page has to mint the same proof the dispatch
    /// mints. That is the point of the signature change: one verifier, not a
    /// second one inside the page that has to keep agreeing with the first.
    async fn row(event: nostr::Event) -> VerifiedProjectEvent {
        VerifiedProjectEvent::verify(event)
            .await
            .expect("test rows are signed")
    }

    /// Admit a row the way the relay dispatch admits one: from the live
    /// registration under `sub_id`.
    ///
    /// Panics when nothing is live there, because that is not a weaker version
    /// of the same test — a frame on an id this agent has not asked about never
    /// reaches a reconstruction at all, and a helper that quietly produced one
    /// anyway would be manufacturing the admission this piece is about.
    async fn admitted(h: &PageHarness, sub_id: &str, event: nostr::Event) -> CatchUpFrame {
        h.requests
            .admit_frame(sub_id)
            .expect("nothing is live under that id, so no frame could be admitted")
            .catch_up(CatchUpOutcome::Row(Box::new(row(event).await)))
    }

    // ---- Piece 2: the RootReconstruction owner -----------------------------

    /// Open a page **through the reconstruction's own cursor**.
    ///
    /// The collector comes from `begin_page`, and the registry does the rest in
    /// one operation — mint the id, write, install, bind. Nothing here names a
    /// subscription id: there is no id until the registry has minted one.
    async fn open_for(
        h: &mut PageHarness,
        recon: &mut RootReconstruction,
        stream: HistoryStream,
    ) -> OpenedHistoryPage {
        let collector = recon
            .begin_page(stream)
            .unwrap_or_else(|| panic!("the owner must want a page for {stream:?}"));
        h.open_with(collector).await
    }

    /// A page for a root/stream this reconstruction would never issue.
    ///
    /// **Only valid for rejection tests.** Its collector comes from a
    /// standalone cursor, so its generation matches no owner's — which is fine
    /// precisely because `attach` must refuse it before generation could
    /// matter. Anything testing a *successful* path must use [`open_for`].
    async fn unissuable_page(
        h: &mut PageHarness,
        root: &str,
        stream: HistoryStream,
    ) -> OpenedHistoryPage {
        let mut cursor = HistoryCursor::new(
            HistoryScope::Root {
                root: root.to_string(),
                stream,
            },
            1_000,
            4,
            1_000,
        );
        h.open_with(cursor.begin_request()).await
    }

    /// The composed lifecycle: issue → open → attach → genuine EOSE → finished.
    ///
    /// The falsifier the first eight were missing. Each of those checked one
    /// edge of the owner in isolation; none crossed the boundary between the
    /// owner and the Piece 1 registry, so a reconstruction that could never
    /// complete a page passed all eight.
    #[tokio::test]
    async fn an_advertised_page_completes_under_its_own_boundary() {
        let (issue, _) = bound_root(KIND_GIT_ISSUE).await;
        let mut recon = RootReconstruction::begin(&issue, 1_000, 4, 1_000);
        let mut h = PageHarness::new();

        let page = open_for(&mut h, &mut recon, HistoryStream::Comments).await;
        let sub_id = page.sub_id().to_string();
        recon.attach(page).map_err(|r| r.error).expect("attaches");

        let witness = h.witness(&sub_id);
        match recon.complete(&witness) {
            Some(StreamAdvance::Finished { stream }) => {
                assert_eq!(stream, HistoryStream::Comments);
            }
            other => panic!("an empty page under the limit is exhausted: {other:?}"),
        }
        assert_eq!(recon.finished_streams().len(), 1);
    }

    /// A saturated page yields `Continue`, and the next page opens from the
    /// **advanced** position.
    ///
    /// The pagination loop end to end. Nothing previously called `complete()`
    /// at all, so `Continue` — the outcome every page but the last produces —
    /// had never once been reached through the owner.
    #[tokio::test]
    async fn a_saturated_page_continues_from_the_advanced_position() {
        let (issue, _) = bound_root(KIND_GIT_ISSUE).await;
        let mut recon = RootReconstruction::begin(&issue, 1_000, 2, 1_000);
        let mut h = PageHarness::new();
        let root = recon.root().to_string();

        let mut page = open_for(&mut h, &mut recon, HistoryStream::Comments).await;
        let sub_id = page.sub_id().to_string();
        page.observe(row(comment_on(&root, 900, "a")).await);
        page.observe(row(comment_on(&root, 880, "b")).await);
        recon.attach(page).map_err(|r| r.error).expect("attaches");

        let witness = h.witness(&sub_id);
        let (until, limit) = match recon.complete(&witness) {
            Some(StreamAdvance::Continue { until, limit, .. }) => (until, limit),
            other => panic!("a page filling the limit continues: {other:?}"),
        };
        assert_eq!(until, 880, "the next page starts at the oldest row seen");

        assert_eq!(
            recon.pages_wanted(),
            vec![(HistoryStream::Comments, until, limit)],
            "the owner advertises the advanced position, not the cutoff"
        );

        // And the next page really opens there.
        let next = open_for(&mut h, &mut recon, HistoryStream::Comments).await;
        assert_eq!(next.until(), until);
        recon.attach(next).map_err(|r| r.error).expect("attaches");
    }

    /// A disconnect after a `Continue` keeps the advanced cursor, not the
    /// original cutoff.
    #[tokio::test]
    async fn a_disconnect_after_continue_keeps_the_advanced_position() {
        let (issue, _) = bound_root(KIND_GIT_ISSUE).await;
        let mut recon = RootReconstruction::begin(&issue, 1_000, 2, 1_000);
        let mut h = PageHarness::new();
        let root = recon.root().to_string();

        let mut page = open_for(&mut h, &mut recon, HistoryStream::Comments).await;
        let sub_id = page.sub_id().to_string();
        page.observe(row(comment_on(&root, 900, "a")).await);
        page.observe(row(comment_on(&root, 880, "b")).await);
        recon.attach(page).map_err(|r| r.error).expect("attaches");
        let witness = h.witness(&sub_id);
        assert!(matches!(
            recon.complete(&witness),
            Some(StreamAdvance::Continue { .. })
        ));

        recon.disconnected();

        assert_eq!(
            recon.pages_wanted(),
            vec![(HistoryStream::Comments, 880, 2)],
            "a reconnect resumes where pagination got to — resuming at the \
             cutoff would re-walk history already retained"
        );
        assert_eq!(recon.cutoff(), 1_000, "and the cutoff is still the cutoff");
    }

    /// A stale boundary refuses, then the page's own boundary completes it.
    ///
    /// The half that makes "does not degrade" worth anything: a page left in
    /// flight is only useful if it can still be finished.
    #[tokio::test]
    async fn after_a_stale_boundary_the_pages_own_boundary_still_completes() {
        let (issue, _) = bound_root(KIND_GIT_ISSUE).await;
        let mut recon = RootReconstruction::begin(&issue, 1_000, 4, 1_000);
        let mut h = PageHarness::new();

        // A first page, and its boundary, held back while the page itself is
        // dropped — the predecessor whose EOSE arrives late.
        let first = open_for(&mut h, &mut recon, HistoryStream::Comments).await;
        let stale = h.witness(first.sub_id());
        drop(first);

        let page = open_for(&mut h, &mut recon, HistoryStream::Comments).await;
        let sub_id = page.sub_id().to_string();
        recon.attach(page).map_err(|r| r.error).expect("attaches");

        assert!(
            recon.complete(&stale).is_none(),
            "a predecessor's boundary names a registration that holds no page"
        );
        assert!(recon.in_flight(HistoryStream::Comments));

        let current = h.witness(&sub_id);
        match recon.complete(&current) {
            Some(StreamAdvance::Finished { .. }) => {}
            other => panic!("the replacement's own boundary must finish it: {other:?}"),
        }
    }

    /// A page opened with the wrong limit is refused at attachment, and comes
    /// back.
    #[tokio::test]
    async fn a_wrong_limit_page_is_refused_and_returned() {
        let (issue, _) = bound_root(KIND_GIT_ISSUE).await;
        let mut recon = RootReconstruction::begin(&issue, 1_000, 4, 1_000);
        let mut h = PageHarness::new();

        // Genuine root, stream and `until`; a different page limit.
        let mut odd = HistoryCursor::new(
            HistoryScope::Root {
                root: recon.root().to_string(),
                stream: HistoryStream::Comments,
            },
            1_000,
            9,
            1_000,
        );
        let page = h.open_with(odd.begin_request()).await;

        let rejected = recon.attach(page).unwrap_err();
        assert_eq!(
            rejected.error,
            AttachError::WrongLimit {
                expected: 4,
                found: 9
            },
            "the cursor compares the limit exactly, so attachment must too — \
             otherwise the page is held to EOSE and degrades the whole root"
        );
        assert_eq!(rejected.page.effective_limit(), 9, "and it comes back");
    }

    /// A boundary cannot be delivered to a stream it does not belong to.
    #[tokio::test]
    async fn a_boundary_reaches_only_the_page_opened_under_its_own_id() {
        let (pr, _) = bound_root(KIND_GIT_PULL_REQUEST).await;
        let mut recon = RootReconstruction::begin(&pr, 1_000, 4, 1_000);
        let mut h = PageHarness::new();

        let comments = open_for(&mut h, &mut recon, HistoryStream::Comments).await;
        let comments_sub = comments.sub_id().to_string();
        let updates = open_for(&mut h, &mut recon, HistoryStream::PullRequestUpdates).await;
        let updates_sub = updates.sub_id().to_string();
        recon
            .attach(comments)
            .map_err(|r| r.error)
            .expect("comments attach");
        recon
            .attach(updates)
            .map_err(|r| r.error)
            .expect("updates attach");

        // Admitted while the registration is still live, so the frame below is
        // a genuine one whose page has since gone — not an unadmissible frame
        // failing for a different reason.
        let late = admitted(&h, &comments_sub, comment_at(900, "late")).await;

        // The comments boundary finishes the comments page, and cannot be
        // aimed anywhere else: there is no stream argument to aim it with.
        let witness = h.witness(&comments_sub);
        match recon.complete(&witness) {
            Some(StreamAdvance::Finished { stream }) => {
                assert_eq!(stream, HistoryStream::Comments);
            }
            other => panic!("expected the comments stream to finish: {other:?}"),
        }
        assert!(
            recon.in_flight(HistoryStream::PullRequestUpdates),
            "the sibling page is untouched"
        );
        assert!(
            h.requests.match_frame(&updates_sub).is_some(),
            "and so is the registration that opened it — a boundary retires the \
             request it answers, not every catch-up in the registry"
        );

        // The boundary retired the comments registration along with the page it
        // completed, so nothing can even be admitted under that id any more —
        // which is the stronger half of "reaches nothing".
        assert!(
            h.requests.admit_frame(&comments_sub).is_none(),
            "a completed catch-up stops being answerable"
        );

        // And a frame admitted while it *was* live still reaches nothing, since
        // the reconstruction no longer holds a page under it.
        assert_eq!(recon.observe(late), FrameRouting::NotOurs);
    }

    /// Both required PR streams open under one registry.
    ///
    /// They are different questions, so they need different subscription ids
    /// and different classes. When the id was root-only they collided and the
    /// second stream could not be opened at all — the owner could enumerate two
    /// and the registry could serve one.
    #[tokio::test]
    async fn both_required_pr_streams_open_under_one_registry() {
        let (pr, _) = bound_root(KIND_GIT_PULL_REQUEST).await;
        let mut recon = RootReconstruction::begin(&pr, 1_000, 4, 1_000);
        let mut h = PageHarness::new();

        let comments = open_for(&mut h, &mut recon, HistoryStream::Comments).await;
        let updates = open_for(&mut h, &mut recon, HistoryStream::PullRequestUpdates).await;
        assert_ne!(
            comments.sub_id(),
            updates.sub_id(),
            "two questions cannot share one subscription id"
        );

        recon
            .attach(comments)
            .map_err(|r| r.error)
            .expect("comments attach");
        recon
            .attach(updates)
            .map_err(|r| r.error)
            .expect("updates attach");
        assert!(recon.in_flight(HistoryStream::Comments));
        assert!(recon.in_flight(HistoryStream::PullRequestUpdates));
    }

    /// One stream degrading terminalises the whole reconstruction.
    #[tokio::test]
    async fn a_degrading_stream_drops_every_other_page() {
        let (pr, _) = bound_root(KIND_GIT_PULL_REQUEST).await;
        let mut recon = RootReconstruction::begin(&pr, 1_000, 4, 1_000);
        let mut h = PageHarness::new();

        let comments = open_for(&mut h, &mut recon, HistoryStream::Comments).await;
        let comments_sub = comments.sub_id().to_string();
        let mut updates = open_for(&mut h, &mut recon, HistoryStream::PullRequestUpdates).await;
        let updates_sub = updates.sub_id().to_string();
        updates.observe_unusable("frame was not parseable as an event");

        recon
            .attach(comments)
            .map_err(|r| r.error)
            .expect("comments attach");
        recon
            .attach(updates)
            .map_err(|r| r.error)
            .expect("updates attach");

        let witness = h.witness(&updates_sub);
        match recon.complete(&witness) {
            Some(StreamAdvance::Degraded { .. }) => {}
            other => panic!("a poisoned page degrades: {other:?}"),
        }

        assert!(recon.abandoned_reason().is_some());
        assert!(
            !recon.in_flight(HistoryStream::Comments),
            "terminal degradation must drop every other connection-owned page"
        );
        let late = admitted(&h, &comments_sub, comment_at(900, "late")).await;
        assert_eq!(
            recon.observe(late),
            FrameRouting::NotOurs,
            "and must accept nothing further"
        );
        assert!(recon.begin_page(HistoryStream::Comments).is_none());
    }

    /// Open a successor registration for the same root and stream.
    ///
    /// What a paginating driver does between pages, and what a retry does after
    /// a page is abandoned. The successor's collector comes from a standalone
    /// cursor because the owner is deliberately *not* told: the point is that
    /// the registry has moved on while the reconstruction may not have.
    async fn reopen(
        h: &mut PageHarness,
        recon: &RootReconstruction,
        stream: HistoryStream,
    ) -> OpenedHistoryPage {
        let mut successor = HistoryCursor::new(
            HistoryScope::Root {
                root: recon.root().to_string(),
                stream,
            },
            1_000,
            4,
            1_000,
        );
        h.open_with(successor.begin_request()).await
    }

    /// A frame admitted by a *different* registration cannot enter this page.
    ///
    /// Two layers now say no, and the outer one is new. A successor
    /// registration has an id of its own, so a frame admitted by it is not even
    /// looked up against the page in flight — `NotOurs`, not absorbed. Under
    /// the old deterministic id the two shared a name, the lookup succeeded,
    /// and only the authority comparison stood between a successor's rows and a
    /// page opened against a different `until` bound.
    ///
    /// This is the *dropped* direction, which is the safe one: nothing enters
    /// the page and nothing is damaged. The frames that must reach a page —
    /// the ones its own registration admitted — are covered by
    /// `a_page_fills_from_the_wire_and_completes_at_its_own_boundary`.
    #[tokio::test]
    async fn a_frame_from_another_registration_cannot_enter_this_page() {
        let (issue, _) = bound_root(KIND_GIT_ISSUE).await;
        let mut recon = RootReconstruction::begin(&issue, 1_000, 4, 1_000);
        let mut h = PageHarness::new();

        let page = open_for(&mut h, &mut recon, HistoryStream::Comments).await;
        let sub_id = page.sub_id().to_string();
        recon.attach(page).map_err(|r| r.error).expect("attaches");

        // The registry moves on; the reconstruction still holds the old page.
        let successor = reopen(&mut h, &recon, HistoryStream::Comments).await;
        assert_ne!(
            successor.sub_id(),
            sub_id,
            "a successor must not be able to wear its predecessor's name"
        );

        let frame = admitted(
            &h,
            successor.sub_id(),
            comment_at(900, "from the successor"),
        )
        .await;
        assert_eq!(
            recon.observe(frame),
            FrameRouting::NotOurs,
            "the page in flight was opened by a different registration"
        );
        assert!(
            recon.abandoned_reason().is_none(),
            "and nothing about that is a contradiction — it is simply not ours"
        );

        // The page is untouched and still completes on its own boundary, with
        // none of the successor's rows in it.
        let witness = h.witness(&sub_id);
        match recon.complete(&witness) {
            Some(StreamAdvance::Finished { .. }) => {}
            other => panic!("the page must still complete: {other:?}"),
        }
        assert_eq!(recon.finished_streams()[0].len(), 0);
    }

    /// A predecessor's row is refused, and the page it was offered to is
    /// untouched.
    ///
    /// The same shape as above from the other side: the frame belongs to a
    /// registration that has *been* replaced rather than one that replaced it.
    /// Also `NotOurs` — a retired registration's id names no page in flight.
    #[tokio::test]
    async fn a_predecessors_row_is_refused_without_touching_the_page() {
        let (issue, _) = bound_root(KIND_GIT_ISSUE).await;
        let mut recon = RootReconstruction::begin(&issue, 1_000, 4, 1_000);
        let mut h = PageHarness::new();

        // A first registration and a frame admitted by it. The page it opened
        // is dropped without ever being attached.
        let first = open_for(&mut h, &mut recon, HistoryStream::Comments).await;
        let stale = admitted(
            &h,
            first.sub_id(),
            comment_at(900, "late from the predecessor"),
        )
        .await;
        drop(first);

        // The successor's page is the one in flight.
        let page = open_for(&mut h, &mut recon, HistoryStream::Comments).await;
        let sub_id = page.sub_id().to_string();
        assert_ne!(stale.sub_id(), sub_id, "two attempts, two names");
        recon.attach(page).map_err(|r| r.error).expect("attaches");

        assert_eq!(recon.observe(stale), FrameRouting::NotOurs);
        assert!(recon.abandoned_reason().is_none(), "and nothing is damaged");

        // The current page still completes under its own boundary, with none of
        // the predecessor's rows in it.
        let witness = h.witness(&sub_id);
        match recon.complete(&witness) {
            Some(StreamAdvance::Finished { .. }) => {}
            other => panic!("the current page is unharmed and completes: {other:?}"),
        }
        assert_eq!(
            recon.finished_streams()[0].len(),
            0,
            "the refused row is not in the retained history"
        );
    }

    /// A root that gave up is never taken as a completed history, even with
    /// every stream it required already exhausted.
    ///
    /// `all_streams_finished` is deliberately narrower than "all streams
    /// retained", and the difference is invisible along the path production
    /// takes today: `complete` returns `None` for an abandoned reconstruction,
    /// so the `Finished` advance that drives `take_completed` cannot be minted
    /// after a root has given up. That makes the check unfalsifiable by the
    /// wire — and an unfalsifiable guard on a state that decides whether a
    /// conversation's history is trusted is exactly the kind that quietly stops
    /// holding.
    ///
    /// So it is pinned here directly. Exhaustion first, abandonment second: the
    /// order that leaves a reconstruction holding a complete-looking set of
    /// retained streams and no right to claim them.
    #[tokio::test]
    async fn an_abandoned_root_is_never_taken_as_a_completed_history() {
        let (issue, _) = bound_root(KIND_GIT_ISSUE).await;
        let mut recon = RootReconstruction::begin(&issue, 1_000, 4, 1_000);
        let root = recon.root().to_string();
        let mut h = PageHarness::new();

        // The one stream an issue requires, driven to proven exhaustion.
        let page = open_for(&mut h, &mut recon, HistoryStream::Comments).await;
        let sub_id = page.sub_id().to_string();
        recon.attach(page).map_err(|r| r.error).expect("attaches");
        let witness = h.witness(&sub_id);
        match recon.complete(&witness) {
            Some(StreamAdvance::Finished { .. }) => {}
            other => panic!("the page completes on its own boundary: {other:?}"),
        }
        assert!(
            recon.all_streams_finished(),
            "precondition: every stream this root requires has reached exhaustion"
        );

        // And then it gives up anyway.
        recon.abandon("the history could not be proven");
        assert!(
            !recon.all_streams_finished(),
            "a root that gave up has proven nothing, whatever its streams hold"
        );

        let mut router = ProjectReconstructions::new();
        assert!(router.insert(recon));
        assert!(
            router.take_completed(&root).is_none(),
            "so nothing may take those rows as this root's history"
        );
        assert_eq!(
            router.abandoned().len(),
            1,
            "and it stays visible as degraded rather than being consumed by the taking"
        );
    }

    /// The router hands each root only the frames its own requests admitted.
    #[tokio::test]
    async fn the_router_gives_each_root_only_its_own_frames() {
        let (one, _) = bound_root(KIND_GIT_ISSUE).await;
        let (two, _) = bound_root(KIND_GIT_ISSUE).await;
        let mut h = PageHarness::new();
        let mut first = RootReconstruction::begin(&one, 1_000, 4, 1_000);
        let mut second = RootReconstruction::begin(&two, 1_000, 4, 1_000);
        let first_id = first.root().to_string();
        let second_id = second.root().to_string();
        assert_ne!(first_id, second_id, "two distinct roots");

        let first_sub = {
            let page = open_for(&mut h, &mut first, HistoryStream::Comments).await;
            let sub_id = page.sub_id().to_string();
            first.attach(page).map_err(|r| r.error).expect("attaches");
            sub_id
        };
        let second_sub = {
            let page = open_for(&mut h, &mut second, HistoryStream::Comments).await;
            let sub_id = page.sub_id().to_string();
            second.attach(page).map_err(|r| r.error).expect("attaches");
            sub_id
        };

        let mut router = ProjectReconstructions::new();
        assert!(router.insert(first));
        assert!(router.insert(second));

        // Deliberately the **second** root's request, and the second inserted.
        // Sending the first root's traffic would pass under a router that
        // ignored the root entirely and always took the reconstruction it
        // happened to hold first — which is how the first version of this test
        // survived exactly that mutant.
        let second_root = second_id.clone();
        let frame = admitted(
            &h,
            &second_sub,
            comment_on(&second_root, 900, "for the second"),
        )
        .await;
        assert_eq!(
            router.observe(frame),
            FrameRouting::Absorbed {
                stream: HistoryStream::Comments
            }
        );

        // Each page completes on its own boundary, and the row is in exactly
        // one of them.
        for (sub_id, root, expected) in [(&second_sub, &second_root, 1), (&first_sub, &first_id, 0)]
        {
            let witness = h.witness(sub_id);
            match router.complete(&witness) {
                Some(StreamAdvance::Finished { .. }) => {}
                other => panic!("{root} completes on its own boundary: {other:?}"),
            }
            assert_eq!(
                router.get(root).expect("still tracked").finished_streams()[0].len(),
                expected,
                "wrong number of rows retained for {root}"
            );
        }
    }

    /// A second reconstruction of one root is refused.
    #[tokio::test]
    async fn one_root_gets_one_reconstruction() {
        let (issue, _) = bound_root(KIND_GIT_ISSUE).await;
        let mut router = ProjectReconstructions::new();
        assert!(router.insert(RootReconstruction::begin(&issue, 1_000, 4, 1_000)));
        assert!(
            !router.insert(RootReconstruction::begin(&issue, 1_000, 4, 1_000)),
            "two owners for one root would both be offered every frame, and \
             which one absorbed it would depend on insertion order"
        );
    }

    /// A stale boundary at the owner level leaves the page in flight.
    #[tokio::test]
    async fn a_stale_boundary_leaves_the_owners_page_in_flight() {
        let (issue, _) = bound_root(KIND_GIT_ISSUE).await;
        let mut recon = RootReconstruction::begin(&issue, 1_000, 4, 1_000);
        let mut h = PageHarness::new();

        // A first page and its boundary, the page dropped without ever being
        // attached — the predecessor whose EOSE arrives late.
        let first = open_for(&mut h, &mut recon, HistoryStream::Comments).await;
        let stale = h.witness(first.sub_id());
        drop(first);

        // The replacement is a strictly later instance, under a name of its
        // own — so the stale boundary does not even reach it.
        let page = open_for(&mut h, &mut recon, HistoryStream::Comments).await;
        assert_ne!(page.incarnation(), stale.incarnation());
        assert_ne!(page.sub_id(), stale.sub_id());
        recon.attach(page).map_err(|r| r.error).expect("attaches");

        match recon.complete(&stale) {
            None => {}
            other => panic!("a predecessor boundary reaches no page, got {other:?}"),
        }
        assert!(
            recon.in_flight(HistoryStream::Comments),
            "the page stays in flight — only its own boundary may finish it"
        );
        assert!(recon.abandoned_reason().is_none());
    }

    /// An issue requires one stream; a pull request requires two.
    #[tokio::test]
    async fn required_streams_come_from_the_proven_root_class() {
        let (issue, _) = bound_root(KIND_GIT_ISSUE).await;
        let (pr, _) = bound_root(KIND_GIT_PULL_REQUEST).await;

        let issue_recon = RootReconstruction::begin(&issue, 1_000, 4, 1_000);
        let pr_recon = RootReconstruction::begin(&pr, 1_000, 4, 1_000);

        assert!(!issue_recon.is_pull_request());
        assert_eq!(issue_recon.pages_wanted().len(), 1);
        assert!(pr_recon.is_pull_request());
        assert_eq!(pr_recon.pages_wanted().len(), 2);
    }

    /// One cutoff, taken once, with no method that moves it.
    #[tokio::test]
    async fn the_cutoff_is_shared_by_every_stream_and_never_moves() {
        let (pr, _) = bound_root(KIND_GIT_PULL_REQUEST).await;
        let recon = RootReconstruction::begin(&pr, 1_000, 4, 1_000);

        assert_eq!(recon.cutoff(), 1_000);
        for (_, until, _) in recon.pages_wanted() {
            assert_eq!(
                until, 1_000,
                "every stream starts at the one cutoff, or the merge compares \
                 histories ending at different moments"
            );
        }

        let source = include_str!("project.rs");
        assert!(
            !source.contains(concat!("fn set_", "cutoff")),
            "nothing may move the cutoff after construction"
        );
    }

    /// A page is routed by what it says it collects for, not by a caller
    /// argument.
    #[tokio::test]
    async fn a_page_is_attached_by_its_own_stream_not_a_caller_claim() {
        let (pr, _) = bound_root(KIND_GIT_PULL_REQUEST).await;
        let mut recon = RootReconstruction::begin(&pr, 1_000, 4, 1_000);
        let mut h = PageHarness::new();

        let page = open_for(&mut h, &mut recon, HistoryStream::PullRequestUpdates).await;
        assert_eq!(
            page.scope(),
            &HistoryScope::Root {
                root: pr.binding().root().to_string(),
                stream: HistoryStream::PullRequestUpdates,
            }
        );
        recon.attach(page).expect("attaches to its own stream");

        assert!(recon.in_flight(HistoryStream::PullRequestUpdates));
        assert!(!recon.in_flight(HistoryStream::Comments));

        // `attach` takes no stream argument at all — the signature cannot
        // express the wrong claim.
        let source = include_str!("project.rs");
        assert!(
            !source.contains(concat!("fn attach(&mut self, stream", ":")),
            "attach must not accept a caller-supplied stream"
        );
    }

    /// One page in flight per stream.
    #[tokio::test]
    async fn a_stream_holds_at_most_one_page_in_flight() {
        let (issue, _) = bound_root(KIND_GIT_ISSUE).await;
        let mut recon = RootReconstruction::begin(&issue, 1_000, 4, 1_000);
        let mut h = PageHarness::new();

        // Two collectors issued before either is attached, then bound in turn.
        let a = recon.begin_page(HistoryStream::Comments).expect("first");
        let b = recon.begin_page(HistoryStream::Comments).expect("second");
        assert_eq!(
            a.generation(),
            b.generation(),
            "issuing is a proposal, not an advance — a second issue must not \
             invalidate a first that is still on its way to the socket"
        );
        let first = h.open_with(a).await;
        let second = h.open_with(b).await;
        recon
            .attach(first)
            .map_err(|r| r.error)
            .expect("first attaches");

        let rejected = recon.attach(second).unwrap_err();
        assert_eq!(
            rejected.error,
            AttachError::AlreadyInFlight,
            "two boundaries on one stream could not be ordered by its cursor"
        );
        assert_eq!(
            rejected.page.scope().stream(),
            Some(HistoryStream::Comments),
            "and the page comes back, so its registration is still reachable"
        );

        // Completion of the survivor is asserted separately, in
        // `a_second_issue_cannot_poison_the_first_genuine_page`: binding twice
        // under one subscription id necessarily retires the first
        // registration, so this test cannot also witness the first page.
    }

    /// Merely *issuing* a second collector must not poison the first.
    ///
    /// The exact counterexample this piece failed. `begin_page` used to call
    /// `begin_request`, advancing the owned cursor on every call, so a second
    /// issue superseded a first page that was already on its way to the socket
    /// — and the first page's own genuine boundary then degraded the whole
    /// reconstruction.
    ///
    /// The previous version of the in-flight test built exactly this state and
    /// asserted only that the *loser* was rejected. It passed while leaving the
    /// reconstruction holding a page its own cursor had superseded.
    #[tokio::test]
    async fn a_second_issue_cannot_poison_the_first_genuine_page() {
        let (issue, _) = bound_root(KIND_GIT_ISSUE).await;
        let mut recon = RootReconstruction::begin(&issue, 1_000, 4, 1_000);
        let mut h = PageHarness::new();

        let first = recon.begin_page(HistoryStream::Comments).expect("first");
        let second = recon.begin_page(HistoryStream::Comments).expect("second");
        assert_eq!(
            first.generation(),
            second.generation(),
            "issuing proposes; it does not advance"
        );
        drop(second);

        let page = h.open_with(first).await;
        let sub_id = page.sub_id().to_string();
        recon.attach(page).map_err(|r| r.error).expect("attaches");

        let witness = h.witness(&sub_id);
        match recon.complete(&witness) {
            Some(StreamAdvance::Finished { .. }) => {}
            other => panic!("the first genuine page must survive a second issue: {other:?}"),
        }
        assert!(recon.abandoned_reason().is_none());
    }

    /// A collector from an unrelated cursor cannot commit the owner's.
    ///
    /// The reviewer's counterexample. An outsider cursor with identical root,
    /// stream, cutoff, bound and limit stamps the *same* generation, so the
    /// number authenticates nothing on its own — the descriptive-value identity
    /// corrected in Piece 1, recurring one layer down.
    #[tokio::test]
    async fn an_unproposed_collector_cannot_commit_the_owners_cursor() {
        let (issue, _) = bound_root(KIND_GIT_ISSUE).await;
        let mut recon = RootReconstruction::begin(&issue, 1_000, 4, 1_000);
        let mut h = PageHarness::new();

        // Never call `recon.begin_page()`.
        let mut outsider = HistoryCursor::new(
            HistoryScope::Root {
                root: recon.root().to_string(),
                stream: HistoryStream::Comments,
            },
            1_000,
            4,
            1_000,
        );
        let collector = outsider.begin_request();
        assert_eq!(
            collector.generation(),
            1,
            "the outsider stamps the same number the owner would"
        );

        let page = h.open_with(collector).await;
        let rejected = recon.attach(page).unwrap_err();
        assert_eq!(
            rejected.error,
            AttachError::Superseded,
            "an unproposed collector must not advance the owner's cursor"
        );

        // The owner is undamaged: its own page still completes.
        let mine = open_for(&mut h, &mut recon, HistoryStream::Comments).await;
        let sub_id = mine.sub_id().to_string();
        recon.attach(mine).map_err(|r| r.error).expect("attaches");
        let witness = h.witness(&sub_id);
        assert!(matches!(
            recon.complete(&witness),
            Some(StreamAdvance::Finished { .. })
        ));
    }

    /// Generation exhaustion fails closed and terminalises.
    ///
    /// Unchecked `+ 1` panicked in debug and wrapped to zero in release — the
    /// same counter divergence already refused for registration incarnations.
    /// A wrapped generation lets a superseded page match a later request, which
    /// is the one thing the generation exists to prevent.
    #[tokio::test]
    async fn exhausted_page_generations_terminalise_rather_than_wrap() {
        let (issue, _) = bound_root(KIND_GIT_ISSUE).await;
        let mut recon = RootReconstruction::begin(&issue, 1_000, 4, 1_000);
        recon.force_stream_generation(HistoryStream::Comments, u64::MAX);

        assert!(
            recon.begin_page(HistoryStream::Comments).is_none(),
            "no collector may be issued once the space is spent"
        );
        assert!(
            recon
                .abandoned_reason()
                .is_some_and(|r| r.contains("exhausted")),
            "and the reconstruction terminalises rather than spinning"
        );
        assert!(
            recon.pages_wanted().is_empty(),
            "an exhausted stream must stop being advertised"
        );
    }

    /// A cursor at the ceiling proposes nothing, rather than wrapping to zero.
    #[test]
    fn a_cursor_at_the_generation_ceiling_proposes_nothing() {
        let mut c = cursor(1_000, 4, 1_000);
        c.force_generation(u64::MAX);
        assert!(c.propose_request().is_none());
    }

    /// A failed transport leaves the stream able to retry.
    ///
    /// The other half of the proposal contract: issuing without advancing must
    /// not strand a stream when the write never lands. Nothing is outstanding
    /// to clear, because nothing moved.
    #[tokio::test]
    async fn a_failed_open_leaves_the_stream_retryable() {
        let (issue, _) = bound_root(KIND_GIT_ISSUE).await;
        let mut recon = RootReconstruction::begin(&issue, 1_000, 4, 1_000);
        let mut h = PageHarness::new();

        let doomed = recon.begin_page(HistoryStream::Comments).expect("issued");
        let mut dead = test_socket().await;
        dead.client.close(None).await.expect("close");
        assert!(matches!(
            h.requests.open_history_page(&mut dead.client, doomed).await,
            PageOpen::WriteFailed(_)
        ));

        // The stream is still asking, at the same bound, and a fresh attempt
        // completes.
        assert_eq!(
            recon.pages_wanted(),
            vec![(HistoryStream::Comments, 1_000, 4)]
        );
        let page = open_for(&mut h, &mut recon, HistoryStream::Comments).await;
        let good_sub = page.sub_id().to_string();
        recon.attach(page).map_err(|r| r.error).expect("attaches");
        let witness = h.witness(&good_sub);
        assert!(matches!(
            recon.complete(&witness),
            Some(StreamAdvance::Finished { .. })
        ));
    }

    /// A page for another root, or a stream this class does not require, is
    /// refused.
    #[tokio::test]
    async fn a_foreign_page_is_refused() {
        let (issue, _) = bound_root(KIND_GIT_ISSUE).await;
        let (other, _) = bound_root(KIND_GIT_ISSUE).await;
        let mut recon = RootReconstruction::begin(&issue, 1_000, 4, 1_000);
        let mut other_recon = RootReconstruction::begin(&other, 1_000, 4, 1_000);
        let mut h = PageHarness::new();

        let foreign = open_for(&mut h, &mut other_recon, HistoryStream::Comments).await;
        assert_eq!(
            recon.attach(foreign).unwrap_err().error,
            AttachError::WrongRoot
        );

        // An issue does not require PR updates. Built on *this* root, so the
        // root check passes and the stream check is what refuses it — the
        // earlier version of this test used a different root and was really
        // asserting `WrongRoot` twice.
        let updates =
            unissuable_page(&mut h, recon.root(), HistoryStream::PullRequestUpdates).await;
        assert_eq!(
            recon.attach(updates).unwrap_err().error,
            AttachError::StreamNotRequired
        );

        other_recon.disconnected();
    }

    /// A disconnect drops pages but keeps the cutoff and the cursors.
    #[tokio::test]
    async fn a_disconnect_drops_pages_and_keeps_position() {
        let (issue, _) = bound_root(KIND_GIT_ISSUE).await;
        let mut recon = RootReconstruction::begin(&issue, 1_000, 4, 1_000);
        let mut h = PageHarness::new();

        let page = open_for(&mut h, &mut recon, HistoryStream::Comments).await;
        recon.attach(page).map_err(|r| r.error).expect("attaches");
        assert!(recon.in_flight(HistoryStream::Comments));

        recon.disconnected();

        assert!(
            !recon.in_flight(HistoryStream::Comments),
            "pages belonged to the connection that died"
        );
        assert_eq!(recon.cutoff(), 1_000, "the cutoff survives the reconnect");
        assert_eq!(
            recon.pages_wanted(),
            vec![(HistoryStream::Comments, 1_000, 4)],
            "and the stream asks again from where it was"
        );
    }

    /// Abandonment is terminal: no further page may attach.
    #[tokio::test]
    async fn an_abandoned_reconstruction_accepts_nothing_further() {
        let (issue, _) = bound_root(KIND_GIT_ISSUE).await;
        let mut recon = RootReconstruction::begin(&issue, 1_000, 4, 1_000);
        let mut h = PageHarness::new();
        let page = open_for(&mut h, &mut recon, HistoryStream::Comments).await;

        recon.abandon("relay refused the catch-up");

        assert_eq!(recon.abandoned_reason(), Some("relay refused the catch-up"));
        assert!(recon.pages_wanted().is_empty());
        assert_eq!(recon.attach(page).unwrap_err().error, AttachError::Closed);
    }

    /// No accessible registry API can manufacture send authority.
    ///
    /// Structural, and checked as source text because the claim is about the
    /// *absence* of a method. An earlier revision had `confirm_sent(&str)` — a
    /// bare lever any crate caller could pull — and a test that called it proved
    /// only that the boolean worked, not that a socket existed.
    ///
    /// Every needle is assembled with `concat!` so this test's own source does
    /// not contain the pattern it forbids. That is not fastidiousness: the first
    /// version of a test in this file failed against itself, and a source-reading
    /// test that matches its own text is a test that can only ever report on
    /// itself.
    #[test]
    fn no_api_can_manufacture_send_authority() {
        let source = include_str!("project.rs");

        assert!(
            !source.contains(concat!("fn confirm", "_sent")),
            "a bare mark-as-sent method is a lever; the write must own the transition"
        );

        // The write must not be a caller-supplied callback. A generic
        // `FnOnce(..) -> Result<(), E>` lets `|_| async { Ok(()) }` manufacture
        // send authority with no socket, which is `confirm_sent` in disguise.
        assert!(
            !source.contains(concat!("write", ": F,")),
            "the openers must not take a caller-supplied writer"
        );
        assert!(
            !source.contains(concat!(
                "Fut: std::future::Future<Output",
                " = Result<(), E>>"
            )),
            "no generic success-returning callback may produce send authority"
        );

        // The sink must actually be sealed. Counting implementations does not
        // establish this — an unsealed trait with one impl today is an open door
        // tomorrow — and an earlier version of this test asserted only the count,
        // so unsealing the trait passed it.
        assert!(
            source.contains(concat!("trait ProjectReqSink: ", "sealed::Sealed")),
            "ProjectReqSink must be sealed by a private supertrait"
        );

        // Exactly one implementation of the sealed sink, and it is for the live
        // socket. A second impl anywhere in the crate would reopen the hole.
        assert_eq!(
            source.matches(concat!("\nimpl ProjectReq", "Sink")).count(),
            1,
            "ProjectReqSink must have exactly one implementation"
        );
        assert_eq!(
            source.matches(concat!("\nimpl sealed::", "Sealed")).count(),
            1,
            "the private supertrait must have exactly one implementation"
        );

        // Anchored to the struct-field form: the bare needle `sent: bool` is a
        // substring of `p_tag_present: bool` elsewhere in this file, so it failed
        // for a reason that had nothing to do with send authority.
        assert!(
            !source.contains(concat!("\n        sent", ": bool,")),
            "no pending/sent flag may exist; installation happens post-write"
        );
    }

    /// No registration is installed before its write returns — at either opener.
    ///
    /// The structural half of cancellation-safety, and the load-bearing half: a
    /// real socket write completes on its first poll, so a *mid-write* cancel
    /// cannot be forced deterministically from a test, and sealing the sink
    /// (correctly) removed the ability to inject a writer that pends. That
    /// trade-off is why this is asserted against the source rather than observed
    /// at runtime.
    ///
    /// There are three openers now — pages, ordinary opens, and replacement —
    /// and this count has been revised twice for the same reason. A count of
    /// one, then two, was never the property; it was the shape the property
    /// happened to have while there were that many openers. What is pinned is
    /// that installation sites and awaited writes strictly alternate, so each
    /// install follows a write that returned and none can drift above the one
    /// before it.
    ///
    /// Revising the number is correct **only** while the alternation assertion
    /// below still passes unchanged. If a new opener ever forces that one to be
    /// weakened, the opener is wrong, not the test.
    #[test]
    fn no_registration_is_installed_before_its_write_returns() {
        let source = include_str!("project.rs");

        let installs: Vec<usize> = source
            .match_indices(concat!("self.live.", "insert("))
            .map(|(i, _)| i)
            .collect();
        let writes: Vec<usize> = source
            .match_indices(concat!("write_project_", "req(text).await"))
            .map(|(i, _)| i)
            .collect();

        assert_eq!(
            installs.len(),
            3,
            "exactly three places install a live registration: {installs:?}"
        );
        assert_eq!(
            writes.len(),
            installs.len(),
            "each installation must have a write of its own: {writes:?}"
        );

        // write, install, write, install — strictly interleaved. A site that
        // moved above its write, or a second install sharing one write, breaks
        // the ordering rather than the count.
        let mut expected = Vec::new();
        for (w, i) in writes.iter().zip(&installs) {
            expected.push(*w);
            expected.push(*i);
        }
        let mut sorted = expected.clone();
        sorted.sort_unstable();
        assert_eq!(
            sorted, expected,
            "every registration must be installed after the write it belongs to \
             returns, or a dropped future strands a pending entry nothing can \
             promote"
        );
    }

    /// The page type is constructed in exactly one place.
    ///
    /// This is what the deleted binding checks were really protecting: not that
    /// a caller passed matching halves, but that nothing outside the registry
    /// can produce an `OpenedHistoryPage` at all. With the open folded into one
    /// operation there is a single construction site, and it sits after a write
    /// that returned.
    #[test]
    fn the_page_type_is_constructed_in_exactly_one_place() {
        let source = include_str!("project.rs");
        // Built with `concat!` so the needle is not itself in the file — the
        // count would otherwise include this test. Definitions, `impl` blocks
        // and return types wear the same name, so they are filtered out rather
        // than counted and explained away: what is left is construction.
        let needle = concat!("OpenedHistoryPage ", "{");
        let sites: Vec<&str> = source
            .lines()
            .filter(|line| line.contains(needle))
            .filter(|line| {
                !line.contains("-> ") && !line.contains("struct ") && !line.contains("impl ")
            })
            .collect();
        assert_eq!(
            sites.len(),
            1,
            "exactly one construction site, inside `open_history_page`: {sites:?}"
        );
    }

    /// Piece 2 makes no readiness claim; that is Piece 6.
    #[test]
    fn the_owner_claims_nothing_about_completeness() {
        let source = include_str!("project.rs");
        for forbidden in [
            concat!("fn is_", "complete"),
            concat!("fn is_", "ready"),
            concat!("fn readiness", "("),
        ] {
            assert!(
                !source.contains(forbidden),
                "readiness depends on backpressure recovery that does not exist \
                 yet: {forbidden}"
            );
        }
    }

    /// The cutoff the merge tests' reconstruction selected, unless one says
    /// otherwise.
    const SNAPSHOT: u64 = 1_000;

    async fn completed_comments_from(
        root: &str,
        cutoff: u64,
        entries: &[(u64, &str)],
    ) -> RetainedStream {
        let mut c = HistoryCursor::new(
            HistoryScope::Root {
                root: root.to_string(),
                stream: HistoryStream::Comments,
            },
            cutoff,
            100,
            1_000,
        );
        let mut h = PageHarness::new();
        let mut page = h.open(&mut c).await;
        for (ts, marker) in entries {
            page.observe(row(comment_on(root, *ts, marker)).await);
        }
        expect_complete(complete_opened(&mut c, &mut h, page))
    }

    async fn completed_updates_from(
        root: &str,
        cutoff: u64,
        entries: &[(u64, &str)],
    ) -> RetainedStream {
        let mut c = HistoryCursor::new(
            HistoryScope::Root {
                root: root.to_string(),
                stream: HistoryStream::PullRequestUpdates,
            },
            cutoff,
            100,
            1_000,
        );
        let mut h = PageHarness::new();
        let mut page = h.open(&mut c).await;
        for (ts, marker) in entries {
            page.observe(row(pr_update_on(root, *ts, marker)).await);
        }
        expect_complete(complete_opened(&mut c, &mut h, page))
    }

    async fn completed_comments(root: &str, entries: &[(u64, &str)]) -> RetainedStream {
        completed_comments_from(root, SNAPSHOT, entries).await
    }

    async fn completed_updates(root: &str, entries: &[(u64, &str)]) -> RetainedStream {
        completed_updates_from(root, SNAPSHOT, entries).await
    }

    #[tokio::test]
    async fn a_merge_puts_the_root_first_then_orders_by_time_and_id() {
        let (bound, root_id) = bound_root(KIND_GIT_PULL_REQUEST).await;
        let comments = completed_comments(&root_id, &[(900, "c1"), (880, "c2")]).await;
        let updates = completed_updates(&root_id, &[(890, "r1")]).await;

        let merged =
            merge_completed_streams(&bound, SNAPSHOT, vec![comments, updates]).expect("merges");

        assert_eq!(merged.len(), 4);
        assert_eq!(merged.root(), root_id);
        assert_eq!(
            merged.cutoff(),
            SNAPSHOT,
            "the output names the snapshot every stream was checked against"
        );
        assert_eq!(
            merged.rows()[0].id(),
            root_id,
            "the root leads because it is the root, not because it is oldest"
        );
        assert_eq!(stamps_of(&merged.rows()[1..]), vec![880, 890, 900]);
    }

    #[tokio::test]
    async fn a_merge_breaks_same_second_ties_on_event_id() {
        let (bound, root_id) = bound_root(KIND_GIT_ISSUE).await;
        let comments = completed_comments(&root_id, &[(900, "a"), (900, "b"), (900, "c")]).await;

        let merged = merge_completed_streams(&bound, SNAPSHOT, vec![comments]).expect("merges");
        let tail = ids_of(&merged.rows()[1..]);
        let mut sorted = tail.clone();
        sorted.sort();
        assert_eq!(tail, sorted, "one order, not whichever was visited first");
    }

    #[tokio::test]
    async fn a_merge_refuses_until_every_required_stream_has_completed() {
        // A `1618` root merged from its comments alone would produce a
        // well-ordered, entirely plausible history with every revision missing.
        let (bound, root_id) = bound_root(KIND_GIT_PULL_REQUEST).await;
        let comments = completed_comments(&root_id, &[(900, "c1")]).await;
        let err = merge_completed_streams(&bound, SNAPSHOT, vec![comments]).expect_err("refuses");
        assert!(err.contains("PullRequestUpdates"), "{err}");

        // The same root merges once the second stream has completed too.
        let comments = completed_comments(&root_id, &[(900, "c1")]).await;
        let updates = completed_updates(&root_id, &[(890, "r1")]).await;
        assert_eq!(
            merge_completed_streams(&bound, SNAPSHOT, vec![comments, updates])
                .expect("merges")
                .len(),
            3
        );
    }

    #[tokio::test]
    async fn a_merge_refuses_a_stream_paginated_for_another_root() {
        let (bound, root_id) = bound_root(KIND_GIT_ISSUE).await;
        let foreign = completed_comments(OTHER_ROOT, &[(900, "c1")]).await;
        let err = merge_completed_streams(&bound, SNAPSHOT, vec![foreign]).expect_err("refuses");
        assert!(err.contains(OTHER_ROOT), "{err}");
        assert!(err.contains(&root_id), "{err}");
    }

    #[tokio::test]
    async fn a_merge_refuses_a_duplicated_stream() {
        // Two Comments streams would double every comment while satisfying any
        // "all required streams present" check written as a subset test.
        let (bound, root_id) = bound_root(KIND_GIT_ISSUE).await;
        let one = completed_comments(&root_id, &[(900, "c1")]).await;
        let two = completed_comments(&root_id, &[(900, "c1")]).await;
        let err = merge_completed_streams(&bound, SNAPSHOT, vec![one, two]).expect_err("refuses");
        assert!(err.contains("requires exactly"), "{err}");
    }

    #[tokio::test]
    async fn a_merge_cannot_be_handed_a_stream_that_did_not_complete() {
        // Structural, not documentary: `RetainedStream` has no constructor
        // outside `mod history`, and the only expression that produces one is
        // `PageOutcome::Complete`. A degraded or still-paginating cursor
        // therefore has nothing to pass to `merge_completed_streams`.
        let (bound, root_id) = bound_root(KIND_GIT_ISSUE).await;
        let mut c = HistoryCursor::new(
            HistoryScope::Root {
                root: root_id.to_string(),
                stream: HistoryStream::Comments,
            },
            1_000,
            2,
            1_000,
        );
        let mut h = PageHarness::new();
        let mut page = h.open(&mut c).await;
        page.observe_unusable("frame was not parseable as an event");
        let (_reason, rows) = expect_degraded(complete_opened(&mut c, &mut h, page));
        assert!(rows.is_empty());

        // The only thing that can be merged is a stream that completed.
        let comments = completed_comments(&root_id, &[(900, "c1")]).await;
        assert_eq!(
            merge_completed_streams(&bound, SNAPSHOT, vec![comments])
                .expect("merges")
                .len(),
            2
        );
    }

    #[tokio::test]
    async fn a_merge_refuses_streams_paginated_from_different_cutoffs() {
        // Both streams completed, both belong to this root, both are of the
        // required classes — and together they describe two different
        // snapshots. Everything in 501..=1_000 that was a PR update is absent,
        // and nothing in the merged result says so. A tidy, deterministic lie.
        let (bound, root_id) = bound_root(KIND_GIT_PULL_REQUEST).await;
        let comments = completed_comments_from(&root_id, 1_000, &[(900, "c1")]).await;
        let updates = completed_updates_from(&root_id, 500, &[(400, "r1")]).await;

        let err =
            merge_completed_streams(&bound, 1_000, vec![comments, updates]).expect_err("refuses");
        assert!(err.contains("PullRequestUpdates"), "{err}");
        assert!(err.contains("500"), "{err}");
        assert!(err.contains("1000"), "{err}");

        // Paginated from one snapshot, it merges.
        let comments = completed_comments_from(&root_id, 1_000, &[(900, "c1")]).await;
        let updates = completed_updates_from(&root_id, 1_000, &[(890, "r1")]).await;
        assert_eq!(
            merge_completed_streams(&bound, 1_000, vec![comments, updates])
                .expect("merges")
                .cutoff(),
            1_000
        );
    }

    #[tokio::test]
    async fn a_single_stream_root_still_has_to_match_the_selected_cutoff() {
        // A one-stream root has nothing to disagree with, so the check has to
        // be against the reconstruction's own cutoff rather than against a
        // sibling. Otherwise a driver cannot tell a stream of the snapshot it
        // chose from a stream of an earlier one it has forgotten about.
        let (bound, root_id) = bound_root(KIND_GIT_ISSUE).await;
        let stale = completed_comments_from(&root_id, 500, &[(400, "c1")]).await;
        let err = merge_completed_streams(&bound, 1_000, vec![stale]).expect_err("refuses");
        assert!(err.contains("Comments"), "{err}");
        assert!(err.contains("500"), "{err}");

        let current = completed_comments_from(&root_id, 1_000, &[(900, "c1")]).await;
        assert_eq!(
            merge_completed_streams(&bound, 1_000, vec![current])
                .expect("merges")
                .cutoff(),
            1_000
        );
    }

    #[tokio::test]
    async fn a_completed_stream_remembers_the_cutoff_it_was_opened_against() {
        // `until` moves as pages are consumed; `cutoff` must not, or there is
        // nothing left for the merge to check by the time a stream completes.
        let mut c = HistoryCursor::new(
            HistoryScope::Root {
                root: ROOT.to_string(),
                stream: HistoryStream::Comments,
            },
            1_000,
            2,
            1_000,
        );
        assert_eq!(c.cutoff(), 1_000);
        assert_eq!(
            expect_continue(run_page(&mut c, &[(900, "a"), (880, "b")]).await),
            (880, 2)
        );
        assert_eq!(c.until(), 880, "pagination has moved");
        assert_eq!(c.cutoff(), 1_000, "the snapshot boundary has not");

        let retained = expect_complete(run_page(&mut c, &[(870, "c")]).await);
        assert_eq!(retained.cutoff(), 1_000);
    }

    #[tokio::test]
    async fn degraded_rows_are_metadata_and_carry_no_replayable_witness() {
        // The previous version handed out `&[VerifiedProjectEvent]` crate-wide,
        // and those witnesses clone: a caller could copy them straight into a
        // fold without ever building a `RetainedStream`. Being unable to reach
        // the history type only ever closed one route. Now the witnesses are
        // described and dropped at the boundary, so what survives a failure is
        // a `DiagnosticRow` — ids, kinds and timestamps — which no authority
        // function accepts.
        // A one-row page against a one-row limit is saturated, so the cursor
        // holds the row rather than completing on it.
        let mut c = cursor(1_000, 1, 1_000);
        let held = comment_at(900, "a");
        assert_eq!(
            expect_continue(run_page_with(&mut c, std::slice::from_ref(&held)).await),
            (1_000, 4)
        );

        let mut h = PageHarness::new();
        let mut page = h.open(&mut c).await;
        page.observe_unusable("frame was not parseable as an event");
        let (_reason, rows) = expect_degraded(complete_opened(&mut c, &mut h, page));

        assert_eq!(
            rows.rows(),
            &[DiagnosticRow {
                id: held.id.to_hex(),
                kind: KIND_TEXT_NOTE,
                created_at: 900,
            }]
        );
    }

    // ── One cursor per exact filter ──────────────────────────────────────────

    #[test]
    fn the_root_is_not_an_exhaustible_stream() {
        // An exact-id query returning zero rows would satisfy "exhausted"
        // while proving no root exists — no root author, no binding, no class.
        // The root is a required object, proven by `VerifiedBoundRoot`.
        assert_eq!(
            HistoryStream::required_for(false),
            &[HistoryStream::Comments]
        );
        assert_eq!(
            HistoryStream::required_for(true),
            &[HistoryStream::Comments, HistoryStream::PullRequestUpdates]
        );
    }

    #[test]
    fn a_cursor_knows_which_stream_it_proves() {
        let c = HistoryCursor::new(
            HistoryScope::Root {
                root: ROOT.to_string(),
                stream: HistoryStream::PullRequestUpdates,
            },
            1_000,
            10,
            1_000,
        );
        assert_eq!(c.scope().stream(), Some(HistoryStream::PullRequestUpdates));
    }

    // ── Addressing resolution ────────────────────────────────────────────────

    /// This agent. Distinct from `STRANGER`, which happens to share this value
    /// elsewhere in the module — a collision that made an earlier version of
    /// `a_novel_bare_p_on_complete_live_history_is_explicit` fail, because the
    /// "unrelated root author" *was* the agent and the structural-presence rule
    /// correctly fired.
    const AGENT_PK: &str = "222b9658e0e4945cbca51ffa8d364a178a02e349d79847e9282e6ee1306a00ce";
    /// A third party who is neither the agent, the owner, nor a participant.
    const THIRD_PARTY: &str = "1111111111111111111111111111111111111111111111111111111111111111";

    fn agent_identity() -> AgentIdentity {
        AgentIdentity::new(&nostr::PublicKey::parse(AGENT_PK).expect("pubkey")).expect("identity")
    }

    fn prior(participant: bool, root_author: &str, owner: &str) -> PriorRootFacts {
        PriorRootFacts::for_test(participant, root_author, owner, RootState::Active)
    }

    fn watched() -> ProjectSubscription {
        ProjectSubscription::Watched { generation: 0 }
    }

    fn addressing(
        source: &ProjectSubscription,
        evidence: AddressingEvidence,
        readiness: &RootHistoryReadiness,
        facts: Option<&PriorRootFacts>,
    ) -> Option<Addressing> {
        resolve_addressing(source, &evidence, readiness, facts, &agent_identity())
    }

    #[test]
    fn no_p_tag_on_a_watched_subscription_means_watched_root() {
        assert_eq!(
            addressing(
                &watched(),
                AddressingEvidence::for_test(false, false),
                &RootHistoryReadiness::Complete,
                Some(&prior(false, THIRD_PARTY, OWNER)),
            ),
            Some(Addressing::WatchedRoot)
        );
    }

    #[test]
    fn an_enrolment_event_without_a_matching_p_is_refused() {
        // It was selected by a `#p` filter and does not carry a matching `p`:
        // the relay is broken or lying. Inventing `WatchedRoot` for it would be
        // treating a filter as authority.
        assert_eq!(
            addressing(
                &ProjectSubscription::Enrolment,
                AddressingEvidence::for_test(false, false),
                &RootHistoryReadiness::Complete,
                Some(&prior(false, THIRD_PARTY, OWNER)),
            ),
            None,
            "even from an authorised human, this must not enrol or wake"
        );
    }

    #[test]
    fn discovery_never_reaches_addressing() {
        assert_eq!(
            addressing(
                &ProjectSubscription::Discovery,
                AddressingEvidence::for_test(true, true),
                &RootHistoryReadiness::Complete,
                Some(&prior(false, THIRD_PARTY, OWNER)),
            ),
            None
        );
    }

    #[test]
    fn a_novel_bare_p_on_complete_history_is_explicit() {
        assert_eq!(
            addressing(
                &watched(),
                AddressingEvidence::for_test(true, false),
                &RootHistoryReadiness::Complete,
                Some(&prior(false, THIRD_PARTY, OWNER)),
            ),
            Some(Addressing::ExplicitMention)
        );
    }

    #[test]
    fn a_bare_p_is_never_explicit_without_complete_history() {
        // "Not seen before" is meaningless when we have not finished looking.
        for readiness in [
            RootHistoryReadiness::Unknown,
            RootHistoryReadiness::Reconstructing,
            RootHistoryReadiness::Degraded("breaker tripped".into()),
        ] {
            assert_eq!(
                addressing(
                    &watched(),
                    AddressingEvidence::for_test(true, false),
                    &readiness,
                    Some(&prior(false, THIRD_PARTY, OWNER)),
                ),
                Some(Addressing::InheritedParticipant),
                "{readiness:?}"
            );
        }
    }

    #[test]
    fn an_already_present_participant_stays_inherited() {
        assert_eq!(
            addressing(
                &watched(),
                AddressingEvidence::for_test(true, false),
                &RootHistoryReadiness::Complete,
                Some(&prior(true, THIRD_PARTY, OWNER)),
            ),
            Some(Addressing::InheritedParticipant)
        );
    }

    #[test]
    fn structural_presence_as_owner_or_root_author_is_not_intent() {
        // Desktop puts both in `p` on every comment, so their appearance says
        // nothing about whether anyone addressed the agent.
        assert_eq!(
            addressing(
                &watched(),
                AddressingEvidence::for_test(true, false),
                &RootHistoryReadiness::Complete,
                Some(&prior(false, AGENT_PK, OWNER)),
            ),
            Some(Addressing::InheritedParticipant),
            "agent is the root author"
        );
        assert_eq!(
            addressing(
                &watched(),
                AddressingEvidence::for_test(true, false),
                &RootHistoryReadiness::Complete,
                Some(&prior(false, THIRD_PARTY, AGENT_PK)),
            ),
            Some(Addressing::InheritedParticipant),
            "agent is the repository owner"
        );
    }

    #[test]
    fn a_visible_mention_is_explicit_even_for_an_existing_participant() {
        assert_eq!(
            addressing(
                &watched(),
                AddressingEvidence::for_test(true, true),
                &RootHistoryReadiness::Complete,
                Some(&prior(true, THIRD_PARTY, OWNER)),
            ),
            Some(Addressing::ExplicitMention)
        );
    }

    #[test]
    fn visible_text_without_the_matching_p_tag_is_not_addressing() {
        assert_eq!(
            addressing(
                &watched(),
                AddressingEvidence::for_test(false, true),
                &RootHistoryReadiness::Complete,
                Some(&prior(false, THIRD_PARTY, OWNER)),
            ),
            Some(Addressing::WatchedRoot)
        );
    }

    // ── Mention syntax, with token boundaries ────────────────────────────────

    async fn evidence_for(content: &str) -> AddressingEvidence {
        let keys = Keys::generate();
        let event = EventBuilder::new(Kind::Custom(KIND_TEXT_NOTE as u16), content)
            .tags([nostr::Tag::parse(tag(&["p", AGENT_PK])).unwrap()])
            .sign_with_keys(&keys)
            .expect("sign");
        let verified = VerifiedProjectEvent::verify(event).await.expect("valid");
        AddressingEvidence::resolve(&verified, &agent_identity())
    }

    // ── Comments aimed at another agent ──────────────────────────────────────

    const AGENT_DISPLAY_NAME: &str = "Claude";

    fn named_identity() -> AgentIdentity {
        agent_identity().with_display_name(AGENT_DISPLAY_NAME)
    }

    /// Evidence from a comment whose `p` set and content are both controlled.
    ///
    /// Both are supplied because the failure this guards needs them to
    /// disagree: the inherited `p` says "delivered to you", the content says
    /// "for somebody else", and only reading them together tells the two apart.
    async fn directed_evidence(content: &str, p_tags: &[&str]) -> AddressingEvidence {
        let keys = Keys::generate();
        let event = EventBuilder::new(Kind::Custom(KIND_TEXT_NOTE as u16), content)
            .tags(
                p_tags
                    .iter()
                    .map(|pk| nostr::Tag::parse(tag(&["p", pk])).unwrap()),
            )
            .sign_with_keys(&keys)
            .expect("sign");
        let verified = VerifiedProjectEvent::verify(event).await.expect("valid");
        AddressingEvidence::resolve(&verified, &named_identity())
    }

    /// The live failure at the layer that produced it, on a real `1621`.
    ///
    /// Roots `b1261034…` and `eb1803a2…` on `…:comment-e2e` had body `test`,
    /// named nobody, and carried exactly one `p` — the repository owner's,
    /// written by Desktop because the agent owns the coordinate. Each queued a
    /// turn, because a root took an early exit to `ExplicitMention` on the
    /// strength of its `p` alone.
    ///
    /// The event is signed and verified rather than described, so the `1621`
    /// kind is really present: this is the case the removed exception was keyed
    /// on, not a comment standing in for it.
    #[tokio::test]
    async fn a_roots_structural_owner_p_tag_is_not_an_address() {
        let keys = Keys::generate();
        let root = EventBuilder::new(Kind::Custom(KIND_GIT_ISSUE as u16), "test")
            .tags([nostr::Tag::parse(tag(&["p", AGENT_PK])).unwrap()])
            .sign_with_keys(&keys)
            .expect("sign");
        let verified = VerifiedProjectEvent::verify(root).await.expect("valid");
        assert_eq!(
            classify_kind(verified.kind()),
            KindEffect::Root,
            "the fixture has to be a root, or it proves nothing about roots"
        );
        let evidence = AddressingEvidence::resolve(&verified, &named_identity());

        assert!(evidence.p_tag_present, "the structural tag is there");
        assert!(!evidence.named_self, "and nothing in the body names us");
        assert_eq!(
            resolve_addressing(
                &ProjectSubscription::Enrolment,
                &evidence,
                // What the process actually holds for a root: nothing. A root
                // has no history preceding it, so no reconstruction can ever
                // improve this — which is exactly why the old exception was
                // reachable and why removing it has to hold here.
                &RootHistoryReadiness::Unknown,
                None,
                &named_identity(),
            ),
            Some(Addressing::InheritedParticipant),
            "a tag Desktop wrote by itself is not somebody addressing this agent"
        );
    }

    /// …and the root the same person meant to send, which must still wake.
    ///
    /// The pair is the point: the fix is target-only, not deaf. `@Claude` plus
    /// this agent's own `p` is the ordinary shape Desktop publishes for a
    /// mention, and it is the only shape that enrols a root now.
    #[tokio::test]
    async fn a_root_that_names_this_agent_is_an_explicit_mention() {
        let keys = Keys::generate();
        let root = EventBuilder::new(
            Kind::Custom(KIND_GIT_ISSUE as u16),
            format!("@{AGENT_DISPLAY_NAME} please take this one"),
        )
        .tags([nostr::Tag::parse(tag(&["p", AGENT_PK])).unwrap()])
        .sign_with_keys(&keys)
        .expect("sign");
        let verified = VerifiedProjectEvent::verify(root).await.expect("valid");
        let evidence = AddressingEvidence::resolve(&verified, &named_identity());

        assert_eq!(
            resolve_addressing(
                &ProjectSubscription::Enrolment,
                &evidence,
                &RootHistoryReadiness::Unknown,
                None,
                &named_identity(),
            ),
            Some(Addressing::ExplicitMention),
        );
        assert_eq!(
            classify_project_event(
                classify_kind(verified.kind()),
                ProjectAuthor::AuthorisedHuman,
                CallMarker::None,
                RootState::Unknown,
                Addressing::ExplicitMention,
                false,
                evidence.directed_at_another_party(),
            ),
            ProjectEffect::EnrolAndWake,
        );
    }

    /// The demonstrated live failure: an approved human hands the work to a
    /// different agent, and this agent — enrolled long ago, and carried along
    /// in the copied `p` set ever since — takes it as its own turn.
    #[tokio::test]
    async fn an_active_root_ignores_a_comment_handed_to_another_agent() {
        let evidence =
            directed_evidence("@Hermes please take this one", &[AGENT_PK, THIRD_PARTY]).await;

        assert!(evidence.p_tag_present, "the inherited `p` is still there");
        assert!(
            evidence.directed_at_another_party(),
            "a named, p-tagged party that is not us is addressing somebody else"
        );
        assert_eq!(
            classify_project_event(
                classify_kind(KIND_TEXT_NOTE),
                ProjectAuthor::AuthorisedHuman,
                CallMarker::None,
                RootState::Active,
                Addressing::InheritedParticipant,
                false,
                evidence.directed_at_another_party(),
            ),
            ProjectEffect::Ignore,
            "an enrolled agent must not wake on a comment aimed at another agent"
        );
    }

    /// Target-only: a bare follow-up names nobody, so it wakes nobody.
    ///
    /// This is the deliberate cost of the policy, asserted rather than left
    /// implicit. "Names nobody" and "names somebody else" stay distinguishable —
    /// `directed_at_another_party` is still false here — but on a shared root
    /// they now reach the same effect, because an inherited `p` is the only
    /// thing a bare follow-up offers and propagation is not intent.
    #[tokio::test]
    async fn an_active_root_does_not_wake_on_a_bare_follow_up() {
        let evidence = directed_evidence("yes, go ahead with that", &[AGENT_PK]).await;

        assert!(
            !evidence.directed_at_another_party(),
            "naming nobody is not naming somebody else"
        );
        assert_eq!(
            classify_project_event(
                classify_kind(KIND_TEXT_NOTE),
                ProjectAuthor::AuthorisedHuman,
                CallMarker::None,
                RootState::Active,
                Addressing::InheritedParticipant,
                false,
                evidence.directed_at_another_party(),
            ),
            ProjectEffect::Ignore,
            "an inherited `p` is propagation, not a request for a turn"
        );
    }

    /// …and the same root wakes the moment the comment names this agent.
    ///
    /// The pair matters more than either half: it is the difference between
    /// "target-only" and "deaf".
    #[tokio::test]
    async fn an_active_root_wakes_when_the_follow_up_names_this_agent() {
        let evidence = directed_evidence("@Claude yes, go ahead with that", &[AGENT_PK]).await;

        assert!(
            !evidence.directed_at_another_party(),
            "it names us, so it is not handed elsewhere"
        );
        assert_eq!(
            classify_project_event(
                classify_kind(KIND_TEXT_NOTE),
                ProjectAuthor::AuthorisedHuman,
                CallMarker::None,
                RootState::Active,
                Addressing::ExplicitMention,
                false,
                evidence.directed_at_another_party(),
            ),
            ProjectEffect::Wake,
            "an addressed follow-up is this agent's turn"
        );
    }

    /// A comment is ours the moment it names us, whoever else it also names.
    #[tokio::test]
    async fn being_named_alongside_another_agent_is_still_our_turn() {
        let evidence = directed_evidence(
            "@Hermes and @Claude — please compare notes",
            &[AGENT_PK, THIRD_PARTY],
        )
        .await;

        assert!(
            !evidence.directed_at_another_party(),
            "our display name is present, so this is not somebody else's comment"
        );
    }

    /// The display name is admitted only against a real `p`. Prose naming a
    /// party the event never addressed must not silence a turn.
    #[tokio::test]
    async fn a_named_party_with_no_matching_p_tag_does_not_silence_a_turn() {
        let evidence = directed_evidence("@Hermes wrote this originally", &[AGENT_PK]).await;

        assert!(
            !evidence.directed_at_another_party(),
            "without another p-tagged key the name addresses nobody"
        );
    }

    /// A display name is not *key* mention syntax, and on its own it addresses
    /// nobody: without a matching `p` behind it, it cannot enrol an unknown
    /// root or reanimate a dormant one.
    ///
    /// The conjunction — name plus this agent's own `p` — is explicit, and
    /// `a_named_agent_with_its_own_p_tag_is_explicitly_addressed` covers that
    /// side. What is asserted here is that the name alone buys nothing.
    #[tokio::test]
    async fn a_display_name_still_grants_no_enrolment_authority() {
        let evidence = directed_evidence("@Claude please look", &[AGENT_PK]).await;

        assert!(
            !evidence.visible_mention,
            "a display name is neither unique nor owned and must not read as key mention syntax"
        );
        for state in [RootState::Unknown, RootState::Dormant] {
            assert_eq!(
                classify_project_event(
                    classify_kind(KIND_TEXT_NOTE),
                    ProjectAuthor::AuthorisedHuman,
                    CallMarker::None,
                    state,
                    Addressing::InheritedParticipant,
                    false,
                    evidence.directed_at_another_party(),
                ),
                ProjectEffect::Ignore,
                "{state:?} must still refuse a display-name-only comment"
            );
        }
    }

    /// **The live Phase 3d failure, at the layer that produced it.**
    ///
    /// Comment `74f92354…` on root `d2986fa7…` was addressed to
    /// `@hermes-gateway` and carried both parties' `p` tags. The root had been
    /// correctly ignored twenty-five seconds earlier, so it was `Unknown` — and
    /// `Unknown` was the one row the guard did not cover. The hyphenated handle
    /// is the fixture's whole point: it is what a display handle looks like,
    /// and a token grammar that stopped at the `-` would read the address as
    /// `@hermes`, which is a different question than the one being asked here.
    ///
    /// `Addressing::ExplicitMention` is supplied deliberately — the strongest
    /// addressing any route could hand this comment, whether from a fresh `p`
    /// with complete history or, before this change, from the comment-first
    /// promotion. Naming somebody else has to beat all of it, in every state.
    #[tokio::test]
    async fn a_comment_addressed_to_a_hyphenated_handle_is_nobody_elses_turn() {
        let evidence = directed_evidence(
            "@hermes-gateway please pick this up",
            &[AGENT_PK, THIRD_PARTY],
        )
        .await;

        assert!(
            evidence.p_tag_present,
            "the agent's own `p` is on the comment — that is the whole trap"
        );
        assert!(
            evidence.directed_at_another_party(),
            "a hyphenated handle is a mention token, and it is not our name"
        );
        for state in [RootState::Unknown, RootState::Active, RootState::Dormant] {
            assert_eq!(
                classify_project_event(
                    classify_kind(KIND_TEXT_NOTE),
                    ProjectAuthor::AuthorisedHuman,
                    CallMarker::None,
                    state,
                    Addressing::ExplicitMention,
                    false,
                    evidence.directed_at_another_party(),
                ),
                ProjectEffect::Ignore,
                "{state:?} woke on a comment handed to another agent"
            );
        }
    }

    /// A handle this agent's name is only the *start* of is not this agent.
    ///
    /// `@claude-bot` used to match the display name `Claude`: the token grammar
    /// ended at the hyphen, so the trailing boundary check passed and
    /// `named_self` was true. The comment then read as *ours*, which is worse
    /// than the failure it sits beside — it does not merely fail to suppress a
    /// turn, it manufactures one, and it does so on the comment that names the
    /// agent the work was actually for.
    #[tokio::test]
    async fn a_handle_that_only_starts_with_our_name_is_not_our_mention() {
        let evidence =
            directed_evidence("@claude-bot please pick this up", &[AGENT_PK, THIRD_PARTY]).await;

        assert!(
            !evidence.named_self,
            "`@claude-bot` names claude-bot, not Claude"
        );
        assert!(
            evidence.directed_at_another_party(),
            "and it names a p-tagged party that is not us"
        );
        assert_eq!(
            resolve_addressing(
                &ProjectSubscription::Enrolment,
                &evidence,
                &RootHistoryReadiness::Unknown,
                None,
                &named_identity(),
            ),
            Some(Addressing::InheritedParticipant),
            "the only claim left on this agent is a tag it was carried along in"
        );
    }

    /// …and the agent whose handle it is still finds itself in it.
    ///
    /// The negative above is only worth having if the hyphenated handle is a
    /// real address for somebody: an agent configured as `hermes-gateway` has
    /// to be addressable by the name people write.
    #[tokio::test]
    async fn an_agent_whose_handle_is_hyphenated_is_addressed_by_it() {
        let keys = Keys::generate();
        let event = EventBuilder::new(
            Kind::Custom(KIND_TEXT_NOTE as u16),
            "@hermes-gateway please pick this up",
        )
        .tags([nostr::Tag::parse(tag(&["p", AGENT_PK])).unwrap()])
        .sign_with_keys(&keys)
        .expect("sign");
        let verified = VerifiedProjectEvent::verify(event).await.expect("valid");
        let hyphenated = agent_identity().with_display_name("hermes-gateway");
        let evidence = AddressingEvidence::resolve(&verified, &hyphenated);

        assert!(evidence.named_self, "it is written exactly as configured");
        assert!(
            !evidence.directed_at_another_party(),
            "the comment names this agent, so it is not handed elsewhere"
        );
        assert_eq!(
            resolve_addressing(
                &ProjectSubscription::Enrolment,
                &evidence,
                &RootHistoryReadiness::Unknown,
                None,
                &hyphenated,
            ),
            Some(Addressing::ExplicitMention),
        );
    }

    /// A NIP-PC invocation is addressed by its envelope, not by its prose.
    ///
    /// The guard widened to every root state, so this is the case that must not
    /// have widened with it: a caller's covering note may name anybody — the
    /// human it is doing this on behalf of, the agent it is handing off from —
    /// and the envelope still names this agent as its callee.
    #[test]
    fn a_peer_call_still_enrols_when_its_prose_names_somebody_else() {
        for state in [RootState::Unknown, RootState::Active, RootState::Dormant] {
            assert_eq!(
                classify_project_event(
                    classify_kind(KIND_TEXT_NOTE),
                    ProjectAuthor::TrustedAgent,
                    CallMarker::Invocation,
                    state,
                    Addressing::InheritedParticipant,
                    false,
                    true,
                ),
                match state {
                    RootState::Active => ProjectEffect::Wake,
                    _ => ProjectEffect::EnrolAndWake,
                },
                "{state:?} refused a call whose envelope named this agent"
            );
        }
    }

    /// An agent with no configured name cannot tell "excluded" from "not
    /// mentioned", so it must not read the comment as excluding it — but under
    /// the target-only rule that no longer buys it a turn either. Both halves
    /// are asserted: the evidence stays honest, and the effect is still nothing.
    #[tokio::test]
    async fn an_unnamed_agent_does_not_infer_exclusion_and_still_does_not_wake() {
        let keys = Keys::generate();
        let event = EventBuilder::new(
            Kind::Custom(KIND_TEXT_NOTE as u16),
            "@Hermes please take this one",
        )
        .tags([
            nostr::Tag::parse(tag(&["p", AGENT_PK])).unwrap(),
            nostr::Tag::parse(tag(&["p", THIRD_PARTY])).unwrap(),
        ])
        .sign_with_keys(&keys)
        .expect("sign");
        let verified = VerifiedProjectEvent::verify(event).await.expect("valid");

        // Unnamed: absence of its own name proves nothing, so the agent must
        // not read the comment as excluding it.
        let evidence = AddressingEvidence::resolve(&verified, &agent_identity());
        assert!(
            !evidence.directed_at_another_party(),
            "an agent that cannot recognise its own name must not infer it was excluded"
        );
        assert_eq!(
            classify_project_event(
                classify_kind(KIND_TEXT_NOTE),
                ProjectAuthor::AuthorisedHuman,
                CallMarker::None,
                RootState::Active,
                Addressing::InheritedParticipant,
                false,
                evidence.directed_at_another_party(),
            ),
            ProjectEffect::Ignore,
            "not inferring exclusion is not the same as being addressed"
        );
    }

    /// Addressing resolution for an agent that knows the name it is called by.
    async fn named_addressing(
        content: &str,
        p_tags: &[&str],
        readiness: &RootHistoryReadiness,
        facts: Option<&PriorRootFacts>,
    ) -> Option<Addressing> {
        let evidence = directed_evidence(content, p_tags).await;
        resolve_addressing(&watched(), &evidence, readiness, facts, &named_identity())
    }

    /// The other half of the addressing failure, and the ordinary case.
    ///
    /// Desktop writes a mention as the visible display name plus a `p` tag — it
    /// never puts hex or an npub in the body. Read only as `visible_mention`,
    /// a mention a person actually typed was invisible, so `@Claude` on a
    /// dormant root resolved to `InheritedParticipant` and left it dormant.
    #[tokio::test]
    async fn a_named_agent_with_its_own_p_tag_is_explicitly_addressed() {
        assert_eq!(
            named_addressing(
                "@Claude could you pick this up again?",
                &[AGENT_PK],
                // Incomplete history is the point: this is exactly the state in
                // which a bare `p` is *not* trusted as explicit, so the mention
                // is doing the work here and nothing else could be.
                &RootHistoryReadiness::Unknown,
                None,
            )
            .await,
            Some(Addressing::ExplicitMention),
        );

        // And it reaches the effect the contract asks for, from both the states
        // that a bare inherited `p` must never move.
        for state in [RootState::Unknown, RootState::Dormant] {
            assert_eq!(
                classify_project_event(
                    classify_kind(KIND_TEXT_NOTE),
                    ProjectAuthor::AuthorisedHuman,
                    CallMarker::None,
                    state,
                    Addressing::ExplicitMention,
                    false,
                    false,
                ),
                ProjectEffect::EnrolAndWake,
                "{state:?} must answer a genuine mention"
            );
        }
    }

    /// The negative the contract names alongside it: a bare display name in
    /// prose, carried by a `p` that was only inherited, is not an address.
    #[tokio::test]
    async fn a_bare_name_in_prose_with_an_inherited_p_is_not_explicit() {
        assert_eq!(
            named_addressing(
                "Claude looked at this last week",
                &[AGENT_PK],
                &RootHistoryReadiness::Unknown,
                None,
            )
            .await,
            Some(Addressing::InheritedParticipant),
            "no `@`, so no mention token — the `p` is all there is"
        );
    }

    /// A name with no `p` behind it still addresses nobody the relay agreed to
    /// deliver to, so it cannot enrol.
    #[tokio::test]
    async fn a_mention_without_a_matching_p_cannot_enrol() {
        assert_eq!(
            named_addressing(
                "@Claude please look",
                &[THIRD_PARTY],
                &RootHistoryReadiness::Complete,
                Some(&prior(false, THIRD_PARTY, OWNER)),
            )
            .await,
            Some(Addressing::WatchedRoot),
            "the `p_tag_present` gate is upstream of the mention check"
        );
    }

    #[test]
    fn a_mention_token_needs_something_after_the_prefix() {
        assert!(mention_token_present("@Hermes please"));
        assert!(mention_token_present("nostr:npub1abc"));
        assert!(!mention_token_present("email me at foo@example.com"));
        assert!(!mention_token_present("a bare @ sign"));
        assert!(!mention_token_present("no names at all"));
        // A handle written with a hyphen is a name somebody was called by.
        assert!(mention_token_present("@hermes-gateway please"));
        // A `-` continues a handle but cannot open one.
        assert!(!mention_token_present("wrapped in @-@ dashes"));
        // The local part of an address is still not a mention, hyphens or not.
        assert!(!mention_token_present("write to first-last@example.com"));
    }

    /// The token grammar, at the boundary the hyphen moved.
    ///
    /// Every case here is one identity against one body, so a failure names the
    /// rule that broke rather than a scenario that happened to depend on it.
    #[test]
    fn a_hyphenated_handle_is_one_whole_mention_token() {
        // The defect: `Claude` matched the front of somebody else's handle,
        // because the token was taken to end at the `-`.
        assert!(!explicit_mention_present("@claude-bot please", "Claude"));
        assert!(!explicit_mention_present("@claude-bot please", "claude-bo"));
        // …and the whole handle still matches itself, wherever it sits.
        assert!(explicit_mention_present(
            "@hermes-gateway please",
            "hermes-gateway"
        ));
        assert!(explicit_mention_present(
            "ping @hermes-gateway.",
            "hermes-gateway"
        ));
        assert!(explicit_mention_present(
            "(@hermes-gateway)",
            "hermes-gateway"
        ));
        // A longer handle is a different handle, at either end.
        assert!(!explicit_mention_present(
            "@hermes-gateway-2 please",
            "hermes-gateway"
        ));
        assert!(!explicit_mention_present(
            "@relay-hermes-gateway",
            "hermes-gateway"
        ));
        // The leading boundary is unchanged: an `@` inside a word addresses
        // nobody, which is what keeps an ordinary email address out.
        assert!(!explicit_mention_present(
            "mail ops@claude-bot.example",
            "claude-bot"
        ));
        assert!(!explicit_mention_present(
            "mail hermes@example.com",
            "example"
        ));
    }

    /// The other side of that boundary: a hyphen that joins nothing is prose.
    ///
    /// `mention_char_at` gives the dash to the token only when there is a name
    /// on the far side of it. Every positive here is a mention somebody really
    /// typed, and swallowing the dash would silence the agent on it; every
    /// negative is a dash that does lead into another name — or one that opens
    /// nothing at all — which is the direction that manufactures a turn. Runs
    /// are judged whole, so `--` cannot buy what `-` cannot.
    #[test]
    fn a_hyphen_that_joins_nothing_is_not_part_of_the_handle() {
        // Trailing: the token ends and the dash leads nowhere.
        assert!(explicit_mention_present("@claude- please", "Claude"));
        assert!(explicit_mention_present("@claude-", "Claude"));
        // Hyphen as prose punctuation, one space clear of the name.
        assert!(explicit_mention_present("@Claude - please do X", "Claude"));
        // Doubled, and still leading nowhere.
        assert!(explicit_mention_present("@claude-- please", "Claude"));
        // …but a run that *does* join is one token, so a doubled dash cannot
        // smuggle a prefix match past the rule the single dash is subject to.
        assert!(!explicit_mention_present("@claude--bot please", "Claude"));
        assert!(!explicit_mention_present("@claude-bot please", "Claude"));
        // A handle keeps its own hyphens and is still terminated by a dangling
        // one — the rule is about the dash's neighbours, not its position.
        assert!(explicit_mention_present(
            "@hermes-gateway- ok",
            "hermes-gateway"
        ));
        // Leading: a hyphen cannot open a token, so `@-claude` names nobody at
        // all — not this agent, and not anyone for `named_anyone` to defer to.
        assert!(!mention_token_present("@-claude please"));
        assert!(!explicit_mention_present("@-claude please", "claude"));
        // And a bullet is not a handle: the dash before the `@` joins nothing,
        // so a `- @Claude` list item still reaches this agent.
        assert!(mention_token_present("- @Claude please"));
        assert!(explicit_mention_present("- @Claude please", "Claude"));
        // A dangling dash still names *somebody*, which is what keeps
        // "names nobody" and "names someone else" apart upstream.
        assert!(mention_token_present("@claude- please"));
    }

    /// This agent's npub. Asserted against the derived form in
    /// `the_test_npub_matches_the_derived_identity`, so a stale literal cannot
    /// quietly make every mention test vacuous — the same failure mode as the
    /// `STRANGER` collision earlier in this module.
    const AGENT_NPUB: &str = "npub1yg4evk8quj29e099rlag6dj2z79q9c6f67vy06fg9ehwzvr2qr8qw26j58";

    #[test]
    fn the_test_npub_matches_the_derived_identity() {
        // Without this, a stale literal would make every npub mention test
        // silently assert nothing.
        assert_eq!(agent_identity().npub(), AGENT_NPUB);
        assert_eq!(agent_identity().hex(), AGENT_PK);
    }

    #[tokio::test]
    async fn accepted_mention_forms_are_recognised() {
        for content in [
            format!("nostr:{AGENT_NPUB} please look"),
            format!("@{AGENT_NPUB} please look"),
            format!("nostr:{AGENT_PK} please look"),
            format!("@{AGENT_PK} please look"),
            format!("(see nostr:{AGENT_NPUB})"),
            format!("ping @{AGENT_PK}."),
        ] {
            assert!(
                evidence_for(&content).await.visible_mention,
                "should recognise: {content}"
            );
        }
    }

    #[tokio::test]
    async fn bare_identity_in_prose_or_payload_is_not_a_mention() {
        // The defect this replaces: an authorised human quoting a key in
        // diagnostics could reactivate a dormant agent whose inherited `p` was
        // already present.
        for content in [
            format!("the pubkey is {AGENT_PK}"),
            format!("{{\"pubkey\":\"{AGENT_PK}\"}}"),
            format!("see {AGENT_NPUB} in the logs"),
            format!("author={AGENT_PK}"),
        ] {
            assert!(
                !evidence_for(&content).await.visible_mention,
                "should NOT recognise: {content}"
            );
        }
    }

    #[tokio::test]
    async fn an_identity_inside_a_longer_token_is_not_a_mention() {
        // The defect the previous continuation rule missed: `g` is not a hex
        // digit, but `@<hex>garbage` is plainly still one token. Termination is
        // lexical, not alphabet-specific.
        for content in [
            format!("@{AGENT_PK}garbage"),
            format!("nostr:{AGENT_PK}suffix"),
            format!("@{AGENT_PK}ab"),
            format!("@{AGENT_NPUB}qqqq"),
            format!("nostr:{AGENT_NPUB}x7"),
            format!("@{AGENT_PK}_1"),
        ] {
            assert!(
                !evidence_for(&content).await.visible_mention,
                "should NOT recognise: {content}"
            );
        }
    }

    /// The hyphen rule reaches key mentions too, because it is one lexer.
    ///
    /// `explicit_mention_present` is asked about the display name *and* about
    /// hex and npub, so the grammar has to be right for all three. Neither key
    /// alphabet contains a `-`, so for a key the hyphen only ever decides an
    /// edge: `@<key>-2` is somebody else's token and must not read as this
    /// agent, while a dash a person typed after the key must not lose the
    /// mention. Same rule, same reasons as the display-name case.
    #[tokio::test]
    async fn the_hyphen_rule_reaches_key_mentions_too() {
        for content in [
            format!("@{AGENT_PK}-2"),
            format!("nostr:{AGENT_NPUB}-suffix"),
        ] {
            assert!(
                !evidence_for(&content).await.visible_mention,
                "should NOT recognise: {content}"
            );
        }
        for content in [
            format!("nostr:{AGENT_NPUB} - please look"),
            format!("@{AGENT_PK}- please look"),
            format!("- nostr:{AGENT_NPUB} please look"),
        ] {
            assert!(
                evidence_for(&content).await.visible_mention,
                "should recognise: {content}"
            );
        }
    }

    #[tokio::test]
    async fn a_prefix_inside_a_larger_token_is_not_a_mention() {
        // The leading boundary. Finding `@` or `nostr:` immediately before the
        // identity is not enough — the prefix itself must start a token.
        for content in [
            format!("prefix@{AGENT_PK}"),
            format!("xnostr:{AGENT_NPUB}"),
            format!("email_address@{AGENT_PK}"),
        ] {
            assert!(
                !evidence_for(&content).await.visible_mention,
                "should NOT recognise: {content}"
            );
        }
    }

    #[tokio::test]
    async fn punctuation_and_whitespace_terminate_a_mention() {
        for content in [
            format!("(@{AGENT_PK})"),
            format!("see nostr:{AGENT_NPUB}, please"),
            format!("nostr:{AGENT_NPUB}."),
            format!("[@{AGENT_PK}]"),
            format!("nostr:{AGENT_NPUB}"),
        ] {
            assert!(
                evidence_for(&content).await.visible_mention,
                "should recognise: {content}"
            );
        }
    }

    #[tokio::test]
    async fn a_display_name_in_content_is_not_a_visible_mention() {
        // `content.contains("@Claude")` would let any author invoke any agent
        // by typing its display name. Display names are neither unique nor
        // owned.
        let evidence = evidence_for("hey @Claude look").await;
        assert!(evidence.p_tag_present);
        assert!(!evidence.visible_mention);
    }

    #[tokio::test]
    async fn a_genuine_mention_after_prose_is_still_found() {
        // Scanning must not stop at the first non-qualifying occurrence.
        let content = format!("logs show {AGENT_PK} — anyway, nostr:{AGENT_NPUB} please look");
        assert!(evidence_for(&content).await.visible_mention);
    }

    #[tokio::test]
    async fn a_mention_without_a_matching_p_tag_is_not_addressing() {
        // Bound together: naming an agent the event did not address is not an
        // invocation. Resolution happens through `resolve_addressing`.
        let keys = Keys::generate();
        let event = EventBuilder::new(
            Kind::Custom(KIND_TEXT_NOTE as u16),
            format!("nostr:{AGENT_NPUB} look"),
        )
        .tags([nostr::Tag::parse(tag(&["p", THIRD_PARTY])).unwrap()])
        .sign_with_keys(&keys)
        .expect("sign");
        let verified = VerifiedProjectEvent::verify(event).await.expect("valid");
        let evidence = AddressingEvidence::resolve(&verified, &agent_identity());

        assert!(evidence.visible_mention);
        assert!(!evidence.p_tag_present);
        assert_eq!(
            resolve_addressing(
                &watched(),
                &evidence,
                &RootHistoryReadiness::Complete,
                Some(&prior(false, THIRD_PARTY, OWNER)),
                &agent_identity(),
            ),
            Some(Addressing::WatchedRoot)
        );
    }

    // ── Seeding requires a validated binding ─────────────────────────────────

    #[tokio::test]
    async fn a_root_cannot_be_bound_to_a_repository_it_does_not_claim() {
        // The exact former counterexample: repository A is discovered, the
        // signed root claims repository B. Under the old decomposed signature a
        // caller could reuse the root's genuine id and kind while supplying
        // tags naming A. There is now no API path that does that — the
        // validator and `VerifiedBoundRoot` both read the `a` tag from the same
        // witness they are validating.
        let discovered_a = coord();
        let claimed_b = format!("30617:{THIRD_PARTY}:their-repo");

        let keys = Keys::generate();
        let root = signed(&keys, KIND_GIT_ISSUE, vec![tag(&["a", &claimed_b])]);
        let root = VerifiedProjectEvent::verify(root).await.expect("valid");

        let only_a = known(&[&discovered_a]);
        assert!(
            validate_enrolment_candidate(&root, &only_a).is_none(),
            "a root claiming an undiscovered repository does not validate"
        );
        assert!(
            VerifiedBoundRoot::prove(std::slice::from_ref(&root), &only_a).is_none(),
            "and cannot be bound to the discovered one either"
        );

        // Discovering B does let it through — the refusal is about the claim
        // matching the discovered set, not about rejecting everything.
        let both = known(&[&discovered_a, &claimed_b]);
        let bound = VerifiedBoundRoot::prove(std::slice::from_ref(&root), &both)
            .expect("a root claiming a discovered repository binds");
        assert_eq!(bound.binding().coordinate(), claimed_b);
        assert_ne!(bound.binding().coordinate(), discovered_a);
    }

    #[tokio::test]
    async fn proving_a_root_refuses_a_non_root_kind() {
        let keys = Keys::generate();
        let comment = signed(&keys, KIND_TEXT_NOTE, vec![tag(&["a", &coord()])]);
        let comment = VerifiedProjectEvent::verify(comment).await.expect("valid");
        assert!(
            VerifiedBoundRoot::prove(std::slice::from_ref(&comment), &known(&[&coord()])).is_none()
        );
    }

    #[tokio::test]
    async fn proving_a_root_refuses_zero_or_conflicting_candidates() {
        // The false-complete this closes: an empty result satisfying an
        // exhaustion test while proving nothing exists.
        let keys = Keys::generate();
        let root = signed(&keys, KIND_GIT_ISSUE, vec![tag(&["a", &coord()])]);
        let root = VerifiedProjectEvent::verify(root).await.expect("valid");
        let d = known(&[&coord()]);

        assert!(VerifiedBoundRoot::prove(&[], &d).is_none(), "zero roots");
        assert!(
            VerifiedBoundRoot::prove(&[root.clone(), root.clone()], &d).is_none(),
            "conflicting rows"
        );
        assert!(VerifiedBoundRoot::prove(std::slice::from_ref(&root), &d).is_some());
    }

    #[tokio::test]
    async fn seeding_takes_the_owner_from_the_proven_binding() {
        let keys = Keys::generate();
        let root = signed(&keys, KIND_GIT_ISSUE, vec![tag(&["a", &coord()])]);
        let root = VerifiedProjectEvent::verify(root).await.expect("valid");
        let bound = VerifiedBoundRoot::prove(std::slice::from_ref(&root), &known(&[&coord()]))
            .expect("proves");

        let facts = PriorRootFacts::seed(&bound);
        assert_eq!(facts.repository_owner(), OWNER);
        assert_eq!(facts.root_author(), keys.public_key().to_hex());
        assert!(!facts.agent_was_participant());
    }

    // ── Root to repository binding is immutable ──────────────────────────────

    #[test]
    fn active_root_same_binding_is_unchanged() {
        let mut e = ProjectEnrolments::new();
        e.enrol(&candidate(ROOT, false)).unwrap();
        assert_eq!(
            e.enrol(&candidate(ROOT, false)),
            Ok(EnrolOutcome::Unchanged)
        );
        assert_eq!(e.state_of(ROOT), RootState::Active);
    }

    #[test]
    fn active_root_rejects_a_different_coordinate_and_keeps_the_old_binding() {
        let mut e = ProjectEnrolments::new();
        e.enrol(&candidate(ROOT, false)).unwrap();
        let other = format!("30617:{STRANGER}:elsewhere");

        let err = e
            .enrol(&candidate_at(ROOT, &other, false))
            .expect_err("a root must not move between repositories");
        assert_eq!(err.existing.coordinate, coord());
        assert_eq!(err.attempted.coordinate, other);

        assert_eq!(e.state_of(ROOT), RootState::Active);
        assert_eq!(
            e.get(ROOT).unwrap().coordinate,
            coord(),
            "old binding retained"
        );
    }

    #[test]
    fn active_root_rejects_a_class_flip() {
        let mut e = ProjectEnrolments::new();
        e.enrol(&candidate(ROOT, false)).unwrap();
        assert!(e.enrol(&candidate(ROOT, true)).is_err());
        assert!(!e.get(ROOT).unwrap().is_pull_request);
    }

    #[test]
    fn dormant_root_same_binding_reactivates() {
        let mut e = ProjectEnrolments::new();
        e.enrol(&candidate(ROOT, false)).unwrap();
        e.close(ROOT);
        assert_eq!(
            e.enrol(&candidate(ROOT, false)),
            Ok(EnrolOutcome::Reactivated)
        );
        assert_eq!(e.state_of(ROOT), RootState::Active);
    }

    #[test]
    fn dormant_root_rejects_a_different_binding_and_stays_dormant() {
        // The sharper of the two mismatch paths: silently overwriting here
        // would both relocate the watch and revive it.
        let mut e = ProjectEnrolments::new();
        e.enrol(&candidate(ROOT, false)).unwrap();
        e.close(ROOT);
        let other = format!("30617:{STRANGER}:elsewhere");

        assert!(e.enrol(&candidate_at(ROOT, &other, false)).is_err());
        assert_eq!(e.state_of(ROOT), RootState::Dormant);
        assert_eq!(e.get(ROOT).unwrap().coordinate, coord());

        assert!(e.enrol(&candidate(ROOT, true)).is_err());
        assert_eq!(e.state_of(ROOT), RootState::Dormant);
        assert!(!e.get(ROOT).unwrap().is_pull_request);
    }

    #[test]
    fn only_enrol_and_reactivate_require_a_new_req() {
        assert!(EnrolOutcome::Enrolled.changes_subscription());
        assert!(EnrolOutcome::Reactivated.changes_subscription());
        assert!(!EnrolOutcome::Unchanged.changes_subscription());
    }

    // ── Subscription filters ─────────────────────────────────────────────────

    const AGENT: &str = "222b9658e0e4945cbca51ffa8d364a178a02e349d79847e9282e6ee1306a00ce";

    #[test]
    fn enrolment_filter_scopes_by_project_and_agent() {
        let f = enrolment_filter(&known(&[&coord()]), AGENT, 2_000).expect("filter");
        assert_eq!(f["kinds"], json!([1621, 1618, 1]));
        assert_eq!(f["#a"], json!([coord()]));
        assert_eq!(f["#p"], json!([AGENT]));
    }

    /// The enrolment REQ is a live tail that reaches back exactly one accepted
    /// skew interval, and asks for no history beyond it.
    ///
    /// It used to reach thirty days behind the floor for up to five hundred
    /// rows. Both bounds were false completeness — a root older than the window,
    /// or beyond the five hundredth, was silently absent while the agent
    /// reported full authority — and neither could be fixed by widening, because
    /// this REQ's identity is fixed and a fixed identity cannot paginate. The
    /// walk that can is [`EnrolmentReconstruction`], on its own requests, and it
    /// runs *beside* this one rather than replacing it.
    ///
    /// Then it took the caller's floor literally, which was the opposite error
    /// and a worse one, because it was invisible. A relay matches `since`
    /// against the author's signed `created_at`; the ingest gate accepts
    /// [`ACCEPTED_CLOCK_SKEW_SECS`] of drift in either direction. So a root
    /// accepted with `200 OK`, stored, and readable by `buzz issues get` was
    /// filtered out of the very subscription that existed to deliver it. On a
    /// real relay a `1621` addressed to two agents, stamped 387 seconds before
    /// their startup, reached neither.
    ///
    /// The floor is therefore the caller's minus the exact interval the relay
    /// will accept — not a round number that feels generous. A tail that reaches
    /// back further than the relay will ever accept an event from is asking for
    /// context it did not need; one that reaches back less is deaf, silently,
    /// for the difference.
    #[test]
    fn the_enrolment_tail_covers_the_relays_accepted_skew() {
        let f = enrolment_filter(&known(&[&coord()]), AGENT, 9_000).expect("filter");
        assert_eq!(
            f["since"],
            json!(9_000 - ACCEPTED_CLOCK_SKEW_SECS),
            "a root the relay would accept must not be filtered out before it arrives"
        );
        assert_eq!(
            ACCEPTED_CLOCK_SKEW_SECS, 900,
            "this is MAX_TIMESTAMP_DRIFT_SECS in buzz-relay's ingest path; if that \
             moved, this must move with it or the tail goes partly deaf again"
        );
        assert!(
            f.get("limit").is_none(),
            "a row cap on a standing tail can only ever truncate it: {f}"
        );
        assert!(
            f.get("until").is_none(),
            "the tail is open-ended forwards: {f}"
        );
    }

    /// The reach-back saturates rather than wrapping.
    ///
    /// A watermark inside the skew interval is ordinary on a freshly
    /// provisioned relay, and `0 - 900` on a `u64` is the largest number there
    /// is — which would turn the tail's floor into a ceiling and match nothing
    /// at all.
    #[test]
    fn a_startup_inside_the_skew_interval_floors_at_zero() {
        for watermark in [0u64, 1, ACCEPTED_CLOCK_SKEW_SECS - 1] {
            let f = enrolment_filter(&known(&[&coord()]), AGENT, watermark).expect("filter");
            assert_eq!(
                f["since"],
                json!(0),
                "watermark {watermark} must floor at 0"
            );
        }
    }

    #[test]
    fn enrolment_filter_is_none_without_known_projects() {
        // An empty `#a` matches nothing at some relays and everything at
        // others. Sending no REQ is the only safe reading.
        assert!(enrolment_filter(&DiscoveredRepositories::new(), AGENT, 100).is_none());
    }

    #[test]
    fn watched_roots_use_lowercase_e_and_pr_updates_uppercase_e() {
        let mut e = ProjectEnrolments::new();
        e.enrol(&candidate(ROOT, false)).unwrap();
        e.enrol(&candidate(OTHER_ROOT, true)).unwrap();

        let filters = watched_roots_filters(&e, 100);
        assert_eq!(filters.len(), 2);

        // `43001`/`43004` are the NIP-PC call and result. Without them in this
        // list the live watched REQ never delivers a peer call on an enrolled
        // root, so the whole project half of Phase 1b would be unreachable
        // while every unit test around it still passed.
        assert_eq!(
            filters[0]["kinds"],
            json!([1, 1630, 1631, 1632, 1633, 43001, 43004])
        );
        assert_eq!(filters[0]["#e"], json!([ROOT, OTHER_ROOT]));
        assert!(filters[0].get("#E").is_none());

        // The bug this shape exists to prevent: a lowercase-only filter misses
        // every PR revision.
        assert_eq!(filters[1]["kinds"], json!([1619]));
        assert_eq!(filters[1]["#E"], json!([OTHER_ROOT]));
        assert!(filters[1].get("#e").is_none());
    }

    #[test]
    fn watched_roots_include_dormant_roots() {
        let mut e = ProjectEnrolments::new();
        e.enrol(&candidate(ROOT, false)).unwrap();
        e.close(ROOT);
        let filters = watched_roots_filters(&e, 100);
        assert_eq!(filters[0]["#e"], json!([ROOT]));
    }

    #[test]
    fn watched_roots_filters_are_empty_when_nothing_is_enrolled() {
        assert!(watched_roots_filters(&ProjectEnrolments::new(), 100).is_empty());
    }

    // ── R1 gate, checked on the frames actually produced ─────────────────────

    #[test]
    fn flag_off_produces_no_project_req_at_all() {
        let mut e = ProjectEnrolments::new();
        e.enrol(&candidate(ROOT, true)).unwrap();
        let frames = project_req_frames(false, &known(&[&coord()]), &e, AGENT, 100);
        assert!(
            frames.is_empty(),
            "project routing disabled must issue no REQ even with coordinates and enrolments present, got: {frames:?}"
        );
    }

    #[test]
    fn flag_on_produces_exactly_the_enrolment_and_watched_root_reqs() {
        let mut e = ProjectEnrolments::new();
        e.enrol(&candidate(ROOT, false)).unwrap();
        e.enrol(&candidate(OTHER_ROOT, true)).unwrap();
        let frames = project_req_frames(true, &known(&[&coord()]), &e, AGENT, 100);

        assert_eq!(frames.len(), 2);
        assert_eq!(frames[0][0], json!("REQ"));
        assert_eq!(frames[0][1], json!(PROJECT_ENROL_SUB_ID));
        assert_eq!(frames[0][2]["#p"], json!([AGENT]));

        assert_eq!(frames[1][0], json!("REQ"));
        assert_eq!(frames[1][1], json!(PROJECT_ROOTS_SUB_ID));
        // Both filters ride in one REQ.
        assert_eq!(frames[1][2]["#e"], json!([ROOT, OTHER_ROOT]));
        assert_eq!(frames[1][3]["#E"], json!([OTHER_ROOT]));

        for frame in &frames {
            assert!(
                frame
                    .as_array()
                    .is_some_and(|f| f.iter().skip(2).all(|filter| filter.get("#h").is_none())),
                "a project REQ must carry no channel scope"
            );
        }
    }

    #[test]
    fn flag_on_with_nothing_discovered_produces_no_frames() {
        let frames = project_req_frames(
            true,
            &DiscoveredRepositories::new(),
            &ProjectEnrolments::new(),
            AGENT,
            100,
        );
        assert!(frames.is_empty());
    }

    // ── Recovered from 0e5f20ae ──────────────────────────────────────────────
    //
    // Restored verbatim from the approved commit rather than rewritten from
    // memory. Signatures that changed since are adapted below; the assertions
    // and fixtures are the originals.

    #[tokio::test]
    async fn a_caller_cannot_hand_the_validator_a_fabricated_repository_set() {
        // Private candidate fields close struct-literal forgery; this closes
        // validator-assisted forgery. With no production insertion method, a
        // freshly built `DiscoveredRepositories` admits nothing.
        let fabricated = format!("30617:{STRANGER}:looks-plausible");
        assert!(validate_enrolment_candidate(
            &verified_with(KIND_GIT_ISSUE, &tags(&[&["a", &fabricated]])).await,
            &DiscoveredRepositories::new(),
        )
        .is_none());
    }

    #[tokio::test]
    async fn a_discovered_issue_root_is_a_valid_candidate() {
        let event =
            verified_with(KIND_GIT_ISSUE, &tags(&[&["a", &coord()], &["p", STRANGER]])).await;
        let c = validate_enrolment_candidate(&event, &known(&[&coord()])).expect("should validate");
        // The root id comes from the event, not from a caller-chosen constant —
        // that substitution is what the witness-based signature removed.
        assert_eq!(c.root(), event.id());
        assert_eq!(c.coordinate(), coord());
        assert!(!c.is_pull_request());
    }

    #[tokio::test]
    async fn a_malformed_coordinate_poisons_a_later_valid_one() {
        // Two `a` tags is ambiguous regardless, but the malformed-first
        // ordering is the one where a sloppy scan would skip and accept.
        let k = known(&[&coord()]);
        assert!(validate_enrolment_candidate(
            &verified_with(KIND_GIT_ISSUE, &tags(&[&["a"], &["a", &coord()]])).await,
            &k,
        )
        .is_none());
        assert!(validate_enrolment_candidate(
            &verified_with(KIND_GIT_ISSUE, &tags(&[&["a", ""], &["a", &coord()]])).await,
            &k,
        )
        .is_none());
    }

    #[tokio::test]
    async fn a_pull_request_root_is_marked_as_one() {
        let c = validate_enrolment_candidate(
            &verified_with(KIND_GIT_PULL_REQUEST, &tags(&[&["a", &coord()]])).await,
            &known(&[&coord()]),
        )
        .expect("should validate");
        assert!(c.is_pull_request);
    }

    #[test]
    fn a_result_from_a_non_agent_author_is_a_forged_correlation() {
        assert_eq!(
            comment(
                ProjectAuthor::Untrusted,
                CallMarker::Result,
                RootState::Active,
                Addressing::ExplicitMention,
            ),
            ProjectEffect::UntrustedContext
        );
        assert_eq!(
            comment(
                ProjectAuthor::AuthorisedHuman,
                CallMarker::Result,
                RootState::Active,
                Addressing::ExplicitMention,
            ),
            ProjectEffect::Ignore
        );
    }

    /// A `CallMarker::Result` is only meaningful on the surface-native result
    /// kind — currently a trusted-agent kind-`1` comment. On any other class it
    /// must not promote the event into a call resumption, because that would
    /// route around the locked rules that `1630`-`1633` are lifecycle-only and
    /// `1619` is context-only.
    #[test]
    fn a_result_marker_never_resumes_a_lifecycle_event() {
        for authorised in [true, false] {
            let out = classify_project_event(
                KindEffect::Lifecycle,
                ProjectAuthor::TrustedAgent,
                CallMarker::Result,
                RootState::Active,
                Addressing::ExplicitMention,
                authorised,
                false,
            );
            assert_ne!(out, ProjectEffect::ResumeCall);
            assert_eq!(
                out,
                if authorised {
                    ProjectEffect::ApplyLifecycle
                } else {
                    ProjectEffect::Ignore
                }
            );
        }
    }

    #[test]
    fn a_result_marker_never_resumes_a_pr_update() {
        let out = classify_project_event(
            KindEffect::ContextRefresh,
            ProjectAuthor::TrustedAgent,
            CallMarker::Result,
            RootState::Active,
            Addressing::ExplicitMention,
            false,
            false,
        );
        assert_ne!(out, ProjectEffect::ResumeCall);
        assert_eq!(out, ProjectEffect::RefreshContext);
    }

    #[test]
    fn a_result_marker_never_resumes_a_root() {
        // A `1621`/`1618` root is never a call result; the marker is malformed.
        let out = classify_project_event(
            KindEffect::Root,
            ProjectAuthor::TrustedAgent,
            CallMarker::Result,
            RootState::Unknown,
            Addressing::ExplicitMention,
            false,
            false,
        );
        assert_ne!(out, ProjectEffect::ResumeCall);
        assert_eq!(out, ProjectEffect::Ignore);
    }

    #[test]
    fn a_result_marker_never_resumes_an_ignored_kind() {
        let out = classify_project_event(
            KindEffect::Ignore,
            ProjectAuthor::TrustedAgent,
            CallMarker::Result,
            RootState::Active,
            Addressing::ExplicitMention,
            true,
            false,
        );
        assert_ne!(out, ProjectEffect::ResumeCall);
        assert_eq!(out, ProjectEffect::Ignore);
    }

    #[test]
    fn a_root_enrols_only_on_an_explicit_mention() {
        assert_eq!(
            classify_project_event(
                KindEffect::Root,
                ProjectAuthor::AuthorisedHuman,
                CallMarker::None,
                RootState::Unknown,
                Addressing::ExplicitMention,
                false,
                false,
            ),
            ProjectEffect::EnrolAndWake
        );
        // Real case `48be1cc2…`: an issue with no `p` at all mentions nobody, so
        // it enrols nobody and wakes nobody — and does not error.
        assert_eq!(
            classify_project_event(
                KindEffect::Root,
                ProjectAuthor::AuthorisedHuman,
                CallMarker::None,
                RootState::Unknown,
                Addressing::WatchedRoot,
                false,
                false,
            ),
            ProjectEffect::Ignore
        );
        // Real cases `b1261034…` and `eb1803a2…`: an issue whose only `p` is the
        // repository owner's, stamped on by Desktop. It reaches this row now
        // that a root no longer takes an earlier exit to `ExplicitMention`, and
        // it must answer the same way — the tag is the client's structure, not
        // a person's request.
        assert_eq!(
            classify_project_event(
                KindEffect::Root,
                ProjectAuthor::AuthorisedHuman,
                CallMarker::None,
                RootState::Unknown,
                Addressing::InheritedParticipant,
                false,
                false,
            ),
            ProjectEffect::Ignore
        );
    }

    #[tokio::test]
    async fn a_root_with_two_a_tags_is_ambiguous_not_first_wins() {
        // A forged root could otherwise smuggle a known coordinate past the
        // gate while a second tag says something else entirely.
        let k = known(&[&coord()]);
        let other = format!("30617:{STRANGER}:elsewhere");
        assert!(validate_enrolment_candidate(
            &verified_with(KIND_GIT_ISSUE, &tags(&[&["a", &coord()], &["a", &other]])).await,
            &k,
        )
        .is_none());
        // Even two identical tags: ambiguity is about shape, not values.
        assert!(validate_enrolment_candidate(
            &verified_with(KIND_GIT_ISSUE, &tags(&[&["a", &coord()], &["a", &coord()]])).await,
            &k,
        )
        .is_none());
    }

    #[tokio::test]
    async fn a_value_less_or_empty_coordinate_is_rejected() {
        let k = known(&[&coord()]);
        // `["a"]` — the tag exists but carries no value at all.
        assert!(validate_enrolment_candidate(
            &verified_with(KIND_GIT_ISSUE, &tags(&[&["a"]])).await,
            &k
        )
        .is_none(),);
        // `["a", ""]` — present but empty. Rejected here rather than left to a
        // membership check that only fails by luck.
        assert!(validate_enrolment_candidate(
            &verified_with(KIND_GIT_ISSUE, &tags(&[&["a", ""]])).await,
            &k
        )
        .is_none(),);
    }

    #[test]
    fn an_active_root_continues_only_for_a_comment_that_names_this_agent() {
        // Replaces the original Phase 1 assertion that *every* addressing woke
        // an active root. That was written for a two-party conversation; these
        // roots are shared, and a `p` copied forward by Desktop is the only
        // evidence the other two variants carry.
        assert_eq!(
            comment(
                ProjectAuthor::AuthorisedHuman,
                CallMarker::None,
                RootState::Active,
                Addressing::ExplicitMention,
            ),
            ProjectEffect::Wake,
            "an addressed follow-up still needs no *re-enrolment*"
        );
        for addressing in [Addressing::InheritedParticipant, Addressing::WatchedRoot] {
            assert_eq!(
                comment(
                    ProjectAuthor::AuthorisedHuman,
                    CallMarker::None,
                    RootState::Active,
                    addressing,
                ),
                ProjectEffect::Ignore,
                "{addressing:?} is propagation, not intent"
            );
        }
    }

    #[test]
    fn an_explicit_re_tag_reactivates_a_dormant_root() {
        assert_eq!(
            comment(
                ProjectAuthor::AuthorisedHuman,
                CallMarker::None,
                RootState::Dormant,
                Addressing::ExplicitMention,
            ),
            ProjectEffect::EnrolAndWake
        );
    }

    #[test]
    fn an_explicit_re_tag_reactivates_through_enrol() {
        let mut e = ProjectEnrolments::new();
        e.enrol(&candidate(ROOT, false)).unwrap();
        e.close(ROOT);
        assert_eq!(
            e.enrol(&candidate(ROOT, false)),
            Ok(EnrolOutcome::Reactivated)
        );
        assert_eq!(e.state_of(ROOT), RootState::Active);
        assert_eq!(e.dormant_count(), 0, "the root must leave the dormant set");
    }

    #[test]
    fn an_inherited_p_tag_leaves_a_dormant_root_dormant() {
        // The defect this classifier exists to contain: if any `p` on a closed
        // root counted as a re-tag, every issue the agent ever touched would
        // reanimate the moment someone commented on it.
        for addressing in [Addressing::InheritedParticipant, Addressing::WatchedRoot] {
            assert_eq!(
                comment(
                    ProjectAuthor::AuthorisedHuman,
                    CallMarker::None,
                    RootState::Dormant,
                    addressing,
                ),
                ProjectEffect::Ignore,
                "{addressing:?}"
            );
        }
    }

    #[test]
    fn an_unknown_root_without_an_explicit_mention_does_not_enrol() {
        // The enrolment REQ matches on `#p`, and Desktop copies every prior
        // participant forward — so reaching us is not the same as being asked.
        for addressing in [Addressing::InheritedParticipant, Addressing::WatchedRoot] {
            assert_eq!(
                comment(
                    ProjectAuthor::AuthorisedHuman,
                    CallMarker::None,
                    RootState::Unknown,
                    addressing,
                ),
                ProjectEffect::Ignore,
                "{addressing:?}"
            );
        }
    }

    #[test]
    fn authorised_human_enrols_on_an_explicit_mention() {
        assert_eq!(
            comment(
                ProjectAuthor::AuthorisedHuman,
                CallMarker::None,
                RootState::Unknown,
                Addressing::ExplicitMention,
            ),
            ProjectEffect::EnrolAndWake
        );
    }

    /// Self-suppression lives in the root/comment arms, not at the top of the
    /// classifier. Suppressing self-authorship before the event class was read
    /// also threw away the agent's own authorised state events, so an agent
    /// that opened an issue and later closed it ignored its own `1632` and left
    /// the watch active forever.
    #[test]
    fn authorised_self_authored_lifecycle_updates_state() {
        assert_eq!(
            classify_project_event(
                KindEffect::Lifecycle,
                ProjectAuthor::SelfAuthored,
                CallMarker::None,
                RootState::Active,
                Addressing::WatchedRoot,
                true,
                false,
            ),
            ProjectEffect::ApplyLifecycle
        );
    }

    #[test]
    fn call_result_resumes_and_never_invokes() {
        for state in [RootState::Active, RootState::Unknown, RootState::Dormant] {
            assert_eq!(
                comment(
                    ProjectAuthor::TrustedAgent,
                    CallMarker::Result,
                    state,
                    Addressing::WatchedRoot,
                ),
                ProjectEffect::ResumeCall,
                "{state:?}"
            );
        }
    }

    #[test]
    fn candidate_accessors_report_what_validation_established() {
        let c = candidate_at(ROOT, &coord(), true);
        assert_eq!(c.root(), ROOT);
        assert_eq!(c.coordinate(), coord());
        assert!(c.is_pull_request());
    }

    #[tokio::test]
    async fn candidate_validation_fails_closed() {
        let k = known(&[&coord()]);
        // Not a root kind.
        assert!(validate_enrolment_candidate(
            &verified_with(KIND_TEXT_NOTE, &tags(&[&["a", &coord()]])).await,
            &k,
        )
        .is_none());
        // No `a` tag at all — the real `48be1cc2…` shape. Enrols nobody, no error.
        assert!(validate_enrolment_candidate(
            &verified_with(KIND_GIT_ISSUE, &tags(&[&["subject", "hi"]])).await,
            &k,
        )
        .is_none());
        // Malformed coordinate.
        assert!(validate_enrolment_candidate(
            &verified_with(KIND_GIT_ISSUE, &tags(&[&["a", "nonsense"]])).await,
            &k,
        )
        .is_none());
        // Well-formed but never announced: an `a` tag is an unauthenticated claim.
        let other = format!("30617:{STRANGER}:elsewhere");
        assert!(validate_enrolment_candidate(
            &verified_with(KIND_GIT_ISSUE, &tags(&[&["a", &other]])).await,
            &k,
        )
        .is_none());
        // Nothing discovered yet ⇒ nothing enrollable.
        assert!(validate_enrolment_candidate(
            &verified_with(KIND_GIT_ISSUE, &tags(&[&["a", &coord()]])).await,
            &DiscoveredRepositories::new(),
        )
        .is_none());
        // The old "malformed root id" case is deliberately absent: the id now
        // comes from the verified event, so an ill-formed one is not
        // representable at this boundary at all. A case that can no longer be
        // constructed is stronger than one that is rejected — but it must not
        // be left asserting `is_none()` against a perfectly valid event, which
        // is how it failed when the signature changed.
    }

    #[test]
    fn close_and_reopen_are_no_ops_in_the_wrong_state() {
        let mut e = ProjectEnrolments::new();
        assert!(!e.close(ROOT), "closing an unknown root changes nothing");
        assert!(!e.reopen(ROOT), "reopening an unknown root changes nothing");
        e.enrol(&candidate(ROOT, false)).unwrap();
        assert!(!e.reopen(ROOT), "reopening an active root changes nothing");
        e.close(ROOT);
        assert!(!e.close(ROOT), "closing a dormant root changes nothing");
    }

    #[test]
    fn close_then_reopen_round_trips_and_stays_subscribed() {
        let mut e = ProjectEnrolments::new();
        e.enrol(&candidate(ROOT, false)).unwrap();

        assert!(e.close(ROOT));
        assert_eq!(e.state_of(ROOT), RootState::Dormant);
        // The whole point of the dormant set: still in the `#e` filter, so the
        // reopen that revives the watch is actually observable.
        assert!(
            e.all_roots().contains(&ROOT.to_string()),
            "a dormant root must remain subscribed or reopen can never arrive"
        );

        assert!(e.reopen(ROOT));
        assert_eq!(e.state_of(ROOT), RootState::Active);
    }

    #[test]
    fn comments_and_pr_updates_require_a_matching_coordinate() {
        for kind in [KIND_TEXT_NOTE, KIND_GIT_PR_UPDATE] {
            assert!(
                follow_up_coordinate_allowed(
                    kind,
                    &coordinate_claim(&tags(&[&["a", &coord()]])),
                    &coord()
                ),
                "kind {kind} with the right coordinate"
            );
            // Both builders always emit `a`, so absence is malformed here.
            assert!(
                !follow_up_coordinate_allowed(
                    kind,
                    &coordinate_claim(&tags(&[&["e", ROOT]])),
                    &coord()
                ),
                "kind {kind} must not be admitted on an `#e` match alone"
            );
            let other = format!("30617:{STRANGER}:elsewhere");
            assert!(
                !follow_up_coordinate_allowed(
                    kind,
                    &coordinate_claim(&tags(&[&["a", &other]])),
                    &coord()
                ),
                "kind {kind} may not move its root to another project"
            );
        }
    }

    #[test]
    fn coordinate_allows_colons_in_the_identifier() {
        assert_eq!(
            normalise_coordinate(&format!("30617:{OWNER}:a:b")),
            Some(format!("30617:{OWNER}:a:b"))
        );
    }

    #[test]
    fn coordinate_fails_closed() {
        // Wrong kind, bad owner, missing or empty identifier, padding.
        assert_eq!(normalise_coordinate(&format!("30618:{OWNER}:r")), None);
        assert_eq!(normalise_coordinate("30617:short:r"), None);
        assert_eq!(normalise_coordinate(&format!("30617:{OWNER}")), None);
        assert_eq!(normalise_coordinate(&format!("30617:{OWNER}:")), None);
        assert_eq!(normalise_coordinate(&format!("30617: {OWNER}:r")), None);
        assert_eq!(normalise_coordinate(""), None);
    }

    #[test]
    fn coordinate_normalises_and_lowercases_owner() {
        assert_eq!(
            normalise_coordinate(&format!("30617:{}:my-repo", OWNER.to_ascii_uppercase())),
            Some(format!("30617:{OWNER}:my-repo"))
        );
    }

    #[test]
    fn discovered_repositories_starts_empty_in_production() {
        let d = DiscoveredRepositories::new();
        assert!(d.is_empty());
        assert_eq!(d.len(), 0);
        assert!(!d.contains(&coord()));
        assert_eq!(d.iter().count(), 0);
    }

    #[test]
    fn duplicate_coordinates_are_rejected_for_every_event_class() {
        for kind in [
            KIND_TEXT_NOTE,
            KIND_GIT_PR_UPDATE,
            KIND_GIT_STATUS_OPEN,
            KIND_GIT_STATUS_CLOSED,
        ] {
            assert!(
                !follow_up_coordinate_allowed(
                    kind,
                    &coordinate_claim(&tags(&[&["a", &coord()], &["a", &coord()]])),
                    &coord()
                ),
                "kind {kind}: two coordinates is ambiguity, not redundancy"
            );
        }
    }

    #[test]
    fn enrol_moves_an_unknown_root_to_active() {
        let mut e = ProjectEnrolments::new();
        assert_eq!(e.enrol(&candidate(ROOT, false)), Ok(EnrolOutcome::Enrolled));
        assert_eq!(e.state_of(ROOT), RootState::Active);
        assert_eq!(e.active_count(), 1);
        assert_eq!(e.dormant_count(), 0);
    }

    #[tokio::test]
    async fn enrolment_matches_the_discovered_coordinate_byte_for_byte() {
        // Parsing must not widen acceptance: an uppercase-owner coordinate is
        // not silently equivalent to the canonical discovered string.
        let k = known(&[&coord()]);
        let shouty = format!("30617:{}:repo", OWNER.to_ascii_uppercase());
        assert_eq!(
            normalise_coordinate(&shouty).as_deref(),
            Some(coord().as_str())
        );
        assert!(
            validate_enrolment_candidate(
                &verified_with(KIND_GIT_ISSUE, &tags(&[&["a", &shouty]])).await,
                &k,
            )
            .is_none(),
            "a non-canonical coordinate must not match the discovered set through the parser"
        );
    }

    #[test]
    fn follow_up_rejects_value_less_and_empty_coordinates() {
        for kind in [KIND_TEXT_NOTE, KIND_GIT_PR_UPDATE, KIND_GIT_STATUS_CLOSED] {
            assert!(!follow_up_coordinate_allowed(
                kind,
                &coordinate_claim(&tags(&[&["a"]])),
                &coord()
            ));
            assert!(!follow_up_coordinate_allowed(
                kind,
                &coordinate_claim(&tags(&[&["a", ""]])),
                &coord()
            ));
            assert!(!follow_up_coordinate_allowed(
                kind,
                &coordinate_claim(&tags(&[&["a", ""], &["a", &coord()]])),
                &coord()
            ));
        }
    }

    #[test]
    fn lifecycle_is_never_a_model_turn() {
        for author in [
            ProjectAuthor::AuthorisedHuman,
            ProjectAuthor::TrustedAgent,
            ProjectAuthor::Untrusted,
        ] {
            for call in [CallMarker::None, CallMarker::Invocation, CallMarker::Result] {
                let out = classify_project_event(
                    KindEffect::Lifecycle,
                    author,
                    call,
                    RootState::Active,
                    Addressing::ExplicitMention,
                    true,
                    false,
                );
                assert!(
                    matches!(out, ProjectEffect::ApplyLifecycle | ProjectEffect::Ignore),
                    "{author:?} / {call:?} produced {out:?}"
                );
            }
        }
    }

    #[test]
    fn lifecycle_may_omit_the_coordinate_but_not_contradict_it() {
        for kind in [
            KIND_GIT_STATUS_OPEN,
            KIND_GIT_STATUS_MERGED,
            KIND_GIT_STATUS_CLOSED,
            KIND_GIT_STATUS_DRAFT,
        ] {
            // `GitStatusMeta.repo` is optional, so absence is legitimate — the
            // event is already root-bound by `e`.
            assert!(follow_up_coordinate_allowed(
                kind,
                &coordinate_claim(&tags(&[&["e", ROOT]])),
                &coord()
            ));
            assert!(follow_up_coordinate_allowed(
                kind,
                &coordinate_claim(&tags(&[&["a", &coord()]])),
                &coord()
            ));
            let other = format!("30617:{STRANGER}:elsewhere");
            assert!(!follow_up_coordinate_allowed(
                kind,
                &coordinate_claim(&tags(&[&["a", &other]])),
                &coord()
            ));
        }
    }

    #[test]
    fn pr_update_alone_never_creates_a_turn() {
        assert_eq!(
            classify_project_event(
                KindEffect::ContextRefresh,
                ProjectAuthor::AuthorisedHuman,
                CallMarker::None,
                RootState::Active,
                Addressing::ExplicitMention,
                false,
                false,
            ),
            ProjectEffect::RefreshContext
        );
        assert_eq!(
            classify_project_event(
                KindEffect::ContextRefresh,
                ProjectAuthor::Untrusted,
                CallMarker::None,
                RootState::Active,
                Addressing::ExplicitMention,
                false,
                false,
            ),
            ProjectEffect::UntrustedContext
        );
    }

    #[test]
    fn pull_request_roots_are_tracked_separately_from_all_roots() {
        let mut e = ProjectEnrolments::new();
        e.enrol(&candidate(ROOT, false)).unwrap();
        e.enrol(&candidate(OTHER_ROOT, true)).unwrap();
        assert_eq!(
            e.all_roots(),
            vec![ROOT.to_string(), OTHER_ROOT.to_string()]
        );
        assert_eq!(e.pull_request_roots(), vec![OTHER_ROOT.to_string()]);
        // Dormant PRs still need `#E`, or a revision on a closed PR is invisible.
        e.close(OTHER_ROOT);
        assert_eq!(e.pull_request_roots(), vec![OTHER_ROOT.to_string()]);
    }

    #[test]
    fn re_enrolling_an_active_root_does_not_churn_the_subscription() {
        let mut e = ProjectEnrolments::new();
        e.enrol(&candidate(ROOT, false)).unwrap();
        assert_eq!(
            e.enrol(&candidate(ROOT, false)),
            Ok(EnrolOutcome::Unchanged),
            "an ordinary re-mention must not force a REQ replacement"
        );
        assert!(!EnrolOutcome::Unchanged.changes_subscription());
    }

    #[test]
    fn self_authored_pr_update_refreshes_context() {
        assert_eq!(
            classify_project_event(
                KindEffect::ContextRefresh,
                ProjectAuthor::SelfAuthored,
                CallMarker::None,
                RootState::Active,
                Addressing::WatchedRoot,
                false,
                false,
            ),
            ProjectEffect::RefreshContext
        );
    }

    #[test]
    fn self_authored_roots_and_comments_remain_suppressed() {
        // Regression guard for the arms the early return used to cover.
        for state in [RootState::Active, RootState::Unknown, RootState::Dormant] {
            for addressing in ALL_ADDRESSING {
                assert_eq!(
                    classify_project_event(
                        KindEffect::Root,
                        ProjectAuthor::SelfAuthored,
                        CallMarker::None,
                        state,
                        addressing,
                        true,
                        false,
                    ),
                    ProjectEffect::Ignore,
                    "root: {state:?} / {addressing:?}"
                );
                assert_eq!(
                    comment(
                        ProjectAuthor::SelfAuthored,
                        CallMarker::None,
                        state,
                        addressing
                    ),
                    ProjectEffect::Ignore,
                    "comment: {state:?} / {addressing:?}"
                );
            }
        }
    }

    #[test]
    fn self_authored_state_events_never_create_a_turn() {
        // The other half of the rule: state updates are permitted, turns are
        // not. Nothing self-authored may reach a waking effect.
        for kind_effect in [
            KindEffect::Lifecycle,
            KindEffect::ContextRefresh,
            KindEffect::Root,
            KindEffect::Comment,
            KindEffect::Ignore,
        ] {
            for call in [CallMarker::None, CallMarker::Invocation, CallMarker::Result] {
                for addressing in ALL_ADDRESSING {
                    let out = classify_project_event(
                        kind_effect,
                        ProjectAuthor::SelfAuthored,
                        call,
                        RootState::Active,
                        addressing,
                        true,
                        false,
                    );
                    assert!(
                        !matches!(
                            out,
                            ProjectEffect::EnrolAndWake
                                | ProjectEffect::Wake
                                | ProjectEffect::ResumeCall
                        ),
                        "{kind_effect:?} / {call:?} / {addressing:?} produced {out:?}"
                    );
                }
            }
        }
    }

    #[test]
    fn the_agents_own_reply_neither_enrols_nor_wakes() {
        for call in [CallMarker::None, CallMarker::Invocation, CallMarker::Result] {
            for state in [RootState::Active, RootState::Unknown, RootState::Dormant] {
                for addressing in ALL_ADDRESSING {
                    assert_eq!(
                        comment(ProjectAuthor::SelfAuthored, call, state, addressing),
                        ProjectEffect::Ignore,
                        "{call:?} / {state:?} / {addressing:?}"
                    );
                }
            }
        }
    }

    #[test]
    fn trusted_agent_bare_p_tag_is_never_an_invocation() {
        // Two agents watching one root must not wake each other with ordinary
        // participant-`p`-tagged replies — not even an explicitly addressed one.
        for state in [RootState::Active, RootState::Unknown, RootState::Dormant] {
            for addressing in ALL_ADDRESSING {
                assert_eq!(
                    comment(
                        ProjectAuthor::TrustedAgent,
                        CallMarker::None,
                        state,
                        addressing,
                    ),
                    ProjectEffect::Ignore,
                    "{state:?} / {addressing:?}"
                );
            }
        }
    }

    #[test]
    fn trusted_agent_with_a_call_envelope_invokes() {
        // The envelope names its callee, so it is explicit addressing by
        // construction — an invocation does not additionally need a fresh `p`.
        for addressing in ALL_ADDRESSING {
            assert_eq!(
                comment(
                    ProjectAuthor::TrustedAgent,
                    CallMarker::Invocation,
                    RootState::Unknown,
                    addressing,
                ),
                ProjectEffect::EnrolAndWake,
                "{addressing:?}"
            );
            assert_eq!(
                comment(
                    ProjectAuthor::TrustedAgent,
                    CallMarker::Invocation,
                    RootState::Active,
                    addressing,
                ),
                ProjectEffect::Wake,
                "{addressing:?}"
            );
            assert_eq!(
                comment(
                    ProjectAuthor::TrustedAgent,
                    CallMarker::Invocation,
                    RootState::Dormant,
                    addressing,
                ),
                ProjectEffect::EnrolAndWake,
                "{addressing:?}"
            );
        }
    }

    #[test]
    fn unauthorised_self_authored_lifecycle_is_ignored() {
        // Self-authorship is not its own authority: the signer check still runs.
        assert_eq!(
            classify_project_event(
                KindEffect::Lifecycle,
                ProjectAuthor::SelfAuthored,
                CallMarker::None,
                RootState::Active,
                Addressing::WatchedRoot,
                false,
                false,
            ),
            ProjectEffect::Ignore
        );
    }

    #[test]
    fn unauthorised_status_event_does_not_close_the_watch() {
        assert_eq!(
            classify_project_event(
                KindEffect::Lifecycle,
                ProjectAuthor::AuthorisedHuman,
                CallMarker::None,
                RootState::Active,
                Addressing::ExplicitMention,
                false,
                false,
            ),
            ProjectEffect::Ignore
        );
        assert_eq!(
            classify_project_event(
                KindEffect::Lifecycle,
                ProjectAuthor::AuthorisedHuman,
                CallMarker::None,
                RootState::Active,
                Addressing::ExplicitMention,
                true,
                false,
            ),
            ProjectEffect::ApplyLifecycle
        );
    }

    #[test]
    fn untrusted_identity_is_context_and_cannot_invoke() {
        for call in [CallMarker::None, CallMarker::Invocation, CallMarker::Result] {
            for state in [RootState::Active, RootState::Unknown, RootState::Dormant] {
                assert_eq!(
                    comment(
                        ProjectAuthor::Untrusted,
                        call,
                        state,
                        Addressing::ExplicitMention,
                    ),
                    ProjectEffect::UntrustedContext,
                    "{call:?} / {state:?}"
                );
            }
        }
    }

    // ── Reconstructed WIP: authenticated discovery ───────────────────────────
    //
    // Rebuilt from notes rather than recovered from Git. Each keeps the
    // falsifier that makes it meaningful — a forged event must *parse* before
    // verification rejects it, or the test passes for the wrong reason.

    #[tokio::test]
    async fn ingest_derives_the_coordinate_from_the_signer_not_the_announcement() {
        let keys = Keys::generate();
        let signer = keys.public_key().to_hex();
        // A genuine announcement that also claims someone else's coordinate.
        // A valid signature proves who wrote the event, not that its contents
        // are honest — so `a` must not be read.
        let forged = format!("30617:{THIRD_PARTY}:not-mine");
        let event = signed(
            &keys,
            KIND_GIT_REPO_ANNOUNCEMENT,
            vec![tag(&["d", "my-repo"]), tag(&["a", &forged])],
        );

        let proven =
            VerifiedAnnouncement::prove(VerifiedProjectEvent::verify(event).await.expect("valid"))
                .expect("well-formed despite the forged `a`");
        let mut d = DiscoveredRepositories::new();
        let added = d.ingest(&proven);

        assert_eq!(added, Discovered::Added(format!("30617:{signer}:my-repo")));
        let added = format!("30617:{signer}:my-repo");
        assert!(d.contains(&added));
        assert!(
            !d.contains(&forged),
            "the announcement's own `a` claim must never enter the set"
        );
    }

    #[tokio::test]
    async fn a_tampered_announcement_cannot_be_verified_or_ingested() {
        let keys = Keys::generate();
        let event = signed(&keys, KIND_GIT_REPO_ANNOUNCEMENT, vec![tag(&["d", "mine"])]);
        let rewritten = tampered(&event, serde_json::json!([["d", "someone-elses-repo"]]));

        // Falsifier: the tampered event must still parse, or this proves only
        // that `from_json` is strict.
        assert_eq!(
            rewritten.tags.len(),
            1,
            "the tampered event parsed, so verification is what rejects it"
        );
        assert!(VerifiedProjectEvent::verify(rewritten).await.is_err());

        // And with no witness it cannot reach ingestion at all.
        let d = DiscoveredRepositories::new();
        assert!(d.is_empty());
    }

    #[tokio::test]
    async fn a_forged_author_cannot_be_verified() {
        // The attack that matters most: project authority reads `event.pubkey`
        // for owner, root author, authorised human and sibling checks.
        let keys = Keys::generate();
        let event = signed(&keys, KIND_TEXT_NOTE, vec![tag(&["e", ROOT, "", "root"])]);
        let impersonated = forged_author(&event, OWNER);

        // Falsifier: without this the test would pass if `from_json` rejected
        // the event, proving nothing about the witness.
        assert_eq!(
            impersonated.pubkey.to_hex(),
            OWNER,
            "the forgery is in place"
        );
        assert!(
            VerifiedProjectEvent::verify(impersonated).await.is_err(),
            "a forged author must not survive verification"
        );
    }

    #[tokio::test]
    async fn forged_lifecycle_and_comment_events_never_yield_a_witness() {
        // Each would otherwise pass the authority gate: an owner close, a
        // root-author reopen, an authorised human's comment, a PR update.
        let keys = Keys::generate();
        for kind in [
            KIND_GIT_STATUS_CLOSED,
            KIND_GIT_STATUS_OPEN,
            KIND_TEXT_NOTE,
            KIND_GIT_PR_UPDATE,
        ] {
            let genuine = signed(&keys, kind, vec![tag(&["e", ROOT, "", "root"])]);

            let impersonated = forged_author(&genuine, OWNER);
            assert_eq!(
                impersonated.pubkey.to_hex(),
                OWNER,
                "kind {kind}: forgery parsed"
            );
            assert!(
                VerifiedProjectEvent::verify(impersonated).await.is_err(),
                "kind {kind}: forged owner must not verify"
            );

            let rewritten = tampered(&genuine, serde_json::json!([["e", OTHER_ROOT, "", "root"]]));
            assert_eq!(rewritten.tags.len(), 1, "kind {kind}: retarget parsed");
            assert!(
                VerifiedProjectEvent::verify(rewritten).await.is_err(),
                "kind {kind}: retargeted root must not verify"
            );
        }
    }

    #[tokio::test]
    async fn a_verified_event_exposes_the_fields_authority_depends_on() {
        let keys = Keys::generate();
        let event = signed(&keys, KIND_TEXT_NOTE, vec![tag(&["e", ROOT, "", "root"])]);
        let id = event.id.to_hex();
        let verified = VerifiedProjectEvent::verify(event).await.expect("valid");

        assert_eq!(verified.author(), keys.public_key().to_hex());
        assert_eq!(verified.kind(), KIND_TEXT_NOTE);
        assert_eq!(verified.id(), id);
        assert_eq!(
            root_event_id(verified.kind(), &verified.id(), &verified.tag_vecs()),
            Some(ROOT.to_string())
        );
    }

    #[tokio::test]
    async fn proving_an_announcement_rejects_wrong_kind_and_ambiguous_identifiers() {
        // These refusals moved up to the proof boundary. `ingest` can no longer
        // reject anything, because nothing rejectable can reach it.
        let keys = Keys::generate();

        // Right shape, wrong kind.
        let note = signed(&keys, KIND_TEXT_NOTE, vec![tag(&["d", "repo"])]);
        assert!(
            VerifiedAnnouncement::prove(VerifiedProjectEvent::verify(note).await.unwrap())
                .is_none()
        );

        for (label, tags) in [
            ("no `d`", vec![tag(&["a", "30617:x:y"])]),
            ("empty `d`", vec![tag(&["d", ""])]),
            (
                "conflicting `d`",
                vec![tag(&["d", "one"]), tag(&["d", "two"])],
            ),
            // Two tags that agree is still two tags. "They happen to match"
            // and "there is one" are different claims, and only the second is
            // unambiguous — a rule that reads the first would be picking a
            // winner by tag order.
            (
                "duplicate equal `d`",
                vec![tag(&["d", "same"]), tag(&["d", "same"])],
            ),
        ] {
            let event = signed(&keys, KIND_GIT_REPO_ANNOUNCEMENT, tags);
            assert!(
                VerifiedAnnouncement::prove(VerifiedProjectEvent::verify(event).await.unwrap())
                    .is_none(),
                "{label} must not prove an announcement"
            );
        }
    }

    #[tokio::test]
    async fn a_proven_announcement_carries_the_coordinate_it_established() {
        // The coordinate is computed once, at the proof boundary. Nothing
        // downstream parses `d` a second time and risks a different answer.
        let keys = Keys::generate();
        let signer = keys.public_key().to_hex();
        let event = signed(
            &keys,
            KIND_GIT_REPO_ANNOUNCEMENT,
            vec![tag(&["d", "my-repo"])],
        );
        let proven =
            VerifiedAnnouncement::prove(VerifiedProjectEvent::verify(event).await.unwrap())
                .expect("well-formed");

        assert_eq!(proven.coordinate(), format!("30617:{signer}:my-repo"));
        assert_eq!(proven.event().author(), signer);

        let mut d = DiscoveredRepositories::new();
        assert_eq!(
            d.ingest(&proven),
            Discovered::Added(proven.coordinate().to_string())
        );
        assert!(d.contains(proven.coordinate()));
    }

    #[tokio::test]
    async fn an_ingested_coordinate_is_what_enrolment_will_accept() {
        // The two ends must agree byte-for-byte or discovery is decorative.
        let keys = Keys::generate();
        let event = signed(
            &keys,
            KIND_GIT_REPO_ANNOUNCEMENT,
            vec![tag(&["d", "my-repo"])],
        );
        let proven =
            VerifiedAnnouncement::prove(VerifiedProjectEvent::verify(event).await.expect("valid"))
                .expect("well-formed");
        let mut d = DiscoveredRepositories::new();
        let Discovered::Added(coordinate) = d.ingest(&proven) else {
            panic!("a fresh announcement is added");
        };

        let candidate = validate_enrolment_candidate(
            &verified_with(KIND_GIT_ISSUE, &tags(&[&["a", &coordinate]])).await,
            &d,
        )
        .expect("an issue on a discovered repo enrols");
        assert_eq!(candidate.coordinate(), coordinate);
    }

    #[tokio::test]
    async fn the_discovery_ceiling_refuses_rather_than_evicts() {
        // The set is fed by a global `kinds: [30617]` REQ, so anyone can grow
        // it. Bounding it by eviction would have traded a memory bound for
        // silent authority-state amnesia: a repository would disappear while
        // the set still looked complete, and an enrolment that used to be valid
        // would stop being so with nothing saying why.
        let keys = Keys::generate();
        let mut d = DiscoveredRepositories::for_test(
            (0..DISCOVERY_CEILING).map(|i| format!("30617:{}:repo-{i}", "a".repeat(64))),
        );
        assert_eq!(d.len(), DISCOVERY_CEILING);
        assert!(
            !d.has_overflowed(),
            "a full set is not yet an overflowed one"
        );

        let event = signed(
            &keys,
            KIND_GIT_REPO_ANNOUNCEMENT,
            vec![tag(&["d", "one-too-many"])],
        );
        let proven =
            VerifiedAnnouncement::prove(VerifiedProjectEvent::verify(event).await.expect("valid"))
                .expect("well-formed");

        assert_eq!(
            d.ingest(&proven),
            Discovered::Refused {
                because: RefusedBecause::Cardinality,
                degradation: Degradation::BecameDegraded,
            }
        );
        assert_eq!(d.len(), DISCOVERY_CEILING, "nothing was evicted");
        assert!(!d.contains(proven.coordinate()));
        assert!(
            d.has_overflowed(),
            "and the incompleteness is visible rather than silent"
        );
        assert_eq!(d.refused_count(), 1);
    }

    /// An announcement whose `d` makes the coordinate exactly `bytes` long.
    async fn announcement_of_coordinate_bytes(keys: &Keys, bytes: usize) -> VerifiedAnnouncement {
        // `30617:` + 64 hex + `:` = 71 bytes of fixed structure.
        let identifier = "x".repeat(bytes - 71);
        let event = signed(
            keys,
            KIND_GIT_REPO_ANNOUNCEMENT,
            vec![tag(&["d", &identifier])],
        );
        let proven =
            VerifiedAnnouncement::prove(VerifiedProjectEvent::verify(event).await.expect("valid"))
                .expect("well-formed");
        assert_eq!(
            proven.coordinate().len(),
            bytes,
            "fixture builds the size it claims"
        );
        proven
    }

    #[tokio::test]
    async fn an_oversized_coordinate_is_refused_even_by_an_empty_set() {
        // Cardinality does not bound bytes. One announcement with a large
        // enough `d` is a large allocation regardless of how few there are, and
        // the relay in this repository accepts 512 KiB frames by default.
        let keys = Keys::generate();
        let mut d = DiscoveredRepositories::new();
        let huge = announcement_of_coordinate_bytes(&keys, DISCOVERY_COORDINATE_BYTES + 1).await;

        assert_eq!(
            d.ingest(&huge),
            Discovered::Refused {
                because: RefusedBecause::CoordinateTooLarge,
                degradation: Degradation::BecameDegraded,
            }
        );
        assert!(d.is_empty());
        assert_eq!(d.retained_bytes(), 0, "a refused coordinate is not charged");
        assert!(d.has_overflowed());

        // And the boundary is inclusive on the accepting side.
        let mut d = DiscoveredRepositories::new();
        let exact = announcement_of_coordinate_bytes(&keys, DISCOVERY_COORDINATE_BYTES).await;
        assert!(matches!(d.ingest(&exact), Discovered::Added(_)));
        assert_eq!(d.retained_bytes(), DISCOVERY_COORDINATE_BYTES);
        assert!(!d.has_overflowed());
    }

    #[tokio::test]
    async fn the_byte_ceiling_can_trip_before_the_count_ceiling() {
        // The case the count ceiling alone misses: far fewer than
        // DISCOVERY_CEILING coordinates, each individually acceptable, adding
        // up to more memory than the agent will hold.
        let keys = Keys::generate();
        let filler: Vec<String> = (0..DISCOVERY_RETAINED_BYTES / DISCOVERY_COORDINATE_BYTES)
            // 71 bytes of fixed structure + 441 = exactly
            // DISCOVERY_COORDINATE_BYTES per coordinate.
            .map(|i| format!("30617:{}:{i:0>441}", "a".repeat(64)))
            .collect();
        let mut d = DiscoveredRepositories::for_test(filler);

        assert!(
            d.len() < DISCOVERY_CEILING,
            "the count ceiling is nowhere near: {} of {DISCOVERY_CEILING}",
            d.len()
        );
        assert_eq!(d.retained_bytes(), DISCOVERY_RETAINED_BYTES);

        let one_more = announcement_of_coordinate_bytes(&keys, 100).await;
        assert_eq!(
            d.ingest(&one_more),
            Discovered::Refused {
                because: RefusedBecause::RetainedBytes,
                degradation: Degradation::BecameDegraded,
            }
        );
        assert_eq!(
            d.retained_bytes(),
            DISCOVERY_RETAINED_BYTES,
            "nothing was charged and nothing was evicted"
        );
    }

    #[tokio::test]
    async fn a_duplicate_announcement_is_charged_no_bytes() {
        // Duplicates arrive constantly on a live REQ. Charging them would walk
        // the byte total to the ceiling on no new information at all.
        let keys = Keys::generate();
        let mut d = DiscoveredRepositories::new();
        let proven = announcement_of_coordinate_bytes(&keys, 200).await;

        assert!(matches!(d.ingest(&proven), Discovered::Added(_)));
        let after_first = d.retained_bytes();
        assert_eq!(after_first, 200);

        for _ in 0..50 {
            assert!(matches!(d.ingest(&proven), Discovered::AlreadyKnown(_)));
        }
        assert_eq!(d.retained_bytes(), after_first);
        assert_eq!(d.len(), 1);
        assert!(!d.has_overflowed());
    }

    #[tokio::test]
    async fn degradation_is_reported_once_and_counted_thereafter() {
        // The log-amplifier fix, at the type. Every refusal after the first is
        // `AlreadyDegraded`, so a caller has something bounded to say — and the
        // outcome carries no coordinate, so there is nothing attacker-chosen to
        // print even by accident.
        let keys = Keys::generate();
        let mut d = DiscoveredRepositories::new();

        let first = announcement_of_coordinate_bytes(&keys, DISCOVERY_COORDINATE_BYTES + 1).await;
        assert!(matches!(
            d.ingest(&first),
            Discovered::Refused {
                degradation: Degradation::BecameDegraded,
                ..
            }
        ));

        for i in 0..5 {
            let next =
                announcement_of_coordinate_bytes(&keys, DISCOVERY_COORDINATE_BYTES + 2 + i).await;
            assert!(
                matches!(
                    d.ingest(&next),
                    Discovered::Refused {
                        degradation: Degradation::AlreadyDegraded,
                        ..
                    }
                ),
                "only the transition is newsworthy"
            );
        }
        assert_eq!(d.refused_count(), 6);
        assert!(d.has_overflowed());
    }

    #[tokio::test]
    async fn a_full_set_still_accepts_a_repeat_of_something_it_already_holds() {
        // The ceiling bounds *growth*. Refusing a coordinate already in the set
        // would report a spurious overflow on ordinary duplicate traffic, and
        // the relay's live REQ delivers duplicates routinely.
        let keys = Keys::generate();
        let event = signed(
            &keys,
            KIND_GIT_REPO_ANNOUNCEMENT,
            vec![tag(&["d", "already-here"])],
        );
        let proven =
            VerifiedAnnouncement::prove(VerifiedProjectEvent::verify(event).await.expect("valid"))
                .expect("well-formed");

        let mut filler: Vec<String> = (0..DISCOVERY_CEILING - 1)
            .map(|i| format!("30617:{}:repo-{i}", "a".repeat(64)))
            .collect();
        filler.push(proven.coordinate().to_string());
        let mut d = DiscoveredRepositories::for_test(filler);
        assert_eq!(d.len(), DISCOVERY_CEILING);

        assert_eq!(
            d.ingest(&proven),
            Discovered::AlreadyKnown(proven.coordinate().to_string())
        );
        assert!(!d.has_overflowed(), "a duplicate is not an overflow");
    }

    #[tokio::test]
    async fn discovery_incompleteness_is_permanent_once_the_ceiling_has_refused() {
        // There is no way back: the refused announcements are gone and the set
        // cannot learn what it missed. Anything that later wants to claim a
        // complete enrolment filter has to consult `has_overflowed`.
        let keys = Keys::generate();

        // One coordinate the set already holds, so the duplicate below takes
        // the `AlreadyKnown` path. My first version re-ingested the *refused*
        // coordinate — which is never in the set, so it simply re-entered the
        // overflow branch, and the test passed even when the flag was cleared
        // on every duplicate.
        let held = signed(&keys, KIND_GIT_REPO_ANNOUNCEMENT, vec![tag(&["d", "held"])]);
        let held =
            VerifiedAnnouncement::prove(VerifiedProjectEvent::verify(held).await.expect("valid"))
                .expect("well-formed");

        let mut filler: Vec<String> = (0..DISCOVERY_CEILING - 1)
            .map(|i| format!("30617:{}:repo-{i}", "a".repeat(64)))
            .collect();
        filler.push(held.coordinate().to_string());
        let mut d = DiscoveredRepositories::for_test(filler);

        let refused = signed(&keys, KIND_GIT_REPO_ANNOUNCEMENT, vec![tag(&["d", "lost"])]);
        let refused = VerifiedAnnouncement::prove(
            VerifiedProjectEvent::verify(refused).await.expect("valid"),
        )
        .expect("well-formed");
        assert!(matches!(d.ingest(&refused), Discovered::Refused { .. }));
        assert!(d.has_overflowed());

        // An ordinary duplicate of something the set does hold — the common
        // case on a live REQ — must not launder it back to complete.
        assert!(matches!(d.ingest(&held), Discovered::AlreadyKnown(_)));
        assert!(d.has_overflowed(), "incompleteness does not heal");
    }

    #[tokio::test]
    async fn ingest_is_idempotent() {
        let keys = Keys::generate();
        let event = signed(
            &keys,
            KIND_GIT_REPO_ANNOUNCEMENT,
            vec![tag(&["d", "my-repo"])],
        );
        let mut d = DiscoveredRepositories::new();
        let prove = |e: nostr::Event| async move {
            VerifiedAnnouncement::prove(VerifiedProjectEvent::verify(e).await.unwrap())
                .expect("well-formed")
        };
        let first = d.ingest(&prove(event.clone()).await);
        let second = d.ingest(&prove(event).await);
        assert!(matches!(first, Discovered::Added(_)));
        assert!(matches!(second, Discovered::AlreadyKnown(_)));
        assert_eq!(d.len(), 1);
    }

    // ── Reconstructed WIP: replay, folding, ancestry, routes ─────────────────

    #[test]
    fn replay_recovers_a_historical_bare_p_enrolment_without_a_turn() {
        // The defect this replaces: suppressing replayed bare `p` as inherited
        // meant a root enrolled by an authorised human's structural `p` was
        // silently forgotten across a restart.
        let effect = classify_project_event(
            classify_kind(KIND_TEXT_NOTE),
            ProjectAuthor::AuthorisedHuman,
            CallMarker::None,
            RootState::Unknown,
            Addressing::ExplicitMention,
            false,
            false,
        );
        // Two separate assertions, per the falsifier: state is restored, and
        // no turn is produced.
        assert_eq!(effect, ProjectEffect::EnrolAndWake, "meaning is unchanged");
        assert_eq!(
            apply_processing_mode(effect, ProcessingMode::Replay),
            ProjectEffect::Enrol,
            "the watch is restored"
        );
        assert_ne!(
            apply_processing_mode(effect, ProcessingMode::Replay),
            ProjectEffect::EnrolAndWake,
            "and the model is not woken"
        );
        assert_eq!(
            apply_processing_mode(effect, ProcessingMode::Live),
            ProjectEffect::EnrolAndWake
        );
    }

    #[test]
    fn replay_never_produces_a_waking_effect() {
        for effect in [
            ProjectEffect::EnrolAndWake,
            ProjectEffect::Wake,
            ProjectEffect::ResumeCall,
            ProjectEffect::ApplyLifecycle,
            ProjectEffect::RefreshContext,
            ProjectEffect::UntrustedContext,
            ProjectEffect::Enrol,
            ProjectEffect::Ignore,
        ] {
            let replayed = apply_processing_mode(effect, ProcessingMode::Replay);
            assert!(
                !matches!(
                    replayed,
                    ProjectEffect::EnrolAndWake | ProjectEffect::Wake | ProjectEffect::ResumeCall
                ),
                "{effect:?} replayed to {replayed:?}"
            );
        }
    }

    #[test]
    fn a_later_inherited_p_neither_re_enrols_nor_wakes() {
        let effect = classify_project_event(
            classify_kind(KIND_TEXT_NOTE),
            ProjectAuthor::AuthorisedHuman,
            CallMarker::None,
            RootState::Active,
            Addressing::InheritedParticipant,
            false,
            false,
        );
        assert_eq!(
            effect,
            ProjectEffect::Ignore,
            "a copied `p` asks for nothing"
        );
        assert_ne!(effect, ProjectEffect::EnrolAndWake, "it does not re-enrol");

        // The addressed counterpart still both wakes and declines to re-enrol,
        // which is the distinction this test was originally written to hold.
        let addressed = classify_project_event(
            classify_kind(KIND_TEXT_NOTE),
            ProjectAuthor::AuthorisedHuman,
            CallMarker::None,
            RootState::Active,
            Addressing::ExplicitMention,
            false,
            false,
        );
        assert_eq!(addressed, ProjectEffect::Wake, "an active root continues");
        assert_ne!(
            addressed,
            ProjectEffect::EnrolAndWake,
            "it does not re-enrol"
        );
        assert_eq!(
            apply_processing_mode(addressed, ProcessingMode::Replay),
            ProjectEffect::RefreshContext
        );
    }

    #[tokio::test]
    async fn prior_facts_are_folded_after_evaluation_not_before() {
        // Sequential proof, not final state: incorporating an event's own `p`
        // before evaluating it would make the first genuine mention see itself
        // and classify as inherited.
        let keys = Keys::generate();
        let root = signed(&keys, KIND_GIT_ISSUE, vec![tag(&["a", &coord()])]);
        let root = VerifiedProjectEvent::verify(root).await.expect("valid");
        let bound = VerifiedBoundRoot::prove(std::slice::from_ref(&root), &known(&[&coord()]))
            .expect("proves");
        let mut facts = PriorRootFacts::seed(&bound);

        let mention = signed(&keys, KIND_TEXT_NOTE, vec![tag(&["p", AGENT_PK])]);
        let mention = VerifiedProjectEvent::verify(mention).await.expect("valid");
        let evidence = AddressingEvidence::resolve(&mention, &agent_identity());

        assert!(
            !facts.agent_was_participant(),
            "state before the first mention"
        );
        assert_eq!(
            resolve_addressing(
                &watched(),
                &evidence,
                &RootHistoryReadiness::Complete,
                Some(&facts),
                &agent_identity(),
            ),
            Some(Addressing::ExplicitMention),
            "the first mention must not see itself"
        );

        facts.observe(&mention, &agent_identity());
        assert!(facts.agent_was_participant(), "only now is it folded in");

        let later = signed(&keys, KIND_TEXT_NOTE, vec![tag(&["p", AGENT_PK])]);
        let later = VerifiedProjectEvent::verify(later).await.expect("valid");
        let later_evidence = AddressingEvidence::resolve(&later, &agent_identity());
        assert_eq!(
            resolve_addressing(
                &watched(),
                &later_evidence,
                &RootHistoryReadiness::Complete,
                Some(&facts),
                &agent_identity(),
            ),
            Some(Addressing::InheritedParticipant),
            "the next bare `p` is propagation"
        );
    }

    #[tokio::test]
    async fn observe_records_the_agent_only_from_verified_p_tags() {
        let keys = Keys::generate();
        let root = signed(&keys, KIND_GIT_ISSUE, vec![tag(&["a", &coord()])]);
        let root = VerifiedProjectEvent::verify(root).await.expect("valid");
        let bound = VerifiedBoundRoot::prove(std::slice::from_ref(&root), &known(&[&coord()]))
            .expect("proves");
        let mut facts = PriorRootFacts::seed(&bound);

        assert!(!facts.agent_was_participant());
        assert_eq!(facts.root_author(), keys.public_key().to_hex());
        assert_eq!(facts.repository_owner(), OWNER);

        let unrelated = signed(&keys, KIND_TEXT_NOTE, vec![tag(&["p", THIRD_PARTY])]);
        let unrelated = VerifiedProjectEvent::verify(unrelated)
            .await
            .expect("valid");
        facts.observe(&unrelated, &agent_identity());
        assert!(!facts.agent_was_participant());

        // The unverified negative: a raw event tagging the agent cannot be
        // observed at all, because `observe` takes only a witness. Its forged
        // form does not survive verification, so it never reaches the fold.
        let tagging = signed(&keys, KIND_TEXT_NOTE, vec![tag(&["p", AGENT_PK])]);
        let forged = forged_author(&tagging, OWNER);
        assert_eq!(forged.pubkey.to_hex(), OWNER, "the forgery parsed");
        assert!(
            VerifiedProjectEvent::verify(forged).await.is_err(),
            "an unverifiable event yields no witness, so it cannot be folded"
        );
        assert!(!facts.agent_was_participant());

        let tagging = VerifiedProjectEvent::verify(tagging).await.expect("valid");
        facts.observe(&tagging, &agent_identity());
        assert!(facts.agent_was_participant());
    }

    #[test]
    fn history_orders_the_root_first_then_time_then_id() {
        // Relay arrival order is not history. Both runtimes must fold the same
        // events in the same order or reconstruct different facts from them.
        let root_key = history_order_key(ROOT, ROOT, 500);
        let later = history_order_key(ROOT, OTHER_ROOT, 100);
        assert!(root_key < later, "the root sorts first despite being newer");

        let a = history_order_key(ROOT, "aa".repeat(32).as_str(), 100);
        let b = history_order_key(ROOT, "bb".repeat(32).as_str(), 100);
        assert!(a < b, "equal timestamps tie-break on event id");

        let early = history_order_key(ROOT, "ff".repeat(32).as_str(), 100);
        let late = history_order_key(ROOT, "aa".repeat(32).as_str(), 200);
        assert!(early < late, "time dominates the id tie-break");
    }

    // ── Reconstructed WIP: strict ancestry ───────────────────────────────────

    #[test]
    fn conflicting_root_markers_are_refused_not_ordered() {
        // The defect once `ProjectRoute` made this the session key: an author
        // could otherwise pick their conversation by tag order.
        assert_eq!(
            root_event_id(
                KIND_TEXT_NOTE,
                THIRD_PARTY,
                &tags(&[&["e", ROOT, "", "root"], &["e", OTHER_ROOT, "", "root"]])
            ),
            None
        );
        // Reversed order must also be refused, not resolved differently.
        assert_eq!(
            root_event_id(
                KIND_TEXT_NOTE,
                THIRD_PARTY,
                &tags(&[&["e", OTHER_ROOT, "", "root"], &["e", ROOT, "", "root"]])
            ),
            None
        );
    }

    #[test]
    fn a_malformed_marked_root_is_not_rescued_by_a_valid_fallback() {
        // "My root is <garbage>" is malformed, not legacy.
        assert_eq!(
            root_event_id(
                KIND_TEXT_NOTE,
                THIRD_PARTY,
                &tags(&[&["e", "garbage", "", "root"], &["e", ROOT]])
            ),
            None
        );
    }

    #[test]
    fn multiple_unmarked_candidates_are_refused() {
        assert_eq!(
            root_event_id(
                KIND_TEXT_NOTE,
                THIRD_PARTY,
                &tags(&[&["e", ROOT], &["e", OTHER_ROOT]])
            ),
            None
        );
    }

    #[test]
    fn a_root_marker_plus_a_reply_reference_resolves_to_the_root() {
        // The legitimate status-event shape: root plus accepted revision.
        // Order-independence is the property that was missing.
        assert_eq!(
            root_event_id(
                KIND_GIT_STATUS_CLOSED,
                THIRD_PARTY,
                &tags(&[&["e", ROOT, "", "root"], &["e", OTHER_ROOT, "", "reply"]])
            ),
            Some(ROOT.to_string())
        );
        assert_eq!(
            root_event_id(
                KIND_GIT_STATUS_CLOSED,
                THIRD_PARTY,
                &tags(&[&["e", OTHER_ROOT, "", "reply"], &["e", ROOT, "", "root"]])
            ),
            Some(ROOT.to_string())
        );
    }

    #[test]
    fn a_lone_reply_reference_with_no_root_marker_is_refused() {
        // Nothing here says which event is the root.
        assert_eq!(
            root_event_id(
                KIND_TEXT_NOTE,
                THIRD_PARTY,
                &tags(&[&["e", ROOT, "", "reply"]])
            ),
            None
        );
    }

    #[test]
    fn a_single_unmarked_reference_is_still_accepted() {
        assert_eq!(
            root_event_id(KIND_GIT_STATUS_OPEN, THIRD_PARTY, &tags(&[&["e", ROOT]])),
            Some(ROOT.to_string())
        );
    }

    #[test]
    fn pr_updates_need_exactly_one_uppercase_e() {
        assert_eq!(
            root_event_id(KIND_GIT_PR_UPDATE, THIRD_PARTY, &tags(&[&["E", ROOT]])),
            Some(ROOT.to_string())
        );
        assert_eq!(
            root_event_id(
                KIND_GIT_PR_UPDATE,
                THIRD_PARTY,
                &tags(&[&["E", ROOT], &["E", OTHER_ROOT]])
            ),
            None
        );
        // Lowercase does not stand in for uppercase.
        assert_eq!(
            root_event_id(KIND_GIT_PR_UPDATE, THIRD_PARTY, &tags(&[&["e", ROOT]])),
            None
        );
    }

    #[test]
    fn coordinate_claims_distinguish_absent_from_incoherent() {
        // The distinction the old `Option<String>` destroyed.
        assert_eq!(
            coordinate_claim(&tags(&[&["e", ROOT]])),
            CoordinateClaim::Absent
        );
        assert_eq!(
            coordinate_claim(&tags(&[&["a", &coord()]])),
            CoordinateClaim::Unique(coord())
        );
        assert_eq!(coordinate_claim(&tags(&[&["a"]])), CoordinateClaim::Invalid);
        assert_eq!(
            coordinate_claim(&tags(&[&["a", ""]])),
            CoordinateClaim::Invalid
        );
        assert_eq!(
            coordinate_claim(&tags(&[&["a", &coord()], &["a", &coord()]])),
            CoordinateClaim::Invalid
        );
    }

    // ── Reconstructed WIP: routes and subscription classification ────────────

    #[test]
    fn an_announcement_has_no_root_route() {
        assert_eq!(
            root_event_id(
                KIND_GIT_REPO_ANNOUNCEMENT,
                ROOT,
                &tags(&[&["d", "my-repo"]])
            ),
            None
        );
    }

    #[tokio::test]
    async fn a_verified_announcement_derives_no_project_route() {
        // Why `Discovery` is its own variant: derivation correctly finds
        // nothing, so a `Routed`-only design would drop every announcement
        // through a path that looks like it handled it.
        let keys = Keys::generate();
        let event = signed(
            &keys,
            KIND_GIT_REPO_ANNOUNCEMENT,
            vec![tag(&["d", "my-repo"])],
        );
        let verified = VerifiedProjectEvent::verify(event).await.expect("valid");
        assert!(ProjectRoute::derive(&verified).is_none());
    }

    #[tokio::test]
    async fn a_routed_event_derives_its_root_and_claim() {
        let keys = Keys::generate();
        let event = signed(
            &keys,
            KIND_TEXT_NOTE,
            vec![tag(&["e", ROOT, "", "root"]), tag(&["a", &coord()])],
        );
        let verified = VerifiedProjectEvent::verify(event).await.expect("valid");
        let route = ProjectRoute::derive(&verified).expect("routes");

        assert_eq!(route.root(), ROOT);
        assert_eq!(route.key(), project_route_key(ROOT).unwrap());
        assert_eq!(route.coordinate_claim(), &CoordinateClaim::Unique(coord()));
    }

    #[tokio::test]
    async fn a_catch_up_route_can_be_bound_to_its_expected_root() {
        // Relay filters are candidate selection, not authority: a catch-up
        // subscription answering with a different root must be detectable.
        let keys = Keys::generate();
        let event = signed(
            &keys,
            KIND_TEXT_NOTE,
            vec![tag(&["e", OTHER_ROOT, "", "root"])],
        );
        let verified = VerifiedProjectEvent::verify(event).await.expect("valid");
        let route = ProjectRoute::derive(&verified).expect("routes");

        let mut h = PageHarness::new();
        let mut c = HistoryCursor::new(
            HistoryScope::Root {
                root: ROOT.to_string(),
                stream: HistoryStream::Comments,
            },
            1_000,
            4,
            1_000,
        );
        let page = h.open_with(c.begin_request()).await;
        let Some(ProjectSubscription::RootCatchUp { root: expected, .. }) =
            h.requests.match_frame(page.sub_id())
        else {
            panic!("the request we registered should match");
        };
        assert_ne!(
            route.root(),
            expected,
            "the mismatch the dispatch branch refuses"
        );
    }

    #[tokio::test]
    async fn a_catch_up_subscription_id_fits_this_relay() {
        // `proj-catchup-` + a one-character stream marker + `-` + the
        // 64-character root + `-` + the attempt's incarnation.
        // `buzz-relay/src/protocol.rs:9` advertises 256, so this is accepted
        // here; NIP-01's conventional cap is 64, which the root alone already
        // exceeded before this piece.
        //
        // The stream marker is what stops a pull request's two required
        // streams colliding on one id — they are different questions and the
        // registry must be able to hold both. The incarnation is what stops two
        // *attempts* colliding, which is the same argument one level down.
        let mut h = PageHarness::new();
        let mut comments_cursor = HistoryCursor::new(
            HistoryScope::Root {
                root: ROOT.to_string(),
                stream: HistoryStream::Comments,
            },
            1_000,
            4,
            1_000,
        );
        let mut updates_cursor = HistoryCursor::new(
            HistoryScope::Root {
                root: ROOT.to_string(),
                stream: HistoryStream::PullRequestUpdates,
            },
            1_000,
            4,
            1_000,
        );
        let comments = h.open_with(comments_cursor.begin_request()).await;
        let updates = h.open_with(updates_cursor.begin_request()).await;

        assert!(
            comments.sub_id().len() <= 256,
            "{} is longer than this relay accepts",
            comments.sub_id().len()
        );
        assert!(
            comments.sub_id().contains(ROOT),
            "the root is named exactly"
        );
        assert_ne!(
            comments.sub_id(),
            updates.sub_id(),
            "one id per stream, not per root"
        );
    }

    // ── Project-specific author classification ───────────────────────────────
    //
    // Rebuilt after discovering they were lost in the damage and missed by my
    // own recovery check: I verified against test names I could recall having
    // published, which is not an inventory.

    fn no_one() -> BTreeSet<String> {
        BTreeSet::new()
    }

    fn set_of(keys: &[&str]) -> BTreeSet<String> {
        keys.iter().map(|k| k.to_string()).collect()
    }

    async fn some_event() -> (VerifiedProjectEvent, String) {
        let keys = Keys::generate();
        let event = signed(&keys, KIND_TEXT_NOTE, vec![tag(&["e", ROOT, "", "root"])]);
        let author = keys.public_key().to_hex();
        (
            VerifiedProjectEvent::verify(event).await.expect("valid"),
            author,
        )
    }

    struct StubResolver {
        siblings: Vec<(String, String)>,
    }

    impl SiblingResolver for StubResolver {
        fn is_same_owner_sibling(&self, author: &str, owner: &str) -> bool {
            self.siblings.iter().any(|(a, o)| a == author && o == owner)
        }
    }

    #[tokio::test]
    async fn self_is_classified_before_anything_else() {
        let secret = "0000000000000000000000000000000000000000000000000000000000000001";
        let keys = Keys::parse(secret).expect("keys");
        let event = signed(&keys, KIND_TEXT_NOTE, vec![tag(&["e", ROOT, "", "root"])]);
        let event = VerifiedProjectEvent::verify(event).await.expect("valid");
        let identity = AgentIdentity::new(&keys.public_key()).expect("identity");
        let author = event.author();

        assert_eq!(
            classify_project_author(
                &event,
                &identity,
                Some(&author),
                &set_of(&[&author]),
                None,
                &set_of(&[&author]),
            ),
            ProjectAuthor::SelfAuthored,
            "the agent cannot become an authorised human by owning things"
        );
    }

    #[tokio::test]
    async fn the_agent_owner_and_approved_humans_are_authorised() {
        let (event, author) = some_event().await;
        assert_eq!(
            classify_project_author(
                &event,
                &agent_identity(),
                Some(&author),
                &no_one(),
                None,
                &no_one()
            ),
            ProjectAuthor::AuthorisedHuman,
            "the agent owner"
        );
        assert_eq!(
            classify_project_author(
                &event,
                &agent_identity(),
                Some(OWNER),
                &set_of(&[&author]),
                None,
                &no_one()
            ),
            ProjectAuthor::AuthorisedHuman,
            "an explicitly approved human"
        );
    }

    #[tokio::test]
    async fn an_attested_sibling_binds_to_the_agent_owner() {
        // A NIP-OA sibling shares *this agent's* owner, not the owner of
        // whichever repository is being discussed.
        let (event, author) = some_event().await;
        let resolver = StubResolver {
            siblings: vec![(author.clone(), OWNER.to_string())],
        };
        let proof = resolver.resolve(&author, OWNER).expect("proof");

        assert_eq!(
            classify_project_author(
                &event,
                &agent_identity(),
                Some(OWNER),
                &no_one(),
                Some(&proof),
                &no_one()
            ),
            ProjectAuthor::TrustedAgent
        );
        assert_eq!(
            classify_project_author(
                &event,
                &agent_identity(),
                Some(THIRD_PARTY),
                &no_one(),
                Some(&proof),
                &no_one()
            ),
            ProjectAuthor::Untrusted,
            "a proof against a different owner does not transfer"
        );
    }

    #[tokio::test]
    async fn a_sibling_proof_for_someone_else_grants_nothing() {
        let (event, _) = some_event().await;
        let resolver = StubResolver {
            siblings: vec![(THIRD_PARTY.to_string(), OWNER.to_string())],
        };
        let wrong_author = resolver.resolve(THIRD_PARTY, OWNER).expect("proof");
        assert_eq!(
            classify_project_author(
                &event,
                &agent_identity(),
                Some(OWNER),
                &no_one(),
                Some(&wrong_author),
                &no_one()
            ),
            ProjectAuthor::Untrusted
        );
    }

    #[tokio::test]
    async fn an_owner_approved_external_agent_is_trusted_without_a_sibling_proof() {
        let (event, author) = some_event().await;
        assert_eq!(
            classify_project_author(
                &event,
                &agent_identity(),
                Some(OWNER),
                &no_one(),
                None,
                &set_of(&[&author])
            ),
            ProjectAuthor::TrustedAgent
        );
    }

    #[tokio::test]
    async fn an_empty_approval_set_means_nobody_not_everybody() {
        // The Hermes allow-list convention that empty means allow-all must not
        // leak in, and `RespondTo::Anyone` is not consulted at all.
        let (event, _) = some_event().await;
        assert_eq!(
            classify_project_author(&event, &agent_identity(), None, &no_one(), None, &no_one()),
            ProjectAuthor::Untrusted
        );
    }

    #[test]
    fn a_sibling_proof_exists_only_where_the_lookup_succeeded() {
        let resolver = StubResolver {
            siblings: vec![(THIRD_PARTY.to_string(), OWNER.to_string())],
        };
        assert!(resolver.resolve(THIRD_PARTY, OWNER).is_some());
        assert!(resolver.resolve(THIRD_PARTY, AGENT_PK).is_none());
        assert!(resolver.resolve(OWNER, OWNER).is_none());
    }

    #[tokio::test]
    async fn an_untrusted_author_cannot_enrol_or_wake() {
        // Deliberately *not* "cannot produce any effect": it produces
        // `UntrustedContext`, which is an effect and remains an open blocker.
        // Attacker-controlled prose reaching a model prompt is steering
        // whatever the variant is called. The stronger name becomes available
        // once untrusted content is excluded from model context entirely.
        for addressing in ALL_ADDRESSING {
            for state in [RootState::Active, RootState::Unknown, RootState::Dormant] {
                let effect = classify_project_event(
                    classify_kind(KIND_TEXT_NOTE),
                    ProjectAuthor::Untrusted,
                    CallMarker::None,
                    state,
                    addressing,
                    false,
                    false,
                );
                assert_eq!(effect, ProjectEffect::UntrustedContext);
                assert!(!matches!(
                    apply_processing_mode(effect, ProcessingMode::Live),
                    ProjectEffect::EnrolAndWake | ProjectEffect::Wake | ProjectEffect::Enrol
                ));
            }
        }
    }

    #[tokio::test]
    async fn announcing_a_repository_does_not_authorise_invoking_the_agent() {
        // The privilege escalation the agent-owner/repository-owner split
        // closes. Anyone can sign a valid kind-30617 for a repository they
        // invent, so if repository ownership implied invocation authority, any
        // relay user could announce a repo, open an issue under it, tag the
        // agent, and operate somebody else's agent.
        let intruder = Keys::generate();
        let intruder_hex = intruder.public_key().to_hex();

        let announcement = signed(
            &intruder,
            KIND_GIT_REPO_ANNOUNCEMENT,
            vec![tag(&["d", "my-very-own-repo"])],
        );
        let announcement = VerifiedAnnouncement::prove(
            VerifiedProjectEvent::verify(announcement)
                .await
                .expect("valid"),
        )
        .expect("a genuine announcement");
        let mut discovered = DiscoveredRepositories::new();
        let Discovered::Added(coordinate) = discovered.ingest(&announcement) else {
            panic!("a fresh announcement is added");
        };
        assert!(
            coordinate.contains(&intruder_hex),
            "they really do own this repository"
        );

        let root = EventBuilder::new(
            Kind::Custom(KIND_GIT_ISSUE as u16),
            format!("please look, nostr:{AGENT_NPUB}"),
        )
        .tags([
            nostr::Tag::parse(tag(&["a", &coordinate])).unwrap(),
            nostr::Tag::parse(tag(&["p", AGENT_PK])).unwrap(),
        ])
        .sign_with_keys(&intruder)
        .expect("sign");
        let root = VerifiedProjectEvent::verify(root).await.expect("valid");

        // It verifies, routes, and is fully addressed — transport and discovery
        // are candidate selection, and all of that is expected to succeed.
        let candidate = validate_enrolment_candidate(&root, &discovered)
            .expect("a discovered coordinate does route");
        assert_eq!(candidate.owner(), intruder_hex);

        let evidence = AddressingEvidence::resolve(&root, &agent_identity());
        assert!(
            evidence.p_tag_present && evidence.visible_mention,
            "fully addressed, structurally and visibly"
        );

        // And the author is still untrusted, because the agent's owner never
        // approved them.
        let author = classify_project_author(
            &root,
            &agent_identity(),
            Some(OWNER),
            &no_one(),
            None,
            &no_one(),
        );
        assert_eq!(
            author,
            ProjectAuthor::Untrusted,
            "repository ownership is not invocation authority"
        );

        for addressing in ALL_ADDRESSING {
            let effect = classify_project_event(
                classify_kind(KIND_GIT_ISSUE),
                author,
                CallMarker::None,
                RootState::Unknown,
                addressing,
                false,
                false,
            );
            assert_eq!(effect, ProjectEffect::UntrustedContext, "{addressing:?}");
            assert!(!matches!(
                apply_processing_mode(effect, ProcessingMode::Live),
                ProjectEffect::EnrolAndWake | ProjectEffect::Wake | ProjectEffect::Enrol
            ));
        }
    }

    #[tokio::test]
    async fn a_repository_owner_still_holds_lifecycle_authority() {
        // The power that legitimately follows from repository ownership, kept
        // separate from invocation.
        let owner = Keys::generate();
        let owner_hex = owner.public_key().to_hex();
        assert!(
            lifecycle_actor_allowed(&owner_hex, THIRD_PARTY, &owner_hex),
            "the repository owner may close their own root"
        );

        let event = signed(
            &owner,
            KIND_GIT_STATUS_CLOSED,
            vec![tag(&["e", ROOT, "", "root"])],
        );
        let event = VerifiedProjectEvent::verify(event).await.expect("valid");
        assert_eq!(
            classify_project_author(
                &event,
                &agent_identity(),
                Some(OWNER),
                &no_one(),
                None,
                &no_one()
            ),
            ProjectAuthor::Untrusted,
            "…while still not being able to invoke the agent"
        );
    }
}
