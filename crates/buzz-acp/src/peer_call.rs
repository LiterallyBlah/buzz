//! NIP-PC: peer agent calls — envelope parsing, loop controls and the ledger.
//!
//! The wire contract this implements is pinned in `docs/nips/NIP-PC.md`; the
//! parts a second runtime must compute identically (route token, call-id
//! derivation, limits) live in `buzz_core::peer_call` and are shared with the
//! builders in `buzz-sdk`. What is here is everything that depends on *this*
//! process's state: who it trusts, which calls it has already seen, and which
//! of its own calls are still outstanding.
//!
//! # Why an envelope at all
//!
//! An ordinary reply carries a `p` tag for its recipient. Two agents that treat
//! each other's `p` tags as invocations wake each other indefinitely without
//! either one deciding to call anything — Phase 1 records this as the reason
//! `project_call_marker` was hardcoded to [`CallMarker::None`] until the wire
//! format existed. Invocation therefore has to be a distinguishable act, not an
//! inference from tag presence, and that is the entire justification for a
//! separate kind carrying a separate envelope.
//!
//! # Shape of this module
//!
//! Parsing is total and refusal-typed: [`CallEnvelope::parse`] either produces
//! an envelope in which every NIP-PC rule already holds, or a
//! [`EnvelopeReject`] naming the rule that failed. Nothing downstream re-checks
//! the wire, and nothing downstream can construct an envelope without parsing
//! one — the fields are private and there is no other constructor outside
//! tests.
//!
//! Admission is then a pure function of an envelope, this agent's identity, the
//! caller's trust class, and the ledger. It returns a decision; it does not
//! perform an effect. The ledger is written only after the decision is taken,
//! by the caller, which is what keeps a refused call from consuming a replay
//! slot or a fan-out slot.

use std::collections::{BTreeMap, BTreeSet};

use buzz_core::peer_call::{
    derive_call_id, is_lowercase_hex, parse_hop, PeerCallRoute, KIND_PEER_CALL,
    KIND_PEER_CALL_RESULT, MAX_CALL_CONTENT_BYTES,
};
use uuid::Uuid;

use crate::project::{project_route_key, CallMarker};

/// An event whose signature and id this process has actually checked.
///
/// The wrapper exists because "verified" is otherwise a claim in a comment.
/// Every parse entry point in this module takes one of these, so there is no
/// route by which an unverified event reaches envelope parsing — a caller
/// holding a bare `nostr::Event` cannot call `parse` at all.
#[derive(Debug, Clone)]
pub(crate) struct VerifiedPeerEvent(nostr::Event);

impl VerifiedPeerEvent {
    /// Verify an inbound event, or refuse it.
    ///
    /// `nostr::Event::verify` checks the id commitment and the Schnorr
    /// signature together, which is what makes "the caller is the author" a
    /// fact about this event rather than a field anybody could write.
    pub(crate) fn verify(event: nostr::Event) -> Option<Self> {
        event.verify().ok().map(|()| Self(event))
    }

    /// Reuse a project-path verification instead of repeating it.
    ///
    /// Sound because [`VerifiedProjectEvent`] has exactly one constructor and
    /// that constructor verifies. This is a conversion between two proofs of
    /// the same fact, not a way to mint one: a caller with an unverified event
    /// cannot produce the input either.
    ///
    /// [`VerifiedProjectEvent`]: crate::project::VerifiedProjectEvent
    pub(crate) fn from_project(event: &crate::project::VerifiedProjectEvent) -> Self {
        Self(event.event().clone())
    }

    pub(crate) fn kind(&self) -> u32 {
        u32::from(self.0.kind.as_u16())
    }

    /// The event's author, lowercased. This is the caller of a call and the
    /// callee of a result — NIP-PC has no separate identity field for either.
    pub(crate) fn author(&self) -> String {
        self.0.pubkey.to_hex().to_ascii_lowercase()
    }

    pub(crate) fn id(&self) -> String {
        self.0.id.to_hex().to_ascii_lowercase()
    }

    pub(crate) fn content(&self) -> &str {
        &self.0.content
    }

    pub(crate) fn event(&self) -> &nostr::Event {
        &self.0
    }

    fn values(&self, name: &str) -> Vec<String> {
        self.0
            .tags
            .iter()
            .filter_map(|t| {
                let s = t.as_slice();
                (s.first().map(String::as_str) == Some(name)).then(|| s.get(1).cloned())?
            })
            .collect()
    }

    /// The sole value of a tag that NIP-PC requires to appear exactly once.
    ///
    /// A duplicate is `None`, not "the first one". Every tag this is used for
    /// is load-bearing — callee, call id, nonce, hop, route — and picking a
    /// winner from a duplicated pair is how an envelope comes to mean one thing
    /// to the validator and another to a reader.
    fn sole(&self, name: &str) -> Option<String> {
        let mut found = self.values(name);
        (found.len() == 1).then(|| found.remove(0))
    }
}

/// Why an envelope is not a valid NIP-PC call or result.
///
/// Every variant names one wire rule. They are distinct rather than collapsed
/// into "malformed" so that a refusal can be logged as the specific thing that
/// failed, and so the tests below can assert *which* rule refused rather than
/// merely that something did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum EnvelopeReject {
    /// Not a peer-call kind at all.
    WrongKind,
    /// `p` absent or duplicated.
    Recipient,
    /// `p` is not a 64-char lowercase hex pubkey.
    RecipientMalformed,
    /// `call` absent, duplicated, or not 64 lowercase hex.
    CallId,
    /// The `call` value is not the derivation of this envelope's own fields.
    CallIdMismatch,
    /// `nonce` absent, duplicated, or not 32 lowercase hex.
    Nonce,
    /// `hop` absent, duplicated, or outside `1..=MAX_HOP`.
    Hop,
    /// `visited` empty, malformed, or repeating an agent.
    Visited,
    /// `visited` does not contain the caller.
    VisitedMissingCaller,
    /// `visited.len()` disagrees with `hop`.
    HopVisitedMismatch,
    /// No route form, both route forms, or a malformed route.
    Route,
    /// Task text empty, or over the content ceiling.
    Task,
    /// Caller and callee are the same agent.
    SelfCall,
}

/// A validated call envelope.
///
/// Private fields with no public constructor: the only way to hold one is to
/// have parsed a verified event, so every accessor below is reporting something
/// that was actually on the wire and actually passed its rule.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CallEnvelope {
    call_id: String,
    caller: String,
    callee: String,
    route: PeerCallRoute,
    /// Resolved during parsing, not at admission.
    ///
    /// A route that cannot produce a session key is a route this agent cannot
    /// act on, so it is a wire failure rather than a late surprise. Deriving it
    /// here also means the admission path has no failure mode left to model: it
    /// reads a `Uuid` that already exists instead of carrying an error arm that
    /// nothing could reach.
    session_key: Uuid,
    hop: u32,
    visited: Vec<String>,
    task: String,
}

