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

use uuid::Uuid;

use buzz_core::kind::{
    KIND_GIT_ISSUE, KIND_GIT_PR_UPDATE, KIND_GIT_PULL_REQUEST, KIND_GIT_STATUS_CLOSED,
    KIND_GIT_STATUS_DRAFT, KIND_GIT_STATUS_MERGED, KIND_GIT_STATUS_OPEN, KIND_TEXT_NOTE,
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
/// `desktop/src/features/projects/projectIssues.mjs:28-32`.
pub(crate) fn repo_owner_from_coordinate(coordinate: &str) -> Option<String> {
    let owner = coordinate.split(':').nth(1)?;
    canonical_root_id(owner)
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
}
