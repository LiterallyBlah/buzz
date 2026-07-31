//! Project (issue / pull-request) routing primitives.
//!
//! Channel routing keys off the subscription id (`ch-<uuid>`) because a channel
//! *is* a UUID. A project root is a 64-char event id, so project routing keys
//! off the event's own root reference instead, mapped through a deterministic
//! UUIDv5 so every downstream mechanism (session isolation, queueing, dedup,
//! turn counts, backpressure, cancellation) keeps working untouched.
//!
//! Everything in this module is pure and inert: nothing here opens a
//! subscription or fires a turn. It is the shared vocabulary that the project
//! REQ/dispatch work builds on, and — critically — that the Hermes Buzz adapter
//! must reimplement byte-for-byte. Where a rule is a cross-runtime invariant it
//! is called out as such.

// The subscription and dispatch work that consumes these lands in the next
// change; until it does, only the tests below call into the module. Landing the
// vocabulary first is deliberate — the Hermes adapter has to agree with these
// exact rules, and pinning them under test is what makes that agreement
// checkable rather than aspirational.
#![allow(dead_code)]

use std::collections::{BTreeMap, BTreeSet};

use serde_json::{json, Value};
use uuid::Uuid;

use buzz_core::kind::{
    KIND_GIT_ISSUE, KIND_GIT_PR_UPDATE, KIND_GIT_PULL_REQUEST, KIND_GIT_REPO_ANNOUNCEMENT,
    KIND_GIT_STATUS_CLOSED, KIND_GIT_STATUS_DRAFT, KIND_GIT_STATUS_MERGED, KIND_GIT_STATUS_OPEN,
    KIND_TEXT_NOTE,
};

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

/// Subscription id for the watched-root REQ (`#e` / `#E`): follow-up traffic on
/// roots this agent is already enrolled in, whether active or dormant.
pub(crate) const PROJECT_ROOTS_SUB_ID: &str = "proj-roots";

/// Does this subscription id belong to the project dispatch branch?
///
/// The counterpart of `channel_id_from_sub_id`: project REQs carry no channel
/// UUID, so the sub id only selects the branch. The route key is then derived
/// from the event's own root reference, not from the subscription.
pub(crate) fn is_project_sub_id(sub_id: &str) -> bool {
    sub_id.starts_with(PROJECT_SUB_ID_PREFIX)
}