impl CallEnvelope {
    /// Parse a verified event as a call, or name the rule that refused it.
    pub(crate) fn parse(event: &VerifiedPeerEvent) -> Result<Self, EnvelopeReject> {
        if event.kind() != KIND_PEER_CALL {
            return Err(EnvelopeReject::WrongKind);
        }

        let caller = event.author();
        let callee = event.sole("p").ok_or(EnvelopeReject::Recipient)?;
        if !is_lowercase_hex(&callee, 64) {
            return Err(EnvelopeReject::RecipientMalformed);
        }
        if caller == callee {
            return Err(EnvelopeReject::SelfCall);
        }

        let call_id = event.sole("call").ok_or(EnvelopeReject::CallId)?;
        if !is_lowercase_hex(&call_id, 64) {
            return Err(EnvelopeReject::CallId);
        }
        let nonce = event.sole("nonce").ok_or(EnvelopeReject::Nonce)?;
        if !is_lowercase_hex(&nonce, buzz_core::peer_call::NONCE_HEX_LEN) {
            return Err(EnvelopeReject::Nonce);
        }
        let hop =
            parse_hop(&event.sole("hop").ok_or(EnvelopeReject::Hop)?).ok_or(EnvelopeReject::Hop)?;

        let visited_raw = event.values("visited");
        if visited_raw.is_empty() {
            return Err(EnvelopeReject::Visited);
        }
        let mut visited: Vec<String> = Vec::with_capacity(visited_raw.len());
        for entry in visited_raw {
            let entry = entry.to_ascii_lowercase();
            if !is_lowercase_hex(&entry, 64) || visited.contains(&entry) {
                return Err(EnvelopeReject::Visited);
            }
            visited.push(entry);
        }
        if !visited.contains(&caller) {
            return Err(EnvelopeReject::VisitedMissingCaller);
        }
        if visited.len() != hop as usize {
            return Err(EnvelopeReject::HopVisitedMismatch);
        }

        let route = parse_route(event)?;
        let session_key = route_session_key(&route).ok_or(EnvelopeReject::Route)?;

        // The id is recomputed rather than trusted. A captured id re-signed
        // toward another callee, or replayed onto another route, fails here —
        // which is the whole reason the id is derived instead of chosen.
        if derive_call_id(&caller, &callee, &route, &nonce) != call_id {
            return Err(EnvelopeReject::CallIdMismatch);
        }

        let task = event.content().to_string();
        if task.trim().is_empty() || task.len() > MAX_CALL_CONTENT_BYTES {
            return Err(EnvelopeReject::Task);
        }

        Ok(Self {
            call_id,
            caller,
            callee,
            route,
            session_key,
            hop,
            visited,
            task,
        })
    }

    pub(crate) fn call_id(&self) -> &str {
        &self.call_id
    }

    pub(crate) fn caller(&self) -> &str {
        &self.caller
    }

    pub(crate) fn callee(&self) -> &str {
        &self.callee
    }

    pub(crate) fn route(&self) -> &PeerCallRoute {
        &self.route
    }

    /// The bounded task. Read by assertions rather than by the harness: what
    /// the agent actually sees is the event itself, rendered into its prompt.
    #[cfg(test)]
    pub(crate) fn task(&self) -> &str {
        &self.task
    }

    pub(crate) fn hop(&self) -> u32 {
        self.hop
    }

    pub(crate) fn visited(&self) -> &[String] {
        &self.visited
    }
}

/// A validated result envelope.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ResultEnvelope {
    call_id: String,
    callee: String,
    caller: String,
    route: PeerCallRoute,
    session_key: Uuid,
    body: String,
}

impl ResultEnvelope {
    /// Parse a verified event as a result, or name the rule that refused it.
    ///
    /// A result carries no nonce, hop or visited set, so there is nothing here
    /// to recompute the call id from. Correlation is instead done against the
    /// outstanding-call ledger by [`admit_result`], which knows what this agent
    /// actually sent — a receiver that recomputed from the result alone would
    /// be checking the sender's arithmetic against itself.
    pub(crate) fn parse(event: &VerifiedPeerEvent) -> Result<Self, EnvelopeReject> {
        if event.kind() != KIND_PEER_CALL_RESULT {
            return Err(EnvelopeReject::WrongKind);
        }

        let callee = event.author();
        let caller = event.sole("p").ok_or(EnvelopeReject::Recipient)?;
        if !is_lowercase_hex(&caller, 64) {
            return Err(EnvelopeReject::RecipientMalformed);
        }
        if caller == callee {
            return Err(EnvelopeReject::SelfCall);
        }

        let call_id = event.sole("call").ok_or(EnvelopeReject::CallId)?;
        if !is_lowercase_hex(&call_id, 64) {
            return Err(EnvelopeReject::CallId);
        }

        let route = parse_route(event)?;
        let session_key = route_session_key(&route).ok_or(EnvelopeReject::Route)?;

        let body = event.content().to_string();
        if body.len() > MAX_CALL_CONTENT_BYTES {
            return Err(EnvelopeReject::Task);
        }

        Ok(Self {
            call_id,
            callee,
            caller,
            route,
            session_key,
            body,
        })
    }

    pub(crate) fn call_id(&self) -> &str {
        &self.call_id
    }

    pub(crate) fn callee(&self) -> &str {
        &self.callee
    }

    #[cfg(test)]
    pub(crate) fn body(&self) -> &str {
        &self.body
    }
}

/// The exact command a callee runs to answer the call it was woken by.
///
/// # Why this is not advice
///
/// A call arrives as an ordinary turn. Everything the harness normally tells an
/// agent about replying — `--reply-to` on a channel, `buzz issues comment` on a
/// project root — produces a *message*, and a message is not a result: it leaves
/// the caller's outstanding call open forever and the correlated answer never
/// happens. An agent that is handed a call and the ordinary reply instruction
/// has been told to do the wrong thing, competently.
///
/// So the call turn carries this instead, and it is spelled out rather than
/// described. The three values a result needs — the original caller, the call
/// id, and the route — are not things an agent can be expected to assemble
/// correctly from the raw tag list under time pressure; the call id in
/// particular is a derived 64-hex string that means nothing on inspection. A
/// command the agent can run verbatim is the difference between a lifecycle that
/// closes and one that only closes in tests.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ResultDirective {
    caller: String,
    call_id: String,
    route: PeerCallRoute,
}

impl ResultDirective {
    /// The `buzz agents call-result` invocation that answers this call.
    ///
    /// The route flags are the same ones `resolve_route` in the CLI accepts, so
    /// the emitted command is one the binary actually parses rather than a
    /// plausible-looking rendering of it.
    pub(crate) fn command(&self) -> String {
        let route = match &self.route {
            PeerCallRoute::Channel {
                channel,
                thread_root,
            } => match thread_root {
                Some(root) => format!("--channel {channel} \\\n  --thread {root}"),
                None => format!("--channel {channel}"),
            },
            PeerCallRoute::Project { coordinate, root } => {
                format!("--project {coordinate} \\\n  --root {root}")
            }
        };
        format!(
            "buzz agents call-result \\\n  \
             --to {} \\\n  \
             --call {} \\\n  \
             {route} \\\n  \
             --body -",
            self.caller, self.call_id
        )
    }

    pub(crate) fn call_id(&self) -> &str {
        &self.call_id
    }
}

/// The result directive for an inbound call, or `None` if the event is not one.
///
/// Verification is repeated here rather than threaded from admission. The
/// alternative was a new field on `QueuedEvent` and `BatchEvent` — and on the
/// seventy-five places that build them — to carry a value that is already
/// written on the event in front of us. What matters is that this cannot
/// *invent* a directive: it goes through the same [`CallEnvelope::parse`] the
/// admission path used, so an event that would not have been admitted produces
/// no command, and the caller supplies the fact that admission happened.
pub(crate) fn result_directive(event: &nostr::Event) -> Option<ResultDirective> {
    let peer = VerifiedPeerEvent::verify(event.clone())?;
    let envelope = CallEnvelope::parse(&peer).ok()?;
    Some(ResultDirective {
        caller: envelope.caller,
        call_id: envelope.call_id,
        route: envelope.route,
    })
}

/// Read the one route form an envelope carries.
///
/// Presence of both forms is a refusal rather than a precedence rule. An event
/// carrying an `h` *and* an `a` names two different conversations, and any
/// tie-break would let a caller aim the result at one surface while the
/// validator bound the call to the other.
fn parse_route(event: &VerifiedPeerEvent) -> Result<PeerCallRoute, EnvelopeReject> {
    let channels = event.values("h");
    let coordinates = event.values("a");
    let roots: Vec<String> = event
        .event()
        .tags
        .iter()
        .filter_map(|t| {
            let s = t.as_slice();
            (s.first().map(String::as_str) == Some("e")).then(|| s.get(1).cloned())?
        })
        .collect();

    match (channels.len(), coordinates.len()) {
        (1, 0) => {
            let channel = Uuid::parse_str(&channels[0]).map_err(|_| EnvelopeReject::Route)?;
            let thread_root = match roots.len() {
                0 => None,
                1 => {
                    let root = roots[0].to_ascii_lowercase();
                    if !is_lowercase_hex(&root, 64) {
                        return Err(EnvelopeReject::Route);
                    }
                    Some(root)
                }
                _ => return Err(EnvelopeReject::Route),
            };
            Ok(PeerCallRoute::Channel {
                channel: channel.to_string(),
                thread_root,
            })
        }
        (0, 1) => {
            // A project call with no root names no conversation, so its `e` is
            // required where a channel's is optional.
            if roots.len() != 1 {
                return Err(EnvelopeReject::Route);
            }
            let root = roots[0].to_ascii_lowercase();
            if !is_lowercase_hex(&root, 64) {
                return Err(EnvelopeReject::Route);
            }
            let coordinate = coordinates[0].clone();
            if coordinate.split(':').count() != 3 || !coordinate.starts_with("30617:") {
                return Err(EnvelopeReject::Route);
            }
            Ok(PeerCallRoute::Project { coordinate, root })
        }
        _ => Err(EnvelopeReject::Route),
    }
}

/// The session/queue key a route resolves to.
///
/// Channel routes key on the channel UUID; project routes key on the UUIDv5 of
/// the root, exactly as Phase 1 derives it. Sharing `project_route_key` rather
/// than re-deriving here is deliberate: two derivations of one session key is
/// how the same issue comes to have two sessions.
pub(crate) fn route_session_key(route: &PeerCallRoute) -> Option<Uuid> {
    match route {
        PeerCallRoute::Channel { channel, .. } => Uuid::parse_str(channel).ok(),
        PeerCallRoute::Project { root, .. } => project_route_key(root),
    }
}

/// How much authority the event's author has here.
///
/// Deliberately not [`crate::project::ProjectAuthor`]: that type also carries
/// `AuthorisedHuman` for enrolment purposes and is derived from project-only
/// inputs. Peer calls are a channel *and* project concern, and the only
/// question they ask is whether this author may invoke — so the type asks that
/// question and no other.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PeerTrust {
    /// This agent. Its own events never invoke it.
    SelfAuthored,
    /// The agent's owner, or a human the owner approved. Strictly more
    /// authorised than a sibling, and the identity behind `buzz agents call`
    /// run by a person.
    Owner,
    /// A verified same-owner NIP-OA sibling, or an owner-approved external
    /// agent pubkey.
    TrustedAgent,
    /// Anyone else on the relay.
    Untrusted,
}

impl PeerTrust {
    /// May an author of this class invoke this agent?
    fn may_invoke(self) -> bool {
        matches!(self, PeerTrust::Owner | PeerTrust::TrustedAgent)
    }
}

/// Why a call was refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum CallRefusal {
    /// This agent authored it. An agent never invokes itself.
    SelfAuthored,
    /// The caller is not a trusted agent or the owner.
    Untrusted,
    /// Addressed to a different agent.
    NotAddressed,
    /// This call id has already been admitted.
    Replay,
    /// This agent is already in the call path.
    Revisit,
}

/// An admitted call, and what handling it requires.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AcceptedCall {
    envelope: CallEnvelope,
    session_key: Uuid,
}

impl AcceptedCall {
    pub(crate) fn envelope(&self) -> &CallEnvelope {
        &self.envelope
    }

    /// The session/queue key this call's turn runs under.
    pub(crate) fn session_key(&self) -> Uuid {
        self.session_key
    }
}

/// Decide an inbound call.
///
/// Pure: it reads the ledger but never writes it. The write happens in
/// [`CallLedger::record_admitted`] once the caller has acted on the decision,
/// so a refused call cannot consume the replay slot that would then refuse the
/// legitimate retry.
///
/// Order is deliberate. Self-authorship first, because an agent's own event is
/// not something to evaluate trust about. Trust before the loop controls,
/// because an untrusted caller must not learn from the refusal which call ids
/// this agent has already seen.
pub(crate) fn admit_call(
    envelope: CallEnvelope,
    agent_hex: &str,
    trust: PeerTrust,
    ledger: &CallLedger,
) -> Result<AcceptedCall, CallRefusal> {
    let agent = agent_hex.to_ascii_lowercase();

    if envelope.caller == agent || trust == PeerTrust::SelfAuthored {
        return Err(CallRefusal::SelfAuthored);
    }
    if !trust.may_invoke() {
        return Err(CallRefusal::Untrusted);
    }
    if envelope.callee != agent {
        return Err(CallRefusal::NotAddressed);
    }
    if ledger.has_seen(&envelope.call_id) {
        return Err(CallRefusal::Replay);
    }
    if envelope.visited.contains(&agent) {
        return Err(CallRefusal::Revisit);
    }

    // Fan-out is deliberately *not* checked here. It is a bound on how many
    // calls an agent may have in flight, which is a fact about the *issuing*
    // side, and it is enforced there — before publication, by the `buzz agents
    // call` gate. A callee-side copy would bound the wrong party: it would
    // refuse a caller's eleventh call after that caller had already spent ten
    // legitimate ones on somebody else.
    let session_key = envelope.session_key;

    Ok(AcceptedCall {
        envelope,
        session_key,
    })
}

/// Why a result was refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ResultRefusal {
    /// This agent authored it.
    SelfAuthored,
    /// No outstanding call bears this id.
    Unknown,
    /// The result's author is not the agent the call was addressed to.
    WrongCallee,
    /// The result landed on a different surface than the call.
    RouteMismatch,
    /// A result for this call was already accepted.
    AlreadyAnswered,
    /// The `p` tag does not name this agent.
    NotAddressed,
}

/// An admitted result, ready to resume its outstanding call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AcceptedResult {
    envelope: ResultEnvelope,
    session_key: Uuid,
}

impl AcceptedResult {
    pub(crate) fn envelope(&self) -> &ResultEnvelope {
        &self.envelope
    }

    pub(crate) fn session_key(&self) -> Uuid {
        self.session_key
    }
}

/// Decide an inbound result against this agent's outstanding calls.
///
/// Trust class is not an input, and that is not an omission. A result is
/// accepted only from the exact pubkey the call was addressed to, which is a
/// stronger condition than any trust class: the callee was already required to
/// be trusted at the moment the call was issued, and re-asking the question now
/// would let a since-revoked peer's legitimate answer be discarded while adding
/// nothing an attacker could not already fail.
pub(crate) fn admit_result(
    envelope: ResultEnvelope,
    agent_hex: &str,
    ledger: &CallLedger,
) -> Result<AcceptedResult, ResultRefusal> {
    let agent = agent_hex.to_ascii_lowercase();

    if envelope.callee == agent {
        return Err(ResultRefusal::SelfAuthored);
    }
    if envelope.caller != agent {
        return Err(ResultRefusal::NotAddressed);
    }
    if ledger.answered.contains(&envelope.call_id) {
        return Err(ResultRefusal::AlreadyAnswered);
    }
    let outstanding = ledger
        .outstanding
        .get(&envelope.call_id)
        .ok_or(ResultRefusal::Unknown)?;
    if outstanding.callee != envelope.callee {
        return Err(ResultRefusal::WrongCallee);
    }
    if outstanding.route != envelope.route {
        return Err(ResultRefusal::RouteMismatch);
    }

    let session_key = envelope.session_key;

    Ok(AcceptedResult {
        envelope,
        session_key,
    })
}

/// A call this agent issued and has not yet had answered.
#[derive(Debug, Clone, PartialEq, Eq)]
struct OutstandingCall {
    callee: String,
    route: PeerCallRoute,
}