/// Canonicalise a root event id for hashing: 64 hex characters, nothing else.
///
/// **Cross-runtime invariant.** The hashed input is the *lowercase* hex id.
/// Accepting an uppercase id and hashing it as-is would produce a second,
/// silently different route key for the same root. Case folding is the only
/// normalisation performed: whitespace padding is rejected rather than trimmed,
/// because trimming coerces malformed input into a plausible-looking session
/// and the contract here is fail-closed.
fn canonical_root_id(raw: &str) -> Option<String> {
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

/// Which root event a project event belongs to.
///
/// A root announces itself (`1621`/`1618` are their own root). Comments and
/// status events reference it with lowercase `e`; **PR updates use uppercase
/// `E`** (`crates/buzz-sdk/src/builders.rs:1444`). A lowercase-only lookup
/// silently drops every PR revision, which is the whole reason this is one
/// function rather than an inline tag scan at each call site.
///
/// `tags` is the event's raw tag list; `event_id` its own id; `kind` its kind.
pub(crate) fn root_event_id<T, S>(kind: u32, event_id: &str, tags: &[T]) -> Option<String>
where
    T: AsRef<[S]>,
    S: AsRef<str>,
{
    match kind {
        KIND_GIT_ISSUE | KIND_GIT_PULL_REQUEST => canonical_root_id(event_id),
        KIND_GIT_PR_UPDATE => first_ref(tags, "E"),
        KIND_TEXT_NOTE
        | KIND_GIT_STATUS_OPEN
        | KIND_GIT_STATUS_MERGED
        | KIND_GIT_STATUS_CLOSED
        | KIND_GIT_STATUS_DRAFT => first_ref(tags, "e"),
        _ => None,
    }
}

/// First usable value for `name`, preferring an explicit `"root"` marker.
///
/// Both comment and status builders emit `["e", root, "", "root"]`, and status
/// events may carry a second `["e", revision, "", "reply"]`
/// (`builders.rs:1230-1234`). Taking the first `e` happens to work today only
/// because the root is written first; honouring the marker means a reordered or
/// reply-carrying event still resolves to the root rather than to a revision.
fn first_ref<T, S>(tags: &[T], name: &str) -> Option<String>
where
    T: AsRef<[S]>,
    S: AsRef<str>,
{
    let matching = || {
        tags.iter().filter_map(|t| {
            let t = t.as_ref();
            let key = t.first()?.as_ref();
            (key == name).then_some(t)
        })
    };

    let marked = matching()
        .find(|t| t.get(3).map(|m| m.as_ref()) == Some("root"))
        .and_then(|t| t.get(1))
        .and_then(|v| canonical_root_id(v.as_ref()));
    if marked.is_some() {
        return marked;
    }

    matching()
        .filter_map(|t| t.get(1))
        .find_map(|v| canonical_root_id(v.as_ref()))
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

// ── Discovered repositories ───────────────────────────────────────────────────

/// Repository coordinates this agent has actually discovered.
///
/// **Opaque on purpose.** The backing set is private and there is no production
/// insertion method yet, so no caller can assemble a plausible-looking
/// coordinate and hand it to [`validate_enrolment_candidate`] to get a
/// "validated" candidate back. Private fields on the candidate stop
/// struct-literal forgery; this stops validator-assisted forgery, which is the
/// same hole reached one step earlier.
///
/// The production ingestion path arrives with the discovery slice and will
/// derive each coordinate from a signature-verified `kind:30617` event — the
/// kind, the **signer's** pubkey as owner, and one non-empty `d` tag as
/// identifier. An announcement's own `a` claim is never trusted: it is
/// attacker-chosen data inside an otherwise authentic event.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct DiscoveredRepositories {
    coordinates: BTreeSet<String>,
}

impl DiscoveredRepositories {
    /// An agent that has discovered nothing yet. The only production
    /// constructor until ingestion lands.
    pub(crate) fn new() -> Self {
        Self::default()
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
        Self {
            coordinates: coordinates.into_iter().map(Into::into).collect(),
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

    pub(crate) fn is_pull_request(&self) -> bool {
        self.is_pull_request
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
pub(crate) fn validate_enrolment_candidate<T, S>(
    kind: u32,
    event_id: &str,
    tags: &[T],
    discovered: &DiscoveredRepositories,
) -> Option<EnrolmentCandidate>
where
    T: AsRef<[S]>,
    S: AsRef<str>,
{
    let is_pull_request = match kind {
        KIND_GIT_ISSUE => false,
        KIND_GIT_PULL_REQUEST => true,
        _ => return None,
    };
    let root = canonical_root_id(event_id)?;
    let coordinate = sole_value(tags, "a")?;
    if !discovered.contains(&coordinate) {
        return None;
    }
    Some(EnrolmentCandidate {
        root,
        coordinate,
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

/// Does a follow-up event on a watched root carry an acceptable `a`?
///
/// The rule is event-class-specific because the builders genuinely differ, and
/// a blanket "missing is fine" would let a malformed comment into the project
/// path on the strength of an `#e` match alone:
///
/// | Kind | `a` required? |
/// |---|---|
/// | `1` comment | yes — `projectIssues.mjs` always emits it |
/// | `1619` PR update | yes — `builders.rs:1434` always emits it |
/// | `1630`-`1633` lifecycle | optional — `GitStatusMeta.repo` is `Option` |
///
/// A duplicated `a` is rejected for every class: two coordinates on one event
/// is ambiguity, not redundancy. When present, the match is byte-identical to
/// the enrolled coordinate, for the same reason as at enrolment.
pub(crate) fn follow_up_coordinate_allowed<T, S>(kind: u32, tags: &[T], enrolled: &str) -> bool
where
    T: AsRef<[S]>,
    S: AsRef<str>,
{
    let count = tags
        .iter()
        .filter(|t| t.as_ref().first().map(|k| k.as_ref()) == Some("a"))
        .count();

    match count {
        0 => matches!(
            kind,
            KIND_GIT_STATUS_OPEN
                | KIND_GIT_STATUS_MERGED
                | KIND_GIT_STATUS_CLOSED
                | KIND_GIT_STATUS_DRAFT
        ),
        1 => sole_value(tags, "a").as_deref() == Some(enrolled),
        _ => false,
    }
}

// ── Enrolment sets ────────────────────────────────────────────────────────────

/// One enrolled root.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Enrolment {
    pub coordinate: String,
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
    pub(crate) fn enrol(
        &mut self,
        candidate: &EnrolmentCandidate,
    ) -> Result<EnrolOutcome, BindingMismatch> {
        let attempted = Enrolment {
            coordinate: candidate.coordinate().to_string(),
            is_pull_request: candidate.is_pull_request(),
        };

        if let Some(existing) = self.get(candidate.root()) {
            if *existing != attempted {
                return Err(BindingMismatch {
                    root: candidate.root().to_string(),
                    existing: existing.clone(),
                    attempted,
                });
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
        "since": since,
    }))
}

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
            "kinds": [
                KIND_TEXT_NOTE,
                KIND_GIT_STATUS_OPEN,
                KIND_GIT_STATUS_MERGED,
                KIND_GIT_STATUS_CLOSED,
                KIND_GIT_STATUS_DRAFT,
            ],
            "#e": roots,
            "since": since,
        }));
    }

    let pr_roots = enrolments.pull_request_roots();
    if !pr_roots.is_empty() {
        filters.push(json!({
            "kinds": [KIND_GIT_PR_UPDATE],
            "#E": pr_roots,
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
        _ => KindEffect::Ignore,
    }
}

/// May `author` change this root's lifecycle?
///
/// Root author or repo owner only, matching `allowedActorsForRoot`
/// (`desktop/src/features/projects/projectIssues.mjs:38-45`). An unauthorised
/// status event is ignored, not merely deprioritised.
pub(crate) fn lifecycle_actor_allowed(
    author: &str,
    root_author: &str,
    root_coordinate: Option<&str>,
) -> bool {
    let Some(author) = canonical_root_id(author) else {
        return false;
    };
    if canonical_root_id(root_author).as_deref() == Some(author.as_str()) {
        return true;
    }
    root_coordinate
        .and_then(repo_owner_from_coordinate)
        .is_some_and(|owner| owner == author)
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
    /// The agent's pubkey appears only because an earlier participant list was
    /// copied forward. Never an enrolment signal.
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
pub(crate) fn classify_project_event(
    kind_effect: KindEffect,
    author: ProjectAuthor,
    call: CallMarker,
    root_state: RootState,
    addressing: Addressing,
    lifecycle_authorised: bool,
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
                _ => wake_or_enrol(root_state, addressing),
            },

            // A trusted agent's bare `p` is never an invocation — that is the
            // reply loop. It needs an explicit call envelope.
            ProjectAuthor::TrustedAgent => match call {
                CallMarker::Result => ProjectEffect::ResumeCall,
                // An invocation envelope names its callee, so it is explicit
                // addressing by construction and needs no separate re-tag.
                CallMarker::Invocation => wake_or_enrol(root_state, Addressing::ExplicitMention),
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
/// | `Active` | wake | wake — continuation needs no re-tag |
/// | `Dormant` | reactivate and wake | ignore — stays dormant |
fn wake_or_enrol(root_state: RootState, addressing: Addressing) -> ProjectEffect {
    match root_state {
        // An active root continues without re-tagging: this is the follow-up
        // comment that the enrolment `#p` REQ alone could never deliver.
        RootState::Active => ProjectEffect::Wake,
        RootState::Unknown | RootState::Dormant => match addressing {
            Addressing::ExplicitMention => ProjectEffect::EnrolAndWake,
            Addressing::InheritedParticipant | Addressing::WatchedRoot => ProjectEffect::Ignore,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
        let coord = format!("30617:{OWNER}:repo");
        assert!(!lifecycle_actor_allowed(
            &format!(" {OWNER}"),
            STRANGER,
            Some(&coord)
        ));
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

    #[test]
    fn project_sub_ids_are_recognised() {
        assert!(is_project_sub_id(PROJECT_ENROL_SUB_ID));
        assert!(is_project_sub_id(PROJECT_ROOTS_SUB_ID));
    }

    #[test]
    fn channel_and_control_sub_ids_are_not_project_sub_ids() {
        assert!(!is_project_sub_id(
            "ch-550e8400-e29b-41d4-a716-446655440000"
        ));
        assert!(!is_project_sub_id("membership-notif"));
        assert!(!is_project_sub_id("agent-observer-control"));
        assert!(!is_project_sub_id(""));
    }

    // ── Root extraction ──────────────────────────────────────────────────────

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
    fn lifecycle_authority_ignores_a_wrong_kind_coordinate() {
        // Fail closed: an unparseable coordinate yields no owner, so authority
        // falls back to the root author alone.
        let bogus = format!("30618:{OWNER}:repo");
        assert!(!lifecycle_actor_allowed(OWNER, STRANGER, Some(&bogus)));
        assert!(lifecycle_actor_allowed(STRANGER, STRANGER, Some(&bogus)));
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
        let coord = format!("30617:{OWNER}:repo");
        assert!(lifecycle_actor_allowed(STRANGER, STRANGER, Some(&coord)));
        assert!(lifecycle_actor_allowed(OWNER, STRANGER, Some(&coord)));
    }

    #[test]
    fn a_third_party_may_not_change_lifecycle() {
        let coord = format!("30617:{OWNER}:repo");
        let third = "1111111111111111111111111111111111111111111111111111111111111111";
        assert!(!lifecycle_actor_allowed(third, STRANGER, Some(&coord)));
        // No coordinate to resolve an owner from ⇒ root author only.
        assert!(!lifecycle_actor_allowed(OWNER, STRANGER, None));
    }

    #[test]
    fn lifecycle_authority_is_case_insensitive() {
        let coord = format!("30617:{}:repo", OWNER.to_ascii_uppercase());
        assert!(lifecycle_actor_allowed(OWNER, STRANGER, Some(&coord)));
    }

    // ── Author gate: comments ────────────────────────────────────────────────

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
        )
    }

    const ALL_ADDRESSING: [Addressing; 3] = [
        Addressing::ExplicitMention,
        Addressing::InheritedParticipant,
        Addressing::WatchedRoot,
    ];

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
    fn an_active_root_continues_without_re_tagging() {
        // The regression test for the original Phase 1 gap: a follow-up comment
        // does not tag the agent again, and must still reach the session.
        for addressing in ALL_ADDRESSING {
            assert_eq!(
                comment(
                    ProjectAuthor::AuthorisedHuman,
                    CallMarker::None,
                    RootState::Active,
                    addressing,
                ),
                ProjectEffect::Wake,
                "{addressing:?}"
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

    // ── Author gate: a result marker must not override event class ───────────

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
        );
        assert_ne!(out, ProjectEffect::ResumeCall);
        assert_eq!(out, ProjectEffect::Ignore);
    }

    // ── Author gate: roots and lifecycle ─────────────────────────────────────

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
            ),
            ProjectEffect::Ignore
        );
    }

    // ── Author gate: self-authorship stops turns, not state ──────────────────

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
            ),
            ProjectEffect::ApplyLifecycle
        );
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
            ),
            ProjectEffect::Ignore
        );
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
            ),
            ProjectEffect::RefreshContext
        );
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
    fn unauthorised_status_event_does_not_close_the_watch() {
        assert_eq!(
            classify_project_event(
                KindEffect::Lifecycle,
                ProjectAuthor::AuthorisedHuman,
                CallMarker::None,
                RootState::Active,
                Addressing::ExplicitMention,
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
            ),
            ProjectEffect::ApplyLifecycle
        );
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
                );
                assert!(
                    matches!(out, ProjectEffect::ApplyLifecycle | ProjectEffect::Ignore),
                    "{author:?} / {call:?} produced {out:?}"
                );
            }
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
            ),
            ProjectEffect::UntrustedContext
        );
    }
    // ── Coordinate normalisation ─────────────────────────────────────────────

    #[test]
    fn coordinate_normalises_and_lowercases_owner() {
        assert_eq!(
            normalise_coordinate(&format!("30617:{}:my-repo", OWNER.to_ascii_uppercase())),
            Some(format!("30617:{OWNER}:my-repo"))
        );
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

    // ── Enrolment candidate validation ───────────────────────────────────────

    fn known(coords: &[&str]) -> DiscoveredRepositories {
        DiscoveredRepositories::for_test(coords.iter().map(|c| c.to_string()))
    }

    fn coord() -> String {
        format!("30617:{OWNER}:repo")
    }

    #[test]
    fn a_discovered_issue_root_is_a_valid_candidate() {
        let c = validate_enrolment_candidate(
            KIND_GIT_ISSUE,
            ROOT,
            &tags(&[&["a", &coord()], &["p", STRANGER]]),
            &known(&[&coord()]),
        )
        .expect("should validate");
        assert_eq!(c.root, ROOT);
        assert_eq!(c.coordinate, coord());
        assert!(!c.is_pull_request);
    }

    #[test]
    fn a_pull_request_root_is_marked_as_one() {
        let c = validate_enrolment_candidate(
            KIND_GIT_PULL_REQUEST,
            ROOT,
            &tags(&[&["a", &coord()]]),
            &known(&[&coord()]),
        )
        .expect("should validate");
        assert!(c.is_pull_request);
    }

    #[test]
    fn candidate_validation_fails_closed() {
        let k = known(&[&coord()]);
        // Not a root kind.
        assert!(
            validate_enrolment_candidate(KIND_TEXT_NOTE, ROOT, &tags(&[&["a", &coord()]]), &k)
                .is_none()
        );
        // No `a` tag at all — the real `48be1cc2…` shape. Enrols nobody, no error.
        assert!(validate_enrolment_candidate(
            KIND_GIT_ISSUE,
            ROOT,
            &tags(&[&["subject", "hi"]]),
            &k
        )
        .is_none());
        // Malformed coordinate.
        assert!(validate_enrolment_candidate(
            KIND_GIT_ISSUE,
            ROOT,
            &tags(&[&["a", "nonsense"]]),
            &k
        )
        .is_none());
        // Well-formed but never announced: an `a` tag is an unauthenticated claim.
        let other = format!("30617:{STRANGER}:elsewhere");
        assert!(
            validate_enrolment_candidate(KIND_GIT_ISSUE, ROOT, &tags(&[&["a", &other]]), &k)
                .is_none()
        );
        // Nothing discovered yet ⇒ nothing enrollable.
        assert!(validate_enrolment_candidate(
            KIND_GIT_ISSUE,
            ROOT,
            &tags(&[&["a", &coord()]]),
            &DiscoveredRepositories::new()
        )
        .is_none());
        // Malformed root id.
        assert!(validate_enrolment_candidate(
            KIND_GIT_ISSUE,
            "nope",
            &tags(&[&["a", &coord()]]),
            &k
        )
        .is_none());
    }

    #[test]
    fn a_root_with_two_a_tags_is_ambiguous_not_first_wins() {
        // A forged root could otherwise smuggle a known coordinate past the
        // gate while a second tag says something else entirely.
        let k = known(&[&coord()]);
        let other = format!("30617:{STRANGER}:elsewhere");
        assert!(validate_enrolment_candidate(
            KIND_GIT_ISSUE,
            ROOT,
            &tags(&[&["a", &coord()], &["a", &other]]),
            &k
        )
        .is_none());
        // Even two identical tags: ambiguity is about shape, not values.
        assert!(validate_enrolment_candidate(
            KIND_GIT_ISSUE,
            ROOT,
            &tags(&[&["a", &coord()], &["a", &coord()]]),
            &k
        )
        .is_none());
    }

    #[test]
    fn enrolment_matches_the_discovered_coordinate_byte_for_byte() {
        // Parsing must not widen acceptance: an uppercase-owner coordinate is
        // not silently equivalent to the canonical discovered string.
        let k = known(&[&coord()]);
        let shouty = format!("30617:{}:repo", OWNER.to_ascii_uppercase());
        assert_eq!(
            normalise_coordinate(&shouty).as_deref(),
            Some(coord().as_str())
        );
        assert!(
            validate_enrolment_candidate(KIND_GIT_ISSUE, ROOT, &tags(&[&["a", &shouty]]), &k)
                .is_none(),
            "a non-canonical coordinate must not match the discovered set through the parser"
        );
    }

    #[test]
    fn a_caller_cannot_hand_the_validator_a_fabricated_repository_set() {
        // Private candidate fields close struct-literal forgery; this closes
        // validator-assisted forgery. With no production insertion method, a
        // freshly built `DiscoveredRepositories` admits nothing.
        let fabricated = format!("30617:{STRANGER}:looks-plausible");
        assert!(validate_enrolment_candidate(
            KIND_GIT_ISSUE,
            ROOT,
            &tags(&[&["a", &fabricated]]),
            &DiscoveredRepositories::new(),
        )
        .is_none());
    }

    #[test]
    fn discovered_repositories_starts_empty_in_production() {
        let d = DiscoveredRepositories::new();
        assert!(d.is_empty());
        assert_eq!(d.len(), 0);
        assert!(!d.contains(&coord()));
        assert_eq!(d.iter().count(), 0);
    }

    // ── Malformed and empty tag values ───────────────────────────────────────

    #[test]
    fn a_value_less_or_empty_coordinate_is_rejected() {
        let k = known(&[&coord()]);
        // `["a"]` — the tag exists but carries no value at all.
        assert!(validate_enrolment_candidate(KIND_GIT_ISSUE, ROOT, &tags(&[&["a"]]), &k).is_none());
        // `["a", ""]` — present but empty. Rejected here rather than left to a
        // membership check that only fails by luck.
        assert!(
            validate_enrolment_candidate(KIND_GIT_ISSUE, ROOT, &tags(&[&["a", ""]]), &k).is_none()
        );
    }

    #[test]
    fn a_malformed_coordinate_poisons_a_later_valid_one() {
        // Two `a` tags is ambiguous regardless, but the malformed-first
        // ordering is the one where a sloppy scan would skip and accept.
        let k = known(&[&coord()]);
        assert!(validate_enrolment_candidate(
            KIND_GIT_ISSUE,
            ROOT,
            &tags(&[&["a"], &["a", &coord()]]),
            &k
        )
        .is_none());
        assert!(validate_enrolment_candidate(
            KIND_GIT_ISSUE,
            ROOT,
            &tags(&[&["a", ""], &["a", &coord()]]),
            &k
        )
        .is_none());
    }

    #[test]
    fn follow_up_rejects_value_less_and_empty_coordinates() {
        for kind in [KIND_TEXT_NOTE, KIND_GIT_PR_UPDATE, KIND_GIT_STATUS_CLOSED] {
            assert!(!follow_up_coordinate_allowed(
                kind,
                &tags(&[&["a"]]),
                &coord()
            ));
            assert!(!follow_up_coordinate_allowed(
                kind,
                &tags(&[&["a", ""]]),
                &coord()
            ));
            assert!(!follow_up_coordinate_allowed(
                kind,
                &tags(&[&["a", ""], &["a", &coord()]]),
                &coord()
            ));
        }
    }

    // ── Follow-up coordinate rules, per event class ──────────────────────────

    #[test]
    fn comments_and_pr_updates_require_a_matching_coordinate() {
        for kind in [KIND_TEXT_NOTE, KIND_GIT_PR_UPDATE] {
            assert!(
                follow_up_coordinate_allowed(kind, &tags(&[&["a", &coord()]]), &coord()),
                "kind {kind} with the right coordinate"
            );
            // Both builders always emit `a`, so absence is malformed here.
            assert!(
                !follow_up_coordinate_allowed(kind, &tags(&[&["e", ROOT]]), &coord()),
                "kind {kind} must not be admitted on an `#e` match alone"
            );
            let other = format!("30617:{STRANGER}:elsewhere");
            assert!(
                !follow_up_coordinate_allowed(kind, &tags(&[&["a", &other]]), &coord()),
                "kind {kind} may not move its root to another project"
            );
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
                &tags(&[&["e", ROOT]]),
                &coord()
            ));
            assert!(follow_up_coordinate_allowed(
                kind,
                &tags(&[&["a", &coord()]]),
                &coord()
            ));
            let other = format!("30617:{STRANGER}:elsewhere");
            assert!(!follow_up_coordinate_allowed(
                kind,
                &tags(&[&["a", &other]]),
                &coord()
            ));
        }
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
                    &tags(&[&["a", &coord()], &["a", &coord()]]),
                    &coord()
                ),
                "kind {kind}: two coordinates is ambiguity, not redundancy"
            );
        }
    }

    // ── Enrolment sets ───────────────────────────────────────────────────────

    /// Build a candidate the only way production can: through the validator.
    ///
    /// The helpers deliberately do not use a struct literal. `mod tests` is a
    /// child module and *could* reach the private fields, but then the tests
    /// would be exercising a construction path no caller has.
    fn candidate_at(root: &str, coordinate: &str, pr: bool) -> EnrolmentCandidate {
        let kind = if pr {
            KIND_GIT_PULL_REQUEST
        } else {
            KIND_GIT_ISSUE
        };
        validate_enrolment_candidate(
            kind,
            root,
            &tags(&[&["a", coordinate]]),
            &known(&[coordinate]),
        )
        .expect("test candidate must pass real validation")
    }

    fn candidate(root: &str, pr: bool) -> EnrolmentCandidate {
        candidate_at(root, &coord(), pr)
    }

    #[test]
    fn enrol_moves_an_unknown_root_to_active() {
        let mut e = ProjectEnrolments::new();
        assert_eq!(e.enrol(&candidate(ROOT, false)), Ok(EnrolOutcome::Enrolled));
        assert_eq!(e.state_of(ROOT), RootState::Active);
        assert_eq!(e.active_count(), 1);
        assert_eq!(e.dormant_count(), 0);
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
    fn candidate_accessors_report_what_validation_established() {
        let c = candidate_at(ROOT, &coord(), true);
        assert_eq!(c.root(), ROOT);
        assert_eq!(c.coordinate(), coord());
        assert!(c.is_pull_request());
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
        let f = enrolment_filter(&known(&[&coord()]), AGENT, 100).expect("filter");
        assert_eq!(f["kinds"], json!([1621, 1618, 1]));
        assert_eq!(f["#a"], json!([coord()]));
        assert_eq!(f["#p"], json!([AGENT]));
        assert_eq!(f["since"], json!(100));
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

        assert_eq!(filters[0]["kinds"], json!([1, 1630, 1631, 1632, 1633]));
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
}