/// This process's memory of calls: what it has admitted, and what it awaits.
///
/// Both halves are needed and neither substitutes for the other. `seen` is the
/// replay defence on the receiving side; `outstanding` is what makes a result
/// correlatable rather than merely well-formed. `answered` is kept separately
/// from removing the outstanding entry so that a second result reports
/// `AlreadyAnswered` rather than `Unknown` — the distinction matters when
/// diagnosing a peer that retries.
#[derive(Debug, Default, Clone)]
pub(crate) struct CallLedger {
    seen: BTreeSet<String>,
    outstanding: BTreeMap<String, OutstandingCall>,
    answered: BTreeSet<String>,
}

impl CallLedger {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    fn has_seen(&self, call_id: &str) -> bool {
        self.seen.contains(call_id)
    }

    /// Record that a call was admitted, closing its id against replay.
    pub(crate) fn record_admitted(&mut self, call: &AcceptedCall) {
        self.seen.insert(call.envelope.call_id.clone());
    }

    /// Record a call this agent published, so its result can correlate.
    ///
    /// Deliberately **not** where the fan-out ceiling lives. This runs when the
    /// agent's own call comes back off the wire, which is after the callee could
    /// already have been invoked; a refusal here would not stop an eleventh call
    /// from doing its work, only from having its answer heard. The ceiling is
    /// enforced by the issuing side before publication — see
    /// [`buzz_core::peer_call::MAX_FANOUT`] and the `buzz agents call` gate — and
    /// putting a second, weaker copy of it here would mean two authorities
    /// disagreeing about the same number.
    pub(crate) fn register_outgoing(&mut self, call_id: &str, callee: &str, route: &PeerCallRoute) {
        self.outstanding.insert(
            call_id.to_ascii_lowercase(),
            OutstandingCall {
                callee: callee.to_ascii_lowercase(),
                route: route.clone(),
            },
        );
    }

    /// How many of this agent's calls on `route` are still awaiting a result.
    ///
    /// Reported rather than enforced here: the issuing gate is the authority,
    /// and this is what the harness can say about its own in-process view.
    pub(crate) fn outstanding_on_route(&self, route: &PeerCallRoute) -> usize {
        let token = route.route_token();
        self.outstanding
            .values()
            .filter(|c| c.route.route_token() == token)
            .count()
    }

    /// Close an outstanding call once its result has been accepted.
    pub(crate) fn record_answered(&mut self, result: &AcceptedResult) {
        self.answered.insert(result.envelope.call_id.clone());
        self.outstanding.remove(&result.envelope.call_id);
    }

    pub(crate) fn outstanding_count(&self) -> usize {
        self.outstanding.len()
    }
}

/// Classify a verified event's peer-call marker **for this agent**.
///
/// This is what Phase 1's [`crate::project::project_call_marker`] stood in for.
/// It reports what the event is *to us*, never whether the author is allowed:
/// a call from an untrusted stranger still reports `Invocation`, and the
/// authority decision stays in [`crate::project::classify_project_event`] rather
/// than being hidden inside a parser.
///
/// # Why the agent is an argument
///
/// An earlier version took only the event, so any well-formed call reported
/// `Invocation`. Addressing is not checked by [`CallEnvelope::parse`] — it is
/// checked by [`admit_call`], which the project authority path does not go
/// through — so on a watched root, where events arrive because the agent is
/// *enrolled* rather than because it was named, a call addressed to somebody
/// else woke this agent and ran a turn on a task meant for another. Two trusted
/// agents watching one issue would each answer every call either of them
/// received.
///
/// A call naming another agent is therefore `None`: not a call *to us*. The
/// same holds for a result whose `p` names somebody else — correlating against
/// another agent's result is how a call gets resumed by an answer nobody sent
/// it.
pub(crate) fn call_marker(event: &VerifiedPeerEvent, agent_hex: &str) -> CallMarker {
    let agent = agent_hex.to_ascii_lowercase();
    match event.kind() {
        KIND_PEER_CALL => match CallEnvelope::parse(event) {
            Ok(envelope) if envelope.callee == agent => CallMarker::Invocation,
            _ => CallMarker::None,
        },
        KIND_PEER_CALL_RESULT => match ResultEnvelope::parse(event) {
            Ok(envelope) if envelope.caller == agent => CallMarker::Result,
            _ => CallMarker::None,
        },
        _ => CallMarker::None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use buzz_core::peer_call::onward_context;
    use buzz_core::peer_call::{MAX_FANOUT, MAX_HOP};
    use nostr::{EventBuilder, Keys, Kind, Tag};

    const CHANNEL: &str = "8f377516-7391-47bf-bcc4-249a1028b212";
    const ROOT: &str = "48be1cc2000000000000000000000000000000000000000000000000000000ab";
    const NONCE: &str = "0123456789abcdef0123456789abcdef";

    fn keys(seed: u8) -> Keys {
        let mut bytes = [7u8; 32];
        bytes[31] = seed;
        Keys::new(nostr::SecretKey::from_slice(&bytes).expect("valid secret key"))
    }

    fn hex_of(k: &Keys) -> String {
        k.public_key().to_hex().to_ascii_lowercase()
    }

    fn channel_route() -> PeerCallRoute {
        PeerCallRoute::Channel {
            channel: CHANNEL.into(),
            thread_root: None,
        }
    }

    fn project_route() -> PeerCallRoute {
        PeerCallRoute::Project {
            coordinate: format!("30617:{}:buzz", hex_of(&keys(1))),
            root: ROOT.into(),
        }
    }

    fn route_tags(route: &PeerCallRoute) -> Vec<Tag> {
        match route {
            PeerCallRoute::Channel {
                channel,
                thread_root,
            } => {
                let mut tags = vec![Tag::parse(["h", channel]).unwrap()];
                if let Some(root) = thread_root {
                    tags.push(Tag::parse(["e", root, "", "root"]).unwrap());
                }
                tags
            }
            PeerCallRoute::Project { coordinate, root } => vec![
                Tag::parse(["a", coordinate]).unwrap(),
                Tag::parse(["e", root, "", "root"]).unwrap(),
            ],
        }
    }

    /// Build and sign a call exactly as the wire contract specifies.
    ///
    /// Tests that need a *malformed* call mutate the tag list this produces,
    /// rather than hand-rolling a second builder — so a change to the contract
    /// breaks the positive and negative cases together instead of leaving the
    /// negatives passing against a shape nothing emits.
    fn signed_call(
        caller: &Keys,
        callee_hex: &str,
        route: &PeerCallRoute,
        task: &str,
    ) -> nostr::Event {
        signed_call_with(caller, callee_hex, route, task, |tags| tags)
    }

    fn signed_call_with(
        caller: &Keys,
        callee_hex: &str,
        route: &PeerCallRoute,
        task: &str,
        mutate: impl FnOnce(Vec<Tag>) -> Vec<Tag>,
    ) -> nostr::Event {
        let caller_hex = hex_of(caller);
        let call_id = derive_call_id(&caller_hex, callee_hex, route, NONCE);
        let mut tags = vec![
            Tag::parse(["p", callee_hex]).unwrap(),
            Tag::parse(["call", &call_id]).unwrap(),
            Tag::parse(["nonce", NONCE]).unwrap(),
            Tag::parse(["hop", "1"]).unwrap(),
            Tag::parse(["visited", &caller_hex]).unwrap(),
        ];
        tags.extend(route_tags(route));
        EventBuilder::new(Kind::Custom(KIND_PEER_CALL as u16), task)
            .tags(mutate(tags))
            // `nostr::EventBuilder` silently drops a `p` tag naming the signer
            // unless self-tagging is allowed (`event/builder.rs:436`). Without
            // this, a test that builds a self-call gets an envelope with no `p`
            // at all and is refused for the wrong reason — it would prove the
            // recipient rule while claiming to prove the self-call rule. A
            // hostile peer writing raw JSON is under no such constraint, so the
            // faithful adversarial event is the self-tagged one.
            .allow_self_tagging()
            .sign_with_keys(caller)
            .expect("sign")
    }

    fn signed_result(
        callee: &Keys,
        caller_hex: &str,
        call_id: &str,
        route: &PeerCallRoute,
        body: &str,
    ) -> nostr::Event {
        let mut tags = vec![
            Tag::parse(["p", caller_hex]).unwrap(),
            Tag::parse(["call", call_id]).unwrap(),
        ];
        tags.extend(route_tags(route));
        EventBuilder::new(Kind::Custom(KIND_PEER_CALL_RESULT as u16), body)
            .tags(tags)
            .allow_self_tagging()
            .sign_with_keys(callee)
            .expect("sign")
    }

    fn verified(event: nostr::Event) -> VerifiedPeerEvent {
        VerifiedPeerEvent::verify(event).expect("signed by the test helper")
    }

    // ── The functional outcome ────────────────────────────────────────────────

    /// One explicit trusted call invokes the callee exactly once, on the route
    /// it was made from.
    #[test]
    fn a_trusted_call_invokes_the_callee_once_on_its_own_route() {
        let caller = keys(1);
        let callee = keys(2);
        let ledger = CallLedger::new();

        let event = verified(signed_call(
            &caller,
            &hex_of(&callee),
            &channel_route(),
            "do it",
        ));
        let envelope = CallEnvelope::parse(&event).expect("valid envelope");
        let accepted = admit_call(envelope, &hex_of(&callee), PeerTrust::TrustedAgent, &ledger)
            .expect("admitted");

        assert_eq!(accepted.envelope().task(), "do it");
        assert_eq!(accepted.envelope().caller(), hex_of(&caller));
        assert_eq!(accepted.session_key(), Uuid::parse_str(CHANNEL).unwrap());

        // Once, not twice: the same event admitted again after the ledger
        // records it is a replay.
        let mut ledger = ledger;
        ledger.record_admitted(&accepted);
        let again = verified(signed_call(
            &caller,
            &hex_of(&callee),
            &channel_route(),
            "do it",
        ));
        let envelope = CallEnvelope::parse(&again).expect("valid envelope");
        assert_eq!(
            admit_call(envelope, &hex_of(&callee), PeerTrust::TrustedAgent, &ledger),
            Err(CallRefusal::Replay)
        );
    }

    /// A project-routed call resolves to the Phase 1 UUIDv5 session key, so a
    /// call about an issue lands in that issue's session rather than a new one.
    #[test]
    fn a_project_call_resolves_to_the_roots_own_session() {
        let caller = keys(1);
        let callee = keys(2);
        let route = project_route();

        let event = verified(signed_call(
            &caller,
            &hex_of(&callee),
            &route,
            "look at this",
        ));
        let envelope = CallEnvelope::parse(&event).expect("valid envelope");
        let accepted = admit_call(
            envelope,
            &hex_of(&callee),
            PeerTrust::TrustedAgent,
            &CallLedger::new(),
        )
        .expect("admitted");

        assert_eq!(
            accepted.session_key(),
            project_route_key(ROOT).expect("root keys"),
        );
        assert_eq!(accepted.envelope().route(), &route);
    }

    /// One correlated result returns to the caller and closes the call.
    #[test]
    fn one_correlated_result_returns_to_the_caller_and_closes_the_call() {
        let caller = keys(1);
        let callee = keys(2);
        let route = channel_route();
        let call_id = derive_call_id(&hex_of(&caller), &hex_of(&callee), &route, NONCE);

        let mut ledger = CallLedger::new();
        ledger.register_outgoing(&call_id, &hex_of(&callee), &route);
        assert_eq!(ledger.outstanding_count(), 1);

        let event = verified(signed_result(
            &callee,
            &hex_of(&caller),
            &call_id,
            &route,
            "done",
        ));
        let envelope = ResultEnvelope::parse(&event).expect("valid result");
        let accepted = admit_result(envelope, &hex_of(&caller), &ledger).expect("admitted");
        assert_eq!(accepted.envelope().body(), "done");

        ledger.record_answered(&accepted);
        assert_eq!(ledger.outstanding_count(), 0);

        // A second result for the same call is refused rather than resuming
        // the call a second time.
        let again = verified(signed_result(
            &callee,
            &hex_of(&caller),
            &call_id,
            &route,
            "done again",
        ));
        let envelope = ResultEnvelope::parse(&again).expect("valid result");
        assert_eq!(
            admit_result(envelope, &hex_of(&caller), &ledger),
            Err(ResultRefusal::AlreadyAnswered)
        );
    }

    /// An ordinary agent reply creates no call. This is the reply-loop case:
    /// the event is a kind:1 comment that `p`-tags the agent, exactly as
    /// Desktop writes them.
    #[test]
    fn an_ordinary_agent_reply_is_not_a_call() {
        let peer = keys(3);
        let agent = keys(2);
        let reply = EventBuilder::new(Kind::Custom(1), "thanks, looking now")
            .tags(vec![
                Tag::parse(["p", &hex_of(&agent)]).unwrap(),
                Tag::parse(["e", ROOT, "", "root"]).unwrap(),
            ])
            .sign_with_keys(&peer)
            .expect("sign");

        let event = verified(reply);
        assert_eq!(call_marker(&event, &hex_of(&agent)), CallMarker::None);
        assert_eq!(CallEnvelope::parse(&event), Err(EnvelopeReject::WrongKind));
    }

    // ── Loop controls ─────────────────────────────────────────────────────────

    #[test]
    fn an_agent_cannot_call_itself() {
        let agent = keys(2);
        // Signed by the agent, addressed to the agent. Refused at parse: the
        // envelope is not merely unauthorised, it is not a call.
        let event = verified(signed_call(
            &agent,
            &hex_of(&agent),
            &channel_route(),
            "loop",
        ));
        assert_eq!(CallEnvelope::parse(&event), Err(EnvelopeReject::SelfCall));
    }

    #[test]
    fn an_agents_own_call_to_someone_else_never_invokes_itself() {
        let agent = keys(2);
        let other = keys(3);
        let event = verified(signed_call(
            &agent,
            &hex_of(&other),
            &channel_route(),
            "task",
        ));
        let envelope = CallEnvelope::parse(&event).expect("valid envelope");
        assert_eq!(
            admit_call(
                envelope,
                &hex_of(&agent),
                PeerTrust::SelfAuthored,
                &CallLedger::new()
            ),
            Err(CallRefusal::SelfAuthored)
        );
    }

    #[test]
    fn an_agent_already_in_the_call_path_is_not_revisited() {
        let caller = keys(1);
        let callee = keys(2);
        let caller_hex = hex_of(&caller);
        let callee_hex = hex_of(&callee);
        let route = channel_route();
        let call_id = derive_call_id(&caller_hex, &callee_hex, &route, NONCE);

        // hop 2 with the callee already visited — a cycle back to an agent
        // that is upstream in this very chain.
        let mut tags = vec![
            Tag::parse(["p", &callee_hex]).unwrap(),
            Tag::parse(["call", &call_id]).unwrap(),
            Tag::parse(["nonce", NONCE]).unwrap(),
            Tag::parse(["hop", "2"]).unwrap(),
            Tag::parse(["visited", &caller_hex]).unwrap(),
            Tag::parse(["visited", &callee_hex]).unwrap(),
        ];
        tags.extend(route_tags(&route));
        let event = verified(
            EventBuilder::new(Kind::Custom(KIND_PEER_CALL as u16), "come back")
                .tags(tags)
                .allow_self_tagging()
                .sign_with_keys(&caller)
                .expect("sign"),
        );

        let envelope = CallEnvelope::parse(&event).expect("structurally valid");
        assert_eq!(
            admit_call(
                envelope,
                &callee_hex,
                PeerTrust::TrustedAgent,
                &CallLedger::new()
            ),
            Err(CallRefusal::Revisit)
        );
    }

    #[test]
    fn a_call_deeper_than_the_ceiling_does_not_parse() {
        let caller = keys(1);
        let callee = keys(2);
        let route = channel_route();
        let over = (MAX_HOP + 1).to_string();
        let event = verified(signed_call_with(
            &caller,
            &hex_of(&callee),
            &route,
            "too deep",
            |tags| {
                tags.into_iter()
                    .map(|t| {
                        if t.as_slice().first().map(String::as_str) == Some("hop") {
                            Tag::parse(["hop", &over]).unwrap()
                        } else {
                            t
                        }
                    })
                    .collect()
            },
        ));
        assert_eq!(CallEnvelope::parse(&event), Err(EnvelopeReject::Hop));
    }

    #[test]
    fn a_hop_count_that_disagrees_with_the_path_is_refused() {
        let caller = keys(1);
        let callee = keys(2);
        // hop says 2, visited carries one agent. Accepting either reading would
        // let a caller launder depth.
        let event = verified(signed_call_with(
            &caller,
            &hex_of(&callee),
            &channel_route(),
            "shallow path, deep claim",
            |tags| {
                tags.into_iter()
                    .map(|t| {
                        if t.as_slice().first().map(String::as_str) == Some("hop") {
                            Tag::parse(["hop", "2"]).unwrap()
                        } else {
                            t
                        }
                    })
                    .collect()
            },
        ));
        assert_eq!(
            CallEnvelope::parse(&event),
            Err(EnvelopeReject::HopVisitedMismatch)
        );
    }

    #[test]
    fn the_ledger_counts_outstanding_calls_per_route_not_in_total() {
        let caller = keys(1);
        let callee = keys(2);
        let route = channel_route();
        let elsewhere = project_route();
        let mut ledger = CallLedger::new();

        for i in 0..MAX_FANOUT {
            let nonce = format!("{i:032x}");
            let call_id = derive_call_id(&hex_of(&caller), &hex_of(&callee), &route, &nonce);
            ledger.register_outgoing(&call_id, &hex_of(&callee), &route);
        }
        let other_id = derive_call_id(&hex_of(&caller), &hex_of(&callee), &elsewhere, NONCE);
        ledger.register_outgoing(&other_id, &hex_of(&callee), &elsewhere);

        assert_eq!(ledger.outstanding_on_route(&route), MAX_FANOUT);
        assert_eq!(
            ledger.outstanding_on_route(&elsewhere),
            1,
            "a route's budget is its own, not a share of one global throttle"
        );
        assert_eq!(ledger.outstanding_count(), MAX_FANOUT + 1);
    }

    /// The ledger does **not** refuse an eleventh call, and that is deliberate.
    ///
    /// It learns of a call only when the agent's own event returns from the
    /// relay, by which point the callee may already be running the task. A
    /// refusal here would discard the answer to work that was done anyway, so
    /// the ceiling lives in the issuing gate before publication instead (see
    /// `buzz-cli`'s `issuing_gate` tests). This asserts the ledger's silence so
    /// that reintroducing a check here has to break a test that says why not.
    #[test]
    fn the_ledger_does_not_pretend_to_bound_what_has_already_been_published() {
        let caller = keys(1);
        let callee = keys(2);
        let route = channel_route();
        let mut ledger = CallLedger::new();

        for i in 0..=MAX_FANOUT {
            let nonce = format!("{i:032x}");
            let call_id = derive_call_id(&hex_of(&caller), &hex_of(&callee), &route, &nonce);
            ledger.register_outgoing(&call_id, &hex_of(&callee), &route);
        }
        assert_eq!(
            ledger.outstanding_on_route(&route),
            MAX_FANOUT + 1,
            "a call that reached the wire is recorded, so its result can still correlate"
        );
    }

    // ── Trust ─────────────────────────────────────────────────────────────────

    #[test]
    fn an_untrusted_relay_identity_cannot_invoke_an_agent() {
        let stranger = keys(9);
        let agent = keys(2);
        let event = verified(signed_call(
            &stranger,
            &hex_of(&agent),
            &channel_route(),
            "run this for me",
        ));
        let envelope = CallEnvelope::parse(&event).expect("well-formed but unauthorised");
        assert_eq!(
            admit_call(
                envelope,
                &hex_of(&agent),
                PeerTrust::Untrusted,
                &CallLedger::new()
            ),
            Err(CallRefusal::Untrusted)
        );
    }

    /// The refusal is about trust and nothing else: the identical envelope from
    /// a trusted author is admitted. Without this the untrusted case above
    /// would also pass if the envelope were simply broken.
    #[test]
    fn the_same_envelope_from_a_trusted_author_is_admitted() {
        for trust in [PeerTrust::TrustedAgent, PeerTrust::Owner] {
            let caller = keys(9);
            let agent = keys(2);
            let event = verified(signed_call(
                &caller,
                &hex_of(&agent),
                &channel_route(),
                "run this for me",
            ));
            let envelope = CallEnvelope::parse(&event).expect("well-formed");
            assert!(
                admit_call(envelope, &hex_of(&agent), trust, &CallLedger::new()).is_ok(),
                "{trust:?} should be able to invoke"
            );
        }
    }

    #[test]
    fn a_call_addressed_to_another_agent_is_not_ours_to_answer() {
        let caller = keys(1);
        let intended = keys(2);
        let bystander = keys(3);
        let event = verified(signed_call(
            &caller,
            &hex_of(&intended),
            &channel_route(),
            "for you",
        ));
        let envelope = CallEnvelope::parse(&event).expect("valid envelope");
        assert_eq!(
            admit_call(
                envelope,
                &hex_of(&bystander),
                PeerTrust::TrustedAgent,
                &CallLedger::new()
            ),
            Err(CallRefusal::NotAddressed)
        );
    }

    // ── Correlation cannot be forged ──────────────────────────────────────────

    #[test]
    fn a_third_party_cannot_answer_for_the_callee() {
        let caller = keys(1);
        let callee = keys(2);
        let impostor = keys(3);
        let route = channel_route();
        let call_id = derive_call_id(&hex_of(&caller), &hex_of(&callee), &route, NONCE);

        let mut ledger = CallLedger::new();
        ledger.register_outgoing(&call_id, &hex_of(&callee), &route);

        let event = verified(signed_result(
            &impostor,
            &hex_of(&caller),
            &call_id,
            &route,
            "I'll take that",
        ));
        let envelope = ResultEnvelope::parse(&event).expect("well-formed");
        assert_eq!(
            admit_result(envelope, &hex_of(&caller), &ledger),
            Err(ResultRefusal::WrongCallee)
        );
    }

    #[test]
    fn a_result_for_a_call_we_never_made_is_not_a_prompt() {
        let caller = keys(1);
        let callee = keys(2);
        let route = channel_route();
        let call_id = derive_call_id(&hex_of(&caller), &hex_of(&callee), &route, NONCE);

        let event = verified(signed_result(
            &callee,
            &hex_of(&caller),
            &call_id,
            &route,
            "here you go",
        ));
        let envelope = ResultEnvelope::parse(&event).expect("well-formed");
        assert_eq!(
            admit_result(envelope, &hex_of(&caller), &CallLedger::new()),
            Err(ResultRefusal::Unknown)
        );
    }

    #[test]
    fn a_result_delivered_to_the_wrong_surface_is_refused() {
        let caller = keys(1);
        let callee = keys(2);
        let route = channel_route();
        let call_id = derive_call_id(&hex_of(&caller), &hex_of(&callee), &route, NONCE);

        let mut ledger = CallLedger::new();
        ledger.register_outgoing(&call_id, &hex_of(&callee), &route);

        // Same id, answered onto the project surface instead.
        let event = verified(signed_result(
            &callee,
            &hex_of(&caller),
            &call_id,
            &project_route(),
            "over here",
        ));
        let envelope = ResultEnvelope::parse(&event).expect("well-formed");
        assert_eq!(
            admit_result(envelope, &hex_of(&caller), &ledger),
            Err(ResultRefusal::RouteMismatch)
        );
    }

    // ── The derivation is load-bearing ────────────────────────────────────────

    #[test]
    fn a_call_id_lifted_from_another_call_does_not_recompute() {
        let caller = keys(1);
        let callee = keys(2);
        let elsewhere = keys(4);
        // An id derived for a different callee, presented on this envelope.
        let stolen = derive_call_id(
            &hex_of(&caller),
            &hex_of(&elsewhere),
            &channel_route(),
            NONCE,
        );
        let event = verified(signed_call_with(
            &caller,
            &hex_of(&callee),
            &channel_route(),
            "task",
            |tags| {
                tags.into_iter()
                    .map(|t| {
                        if t.as_slice().first().map(String::as_str) == Some("call") {
                            Tag::parse(["call", &stolen]).unwrap()
                        } else {
                            t
                        }
                    })
                    .collect()
            },
        ));
        assert_eq!(
            CallEnvelope::parse(&event),
            Err(EnvelopeReject::CallIdMismatch)
        );
    }

    // ── Wire rules ────────────────────────────────────────────────────────────

    #[test]
    fn an_envelope_naming_two_surfaces_is_refused() {
        let caller = keys(1);
        let callee = keys(2);
        let event = verified(signed_call_with(
            &caller,
            &hex_of(&callee),
            &channel_route(),
            "task",
            |mut tags| {
                tags.push(Tag::parse(["a", &format!("30617:{}:buzz", hex_of(&keys(1)))]).unwrap());
                tags
            },
        ));
        assert_eq!(CallEnvelope::parse(&event), Err(EnvelopeReject::Route));
    }

    #[test]
    fn an_envelope_naming_no_surface_is_refused() {
        let caller = keys(1);
        let callee = keys(2);
        let event = verified(signed_call_with(
            &caller,
            &hex_of(&callee),
            &channel_route(),
            "task",
            |tags| {
                tags.into_iter()
                    .filter(|t| t.as_slice().first().map(String::as_str) != Some("h"))
                    .collect()
            },
        ));
        assert_eq!(CallEnvelope::parse(&event), Err(EnvelopeReject::Route));
    }

    #[test]
    fn a_project_call_without_a_root_names_no_conversation() {
        let caller = keys(1);
        let callee = keys(2);
        let event = verified(signed_call_with(
            &caller,
            &hex_of(&callee),
            &project_route(),
            "task",
            |tags| {
                tags.into_iter()
                    .filter(|t| t.as_slice().first().map(String::as_str) != Some("e"))
                    .collect()
            },
        ));
        assert_eq!(CallEnvelope::parse(&event), Err(EnvelopeReject::Route));
    }

    #[test]
    fn a_duplicated_required_tag_is_refused_rather_than_resolved() {
        let caller = keys(1);
        let callee = keys(2);
        let other = keys(3);
        // Two `p` tags: one naming the real callee, one naming somebody else.
        // Picking a winner is how an envelope means two things at once.
        let event = verified(signed_call_with(
            &caller,
            &hex_of(&callee),
            &channel_route(),
            "task",
            |mut tags| {
                tags.push(Tag::parse(["p", &hex_of(&other)]).unwrap());
                tags
            },
        ));
        assert_eq!(CallEnvelope::parse(&event), Err(EnvelopeReject::Recipient));
    }

    #[test]
    fn a_visited_set_that_omits_the_caller_is_refused() {
        let caller = keys(1);
        let callee = keys(2);
        let other = keys(3);
        let event = verified(signed_call_with(
            &caller,
            &hex_of(&callee),
            &channel_route(),
            "task",
            |tags| {
                tags.into_iter()
                    .map(|t| {
                        if t.as_slice().first().map(String::as_str) == Some("visited") {
                            Tag::parse(["visited", &hex_of(&other)]).unwrap()
                        } else {
                            t
                        }
                    })
                    .collect()
            },
        ));
        assert_eq!(
            CallEnvelope::parse(&event),
            Err(EnvelopeReject::VisitedMissingCaller)
        );
    }

    #[test]
    fn an_empty_task_is_not_a_call() {
        let caller = keys(1);
        let callee = keys(2);
        let event = verified(signed_call(
            &caller,
            &hex_of(&callee),
            &channel_route(),
            "   ",
        ));
        assert_eq!(CallEnvelope::parse(&event), Err(EnvelopeReject::Task));
    }

    /// An unsigned or tampered event never reaches parsing at all.
    #[test]
    fn a_tampered_event_does_not_become_verified() {
        let caller = keys(1);
        let callee = keys(2);
        let mut event = signed_call(&caller, &hex_of(&callee), &channel_route(), "task");
        event.content = "something else entirely".into();
        assert!(VerifiedPeerEvent::verify(event).is_none());
    }

    // ── Onward calls ──────────────────────────────────────────────────────────

    #[test]
    fn an_onward_call_carries_the_path_it_inherited() {
        let caller = keys(1);
        let callee = keys(2);
        let event = verified(signed_call(
            &caller,
            &hex_of(&callee),
            &channel_route(),
            "task",
        ));
        let envelope = CallEnvelope::parse(&event).expect("valid");
        let accepted = admit_call(
            envelope,
            &hex_of(&callee),
            PeerTrust::TrustedAgent,
            &CallLedger::new(),
        )
        .expect("admitted");

        let (hop, visited) = onward_context(accepted.envelope().visited(), &hex_of(&callee));
        assert_eq!(hop, 2);
        assert_eq!(visited, vec![hex_of(&caller), hex_of(&callee)]);
        assert_eq!(visited.len(), hop as usize);
    }

    #[test]
    fn the_call_marker_distinguishes_a_call_from_its_result() {
        let caller = keys(1);
        let callee = keys(2);
        let route = channel_route();
        let call = verified(signed_call(&caller, &hex_of(&callee), &route, "task"));
        assert_eq!(call_marker(&call, &hex_of(&callee)), CallMarker::Invocation);
        // ...and is not an invocation for anybody else on the same root.
        assert_eq!(call_marker(&call, &hex_of(&keys(8))), CallMarker::None);

        let call_id = derive_call_id(&hex_of(&caller), &hex_of(&callee), &route, NONCE);
        let result = verified(signed_result(
            &callee,
            &hex_of(&caller),
            &call_id,
            &route,
            "done",
        ));
        assert_eq!(call_marker(&result, &hex_of(&caller)), CallMarker::Result);
        assert_eq!(call_marker(&result, &hex_of(&keys(8))), CallMarker::None);
    }

    // ── The writer and the judge agree ────────────────────────────────────────
    //
    // These are the tests that make NIP-PC a contract rather than two opinions.
    // Everything above builds its events by hand, which proves the validator
    // enforces its rules but says nothing about whether anything *emits* a
    // conforming call. Here the event comes from `buzz-sdk` — the same builder
    // `buzz agents call` uses — and is judged by the production admission path.
    // A drift between builder and validator fails here and nowhere else.

    use buzz_sdk::builders::{build_peer_call, build_peer_call_result, PeerCallMeta};

    fn sdk_call(
        caller: &Keys,
        callee_hex: &str,
        route: &PeerCallRoute,
        task: &str,
        hop: u32,
        visited: Vec<String>,
    ) -> nostr::Event {
        build_peer_call(
            &hex_of(caller),
            task,
            &PeerCallMeta {
                callee: callee_hex.to_string(),
                route: route.clone(),
                nonce: NONCE.to_string(),
                hop,
                visited,
            },
        )
        .expect("the builder accepts a well-formed call")
        .sign_with_keys(caller)
        .expect("sign")
    }

    #[test]
    fn a_call_built_by_the_sdk_is_admitted_by_the_harness() {
        for route in [channel_route(), project_route()] {
            let caller = keys(1);
            let callee = keys(2);
            let event = verified(sdk_call(
                &caller,
                &hex_of(&callee),
                &route,
                "summarise the thread",
                1,
                vec![hex_of(&caller)],
            ));

            let envelope = CallEnvelope::parse(&event)
                .unwrap_or_else(|e| panic!("builder emitted a call the validator refused: {e:?}"));
            let accepted = admit_call(
                envelope,
                &hex_of(&callee),
                PeerTrust::TrustedAgent,
                &CallLedger::new(),
            )
            .expect("admitted");

            assert_eq!(accepted.envelope().route(), &route);
            assert_eq!(accepted.envelope().task(), "summarise the thread");
            assert_eq!(
                accepted.session_key(),
                route_session_key(&route).expect("route keys"),
                "the turn must run on the surface the call came from"
            );
        }
    }

    /// The full loop: caller issues, callee answers, caller correlates. Both
    /// events come from the builders, and the ledger is the production one.
    #[test]
    fn a_call_and_its_result_complete_a_round_trip_through_the_builders() {
        let caller = keys(1);
        let callee = keys(2);
        let route = project_route();

        let call = verified(sdk_call(
            &caller,
            &hex_of(&callee),
            &route,
            "check the failing test",
            1,
            vec![hex_of(&caller)],
        ));
        let envelope = CallEnvelope::parse(&call).expect("valid call");
        let call_id = envelope.call_id().to_string();

        // Caller side: register what it just published.
        let mut caller_ledger = CallLedger::new();
        caller_ledger.register_outgoing(&call_id, &hex_of(&callee), &route);

        // Callee side: admit the call exactly once.
        let mut callee_ledger = CallLedger::new();
        let accepted = admit_call(
            envelope,
            &hex_of(&callee),
            PeerTrust::TrustedAgent,
            &callee_ledger,
        )
        .expect("admitted");
        callee_ledger.record_admitted(&accepted);

        // Callee answers with the builder the CLI uses.
        let result =
            build_peer_call_result(&hex_of(&caller), &call_id, "it was the fixture", &route)
                .expect("valid result")
                .sign_with_keys(&callee)
                .expect("sign");
        let result = verified(result);

        let result_envelope = ResultEnvelope::parse(&result)
            .unwrap_or_else(|e| panic!("builder emitted a result the validator refused: {e:?}"));
        let accepted_result =
            admit_result(result_envelope, &hex_of(&caller), &caller_ledger).expect("correlated");

        assert_eq!(accepted_result.envelope().body(), "it was the fixture");
        assert_eq!(
            accepted_result.session_key(),
            route_session_key(&route).expect("route keys"),
            "the result lands on the originating surface"
        );

        caller_ledger.record_answered(&accepted_result);
        assert_eq!(caller_ledger.outstanding_count(), 0);
    }

    /// An accepted call's onward context feeds the builder, and the chain is
    /// admitted one level deeper. This is what makes the depth ceiling a real
    /// bound rather than a field nobody increments.
    #[test]
    fn a_chain_of_calls_deepens_until_the_ceiling_refuses_it() {
        let first = keys(1);
        let second = keys(2);
        let third = keys(3);
        let fourth = keys(4);
        let route = channel_route();

        // Depth 1: first → second.
        let call = verified(sdk_call(
            &first,
            &hex_of(&second),
            &route,
            "delegate onward",
            1,
            vec![hex_of(&first)],
        ));
        let accepted = admit_call(
            CallEnvelope::parse(&call).expect("valid"),
            &hex_of(&second),
            PeerTrust::TrustedAgent,
            &CallLedger::new(),
        )
        .expect("admitted");

        // Depth 2: second → third, carrying the inherited path.
        let (hop, visited) = onward_context(accepted.envelope().visited(), &hex_of(&second));
        assert_eq!(hop, 2);
        let onward = verified(sdk_call(
            &second,
            &hex_of(&third),
            &route,
            "your turn",
            hop,
            visited,
        ));
        let accepted = admit_call(
            CallEnvelope::parse(&onward).expect("valid at depth 2"),
            &hex_of(&third),
            PeerTrust::TrustedAgent,
            &CallLedger::new(),
        )
        .expect("admitted at depth 2");

        // Depth 3: third → fourth. Still inside the ceiling.
        let (hop, visited) = onward_context(accepted.envelope().visited(), &hex_of(&third));
        assert_eq!(hop, 3);
        let onward = verified(sdk_call(
            &third,
            &hex_of(&fourth),
            &route,
            "last one",
            hop,
            visited,
        ));
        let accepted = admit_call(
            CallEnvelope::parse(&onward).expect("valid at depth 3"),
            &hex_of(&fourth),
            PeerTrust::TrustedAgent,
            &CallLedger::new(),
        )
        .expect("admitted at depth 3");

        // Depth 4: refused by the builder, so a conforming caller cannot even
        // publish it. The validator refuses it too — proved separately by
        // `a_call_deeper_than_the_ceiling_does_not_parse` — so a caller that
        // ignores the builder gains nothing.
        let (hop, visited) = onward_context(accepted.envelope().visited(), &hex_of(&fourth));
        assert_eq!(hop, MAX_HOP + 1);
        let refused = build_peer_call(
            &hex_of(&fourth),
            "one too deep",
            &PeerCallMeta {
                callee: hex_of(&keys(5)),
                route: route.clone(),
                nonce: NONCE.to_string(),
                hop,
                visited,
            },
        );
        assert!(
            refused.is_err(),
            "the builder must not emit a call past the ceiling"
        );
    }

    #[test]
    fn the_builder_refuses_the_envelopes_the_validator_would_have_to_reject() {
        let caller = keys(1);
        let callee = keys(2);
        let route = channel_route();
        let meta = |hop: u32, visited: Vec<String>, callee: String| PeerCallMeta {
            callee,
            route: route.clone(),
            nonce: NONCE.to_string(),
            hop,
            visited,
        };

        // Self-call.
        assert!(build_peer_call(
            &hex_of(&caller),
            "task",
            &meta(1, vec![hex_of(&caller)], hex_of(&caller))
        )
        .is_err());

        // Caller missing from its own path.
        assert!(build_peer_call(
            &hex_of(&caller),
            "task",
            &meta(1, vec![hex_of(&keys(7))], hex_of(&callee))
        )
        .is_err());

        // Hop disagreeing with the path.
        assert!(build_peer_call(
            &hex_of(&caller),
            "task",
            &meta(2, vec![hex_of(&caller)], hex_of(&callee))
        )
        .is_err());

        // Callee already in the path — a cycle, refused before publication.
        assert!(build_peer_call(
            &hex_of(&caller),
            "task",
            &meta(2, vec![hex_of(&caller), hex_of(&callee)], hex_of(&callee))
        )
        .is_err());

        // Empty task.
        assert!(build_peer_call(
            &hex_of(&caller),
            "   ",
            &meta(1, vec![hex_of(&caller)], hex_of(&callee))
        )
        .is_err());

        // The control: with all of the above corrected, the builder emits.
        assert!(build_peer_call(
            &hex_of(&caller),
            "task",
            &meta(1, vec![hex_of(&caller)], hex_of(&callee))
        )
        .is_ok());
    }

    /// A malformed call is not silently downgraded into "no marker at all" by
    /// accident of ordering: it reports `None` because it is not a usable call,
    /// and the authority path treats `None` from a trusted agent as ignore.
    #[test]
    fn a_malformed_call_presents_no_marker() {
        let caller = keys(1);
        let callee = keys(2);
        let event = verified(signed_call_with(
            &caller,
            &hex_of(&callee),
            &channel_route(),
            "task",
            |tags| {
                tags.into_iter()
                    .filter(|t| t.as_slice().first().map(String::as_str) != Some("nonce"))
                    .collect()
            },
        ));
        assert_eq!(call_marker(&event, &hex_of(&callee)), CallMarker::None);
    }
}
