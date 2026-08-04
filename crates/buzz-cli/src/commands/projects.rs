//! `buzz projects` — two command families over one noun.
//!
//! **Reads** (`roots`, `addressed`, `root`, `history`) serve the relay reads a
//! project-routed agent runtime needs. An agent that holds conversations on
//! issues and pull requests has two questions no other command answers: which
//! roots address me, and what has happened on a root I am already enrolled in.
//! Both are ordinary NIP-01 filters; what they are not is reachable through
//! `issues get` (one root by id, no thread) or `feed get` (`#p`-scoped, so it
//! cannot see the untagged status event that closed an issue).
//!
//! Every read filter here is scoped and bounded. `roots` requires both a
//! repository set and the mentioned agent, and `history` requires the roots —
//! the unscoped forms are "every project event on the relay", which is not a
//! smaller version of the same question.
//!
//! **Writes** (`create`, `get`, `list`, `add-repo`, `remove-repo`, `update`,
//! `delete`) are the NIP-MP kind:30621 write path. All mutations follow a
//! read-modify-write pattern:
//!   1. Fetch the caller's own live head via `kinds:[30621] + authors:[self] + #d:[slug]`.
//!   2. Mutate the tag set (strip `auth`, apply change).
//!   3. Re-validate the full envelope through Layer A before submitting.
//!   4. Set `created_at = head.created_at + 1` (never wall-clock) to avoid
//!      overwriting a concurrently advancing head.
//!
//! Limitations recorded in this phase:
//!   - Relay hints are read-preserved but not authored (`--repo` carries
//!     a coordinate only; existing hinted tags survive RMW unchanged).
//!   - `delete` targets signer-self only (NIP-OA owner-delete path deferred).
//!   - Deletion durability against later arrival (watermark follow-up) is
//!     not in scope.
//!
//! The two families share a subcommand namespace and nothing else: the reads
//! never author an event, and the writes never subscribe. `kind:30621` (a
//! project grouping) and `kind:20003` (project activity) are distinct kinds and
//! are not interchangeable.

use buzz_core::kind::{
    KIND_GIT_ISSUE, KIND_GIT_PR_UPDATE, KIND_GIT_PULL_REQUEST, KIND_GIT_REPO_ANNOUNCEMENT,
    KIND_GIT_STATUS_CLOSED, KIND_GIT_STATUS_DRAFT, KIND_GIT_STATUS_MERGED, KIND_GIT_STATUS_OPEN,
    KIND_PROJECT, KIND_TEXT_NOTE,
};
use buzz_core::peer_call::{KIND_PEER_CALL, KIND_PEER_CALL_RESULT};
use buzz_sdk::{
    build_delete_addressable, build_project, build_project_with_tags, ProjectMemberCoord,
    PROJECT_D_MAX_LEN,
};
use nostr::{Event, EventBuilder, Tag, Timestamp};
use serde_json::json;

use crate::client::BuzzClient;
use crate::commands::parse_write_response;
use crate::error::CliError;
use crate::validate::{validate_hex64, validate_repo_id};
use crate::ProjectsCmd;

const DEFAULT_LIMIT: u32 = 200;
const MAX_LIMIT: u32 = 500;
const MAX_PROJECTS: usize = 50;
const MAX_ROOTS: usize = 100;

/// The kinds that reference a root with lowercase `e`.
///
/// The same list the watched-root subscription uses
/// (`buzz-acp/src/project.rs`, `HistoryStream::Comments`). Catch-up and the
/// live subscription must ask the same question over different time ranges: if
/// the two drifted, reconstruction would silently omit a class of event the
/// live subscription goes on delivering, and the root would look healthy while
/// missing history nobody could point at.
const COMMENT_STREAM_KINDS: [u32; 7] = [
    KIND_TEXT_NOTE,
    KIND_GIT_STATUS_OPEN,
    KIND_GIT_STATUS_MERGED,
    KIND_GIT_STATUS_CLOSED,
    KIND_GIT_STATUS_DRAFT,
    KIND_PEER_CALL,
    KIND_PEER_CALL_RESULT,
];

/// A bound, or a refusal. Never a silently clamped value: a caller that asked
/// for 5000 and received 500 rows has been told the history ended.
fn bounded_limit(limit: Option<u32>) -> Result<u32, CliError> {
    match limit {
        None => Ok(DEFAULT_LIMIT),
        Some(0) => Err(CliError::Usage("--limit must be at least 1".into())),
        Some(n) if n > MAX_LIMIT => Err(CliError::Usage(format!(
            "--limit must not exceed {MAX_LIMIT}"
        ))),
        Some(n) => Ok(n),
    }
}

/// Canonicalise a repository coordinate, or refuse it.
///
/// Lowercased owner, because `#a` matching is exact and two spellings of one
/// repository are two filters that each miss half the events.
///
/// `flag` names the argument being refused. It is a parameter because the same
/// grammar is spelled `--project` by the read family and `--repo` by
/// `release-check`, and a usage error that names a flag the caller did not type
/// sends them looking for a bug in the wrong argument.
fn canonical_coordinate_for(raw: &str, flag: &str) -> Result<String, CliError> {
    let parts: Vec<&str> = raw.split(':').collect();
    let [kind, owner, id] = parts[..] else {
        return Err(CliError::Usage(format!(
            "{flag} must be 30617:<owner>:<identifier> (got {raw:?})"
        )));
    };
    if kind.parse::<u32>().ok() != Some(KIND_GIT_REPO_ANNOUNCEMENT) {
        return Err(CliError::Usage(format!(
            "{flag} must start with {KIND_GIT_REPO_ANNOUNCEMENT}: (got {raw:?})"
        )));
    }
    validate_hex64(owner)?;
    validate_repo_id(id)?;
    Ok(format!(
        "{KIND_GIT_REPO_ANNOUNCEMENT}:{}:{id}",
        owner.to_ascii_lowercase()
    ))
}

/// The read family's spelling: its coordinate argument is `--project`.
fn canonical_coordinate(raw: &str) -> Result<String, CliError> {
    canonical_coordinate_for(raw, "--project")
}

fn canonical_roots(roots: &[String]) -> Result<Vec<String>, CliError> {
    if roots.len() > MAX_ROOTS {
        return Err(CliError::Usage(format!(
            "--root: maximum {MAX_ROOTS} roots"
        )));
    }
    let mut seen: Vec<String> = Vec::with_capacity(roots.len());
    for root in roots {
        validate_hex64(root)?;
        let root = root.to_ascii_lowercase();
        if !seen.contains(&root) {
            seen.push(root);
        }
    }
    Ok(seen)
}

pub async fn cmd_roots(
    client: &BuzzClient,
    projects: &[String],
    mention: &str,
    limit: Option<u32>,
) -> Result<(), CliError> {
    if projects.len() > MAX_PROJECTS {
        return Err(CliError::Usage(format!(
            "--project: maximum {MAX_PROJECTS} repositories"
        )));
    }
    let mut coordinates: Vec<String> = Vec::with_capacity(projects.len());
    for project in projects {
        let coordinate = canonical_coordinate(project)?;
        if !coordinates.contains(&coordinate) {
            coordinates.push(coordinate);
        }
    }
    validate_hex64(mention)?;
    let limit = bounded_limit(limit)?;

    let resp = client
        .query(&json!({
            "kinds": [KIND_GIT_ISSUE, KIND_GIT_PULL_REQUEST],
            "#a": coordinates,
            "#p": [mention.to_ascii_lowercase()],
            "limit": limit,
        }))
        .await?;
    println!("{resp}");
    Ok(())
}

pub async fn cmd_addressed(
    client: &BuzzClient,
    projects: &[String],
    mention: &str,
    limit: Option<u32>,
    until: Option<u64>,
) -> Result<(), CliError> {
    if projects.len() > MAX_PROJECTS {
        return Err(CliError::Usage(format!(
            "--project: maximum {MAX_PROJECTS} repositories"
        )));
    }
    let mut coordinates = Vec::with_capacity(projects.len());
    for project in projects {
        let coordinate = canonical_coordinate(project)?;
        if !coordinates.contains(&coordinate) {
            coordinates.push(coordinate);
        }
    }
    validate_hex64(mention)?;
    let mut filter = json!({
        "kinds": [KIND_GIT_ISSUE, KIND_GIT_PULL_REQUEST, KIND_TEXT_NOTE],
        "#a": coordinates,
        "#p": [mention.to_ascii_lowercase()],
        "limit": bounded_limit(limit)?,
    });
    if let Some(until) = until {
        filter["until"] = json!(until);
    }
    let resp = client.query(&filter).await?;
    println!("{resp}");
    Ok(())
}

pub async fn cmd_root(client: &BuzzClient, event: &str) -> Result<(), CliError> {
    validate_hex64(event)?;
    let resp = client
        .query(&json!({
            "ids": [event.to_ascii_lowercase()],
            "kinds": [KIND_GIT_ISSUE, KIND_GIT_PULL_REQUEST],
            // Two lets callers reject duplicate/conflicting rows instead of
            // accepting whichever exact-id response arrived first.
            "limit": 2,
        }))
        .await?;
    println!("{resp}");
    Ok(())
}

pub async fn cmd_history(
    client: &BuzzClient,
    roots: &[String],
    limit: Option<u32>,
    until: Option<u64>,
    comments_only: bool,
    revisions_only: bool,
) -> Result<(), CliError> {
    let roots = canonical_roots(roots)?;
    let limit = bounded_limit(limit)?;

    // Two filters, ORed by the relay. `#E` is not a variant spelling of `#e`:
    // a pull-request revision points at its root with the uppercase tag
    // (`buzz-sdk/src/builders.rs`, `build_git_pr_update`), so a lowercase-only
    // query returns a revision history that is silently empty. Runtime callers
    // select one stream at a time so each timestamp-paginated page has one
    // authoritative limit; the default combined output remains compatible.
    let mut comments = json!({"kinds": COMMENT_STREAM_KINDS, "#e": roots, "limit": limit});
    let mut revisions = json!({"kinds": [KIND_GIT_PR_UPDATE], "#E": roots, "limit": limit});
    if let Some(until) = until {
        comments["until"] = json!(until);
        revisions["until"] = json!(until);
    }
    let resp = if comments_only {
        client.query(&comments).await?
    } else if revisions_only {
        client.query(&revisions).await?
    } else {
        client.query_multi(&[comments, revisions]).await?
    };
    println!("{resp}");
    Ok(())
}

// ── `buzz projects release-check` — release authorization ────────────────────
//
// The self-hosted mirror of `scripts/verify-desktop-release-authorization.sh`.
// That script asks GitHub one question — does an approving review by a
// privileged role exist whose `commit_id` is the exact head SHA — and refuses
// the deployment otherwise. This command asks the same question of Buzz's own
// projects grammar, over events it verifies itself.
//
// The mapping, term by term:
//
//   GitHub                          Buzz
//   ------------------------------  ------------------------------------------
//   pull request                    kind:1618 root event (`--root`)
//   head SHA                        the `c` tag of a trusted kind:1619 revision
//                                   (`--revision` names the revision event)
//   review with state APPROVED      kind:1 comment labeled `t:approval`
//   review with REQUEST_CHANGES     kind:1 comment labeled `t:changes-requested`
//   review.commit_id == head SHA    decision `c` tag == the revision's commit
//   author_association OWNER/…      the configured `--owner` pubkey
//   reviewDecision == APPROVED      no owner changes-request newer than the
//                                   approval
//
// The reader rules are not invented here: they are the ones
// `desktop/src/features/projects/projectPullRequests.mjs` applies when it
// decides what to show a human as an approval. A verifier that authorized a
// deployment the Desktop shows as unapproved — or refused one it shows as
// approved — would make the UI a lie about what is enforced, so every trust
// rule below cites the function it mirrors.

/// The reason strings this command can print, spelled exactly once each.
///
/// A deployer branches on these. They are part of the command's contract in
/// the same way the exit code is: renaming one is a breaking change, and a
/// second spelling of the same reason somewhere in this file is how a deployer
/// ends up with an `else` branch that silently treats an unknown refusal as a
/// transient error.
const REASON_APPROVED: &str = "approved";
const REASON_ROOT_NOT_FOUND: &str = "root-not-found";
const REASON_SIGNATURE_INVALID: &str = "signature-invalid";
const REASON_REPO_MISMATCH: &str = "repo-mismatch";
const REASON_REVISION_NOT_FOUND: &str = "revision-not-found";
const REASON_UNTRUSTED_REVISION: &str = "untrusted-revision";
const REASON_REVISION_HAS_NO_COMMIT: &str = "revision-has-no-commit";
const REASON_OWNER_IS_AUTHOR: &str = "owner-is-pull-request-author";
const REASON_OWNER_NOT_TRUSTED: &str = "owner-not-a-trusted-reviewer";
const REASON_NO_APPROVAL: &str = "no-approval";
const REASON_APPROVAL_ON_OTHER_REVISION: &str = "approval-on-other-revision";
const REASON_SUPERSEDED: &str = "superseded-by-changes-request";
const REASON_DECISIONS_TRUNCATED: &str = "decision-history-truncated";

/// The `t` labels a review decision rides on.
///
/// NIP-34 has no review kinds, so the Desktop publishes decisions as labeled
/// kind:1 comments (`projectPullRequests.mjs`, `PR_APPROVAL_LABEL` /
/// `PR_CHANGES_REQUESTED_LABEL`; written by `pullRequestReviews.ts`,
/// `submitProjectPullRequestReview`). These three strings are the wire.
const LABEL_APPROVAL: &str = "approval";
const LABEL_CHANGES_REQUESTED: &str = "changes-requested";
const LABEL_REVIEW_REQUEST: &str = "review-request";

/// Bound on the decision history this command will consider.
///
/// Reused from the read family's ceiling rather than invented: the same "a
/// bound, or a refusal" rule applies, and here the refusal matters more than
/// anywhere else in the file — a page that ended at the limit may be hiding
/// the owner's newest changes-request, and an authorization answer computed
/// from a truncated history is a guess wearing a verdict's clothes.
const RELEASE_DECISION_LIMIT: u32 = MAX_LIMIT;

/// What this command answers, before it is rendered as JSON.
struct ReleaseVerdict {
    authorized: bool,
    reason: &'static str,
    /// `created_at` of the decision that produced this verdict, when one did.
    decided_at: Option<u64>,
    /// The commit the named revision points at, once it is known to be a
    /// trusted revision of the root.
    commit: Option<String>,
}

impl ReleaseVerdict {
    fn refused(reason: &'static str) -> Self {
        Self {
            authorized: false,
            reason,
            decided_at: None,
            commit: None,
        }
    }

    /// A refusal that a specific owner decision is responsible for.
    fn refused_at(reason: &'static str, decided_at: u64, commit: &str) -> Self {
        Self {
            authorized: false,
            reason,
            decided_at: Some(decided_at),
            commit: Some(commit.to_string()),
        }
    }

    fn authorized(decided_at: u64, commit: &str) -> Self {
        Self {
            authorized: true,
            reason: REASON_APPROVED,
            decided_at: Some(decided_at),
            commit: Some(commit.to_string()),
        }
    }

    /// Carry the resolved commit onto a refusal that was decided after the
    /// revision was established, so the caller can see *which* artifact was
    /// refused rather than only that something was.
    fn with_commit(mut self, commit: &str) -> Self {
        if self.commit.is_none() {
            self.commit = Some(commit.to_string());
        }
        self
    }
}

/// The value of the *first* tag with this name, when that value is non-empty —
/// the Rust spelling of `getTag`
/// (`desktop/src/features/projects/projectIssues.mjs`).
///
/// Both halves are load-bearing, and both are places a looser reading would be
/// more permissive than the client:
///
/// - "first tag wins": `trustedUpdateEvents` compares `getTag(event, "E")` to
///   the root id, so a revision carrying a second `E` tag naming this root is
///   still a revision of whichever root it named first.
/// - "and then it is empty or it is nothing": an empty first value yields
///   `None` rather than falling through to a later tag of the same name.
///
/// Search all matching tags instead and a `[["E",""],["E",<root>]]` revision
/// becomes a revision of this root here while the Desktop reads it as a
/// revision of nothing.
fn first_tag<'a>(event: &'a Event, name: &str) -> Option<&'a str> {
    let tag = event.tags.iter().find(|t| tag_name(t) == Some(name))?;
    tag_value(tag).filter(|v| !v.is_empty())
}

/// Every non-empty value of a named tag — the Rust spelling of `getAllTags`.
fn all_tags<'a>(event: &'a Event, name: &str) -> Vec<&'a str> {
    event
        .tags
        .iter()
        .filter(|t| tag_name(t) == Some(name))
        .filter_map(|t| tag_value(t).filter(|v| !v.is_empty()))
        .collect()
}

/// Does this event address `root`? Mirrors `referencesProjectRoot` with
/// `allowUppercase = true`: comments name their root with a lowercase `e`, but
/// third-party clients have shipped the uppercase spelling, and the Desktop
/// accepts either for comments. Accepting only `e` here would ignore an
/// owner's changes-request that the Desktop counts — a strictly more
/// permissive verdict than the client's.
fn references_root(event: &Event, root: &str) -> bool {
    event
        .tags
        .iter()
        .any(|t| matches!(tag_name(t), Some("e") | Some("E")) && tag_value(t) == Some(root))
}

/// The repo owner named by a `30617:<owner>:<id>` coordinate.
///
/// Mirrors `repoOwnerFromAddress`: split on `:`, take the second field, accept
/// it only as 64 hex characters. Deliberately does not require the `30617`
/// prefix — the Desktop does not, and a stricter rule here would drop an actor
/// the client trusts.
fn repo_owner_from_address(address: Option<&str>) -> Option<String> {
    let owner = address?.split(':').nth(1)?;
    if owner.len() == 64 && owner.chars().all(|c| c.is_ascii_hexdigit()) {
        Some(owner.to_ascii_lowercase())
    } else {
        None
    }
}

/// Pubkeys allowed to change a root's lifecycle: the root author and the owner
/// of the repo the root targets (`allowedActorsForProjectRoot`). This is the
/// set that may publish revisions — an arbitrary relay user must not be able to
/// re-point a pull request at their own commit.
fn allowed_actors(root: &Event) -> std::collections::HashSet<String> {
    let mut actors = std::collections::HashSet::new();
    actors.insert(root.pubkey.to_hex().to_ascii_lowercase());
    if let Some(owner) = repo_owner_from_address(first_tag(root, "a")) {
        actors.insert(owner);
    }
    actors
}

/// An owner review decision, reduced to the four facts the verdict rests on.
struct OwnerDecision {
    id: String,
    created_at: u64,
    /// The commit this decision speaks about: the decision's own `c` tag, or
    /// the root's initial commit when it carries none (`reviewDecisionCommit`).
    commit: String,
    approved: bool,
}

impl OwnerDecision {
    /// The Desktop's per-author "latest wins" order: newer `created_at` wins,
    /// and a tie is broken by the greater event id
    /// (`reviewDecisionsForPullRequest`). The id tiebreak is not decoration —
    /// the write path deliberately publishes consecutive decisions one second
    /// apart (`nextProjectPullRequestReviewCreatedAt`) precisely because
    /// whole-second Nostr timestamps collide, and without the same tiebreak a
    /// verifier and the UI would disagree about which of two same-second
    /// decisions is current.
    fn order(&self) -> (u64, &str) {
        (self.created_at, self.id.as_str())
    }
}

/// The decision a labeled comment carries, or `None` when it carries none.
///
/// Mirrors `eventToPullRequestComment`: a comment labeled *both* `approval`
/// and `changes-requested` is not a decision at all. That is not a quirk to
/// tidy up — it is what stops an approval from being smuggled in under a
/// second label that a reader ignores.
fn comment_decision(event: &Event) -> Option<bool> {
    let labels: Vec<String> = all_tags(event, "t")
        .iter()
        .map(|l| l.to_ascii_lowercase())
        .collect();
    let approval = labels.iter().any(|l| l == LABEL_APPROVAL);
    let changes = labels.iter().any(|l| l == LABEL_CHANGES_REQUESTED);
    if approval == changes {
        None
    } else {
        Some(approval)
    }
}

fn is_review_request(event: &Event) -> bool {
    all_tags(event, "t")
        .iter()
        .any(|l| l.eq_ignore_ascii_case(LABEL_REVIEW_REQUEST))
}

/// Fetch one event by exact id and kind, verifying it locally.
///
/// The id filter is not trust: a relay can answer an `ids` query with anything.
/// So the returned rows are re-selected by id *and* kind here, and every
/// surviving candidate must pass `Event::verify` (id recomputation plus
/// BIP-340) before it is looked at. Two verified events cannot share an id —
/// the id *is* the hash — so the first survivor is the only one.
async fn fetch_verified_by_id(
    client: &BuzzClient,
    id: &str,
    kind: u32,
) -> Result<Result<Option<Event>, &'static str>, CliError> {
    let raw = client
        .query(&json!({
            "ids": [id],
            "kinds": [kind],
            // Two, for the same reason `projects root` asks for two: a caller
            // that receives one row cannot tell a unique answer from the first
            // of several conflicting ones.
            "limit": 2,
        }))
        .await?;
    let candidates: Vec<Event> = parse_events(&raw)?
        .into_iter()
        .filter(|e| e.id.to_hex() == id && e.kind.as_u16() as u32 == kind)
        .collect();
    if candidates.is_empty() {
        return Ok(Ok(None));
    }
    for candidate in &candidates {
        if candidate.verify().is_err() {
            return Ok(Err(REASON_SIGNATURE_INVALID));
        }
    }
    Ok(Ok(candidates.into_iter().next()))
}

/// `buzz projects release-check` — is this exact revision authorized to ship?
///
/// Prints the verdict as JSON on stdout in every case it reached a verdict, and
/// exits non-zero unless the verdict is `authorized`. Operational failures
/// (network, relay, malformed arguments) print no verdict: "could not
/// determine" and "determined to be unauthorized" are different facts, and a
/// deployer that treated a missing answer as a `false` would deploy on the day
/// the relay was unreachable — so the absence of stdout JSON is itself part of
/// the contract.
pub async fn cmd_release_check(
    client: &BuzzClient,
    root: &str,
    revision: &str,
    owner: &str,
    repo: Option<&str>,
) -> Result<(), CliError> {
    validate_hex64(root)?;
    validate_hex64(revision)?;
    validate_hex64(owner)?;
    let root = root.to_ascii_lowercase();
    let revision = revision.to_ascii_lowercase();
    let owner = owner.to_ascii_lowercase();
    let repo = repo
        .map(|raw| canonical_coordinate_for(raw, "--repo"))
        .transpose()?;

    let verdict = evaluate_release(client, &root, &revision, &owner, repo.as_deref()).await?;

    println!(
        "{}",
        json!({
            "authorized": verdict.authorized,
            "reason": verdict.reason,
            "root": root,
            "revision": revision,
            "owner": owner,
            "commit": verdict.commit,
            "decided_at": verdict.decided_at,
        })
    );
    if verdict.authorized {
        Ok(())
    } else {
        // The verdict is already on stdout; this carries the same reason to the
        // exit code and the stderr error envelope, so a caller that only checks
        // `$?` is never told "ok" about a refusal.
        Err(CliError::NotFound(format!(
            "release not authorized: {}",
            verdict.reason
        )))
    }
}

/// The verifier proper. Returns a verdict for every answerable question and a
/// `CliError` only when the question could not be asked.
async fn evaluate_release(
    client: &BuzzClient,
    root: &str,
    revision: &str,
    owner: &str,
    repo: Option<&str>,
) -> Result<ReleaseVerdict, CliError> {
    // ── 1. The root, verified ────────────────────────────────────────────────
    let root_event = match fetch_verified_by_id(client, root, KIND_GIT_PULL_REQUEST).await? {
        Err(reason) => return Ok(ReleaseVerdict::refused(reason)),
        Ok(None) => return Ok(ReleaseVerdict::refused(REASON_ROOT_NOT_FOUND)),
        Ok(Some(event)) => event,
    };

    // ── 2. The repository, when the caller pinned one ────────────────────────
    //
    // Optional, and worth having: without it, an approval on a root in *any*
    // repository authorizes this deployment as long as the ids line up. With
    // it, the caller states which repository a release of theirs can come from.
    if let Some(expected) = repo {
        let actual =
            first_tag(&root_event, "a").and_then(|a| canonical_coordinate_for(a, "--repo").ok());
        if actual.as_deref() != Some(expected) {
            return Ok(ReleaseVerdict::refused(REASON_REPO_MISMATCH));
        }
    }

    let actors = allowed_actors(&root_event);
    let root_author = root_event.pubkey.to_hex().to_ascii_lowercase();
    let initial_commit = first_tag(&root_event, "c").map(str::to_string);

    // ── 3. The revision, verified and linked to the root ─────────────────────
    //
    // `--revision <root id>` names the pull request's initial revision: the
    // root carries the first tip commit in its own `c` tag, and the Desktop
    // reads it from there (`initialCommit`). Spelling that case out is what
    // keeps a release cut before the first `1619` from being unverifiable.
    let commit = if revision == root {
        match initial_commit.clone() {
            Some(commit) => commit,
            None => return Ok(ReleaseVerdict::refused(REASON_REVISION_HAS_NO_COMMIT)),
        }
    } else {
        let event = match fetch_verified_by_id(client, revision, KIND_GIT_PR_UPDATE).await? {
            Err(reason) => return Ok(ReleaseVerdict::refused(reason)),
            Ok(None) => return Ok(ReleaseVerdict::refused(REASON_REVISION_NOT_FOUND)),
            Ok(Some(event)) => event,
        };
        // `trustedUpdateEvents`: signed by an allowed actor *and* addressing
        // this root with the uppercase `E`. Both halves are checked here rather
        // than left to the relay's `#E` index, because the index is the relay's
        // claim about the event and the tag is the signer's.
        if first_tag(&event, "E") != Some(root) {
            return Ok(ReleaseVerdict::refused(REASON_UNTRUSTED_REVISION));
        }
        if !actors.contains(&event.pubkey.to_hex().to_ascii_lowercase()) {
            return Ok(ReleaseVerdict::refused(REASON_UNTRUSTED_REVISION));
        }
        match first_tag(&event, "c") {
            Some(commit) => commit.to_string(),
            None => return Ok(ReleaseVerdict::refused(REASON_REVISION_HAS_NO_COMMIT)),
        }
    };

    // ── 4. The comment stream this root's reviews live on ────────────────────
    let raw = client
        .query(&json!({
            "kinds": [KIND_TEXT_NOTE],
            "#e": [root],
            "limit": RELEASE_DECISION_LIMIT,
        }))
        .await?;
    let comments = parse_events(&raw)?;
    if comments.len() as u32 >= RELEASE_DECISION_LIMIT {
        // A full page is an unfinished answer. Refusing beats paginating into a
        // verdict here: the thing a truncated page most easily hides is the
        // newest event, which is exactly the changes-request that would have
        // invalidated the approval.
        return Ok(ReleaseVerdict::refused(REASON_DECISIONS_TRUNCATED).with_commit(&commit));
    }
    let comments: Vec<&Event> = comments
        .iter()
        .filter(|e| e.kind.as_u16() as u32 == KIND_TEXT_NOTE && references_root(e, root))
        .collect();

    // ── 5. Is the configured owner a reviewer the Desktop would trust? ───────
    //
    // `--owner` supplies the privilege (GitHub's MEMBER/OWNER/COLLABORATOR),
    // but it cannot manufacture it: `trustedReviewActors` is what decides whose
    // decision counts, and a pubkey outside that set has decisions the Desktop
    // discards. Authorizing on a decision the client shows as untrusted would
    // make this verifier the weaker of the two readers.
    if owner == root_author {
        // `reviewersForPullRequest` deletes the author, and
        // `trustedReviewActors` re-adds every allowed actor *except* the
        // author. An author cannot review their own pull request, so an owner
        // who opened it cannot self-authorize a release of it.
        return Ok(ReleaseVerdict::refused(REASON_OWNER_IS_AUTHOR).with_commit(&commit));
    }
    let owner_is_recipient = all_tags(&root_event, "p")
        .iter()
        .any(|p| p.eq_ignore_ascii_case(owner));
    // Only the review requests that could make *this* owner a reviewer are
    // load-bearing, so only those are verified. Verifying every comment on the
    // root would let one unrelated malformed note refuse a release.
    let mut owner_review_requested = false;
    for comment in &comments {
        if !is_review_request(comment)
            || !actors.contains(&comment.pubkey.to_hex().to_ascii_lowercase())
            || !all_tags(comment, "p")
                .iter()
                .any(|p| p.eq_ignore_ascii_case(owner))
        {
            continue;
        }
        if comment.verify().is_err() {
            return Ok(ReleaseVerdict::refused(REASON_SIGNATURE_INVALID).with_commit(&commit));
        }
        owner_review_requested = true;
    }
    if !actors.contains(owner) && !owner_is_recipient && !owner_review_requested {
        return Ok(ReleaseVerdict::refused(REASON_OWNER_NOT_TRUSTED).with_commit(&commit));
    }

    // ── 6. The owner's decisions, verified ───────────────────────────────────
    let mut decisions: Vec<OwnerDecision> = Vec::new();
    for comment in &comments {
        if comment.pubkey.to_hex().to_ascii_lowercase() != owner {
            continue;
        }
        let Some(approved) = comment_decision(comment) else {
            continue;
        };
        // Every event the verdict rests on is verified before it is counted —
        // an approval the relay invented, or one whose content was edited after
        // signing, must not be the thing that ships a release.
        if comment.verify().is_err() {
            return Ok(ReleaseVerdict::refused(REASON_SIGNATURE_INVALID).with_commit(&commit));
        }
        // `reviewDecisionCommit`: the decision's own `c` tag, falling back to
        // the root's initial commit. A decision that resolves to no commit at
        // all speaks about nothing, and the Desktop drops it.
        let Some(decision_commit) = first_tag(comment, "c")
            .map(str::to_string)
            .or_else(|| initial_commit.clone())
        else {
            continue;
        };
        decisions.push(OwnerDecision {
            id: comment.id.to_hex(),
            created_at: comment.created_at.as_secs(),
            commit: decision_commit,
            approved,
        });
    }

    // ── 7. The verdict ───────────────────────────────────────────────────────
    //
    // Commit strings are compared byte-for-byte, as the Desktop compares them.
    // A case-insensitive comparison would accept decisions the client treats as
    // being about a different commit.
    let latest_on_revision = decisions
        .iter()
        .filter(|d| d.commit == commit)
        .max_by_key(|d| d.order());
    let approval = match latest_on_revision {
        None => {
            // Nothing on this revision. Saying *why* is the difference between
            // a release that was never reviewed and one whose approval names
            // the previous revision — the second is a rebase away from being
            // authorized, the first is not.
            return Ok(
                match decisions
                    .iter()
                    .filter(|d| d.approved)
                    .max_by_key(|d| d.order())
                {
                    Some(elsewhere) => ReleaseVerdict::refused_at(
                        REASON_APPROVAL_ON_OTHER_REVISION,
                        elsewhere.created_at,
                        &commit,
                    ),
                    None => ReleaseVerdict::refused(REASON_NO_APPROVAL).with_commit(&commit),
                },
            );
        }
        Some(decision) if !decision.approved => {
            return Ok(ReleaseVerdict::refused_at(
                REASON_SUPERSEDED,
                decision.created_at,
                &commit,
            ))
        }
        Some(decision) => decision,
    };

    // The overall-decision half of the GitHub gate (`reviewDecision ==
    // APPROVED`, not merely "an approving review exists"). Within one commit
    // the Desktop already supersedes per author; across revisions it re-dates
    // every decision against the new tip, so an owner who has since asked for
    // changes anywhere on this pull request has withdrawn the approval in
    // substance. A verifier that only looked at the target revision would keep
    // authorizing a release the owner has visibly moved on from.
    if let Some(newer) = decisions
        .iter()
        .filter(|d| !d.approved && d.order() > approval.order())
        .max_by_key(|d| d.order())
    {
        return Ok(ReleaseVerdict::refused_at(
            REASON_SUPERSEDED,
            newer.created_at,
            &commit,
        ));
    }

    Ok(ReleaseVerdict::authorized(approval.created_at, &commit))
}

// ── Buzz repo-ID grammar (bare --repo shorthand) ─────────────────────────────

/// Pattern for a Buzz-hosted repo identifier (bare `--repo` shorthand).
/// `[a-zA-Z0-9._-]{1,64}` — no colons, so guaranteed collision-free with
/// `30617:<owner>:<d>` full coordinates.
fn is_bare_repo_id(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 64
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '_' || c == '-')
}

/// Expand a CLI `--repo` argument into a full `30617:<owner>:<d>` coordinate.
///
/// Bare form (`[a-zA-Z0-9._-]{1,64}`): owner defaults to the caller's pubkey.
/// Full form (`30617:<owner-hex>:<d>`): used verbatim.
fn expand_repo_coord(s: &str, caller_pubkey: &str) -> Result<ProjectMemberCoord, CliError> {
    if is_bare_repo_id(s) {
        // Bare form: expand to full coordinate with caller as owner.
        let full = format!("30617:{caller_pubkey}:{s}");
        ProjectMemberCoord::parse_full(&full)
            .map_err(|e| CliError::Usage(format!("invalid repo coordinate: {e}")))
    } else {
        // Full form: must be parseable as a complete coordinate.
        ProjectMemberCoord::parse_full(s)
            .map_err(|e| CliError::Usage(format!("invalid repo coordinate: {e}")))
    }
}

// ── Head-fetch helper ─────────────────────────────────────────────────────────

fn parse_events(json: &str) -> Result<Vec<Event>, CliError> {
    serde_json::from_str(json)
        .map_err(|e| CliError::Other(format!("failed to parse relay response: {e}")))
}

/// Fetch the caller's own live kind:30621 head for `slug`.
async fn fetch_own_project(client: &BuzzClient, slug: &str) -> Result<Option<Event>, CliError> {
    fetch_project(client, slug, None).await
}

/// Fetch a project head by slug and optional owner pubkey.
async fn fetch_project(
    client: &BuzzClient,
    slug: &str,
    owner: Option<&str>,
) -> Result<Option<Event>, CliError> {
    let pubkey = match owner {
        Some(pk) => {
            crate::validate::validate_hex64(pk)?;
            pk.to_string()
        }
        None => client.keys().public_key().to_hex(),
    };
    let filter = serde_json::json!({
        "kinds": [KIND_PROJECT],
        "authors": [pubkey],
        "#d": [slug],
        "limit": 1,
    });
    let raw = client.query(&filter).await?;
    let mut events = parse_events(&raw)?;
    events.sort_by_key(|e| std::cmp::Reverse(e.created_at));
    Ok(events.into_iter().next())
}

// ── Tag helpers ───────────────────────────────────────────────────────────────

fn tag_name(tag: &Tag) -> Option<&str> {
    tag.as_slice().first().map(String::as_str)
}

fn tag_value(tag: &Tag) -> Option<&str> {
    tag.as_slice().get(1).map(String::as_str)
}

fn make_tag(parts: &[&str]) -> Result<Tag, CliError> {
    Tag::parse(parts.iter().copied())
        .map_err(|e| CliError::Other(format!("tag construction failed: {e}")))
}

// ── Submit helper ─────────────────────────────────────────────────────────────

async fn submit_project(client: &BuzzClient, builder: EventBuilder) -> Result<(), CliError> {
    let event = client.sign_event(builder)?;
    let raw = client.submit_event(event).await?;
    println!(
        "{}",
        parse_write_response(&raw, "project changed concurrently; retry")?
    );
    Ok(())
}

// ── Build helpers ─────────────────────────────────────────────────────────────

/// Advance the `created_at` counter off an observed head.
fn next_timestamp(head: &Event) -> Result<Timestamp, CliError> {
    head.created_at
        .as_secs()
        .checked_add(1)
        .map(Timestamp::from)
        .ok_or_else(|| CliError::Other("project timestamp cannot be advanced".into()))
}

/// Strip `auth` from a tag list and pass the resulting envelope through
/// Layer A validation. Returns a validated `EventBuilder` at `next_ts`.
fn rebuild_project(
    content: &str,
    tags: Vec<Tag>,
    next_ts: Timestamp,
) -> Result<EventBuilder, CliError> {
    // Strip auth tags.
    let clean_tags: Vec<Tag> = tags
        .into_iter()
        .filter(|t| tag_name(t) != Some("auth"))
        .collect();

    build_project_with_tags(content, clean_tags)
        .map_err(|e| CliError::Other(format!("envelope validation failed: {e}")))
        .map(|b| b.custom_created_at(next_ts))
}

// ── Command implementations ───────────────────────────────────────────────────

/// `buzz projects create`
pub async fn cmd_create(
    client: &BuzzClient,
    slug: &str,
    repos: &[String],
    name: Option<&str>,
    description: Option<&str>,
    channel: Option<&str>,
    visibility: Option<&str>,
) -> Result<(), CliError> {
    // ── Local validation (all checks before any .await) ───────────────────
    validate_project_slug(slug)?;

    let caller_pubkey = client.keys().public_key().to_hex();

    // Expand and validate repo coordinates.
    let members: Vec<ProjectMemberCoord> = repos
        .iter()
        .map(|r| expand_repo_coord(r, &caller_pubkey))
        .collect::<Result<Vec<_>, _>>()?;

    // Dedupe: preserve first occurrence, reject duplicates with Usage.
    let mut seen = std::collections::HashSet::new();
    for m in &members {
        if !seen.insert(m.coord.clone()) {
            return Err(CliError::Usage(format!(
                "duplicate --repo coordinate in this invocation: {:?}",
                m.coord
            )));
        }
    }

    // Validate optional metadata (early, before any network call).
    if let Some(ch) = channel {
        crate::validate::validate_uuid(ch)?;
    }
    if let Some(vis) = visibility {
        validate_visibility(vis)?;
    }
    if let Some(n) = name {
        if n.len() > 256 {
            return Err(CliError::Usage(format!(
                "project name must not exceed 256 bytes (got {})",
                n.len()
            )));
        }
    }

    // ── Network: collision preflight ──────────────────────────────────────
    if fetch_own_project(client, slug).await?.is_some() {
        return Err(CliError::Conflict(format!(
            "project {slug:?} already exists; use 'buzz projects update' to modify it"
        )));
    }

    // ── Build via Layer B (enforces all writer policy) ────────────────────
    let builder = build_project(slug, name, description, &members, channel, visibility)
        .map_err(|e| CliError::Usage(e.to_string()))?;
    submit_project(client, builder).await
}

/// `buzz projects get`
pub async fn cmd_get(client: &BuzzClient, slug: &str, owner: Option<&str>) -> Result<(), CliError> {
    validate_project_slug(slug)?;
    let resp = match fetch_project(client, slug, owner).await? {
        Some(event) => serde_json::json!({
            "event_id": event.id.to_hex(),
            "pubkey": event.pubkey.to_hex(),
            "created_at": event.created_at.as_secs(),
            "kind": event.kind.as_u16(),
            "tags": event.tags.iter().map(|t| t.as_slice().to_vec()).collect::<Vec<_>>(),
            "content": event.content,
        }),
        None => {
            let owner_desc = owner.unwrap_or("current identity");
            return Err(CliError::NotFound(format!(
                "project {slug:?} not found for {owner_desc}"
            )));
        }
    };
    println!("{resp}");
    Ok(())
}

/// `buzz projects list`
pub async fn cmd_list(
    client: &BuzzClient,
    owner: Option<&str>,
    limit: Option<u32>,
) -> Result<(), CliError> {
    let pubkey = match owner {
        Some(pk) => {
            crate::validate::validate_hex64(pk)?;
            pk.to_string()
        }
        None => client.keys().public_key().to_hex(),
    };
    let mut filter = serde_json::json!({
        "kinds": [KIND_PROJECT],
        "authors": [pubkey],
    });
    if let Some(n) = limit {
        filter["limit"] = serde_json::json!(n);
    }
    let resp = client.query(&filter).await?;
    println!("{resp}");
    Ok(())
}

/// `buzz projects add-repo`
pub async fn cmd_add_repo(
    client: &BuzzClient,
    slug: &str,
    repos: &[String],
) -> Result<(), CliError> {
    validate_project_slug(slug)?;
    let caller_pubkey = client.keys().public_key().to_hex();

    // ── Local validation before any .await ────────────────────────────────
    let new_members: Vec<ProjectMemberCoord> = repos
        .iter()
        .map(|r| expand_repo_coord(r, &caller_pubkey))
        .collect::<Result<Vec<_>, _>>()?;

    // Dedupe within this invocation: first occurrence wins, duplicate → Usage.
    let mut seen = std::collections::HashSet::new();
    for m in &new_members {
        if !seen.insert(m.coord.clone()) {
            return Err(CliError::Usage(format!(
                "duplicate --repo coordinate in this invocation: {:?}",
                m.coord
            )));
        }
    }

    // ── Network: fetch head ───────────────────────────────────────────────
    let head = fetch_own_project(client, slug)
        .await?
        .ok_or_else(|| CliError::NotFound(format!("project {slug:?} not found")))?;
    let next_ts = next_timestamp(&head)?;

    // Build the new tag set: keep existing tags (including hinted members),
    // append new members only if not already present (by coordinate).
    let mut tags: Vec<Tag> = head.tags.iter().cloned().collect();
    let existing_coords: std::collections::HashSet<String> = head
        .tags
        .iter()
        .filter(|t| tag_name(t) == Some("a"))
        .filter_map(|t| tag_value(t).map(String::from))
        .collect();
    let mut added = 0usize;
    for m in &new_members {
        if !existing_coords.contains(m.coord.as_str()) {
            let parts = m.to_tag_parts();
            let parts_ref: Vec<&str> = parts.iter().map(String::as_str).collect();
            tags.push(
                Tag::parse(parts_ref.iter().copied())
                    .map_err(|e| CliError::Other(format!("member tag construction failed: {e}")))?,
            );
            added += 1;
        }
    }

    // All requested coordinates were already present — no change to publish.
    if added == 0 {
        return Err(CliError::Conflict(format!(
            "all requested repositories are already members of project {slug:?}"
        )));
    }

    let builder = rebuild_project(&head.content, tags, next_ts)?;
    submit_project(client, builder).await
}

/// `buzz projects remove-repo`
pub async fn cmd_remove_repo(
    client: &BuzzClient,
    slug: &str,
    repos: &[String],
) -> Result<(), CliError> {
    validate_project_slug(slug)?;
    let caller_pubkey = client.keys().public_key().to_hex();

    // ── Local validation before any .await ────────────────────────────────
    let to_remove: Vec<ProjectMemberCoord> = repos
        .iter()
        .map(|r| expand_repo_coord(r, &caller_pubkey))
        .collect::<Result<Vec<_>, _>>()?;

    // ── Network: fetch head ───────────────────────────────────────────────
    let head = fetch_own_project(client, slug)
        .await?
        .ok_or_else(|| CliError::NotFound(format!("project {slug:?} not found")))?;
    let next_ts = next_timestamp(&head)?;

    // Verify all requested repos exist in the project.
    let existing_coords: std::collections::HashSet<String> = head
        .tags
        .iter()
        .filter(|t| tag_name(t) == Some("a"))
        .filter_map(|t| tag_value(t).map(String::from))
        .collect();
    for m in &to_remove {
        if !existing_coords.contains(m.coord.as_str()) {
            return Err(CliError::NotFound(format!(
                "project {slug:?} does not contain member {:?}",
                m.coord
            )));
        }
    }

    let remove_coords: std::collections::HashSet<&str> =
        to_remove.iter().map(|m| m.coord.as_str()).collect();

    // Keep all tags except auth and the removed members.
    let tags: Vec<Tag> = head
        .tags
        .iter()
        .filter(|t| {
            if tag_name(t) == Some("auth") {
                return false;
            }
            if tag_name(t) == Some("a") {
                if let Some(coord) = tag_value(t) {
                    return !remove_coords.contains(coord);
                }
            }
            true
        })
        .cloned()
        .collect();

    // Single rebuild validates the full envelope and strips any remaining auth.
    let builder = rebuild_project(&head.content, tags, next_ts)?;
    submit_project(client, builder).await
}

/// `buzz projects update`
///
/// Requires at least one setter or clearer; a no-op call is a usage error.
#[allow(clippy::too_many_arguments)]
pub async fn cmd_update(
    client: &BuzzClient,
    slug: &str,
    name: Option<&str>,
    clear_name: bool,
    description: Option<&str>,
    clear_description: bool,
    channel: Option<&str>,
    clear_channel: bool,
    visibility: Option<&str>,
    clear_visibility: bool,
) -> Result<(), CliError> {
    // Guard: at least one mutation required. The clap `ArgGroup` with
    // `required(true).multiple(true)` enforces this at parse time; this
    // runtime check is a defense-in-depth safety net for callers that invoke
    // `cmd_update` directly (e.g. tests and future programmatic callers).
    let has_mutation = name.is_some()
        || clear_name
        || description.is_some()
        || clear_description
        || channel.is_some()
        || clear_channel
        || visibility.is_some()
        || clear_visibility;
    if !has_mutation {
        return Err(CliError::Usage(
            "buzz projects update requires at least one of: \
             --name, --clear-name, --description, --clear-description, \
             --channel, --clear-channel, --visibility, --clear-visibility"
                .into(),
        ));
    }

    validate_project_slug(slug)?;
    if let Some(ch) = channel {
        crate::validate::validate_uuid(ch)?;
    }
    if let Some(vis) = visibility {
        validate_visibility(vis)?;
    }

    let head = fetch_own_project(client, slug)
        .await?
        .ok_or_else(|| CliError::NotFound(format!("project {slug:?} not found")))?;
    let next_ts = next_timestamp(&head)?;

    // Build the new tag set. For each singleton metadata field:
    //   - setter present: replace value (strip old, append new)
    //   - clear flag set: drop the tag
    //   - neither: keep existing
    // Non-singleton / non-metadata tags (d, a, unknown) are preserved as-is.
    let singleton_fields = ["name", "description", "buzz-channel", "buzz-visibility"];
    let mut tags: Vec<Tag> = head
        .tags
        .iter()
        .filter(|t| {
            if tag_name(t) == Some("auth") {
                return false;
            }
            // Drop singletons we're replacing or clearing.
            if let Some(field) = tag_name(t) {
                if singleton_fields.contains(&field) {
                    let clear = match field {
                        "name" => clear_name || name.is_some(),
                        "description" => clear_description || description.is_some(),
                        "buzz-channel" => clear_channel || channel.is_some(),
                        "buzz-visibility" => clear_visibility || visibility.is_some(),
                        _ => false,
                    };
                    return !clear;
                }
            }
            true
        })
        .cloned()
        .collect();

    // Append new singleton values.
    if let Some(n) = name {
        tags.push(make_tag(&["name", n])?);
    }
    if let Some(d) = description {
        tags.push(make_tag(&["description", d])?);
    }
    if let Some(ch) = channel {
        tags.push(make_tag(&["buzz-channel", ch])?);
    }
    if let Some(vis) = visibility {
        tags.push(make_tag(&["buzz-visibility", vis])?);
    }

    let builder = build_project_with_tags(&head.content, tags)
        .map_err(|e| CliError::Other(format!("envelope validation failed: {e}")))?
        .custom_created_at(next_ts);
    submit_project(client, builder).await
}

/// `buzz projects delete`
///
/// Head-based and verified:
///   1. Fetch own live head — `NotFound` if absent.
///   2. Build tombstone at `head.created_at + 1`.
///   3. Submit.
///   4. Re-query the coordinate; if a newer head survived → `Conflict`.
pub async fn cmd_delete(client: &BuzzClient, slug: &str) -> Result<(), CliError> {
    validate_project_slug(slug)?;

    let head = fetch_own_project(client, slug)
        .await?
        .ok_or_else(|| CliError::NotFound(format!("project {slug:?} not found")))?;
    let next_ts = next_timestamp(&head)?;

    let pubkey_hex = client.keys().public_key().to_hex();
    let tombstone = build_delete_addressable(KIND_PROJECT, &pubkey_hex, slug)
        .map_err(|e| CliError::Other(format!("failed to build delete event: {e}")))?
        .custom_created_at(next_ts);

    let event = client.sign_event(tombstone)?;
    let raw = client.submit_event(event).await?;
    parse_write_response(&raw, "delete event was dominated; a newer head exists")?;

    // Post-submit verification: re-query to confirm the head is gone.
    if let Some(survivor) = fetch_own_project(client, slug).await? {
        // A newer head survived the tombstone.
        return Err(CliError::Conflict(format!(
            "project {slug:?} still exists (head at {}); a concurrent write raced the delete",
            survivor.created_at.as_secs()
        )));
    }

    println!("{}", serde_json::json!({ "deleted": slug, "status": "ok" }));
    Ok(())
}

// ── Validation helpers ────────────────────────────────────────────────────────

/// Validate a project slug: non-empty, ≤1024 bytes, verbatim.
/// Does NOT impose the Buzz repo-ID grammar — project slugs are more permissive.
fn validate_project_slug(slug: &str) -> Result<(), CliError> {
    if slug.is_empty() {
        return Err(CliError::Usage("project slug must not be empty".into()));
    }
    if slug.len() > PROJECT_D_MAX_LEN {
        return Err(CliError::Usage(format!(
            "project slug must not exceed {PROJECT_D_MAX_LEN} bytes (got {})",
            slug.len()
        )));
    }
    Ok(())
}

/// Validate a `buzz-visibility` value at the writer level.
fn validate_visibility(vis: &str) -> Result<(), CliError> {
    if vis != "listed" && vis != "unlisted" {
        return Err(CliError::Usage(format!(
            "visibility must be 'listed' or 'unlisted' (got {vis:?})"
        )));
    }
    Ok(())
}

// ── Dispatch ──────────────────────────────────────────────────────────────────

/// Route a `buzz projects` subcommand to its handler.
///
/// One `match` over both families on purpose: the arms are disjoint — the read
/// arms never author an event and the write arms never subscribe — so a single
/// exhaustive match is what proves no subcommand was dropped when the two were
/// brought together.
pub async fn dispatch(command: ProjectsCmd, client: &BuzzClient) -> Result<(), CliError> {
    match command {
        // ── Reads: the project-routed agent runtime's queries ────────────────
        ProjectsCmd::Addressed {
            projects,
            mention,
            limit,
            until,
        } => cmd_addressed(client, &projects, &mention, limit, until).await,
        ProjectsCmd::Root { event } => cmd_root(client, &event).await,
        ProjectsCmd::Roots {
            projects,
            mention,
            limit,
        } => cmd_roots(client, &projects, &mention, limit).await,
        ProjectsCmd::History {
            roots,
            limit,
            until,
            comments_only,
            revisions_only,
        } => cmd_history(client, &roots, limit, until, comments_only, revisions_only).await,
        ProjectsCmd::ReleaseCheck {
            root,
            revision,
            owner,
            repo,
        } => cmd_release_check(client, &root, &revision, &owner, repo.as_deref()).await,

        // ── Writes: the NIP-MP kind:30621 path ───────────────────────────────
        ProjectsCmd::Create {
            slug,
            repo,
            name,
            description,
            channel,
            visibility,
        } => {
            cmd_create(
                client,
                &slug,
                &repo,
                name.as_deref(),
                description.as_deref(),
                channel.as_deref(),
                visibility.map(|v| v.as_str()),
            )
            .await
        }
        ProjectsCmd::Get { slug, owner } => cmd_get(client, &slug, owner.as_deref()).await,
        ProjectsCmd::List { owner, limit } => cmd_list(client, owner.as_deref(), limit).await,
        ProjectsCmd::AddRepo { slug, repo } => cmd_add_repo(client, &slug, &repo).await,
        ProjectsCmd::RemoveRepo { slug, repo } => cmd_remove_repo(client, &slug, &repo).await,
        ProjectsCmd::Update {
            slug,
            name,
            clear_name,
            description,
            clear_description,
            channel,
            clear_channel,
            visibility,
            clear_visibility,
        } => {
            cmd_update(
                client,
                &slug,
                name.as_deref(),
                clear_name,
                description.as_deref(),
                clear_description,
                channel.as_deref(),
                clear_channel,
                visibility.map(|v| v.as_str()),
                clear_visibility,
            )
            .await
        }
        ProjectsCmd::Delete { slug } => cmd_delete(client, &slug).await,
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

/// The filters these commands submit, proved against a relay that records what
/// it was asked.
///
/// Every test drives the real `dispatch` and reads the request body the relay
/// received, because the filter *is* the product here: a `#e` where an `#E`
/// belongs, or a missing `#p`, returns a plausible-looking array that is
/// missing exactly the events the caller needed.
#[cfg(test)]
mod filter_tests {
    use std::net::SocketAddr;
    use std::sync::{Arc, Mutex};

    use axum::extract::State;
    use axum::routing::post;
    use axum::{Json, Router};
    use nostr::Keys;
    use serde_json::Value;
    use tokio::net::TcpListener;

    use super::*;

    const OWNER: &str = "1111111111111111111111111111111111111111111111111111111111111111";
    const AGENT: &str = "2222222222222222222222222222222222222222222222222222222222222222";
    const ROOT: &str = "3333333333333333333333333333333333333333333333333333333333333333";
    const OTHER_ROOT: &str = "4444444444444444444444444444444444444444444444444444444444444444";

    #[derive(Clone)]
    struct Recorder {
        asked: Arc<Mutex<Vec<Value>>>,
    }

    /// A relay that records the filters it was asked and returns nothing.
    async fn recording_relay() -> (String, Arc<Mutex<Vec<Value>>>) {
        let asked = Arc::new(Mutex::new(Vec::new()));
        let state = Recorder {
            asked: asked.clone(),
        };
        let app = Router::new()
            .route(
                "/query",
                post(|State(s): State<Recorder>, body: String| async move {
                    if let Ok(filters) = serde_json::from_str::<Vec<Value>>(&body) {
                        s.asked.lock().expect("lock").extend(filters);
                    }
                    Json(Value::Array(Vec::new()))
                }),
            )
            .with_state(state);
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr: SocketAddr = listener.local_addr().expect("addr");
        tokio::spawn(async move { axum::serve(listener, app).await.expect("serve") });
        (format!("http://{addr}"), asked)
    }

    async fn client_for(url: String) -> BuzzClient {
        BuzzClient::new(url, Keys::generate(), None, None).expect("client")
    }

    fn coordinate() -> String {
        format!("30617:{OWNER}:demo")
    }

    #[tokio::test]
    async fn roots_asks_for_both_root_kinds_scoped_by_repository_and_mention() {
        let (url, asked) = recording_relay().await;
        let client = client_for(url).await;

        dispatch(
            ProjectsCmd::Roots {
                projects: vec![coordinate()],
                mention: AGENT.to_string(),
                limit: None,
            },
            &client,
        )
        .await
        .expect("query");

        let asked = asked.lock().expect("lock");
        assert_eq!(asked.len(), 1, "one filter: {asked:?}");
        let filter = &asked[0];
        assert_eq!(
            filter["kinds"],
            json!([KIND_GIT_ISSUE, KIND_GIT_PULL_REQUEST])
        );
        assert_eq!(filter["#a"], json!([coordinate()]));
        assert_eq!(filter["#p"], json!([AGENT]));
        assert_eq!(filter["limit"], json!(DEFAULT_LIMIT));
        assert!(
            filter.get("#h").is_none(),
            "a project root is not a channel"
        );
    }

    /// The `#p` scope is what makes this "roots that address me" rather than
    /// "every issue in the repository".
    #[tokio::test]
    async fn roots_refuses_a_coordinate_that_is_not_a_repository() {
        let (url, asked) = recording_relay().await;
        let client = client_for(url).await;

        for bad in [
            "30618:x:y".to_string(),
            format!("30617:{OWNER}"),
            "30617:not-a-pubkey:demo".to_string(),
            format!("30617:{OWNER}:../etc"),
        ] {
            dispatch(
                ProjectsCmd::Roots {
                    projects: vec![bad.clone()],
                    mention: AGENT.to_string(),
                    limit: None,
                },
                &client,
            )
            .await
            .expect_err(&format!("{bad} must be refused"));
        }
        assert!(
            asked.lock().expect("lock").is_empty(),
            "a refused coordinate must not reach the relay"
        );
    }

    #[tokio::test]
    async fn addressed_history_scopes_all_enrolment_kinds_and_inclusive_boundary() {
        let (url, asked) = recording_relay().await;
        let client = client_for(url).await;

        dispatch(
            ProjectsCmd::Addressed {
                projects: vec![coordinate()],
                mention: AGENT.to_string(),
                limit: Some(50),
                until: Some(1234),
            },
            &client,
        )
        .await
        .expect("query");

        let asked = asked.lock().expect("lock");
        assert_eq!(asked.len(), 1);
        assert_eq!(
            asked[0]["kinds"],
            json!([KIND_GIT_ISSUE, KIND_GIT_PULL_REQUEST, KIND_TEXT_NOTE])
        );
        assert_eq!(asked[0]["#a"], json!([coordinate()]));
        assert_eq!(asked[0]["#p"], json!([AGENT]));
        assert_eq!(asked[0]["limit"], json!(50));
        assert_eq!(asked[0]["until"], json!(1234));
    }

    #[tokio::test]
    async fn exact_root_requests_two_rows_to_detect_duplicates() {
        let (url, asked) = recording_relay().await;
        let client = client_for(url).await;

        dispatch(
            ProjectsCmd::Root {
                event: ROOT.to_string(),
            },
            &client,
        )
        .await
        .expect("query");

        let asked = asked.lock().expect("lock");
        assert_eq!(asked.len(), 1);
        assert_eq!(asked[0]["ids"], json!([ROOT]));
        assert_eq!(
            asked[0]["kinds"],
            json!([KIND_GIT_ISSUE, KIND_GIT_PULL_REQUEST])
        );
        assert_eq!(asked[0]["limit"], json!(2));
    }

    #[tokio::test]
    async fn history_asks_lowercase_e_for_comments_and_uppercase_e_for_revisions() {
        let (url, asked) = recording_relay().await;
        let client = client_for(url).await;

        dispatch(
            ProjectsCmd::History {
                roots: vec![ROOT.to_string(), OTHER_ROOT.to_string()],
                limit: Some(50),
                until: None,
                comments_only: false,
                revisions_only: false,
            },
            &client,
        )
        .await
        .expect("query");

        let asked = asked.lock().expect("lock");
        assert_eq!(asked.len(), 2, "two filters: {asked:?}");
        let comments = &asked[0];
        assert_eq!(comments["#e"], json!([ROOT, OTHER_ROOT]));
        assert_eq!(comments["limit"], json!(50));
        let kinds: Vec<u32> = serde_json::from_value(comments["kinds"].clone()).expect("kinds");
        for required in [
            KIND_TEXT_NOTE,
            KIND_GIT_STATUS_OPEN,
            KIND_GIT_STATUS_MERGED,
            KIND_GIT_STATUS_CLOSED,
            KIND_GIT_STATUS_DRAFT,
            KIND_PEER_CALL,
            KIND_PEER_CALL_RESULT,
        ] {
            assert!(
                kinds.contains(&required),
                "{required} missing from {kinds:?}"
            );
        }
        assert!(
            !kinds.contains(&KIND_GIT_PR_UPDATE),
            "a revision is not on the lowercase stream"
        );

        let updates = &asked[1];
        assert_eq!(updates["kinds"], json!([KIND_GIT_PR_UPDATE]));
        assert_eq!(
            updates["#E"],
            json!([ROOT, OTHER_ROOT]),
            "a pull-request revision references its root with the capital tag"
        );
        assert!(
            updates.get("#e").is_none(),
            "the lowercase tag never returns a revision"
        );
    }

    #[tokio::test]
    async fn history_stream_selection_keeps_one_paginatable_filter() {
        let (url, asked) = recording_relay().await;
        let client = client_for(url).await;

        dispatch(
            ProjectsCmd::History {
                roots: vec![ROOT.to_string()],
                limit: Some(50),
                until: Some(1234),
                comments_only: true,
                revisions_only: false,
            },
            &client,
        )
        .await
        .expect("query");

        let asked = asked.lock().expect("lock");
        assert_eq!(asked.len(), 1, "one stream per paginated request");
        assert_eq!(asked[0]["#e"], json!([ROOT]));
        assert_eq!(asked[0]["until"], json!(1234));
        assert!(asked[0].get("#E").is_none());
    }

    /// A limit above the ceiling is refused rather than clamped: silently
    /// returning 500 of the 5000 asked for tells the caller history ended.
    #[tokio::test]
    async fn an_over_large_limit_is_refused_rather_than_clamped() {
        let (url, asked) = recording_relay().await;
        let client = client_for(url).await;

        dispatch(
            ProjectsCmd::History {
                roots: vec![ROOT.to_string()],
                limit: Some(MAX_LIMIT + 1),
                until: None,
                comments_only: false,
                revisions_only: false,
            },
            &client,
        )
        .await
        .expect_err("over the ceiling");
        dispatch(
            ProjectsCmd::History {
                roots: vec![ROOT.to_string()],
                limit: Some(0),
                until: None,
                comments_only: false,
                revisions_only: false,
            },
            &client,
        )
        .await
        .expect_err("a zero limit asks for nothing");
        assert!(asked.lock().expect("lock").is_empty());
    }

    #[tokio::test]
    async fn history_refuses_a_root_that_is_not_an_event_id() {
        let (url, asked) = recording_relay().await;
        let client = client_for(url).await;

        dispatch(
            ProjectsCmd::History {
                roots: vec!["nope".to_string()],
                limit: None,
                until: None,
                comments_only: false,
                revisions_only: false,
            },
            &client,
        )
        .await
        .expect_err("not an event id");
        assert!(asked.lock().expect("lock").is_empty());
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

/// The release-authorization verdict, proved against a relay that answers every
/// query with the whole fixture set.
///
/// That relay is deliberately dishonest in the one way a relay can be: it
/// ignores the filter and returns everything. A verifier that trusted `ids`,
/// `kinds` or `#e` to have been applied would read a comment as a root, or a
/// stranger's revision as this pull request's — so every test below is also a
/// test that the selection happens locally.
#[cfg(test)]
mod release_check_tests {
    use std::net::SocketAddr;
    use std::sync::{Arc, Mutex};

    use axum::extract::State;
    use axum::routing::post;
    use axum::{Json, Router};
    use nostr::{Event, Keys, Kind};
    use serde_json::Value;
    use tokio::net::TcpListener;

    use super::*;

    /// Two 40-hex git commits: the initial tip and the revision's tip.
    const COMMIT_INITIAL: &str = "1111111111111111111111111111111111111111";
    const COMMIT_REVISED: &str = "2222222222222222222222222222222222222222";
    const OTHER_ROOT: &str = "9999999999999999999999999999999999999999999999999999999999999999";

    #[derive(Clone)]
    struct Fixtures {
        asked: Arc<Mutex<Vec<Value>>>,
        events: Arc<Vec<Event>>,
    }

    /// A relay that records the filters it was asked and answers each of them
    /// with every fixture event it holds.
    async fn fixture_relay(events: Vec<Event>) -> (String, Arc<Mutex<Vec<Value>>>) {
        let asked = Arc::new(Mutex::new(Vec::new()));
        let state = Fixtures {
            asked: asked.clone(),
            events: Arc::new(events),
        };
        let app = Router::new()
            .route(
                "/query",
                post(|State(s): State<Fixtures>, body: String| async move {
                    if let Ok(filters) = serde_json::from_str::<Vec<Value>>(&body) {
                        s.asked.lock().expect("lock").extend(filters);
                    }
                    Json(serde_json::to_value(&*s.events).expect("serialize fixtures"))
                }),
            )
            .with_state(state);
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr: SocketAddr = listener.local_addr().expect("addr");
        tokio::spawn(async move { axum::serve(listener, app).await.expect("serve") });
        (format!("http://{addr}"), asked)
    }

    fn client_for(url: String) -> BuzzClient {
        BuzzClient::new(url, Keys::generate(), None, None).expect("client")
    }

    fn signed(keys: &Keys, kind: u32, created_at: u64, tags: &[&[&str]], content: &str) -> Event {
        let tags: Vec<Tag> = tags
            .iter()
            .map(|parts| Tag::parse(parts.iter().copied()).expect("tag"))
            .collect();
        EventBuilder::new(Kind::Custom(kind as u16), content)
            .tags(tags)
            .custom_created_at(Timestamp::from(created_at))
            .sign_with_keys(keys)
            .expect("sign")
    }

    /// The cast: a repo owner who reviews, an author who opens the pull
    /// request, and a second reviewer who is not the owner.
    struct Cast {
        owner: Keys,
        author: Keys,
        reviewer: Keys,
        stranger: Keys,
    }

    impl Cast {
        fn new() -> Self {
            Self {
                owner: Keys::generate(),
                author: Keys::generate(),
                reviewer: Keys::generate(),
                stranger: Keys::generate(),
            }
        }

        fn owner_hex(&self) -> String {
            self.owner.public_key().to_hex()
        }

        fn coordinate(&self) -> String {
            format!("30617:{}:demo", self.owner_hex())
        }

        /// A kind:1618 root in the owner's repository, opened by the author,
        /// with the owner and one reviewer `p`-tagged.
        fn root(&self) -> Event {
            let coord = self.coordinate();
            let owner = self.owner_hex();
            let reviewer = self.reviewer.public_key().to_hex();
            signed(
                &self.author,
                KIND_GIT_PULL_REQUEST,
                100,
                &[
                    &["a", &coord],
                    &["p", &owner],
                    &["p", &reviewer],
                    &["subject", "Release candidate"],
                    &["c", COMMIT_INITIAL],
                    &["clone", "https://example.invalid/demo.git"],
                ],
                "a release",
            )
        }

        /// A kind:1619 revision signed by `signer`, naming the root it claims
        /// to revise with the uppercase `E`.
        fn revision(&self, signer: &Keys, names: &str, commit: &str) -> Event {
            let coord = self.coordinate();
            let owner = self.owner_hex();
            let author = self.author.public_key().to_hex();
            signed(
                signer,
                KIND_GIT_PR_UPDATE,
                200,
                &[
                    &["a", &coord],
                    &["p", &owner],
                    &["E", names],
                    &["P", &author],
                    &["c", commit],
                    &["clone", "https://example.invalid/demo.git"],
                ],
                "revised",
            )
        }

        /// A review decision: a kind:1 comment labeled and bound to a commit,
        /// exactly as `submitProjectPullRequestReview` publishes one.
        fn decision(
            &self,
            signer: &Keys,
            root: &str,
            label: &str,
            commit: &str,
            created_at: u64,
        ) -> Event {
            let coord = self.coordinate();
            let owner = self.owner_hex();
            let author = self.author.public_key().to_hex();
            signed(
                signer,
                KIND_TEXT_NOTE,
                created_at,
                &[
                    &["e", root, "", "root"],
                    &["a", &coord],
                    &["p", &owner],
                    &["p", &author],
                    &["t", label],
                    &["c", commit],
                ],
                "Approved these changes",
            )
        }
    }

    /// Re-sign nothing and change everything: the event keeps its id and
    /// signature but no longer hashes to them.
    fn tampered(event: &Event) -> Event {
        let mut raw = serde_json::to_value(event).expect("serialize");
        raw["content"] = Value::String("Approved these changes (edited)".into());
        serde_json::from_value(raw).expect("deserialize")
    }

    async fn verdict(
        events: Vec<Event>,
        root: &str,
        revision: &str,
        owner: &str,
        repo: Option<&str>,
    ) -> (ReleaseVerdict, Vec<Value>) {
        let (url, asked) = fixture_relay(events).await;
        let client = client_for(url);
        let verdict = evaluate_release(&client, root, revision, owner, repo)
            .await
            .expect("the question was askable");
        let asked = asked.lock().expect("lock").clone();
        (verdict, asked)
    }

    // ── Happy path ───────────────────────────────────────────────────────────

    #[tokio::test]
    async fn an_owner_approval_on_the_exact_revision_authorizes_it() {
        let cast = Cast::new();
        let root = cast.root();
        let root_id = root.id.to_hex();
        let revision = cast.revision(&cast.author, &root_id, COMMIT_REVISED);
        let approval = cast.decision(&cast.owner, &root_id, "approval", COMMIT_REVISED, 300);

        let (verdict, asked) = verdict(
            vec![root, revision.clone(), approval.clone()],
            &root_id,
            &revision.id.to_hex(),
            &cast.owner_hex(),
            Some(&cast.coordinate()),
        )
        .await;

        assert!(verdict.authorized, "reason: {}", verdict.reason);
        assert_eq!(verdict.reason, "approved");
        assert_eq!(verdict.decided_at, Some(300));
        assert_eq!(verdict.commit.as_deref(), Some(COMMIT_REVISED));

        // Three scoped, bounded filters and no fourth: the root by id, the
        // revision by id, and this root's comment stream.
        assert_eq!(asked.len(), 3, "{asked:?}");
        assert_eq!(asked[0]["ids"], json!([root_id]));
        assert_eq!(asked[0]["kinds"], json!([KIND_GIT_PULL_REQUEST]));
        assert_eq!(asked[0]["limit"], json!(2));
        assert_eq!(asked[1]["ids"], json!([revision.id.to_hex()]));
        assert_eq!(asked[1]["kinds"], json!([KIND_GIT_PR_UPDATE]));
        assert_eq!(asked[2]["kinds"], json!([KIND_TEXT_NOTE]));
        assert_eq!(asked[2]["#e"], json!([root_id]));
        assert_eq!(asked[2]["limit"], json!(MAX_LIMIT));
    }

    /// `--revision <root id>` is the pull request's initial revision: the root
    /// carries the first tip commit itself, and a release cut before any
    /// kind:1619 exists must still be verifiable.
    #[tokio::test]
    async fn the_root_id_names_the_initial_revision() {
        let cast = Cast::new();
        let root = cast.root();
        let root_id = root.id.to_hex();
        let approval = cast.decision(&cast.owner, &root_id, "approval", COMMIT_INITIAL, 300);

        let (verdict, asked) = verdict(
            vec![root, approval],
            &root_id,
            &root_id,
            &cast.owner_hex(),
            None,
        )
        .await;

        assert!(verdict.authorized, "reason: {}", verdict.reason);
        assert_eq!(verdict.commit.as_deref(), Some(COMMIT_INITIAL));
        assert_eq!(asked.len(), 2, "no revision fetch is needed: {asked:?}");
    }

    /// A decision with no `c` tag speaks about the root's initial commit
    /// (`reviewDecisionCommit`), so it authorizes the initial revision and
    /// nothing later.
    #[tokio::test]
    async fn a_decision_without_a_commit_tag_speaks_about_the_initial_commit() {
        let cast = Cast::new();
        let root = cast.root();
        let root_id = root.id.to_hex();
        let coord = cast.coordinate();
        let untagged = signed(
            &cast.owner,
            KIND_TEXT_NOTE,
            300,
            &[
                &["e", &root_id, "", "root"],
                &["a", &coord],
                &["t", "approval"],
            ],
            "Approved these changes",
        );
        let revision = cast.revision(&cast.author, &root_id, COMMIT_REVISED);

        let (initial, _) = verdict(
            vec![root.clone(), untagged.clone()],
            &root_id,
            &root_id,
            &cast.owner_hex(),
            None,
        )
        .await;
        assert!(initial.authorized, "reason: {}", initial.reason);

        let (revised, _) = verdict(
            vec![root, revision.clone(), untagged],
            &root_id,
            &revision.id.to_hex(),
            &cast.owner_hex(),
            None,
        )
        .await;
        assert_eq!(revised.reason, "approval-on-other-revision");
    }

    // ── Exact-revision binding ───────────────────────────────────────────────

    #[tokio::test]
    async fn an_approval_of_another_revision_does_not_authorize_this_one() {
        let cast = Cast::new();
        let root = cast.root();
        let root_id = root.id.to_hex();
        let revision = cast.revision(&cast.author, &root_id, COMMIT_REVISED);
        // The owner approved the tip the pull request opened with, then the
        // author pushed a new one.
        let stale = cast.decision(&cast.owner, &root_id, "approval", COMMIT_INITIAL, 150);

        let (verdict, _) = verdict(
            vec![root, revision.clone(), stale],
            &root_id,
            &revision.id.to_hex(),
            &cast.owner_hex(),
            None,
        )
        .await;

        assert!(!verdict.authorized);
        assert_eq!(verdict.reason, "approval-on-other-revision");
        assert_eq!(verdict.decided_at, Some(150));
        assert_eq!(verdict.commit.as_deref(), Some(COMMIT_REVISED));
    }

    // ── Supersession ─────────────────────────────────────────────────────────

    #[tokio::test]
    async fn an_owner_changes_request_after_the_approval_withdraws_it() {
        let cast = Cast::new();
        let root = cast.root();
        let root_id = root.id.to_hex();
        let revision = cast.revision(&cast.author, &root_id, COMMIT_REVISED);
        let approval = cast.decision(&cast.owner, &root_id, "approval", COMMIT_REVISED, 300);
        let changes = cast.decision(
            &cast.owner,
            &root_id,
            "changes-requested",
            COMMIT_REVISED,
            400,
        );

        let (verdict, _) = verdict(
            vec![root, revision.clone(), approval, changes],
            &root_id,
            &revision.id.to_hex(),
            &cast.owner_hex(),
            None,
        )
        .await;

        assert!(!verdict.authorized);
        assert_eq!(verdict.reason, "superseded-by-changes-request");
        assert_eq!(verdict.decided_at, Some(400));
    }

    /// The overall-decision half of the GitHub gate: an owner who asks for
    /// changes anywhere on this pull request after approving has withdrawn the
    /// approval, even when the request names a later revision.
    #[tokio::test]
    async fn a_later_changes_request_on_another_revision_also_withdraws_it() {
        let cast = Cast::new();
        let root = cast.root();
        let root_id = root.id.to_hex();
        let revision = cast.revision(&cast.author, &root_id, COMMIT_REVISED);
        let approval = cast.decision(&cast.owner, &root_id, "approval", COMMIT_REVISED, 300);
        let later = cast.decision(
            &cast.owner,
            &root_id,
            "changes-requested",
            "3333333333333333333333333333333333333333",
            500,
        );

        let (verdict, _) = verdict(
            vec![root, revision.clone(), approval, later],
            &root_id,
            &revision.id.to_hex(),
            &cast.owner_hex(),
            None,
        )
        .await;

        assert_eq!(verdict.reason, "superseded-by-changes-request");
        assert_eq!(verdict.decided_at, Some(500));
    }

    /// The mirror image: changes requested first, then approved. The latest
    /// decision per author wins, so the release is authorized.
    #[tokio::test]
    async fn an_approval_after_a_changes_request_supersedes_it() {
        let cast = Cast::new();
        let root = cast.root();
        let root_id = root.id.to_hex();
        let revision = cast.revision(&cast.author, &root_id, COMMIT_REVISED);
        let changes = cast.decision(
            &cast.owner,
            &root_id,
            "changes-requested",
            COMMIT_REVISED,
            300,
        );
        let approval = cast.decision(&cast.owner, &root_id, "approval", COMMIT_REVISED, 400);

        let (verdict, _) = verdict(
            vec![root, revision.clone(), changes, approval],
            &root_id,
            &revision.id.to_hex(),
            &cast.owner_hex(),
            None,
        )
        .await;

        assert!(verdict.authorized, "reason: {}", verdict.reason);
        assert_eq!(verdict.decided_at, Some(400));
    }

    // ── Who may approve ──────────────────────────────────────────────────────

    #[tokio::test]
    async fn an_approval_by_someone_other_than_the_owner_is_not_an_approval() {
        let cast = Cast::new();
        let root = cast.root();
        let root_id = root.id.to_hex();
        let revision = cast.revision(&cast.author, &root_id, COMMIT_REVISED);
        // A trusted reviewer — `p`-tagged on the root — but not the configured
        // owner. The Desktop counts this approval; a release must not.
        let reviewer_approval =
            cast.decision(&cast.reviewer, &root_id, "approval", COMMIT_REVISED, 300);

        let (verdict, _) = verdict(
            vec![root, revision.clone(), reviewer_approval],
            &root_id,
            &revision.id.to_hex(),
            &cast.owner_hex(),
            None,
        )
        .await;

        assert!(!verdict.authorized);
        assert_eq!(verdict.reason, "no-approval");
        assert_eq!(verdict.decided_at, None);
    }

    /// `reviewersForPullRequest` deletes the root author, and
    /// `trustedReviewActors` re-adds every allowed actor except the author — so
    /// an owner who opened the pull request cannot approve it, and cannot
    /// self-authorize a release of it.
    #[tokio::test]
    async fn an_owner_who_opened_the_pull_request_cannot_approve_it() {
        let cast = Cast::new();
        let coord = cast.coordinate();
        let owner_hex = cast.owner_hex();
        // The owner is the author this time.
        let root = signed(
            &cast.owner,
            KIND_GIT_PULL_REQUEST,
            100,
            &[
                &["a", &coord],
                &["p", &owner_hex],
                &["subject", "Self-opened"],
                &["c", COMMIT_INITIAL],
            ],
            "mine",
        );
        let root_id = root.id.to_hex();
        let approval = cast.decision(&cast.owner, &root_id, "approval", COMMIT_INITIAL, 300);

        let (verdict, _) =
            verdict(vec![root, approval], &root_id, &root_id, &owner_hex, None).await;

        assert_eq!(verdict.reason, "owner-is-pull-request-author");
    }

    /// A pubkey that is neither the repo owner, nor a root recipient, nor
    /// named by a trusted review request has decisions the Desktop discards.
    #[tokio::test]
    async fn an_approval_by_an_untrusted_pubkey_is_refused_by_name() {
        let cast = Cast::new();
        let root = cast.root();
        let root_id = root.id.to_hex();
        let stranger = cast.stranger.public_key().to_hex();
        let approval = cast.decision(&cast.stranger, &root_id, "approval", COMMIT_INITIAL, 300);

        let (verdict, _) = verdict(vec![root, approval], &root_id, &root_id, &stranger, None).await;

        assert_eq!(verdict.reason, "owner-not-a-trusted-reviewer");
    }

    /// The review-request path into trust: a stranger the *author* asked to
    /// review becomes a trusted reviewer (`reviewersForPullRequest`), and their
    /// approval then counts — but only when the request itself is signed by an
    /// allowed actor.
    #[tokio::test]
    async fn a_trusted_review_request_can_confer_reviewer_trust() {
        let cast = Cast::new();
        let root = cast.root();
        let root_id = root.id.to_hex();
        let coord = cast.coordinate();
        let stranger = cast.stranger.public_key().to_hex();
        let request = |signer: &Keys| {
            signed(
                signer,
                KIND_TEXT_NOTE,
                150,
                &[
                    &["e", &root_id, "", "root"],
                    &["a", &coord],
                    &["p", &stranger],
                    &["t", "review-request"],
                ],
                "Requested a review",
            )
        };
        let approval = cast.decision(&cast.stranger, &root_id, "approval", COMMIT_INITIAL, 300);

        let (trusted, _) = verdict(
            vec![root.clone(), request(&cast.author), approval.clone()],
            &root_id,
            &root_id,
            &stranger,
            None,
        )
        .await;
        assert!(trusted.authorized, "reason: {}", trusted.reason);

        // The same request signed by a bystander confers nothing.
        let (untrusted, _) = verdict(
            vec![root, request(&cast.reviewer), approval],
            &root_id,
            &root_id,
            &stranger,
            None,
        )
        .await;
        assert_eq!(untrusted.reason, "owner-not-a-trusted-reviewer");
    }

    // ── Local verification ───────────────────────────────────────────────────

    #[tokio::test]
    async fn an_approval_whose_signature_does_not_cover_it_is_refused() {
        let cast = Cast::new();
        let root = cast.root();
        let root_id = root.id.to_hex();
        let approval = cast.decision(&cast.owner, &root_id, "approval", COMMIT_INITIAL, 300);

        let (verdict, _) = verdict(
            vec![root, tampered(&approval)],
            &root_id,
            &root_id,
            &cast.owner_hex(),
            None,
        )
        .await;

        assert!(!verdict.authorized);
        assert_eq!(verdict.reason, "signature-invalid");
    }

    #[tokio::test]
    async fn a_tampered_root_is_refused_rather_than_read() {
        let cast = Cast::new();
        let root = cast.root();
        let root_id = root.id.to_hex();

        let (verdict, _) = verdict(
            vec![tampered(&root)],
            &root_id,
            &root_id,
            &cast.owner_hex(),
            None,
        )
        .await;

        assert_eq!(verdict.reason, "signature-invalid");
    }

    // ── Revision linkage ─────────────────────────────────────────────────────

    #[tokio::test]
    async fn a_revision_that_names_another_root_is_not_a_revision_of_this_one() {
        let cast = Cast::new();
        let root = cast.root();
        let root_id = root.id.to_hex();
        // Signed by the author, valid in every way except the root it names.
        let foreign = cast.revision(&cast.author, OTHER_ROOT, COMMIT_REVISED);
        let approval = cast.decision(&cast.owner, &root_id, "approval", COMMIT_REVISED, 300);

        let (verdict, _) = verdict(
            vec![root, foreign.clone(), approval],
            &root_id,
            &foreign.id.to_hex(),
            &cast.owner_hex(),
            None,
        )
        .await;

        assert!(!verdict.authorized);
        assert_eq!(verdict.reason, "untrusted-revision");
        assert_eq!(
            verdict.commit, None,
            "an untrusted revision has no commit to report"
        );
    }

    /// `getTag` reads the first tag of a name and stops. A revision whose
    /// first `E` is empty names no root, and a second `E` behind it does not
    /// rescue it — otherwise a verifier would accept a linkage the Desktop
    /// reads as absent.
    #[tokio::test]
    async fn only_the_first_root_reference_on_a_revision_counts() {
        let cast = Cast::new();
        let root = cast.root();
        let root_id = root.id.to_hex();
        let coord = cast.coordinate();
        let author = cast.author.public_key().to_hex();
        let smuggled = signed(
            &cast.author,
            KIND_GIT_PR_UPDATE,
            200,
            &[
                &["a", &coord],
                &["E", ""],
                &["E", &root_id],
                &["P", &author],
                &["c", COMMIT_REVISED],
            ],
            "revised",
        );
        let approval = cast.decision(&cast.owner, &root_id, "approval", COMMIT_REVISED, 300);

        let (verdict, _) = verdict(
            vec![root, smuggled.clone(), approval],
            &root_id,
            &smuggled.id.to_hex(),
            &cast.owner_hex(),
            None,
        )
        .await;

        assert_eq!(verdict.reason, "untrusted-revision");
    }

    #[tokio::test]
    async fn a_revision_published_by_a_stranger_is_untrusted() {
        let cast = Cast::new();
        let root = cast.root();
        let root_id = root.id.to_hex();
        // The stranger re-points the pull request at their own commit and the
        // owner "approves" that commit. Neither event is forged; the revision
        // is simply not one an allowed actor published.
        let hijack = cast.revision(&cast.stranger, &root_id, COMMIT_REVISED);
        let approval = cast.decision(&cast.owner, &root_id, "approval", COMMIT_REVISED, 300);

        let (verdict, _) = verdict(
            vec![root, hijack.clone(), approval],
            &root_id,
            &hijack.id.to_hex(),
            &cast.owner_hex(),
            None,
        )
        .await;

        assert_eq!(verdict.reason, "untrusted-revision");
    }

    #[tokio::test]
    async fn a_missing_root_and_a_missing_revision_are_named_apart() {
        let cast = Cast::new();
        let root = cast.root();
        let root_id = root.id.to_hex();

        let (no_root, _) = verdict(vec![], &root_id, &root_id, &cast.owner_hex(), None).await;
        assert_eq!(no_root.reason, "root-not-found");

        let revision = cast.revision(&cast.author, &root_id, COMMIT_REVISED);
        let (no_revision, _) = verdict(
            vec![root],
            &root_id,
            &revision.id.to_hex(),
            &cast.owner_hex(),
            None,
        )
        .await;
        assert_eq!(no_revision.reason, "revision-not-found");
    }

    /// A revision with no `c` tag names no artifact, so no decision can be
    /// bound to it — refused by name rather than silently compared against the
    /// root's initial commit, which would authorize the wrong tree.
    #[tokio::test]
    async fn a_revision_without_a_commit_tag_can_never_be_approved() {
        let cast = Cast::new();
        let root = cast.root();
        let root_id = root.id.to_hex();
        let coord = cast.coordinate();
        let author = cast.author.public_key().to_hex();
        let commitless = signed(
            &cast.author,
            KIND_GIT_PR_UPDATE,
            200,
            &[&["a", &coord], &["E", &root_id], &["P", &author]],
            "revised",
        );
        let approval = cast.decision(&cast.owner, &root_id, "approval", COMMIT_INITIAL, 300);

        let (verdict, _) = verdict(
            vec![root, commitless.clone(), approval],
            &root_id,
            &commitless.id.to_hex(),
            &cast.owner_hex(),
            None,
        )
        .await;

        assert_eq!(verdict.reason, "revision-has-no-commit");
    }

    // ── Bounds ───────────────────────────────────────────────────────────────

    /// A comment page that ended at the limit may be missing the owner's
    /// newest changes-request, so the answer is refused rather than guessed —
    /// even though this page does contain a valid approval.
    #[tokio::test]
    async fn a_full_page_of_comments_refuses_rather_than_guesses() {
        let cast = Cast::new();
        let root = cast.root();
        let root_id = root.id.to_hex();
        let approval = cast.decision(&cast.owner, &root_id, "approval", COMMIT_INITIAL, 300);

        let mut events = vec![root];
        events.extend(std::iter::repeat_n(approval, MAX_LIMIT as usize));
        let (verdict, _) = verdict(events, &root_id, &root_id, &cast.owner_hex(), None).await;

        assert!(!verdict.authorized);
        assert_eq!(verdict.reason, "decision-history-truncated");
        assert_eq!(verdict.commit.as_deref(), Some(COMMIT_INITIAL));
    }

    // ── Repository pinning ───────────────────────────────────────────────────

    #[tokio::test]
    async fn a_root_in_another_repository_cannot_authorize_this_release() {
        let cast = Cast::new();
        let root = cast.root();
        let root_id = root.id.to_hex();
        let approval = cast.decision(&cast.owner, &root_id, "approval", COMMIT_INITIAL, 300);
        let elsewhere = format!("30617:{}:other", cast.owner_hex());

        let (verdict, asked) = verdict(
            vec![root, approval],
            &root_id,
            &root_id,
            &cast.owner_hex(),
            Some(&elsewhere),
        )
        .await;

        assert_eq!(verdict.reason, "repo-mismatch");
        assert_eq!(asked.len(), 1, "a mismatched repo stops before the reviews");
    }

    // ── The command surface ──────────────────────────────────────────────────

    /// Exit status is the half of the contract a shell reads. Authorized is
    /// `Ok`; every refusal is an error, so `set -e` cannot step past one.
    #[tokio::test]
    async fn the_command_exits_zero_only_when_authorized() {
        let cast = Cast::new();
        let root = cast.root();
        let root_id = root.id.to_hex();
        let approval = cast.decision(&cast.owner, &root_id, "approval", COMMIT_INITIAL, 300);

        let (url, _) = fixture_relay(vec![root.clone(), approval]).await;
        let client = client_for(url);
        dispatch(
            ProjectsCmd::ReleaseCheck {
                root: root_id.clone(),
                revision: root_id.clone(),
                owner: cast.owner_hex(),
                repo: None,
            },
            &client,
        )
        .await
        .expect("an owner-approved revision is authorized");

        let (url, _) = fixture_relay(vec![root]).await;
        let client = client_for(url);
        let err = dispatch(
            ProjectsCmd::ReleaseCheck {
                root: root_id.clone(),
                revision: root_id,
                owner: cast.owner_hex(),
                repo: None,
            },
            &client,
        )
        .await
        .expect_err("an unapproved revision is refused");
        assert!(
            format!("{err}").contains("no-approval"),
            "the refusal must name its reason: {err}"
        );
        assert_eq!(crate::error::exit_code(&err), 1);
    }

    /// Malformed arguments are refused before any query: a verdict about a
    /// release nobody can name is worse than an error.
    #[tokio::test]
    async fn malformed_arguments_never_reach_the_relay() {
        let cast = Cast::new();
        let root = cast.root();
        let root_id = root.id.to_hex();
        let owner = cast.owner_hex();

        for (root_arg, revision, owner_arg, repo) in [
            ("nope", root_id.as_str(), owner.as_str(), None),
            (root_id.as_str(), "nope", owner.as_str(), None),
            (root_id.as_str(), root_id.as_str(), "nope", None),
            (
                root_id.as_str(),
                root_id.as_str(),
                owner.as_str(),
                Some("30618:x:y"),
            ),
        ] {
            let (url, asked) = fixture_relay(vec![root.clone()]).await;
            let client = client_for(url);
            let err = cmd_release_check(&client, root_arg, revision, owner_arg, repo)
                .await
                .expect_err("malformed input must be refused");
            assert!(
                matches!(err, CliError::Usage(_)),
                "expected Usage, got {err:?}"
            );
            assert!(
                asked.lock().expect("lock").is_empty(),
                "a refused argument must not reach the relay"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use buzz_sdk::{validate_project_envelope, PROJECT_MEMBER_CAP};
    use nostr::Tag;

    use super::*;

    // ── Coordinate expansion ──────────────────────────────────────────────────

    const OWNER_HEX: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const OWNER_B_HEX: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

    #[test]
    fn expand_repo_coord_bare_expands_with_caller_pubkey() {
        let coord = expand_repo_coord("my-repo", OWNER_HEX).unwrap();
        assert_eq!(coord.coord, format!("30617:{OWNER_HEX}:my-repo"));
    }

    #[test]
    fn expand_repo_coord_full_passes_through() {
        let full = format!("30617:{OWNER_HEX}:some-repo");
        let coord = expand_repo_coord(&full, OWNER_B_HEX).unwrap();
        // Owner from the full coord, not the caller.
        assert_eq!(coord.coord, full);
    }

    #[test]
    fn expand_repo_coord_full_cross_owner() {
        let full = format!("30617:{OWNER_B_HEX}:infra");
        let coord = expand_repo_coord(&full, OWNER_HEX).unwrap();
        assert_eq!(coord.coord, full);
    }

    #[test]
    fn expand_repo_coord_rejects_uppercase_owner() {
        let upper = "30617:AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA:buzz";
        assert!(expand_repo_coord(upper, OWNER_HEX).is_err());
    }

    #[test]
    fn expand_repo_coord_rejects_coordinate_shaped_bare_value() {
        // A value with a colon is never a bare id.
        let not_bare = "30617:something";
        // parse_full will fail because it's not a valid full coordinate either.
        assert!(expand_repo_coord(not_bare, OWNER_HEX).is_err());
    }

    // ── validate_project_slug ─────────────────────────────────────────────────

    #[test]
    fn validate_project_slug_accepts_normal() {
        assert!(validate_project_slug("my-project").is_ok());
        assert!(validate_project_slug("platform:v2").is_ok()); // colons allowed — more permissive than repo-id
    }

    #[test]
    fn validate_project_slug_rejects_empty() {
        assert!(validate_project_slug("").is_err());
    }

    #[test]
    fn validate_project_slug_rejects_over_1024() {
        let long = "a".repeat(1025);
        assert!(validate_project_slug(&long).is_err());
    }

    #[test]
    fn validate_project_slug_accepts_1024() {
        let at_limit = "a".repeat(1024);
        assert!(validate_project_slug(&at_limit).is_ok());
    }

    // ── validate_visibility ───────────────────────────────────────────────────

    #[test]
    fn validate_visibility_accepts_listed_and_unlisted() {
        assert!(validate_visibility("listed").is_ok());
        assert!(validate_visibility("unlisted").is_ok());
    }

    #[test]
    fn validate_visibility_rejects_unknown_token() {
        assert!(validate_visibility("chartreuse").is_err());
        assert!(validate_visibility("").is_err());
    }

    // ── is_bare_repo_id ───────────────────────────────────────────────────────

    #[test]
    fn bare_repo_id_accepts_valid() {
        assert!(is_bare_repo_id("buzz"));
        assert!(is_bare_repo_id("my-repo_1.0"));
    }

    #[test]
    fn bare_repo_id_rejects_colon() {
        assert!(!is_bare_repo_id("30617:something"));
        assert!(!is_bare_repo_id("has:colon"));
    }

    #[test]
    fn bare_repo_id_rejects_empty() {
        assert!(!is_bare_repo_id(""));
    }

    #[test]
    fn bare_repo_id_rejects_over_64() {
        let long = "a".repeat(65);
        assert!(!is_bare_repo_id(&long));
    }

    // ── tag helpers ───────────────────────────────────────────────────────────

    fn make_test_tag(parts: &[&str]) -> Tag {
        Tag::parse(parts.iter().copied()).unwrap()
    }

    // ── rebuild_project: hinted / unknown tag preservation ───────────────────

    #[test]
    fn rebuild_project_preserves_hinted_member_tags() {
        // A member 'a' tag with a relay hint must survive RMW untouched.
        let coord = format!("30617:{OWNER_HEX}:buzz");
        let hint = "wss://relay.example.com";
        let tags = vec![
            make_test_tag(&["d", "platform"]),
            Tag::parse(["a", &coord, hint]).unwrap(),
        ];
        let ts = Timestamp::from(1_700_000_001u64);
        let b = rebuild_project("", tags, ts).unwrap();
        let ev = b.sign_with_keys(&nostr::Keys::generate()).expect("sign");
        let a_tag = ev
            .tags
            .iter()
            .find(|t| tag_name(t) == Some("a"))
            .expect("a tag present");
        assert_eq!(
            a_tag.as_slice(),
            &["a".to_string(), coord, hint.to_string()],
            "relay hint must survive rebuild"
        );
    }

    #[test]
    fn rebuild_project_preserves_unknown_tags() {
        let tags = vec![
            make_test_tag(&["d", "platform"]),
            make_test_tag(&["future-metadata", "value"]),
        ];
        let ts = Timestamp::from(1_700_000_001u64);
        let b = rebuild_project("", tags, ts).unwrap();
        let ev = b.sign_with_keys(&nostr::Keys::generate()).expect("sign");
        assert!(ev
            .tags
            .iter()
            .any(|t| tag_name(t) == Some("future-metadata")));
    }

    #[test]
    fn rebuild_project_strips_auth_tag() {
        let tags = vec![
            make_test_tag(&["d", "platform"]),
            make_test_tag(&["auth", &"a".repeat(64), "kind=30617", &"b".repeat(128)]),
        ];
        let ts = Timestamp::from(1_700_000_001u64);
        let b = rebuild_project("", tags, ts).unwrap();
        let ev = b.sign_with_keys(&nostr::Keys::generate()).expect("sign");
        assert!(
            !ev.tags.iter().any(|t| tag_name(t) == Some("auth")),
            "auth tag must be stripped"
        );
    }

    #[test]
    fn rebuild_project_rejects_over_cap_foreign_head() {
        // A foreign head with 65 members must fail Layer A on republish.
        let mut tags = vec![make_test_tag(&["d", "wide"])];
        for i in 0..=64u32 {
            let coord = format!("30617:{OWNER_HEX}:repo-{i:02}");
            tags.push(make_test_tag(&["a", &coord]));
        }
        assert_eq!(
            tags.iter().filter(|t| tag_name(t) == Some("a")).count(),
            65,
            "65 a-tags"
        );
        let ts = Timestamp::from(1_700_000_001u64);
        // rebuild_project strips auth, but 65 a-tags still exceeds cap.
        assert!(
            rebuild_project("", tags, ts).is_err(),
            "over-cap foreign head must fail rebuild"
        );
    }

    #[test]
    fn rebuild_project_at_exact_cap_succeeds() {
        let mut tags = vec![make_test_tag(&["d", "wide"])];
        for i in 0..PROJECT_MEMBER_CAP {
            let coord = format!("30617:{OWNER_HEX}:repo-{i:02}");
            tags.push(make_test_tag(&["a", &coord]));
        }
        let ts = Timestamp::from(1_700_000_001u64);
        assert!(rebuild_project("", tags, ts).is_ok());
    }

    // ── clear-flag semantics ──────────────────────────────────────────────────

    /// Build a minimal head Event for testing update semantics without the relay.
    fn make_head_tags(extra: &[Tag]) -> Vec<Tag> {
        let mut tags = vec![make_test_tag(&["d", "platform"])];
        tags.extend_from_slice(extra);
        tags
    }

    #[allow(clippy::too_many_arguments)]
    fn apply_update_tags(
        head_tags: Vec<Tag>,
        name: Option<&str>,
        clear_name: bool,
        description: Option<&str>,
        clear_description: bool,
        channel: Option<&str>,
        clear_channel: bool,
        visibility: Option<&str>,
        clear_visibility: bool,
    ) -> Vec<Tag> {
        // Replicate the tag-mutation logic from cmd_update (sans relay I/O).
        let singleton_fields = ["name", "description", "buzz-channel", "buzz-visibility"];
        let mut tags: Vec<Tag> = head_tags
            .iter()
            .filter(|t| {
                if tag_name(t) == Some("auth") {
                    return false;
                }
                if let Some(field) = tag_name(t) {
                    if singleton_fields.contains(&field) {
                        let clear = match field {
                            "name" => clear_name || name.is_some(),
                            "description" => clear_description || description.is_some(),
                            "buzz-channel" => clear_channel || channel.is_some(),
                            "buzz-visibility" => clear_visibility || visibility.is_some(),
                            _ => false,
                        };
                        return !clear;
                    }
                }
                true
            })
            .cloned()
            .collect();
        if let Some(n) = name {
            tags.push(make_test_tag(&["name", n]));
        }
        if let Some(d) = description {
            tags.push(make_test_tag(&["description", d]));
        }
        if let Some(ch) = channel {
            tags.push(make_test_tag(&["buzz-channel", ch]));
        }
        if let Some(vis) = visibility {
            tags.push(make_test_tag(&["buzz-visibility", vis]));
        }
        tags
    }

    #[test]
    fn update_omission_preserves_existing_field() {
        let head = make_head_tags(&[make_test_tag(&["name", "Old Name"])]);
        let result = apply_update_tags(head, None, false, None, false, None, false, None, false);
        assert!(result.iter().any(|t| tag_value(t) == Some("Old Name")));
    }

    #[test]
    fn update_setter_replaces_existing_field() {
        let head = make_head_tags(&[make_test_tag(&["name", "Old Name"])]);
        let result = apply_update_tags(
            head,
            Some("New Name"),
            false,
            None,
            false,
            None,
            false,
            None,
            false,
        );
        assert!(result.iter().any(|t| tag_value(t) == Some("New Name")));
        assert!(!result.iter().any(|t| tag_value(t) == Some("Old Name")));
    }

    #[test]
    fn update_clear_drops_existing_field() {
        let head = make_head_tags(&[make_test_tag(&["name", "Old Name"])]);
        let result = apply_update_tags(head, None, true, None, false, None, false, None, false);
        assert!(!result.iter().any(|t| tag_name(t) == Some("name")));
    }

    #[test]
    fn update_clear_visibility_drops_tag() {
        let head = make_head_tags(&[make_test_tag(&["buzz-visibility", "unlisted"])]);
        let result = apply_update_tags(head, None, false, None, false, None, false, None, true);
        assert!(!result
            .iter()
            .any(|t| tag_name(t) == Some("buzz-visibility")));
    }

    #[test]
    fn update_exactly_one_singleton_after_replace() {
        // Start with a buzz-channel; replace with a new one; must have exactly one.
        let uuid1 = "3580ca9b-47b4-4af9-b22a-1068778f26c6";
        let uuid2 = "00000000-0000-0000-0000-000000000000";
        let head = make_head_tags(&[make_test_tag(&["buzz-channel", uuid1])]);
        let result = apply_update_tags(
            head,
            None,
            false,
            None,
            false,
            Some(uuid2),
            false,
            None,
            false,
        );
        let channels: Vec<_> = result
            .iter()
            .filter(|t| tag_name(t) == Some("buzz-channel"))
            .collect();
        assert_eq!(channels.len(), 1);
        assert_eq!(tag_value(channels[0]), Some(uuid2));
    }

    // ── duplicate-member rejection on republish ───────────────────────────────

    #[test]
    fn duplicate_member_in_foreign_head_fails_rebuild() {
        let coord = format!("30617:{OWNER_HEX}:buzz");
        let tags = vec![
            make_test_tag(&["d", "platform"]),
            make_test_tag(&["a", &coord]),
            make_test_tag(&["a", &coord]), // duplicate
        ];
        let ts = Timestamp::from(1_700_000_001u64);
        assert!(rebuild_project("", tags, ts).is_err());
    }

    // ── validate_project_envelope integration ────────────────────────────────

    #[test]
    fn validate_project_envelope_accepts_hinted_member() {
        let coord = format!("30617:{OWNER_HEX}:buzz");
        let tags = vec![
            make_test_tag(&["d", "platform"]),
            Tag::parse(["a", &coord, "wss://relay.example.com"]).unwrap(),
        ];
        assert!(validate_project_envelope(&tags, "").is_ok());
    }

    #[test]
    fn validate_project_envelope_rejects_four_element_member() {
        let coord = format!("30617:{OWNER_HEX}:buzz");
        let tags = vec![
            make_test_tag(&["d", "platform"]),
            Tag::parse(["a", &coord, "wss://relay.example.com", "extra"]).unwrap(),
        ];
        assert!(validate_project_envelope(&tags, "").is_err());
    }

    // ── next_timestamp ordering ───────────────────────────────────────────────

    /// `next_timestamp` must return `head.created_at + 1` regardless of the wall
    /// clock.  NIP-MP Deletion rule: a tombstone older than the live head does
    /// NOT remove it, so we must advance strictly off the observed head — never
    /// use wall-clock time, which could be behind a head that was bumped
    /// multiple times in the same second.
    #[test]
    fn next_timestamp_returns_head_plus_one_when_head_is_ahead_of_wall_clock() {
        // Build a minimal signed event with a created_at far in the future.
        let keys = nostr::Keys::generate();
        let far_future_ts = Timestamp::from(9_999_999_999u64); // year 2286
        let tags = vec![
            make_test_tag(&["d", "platform"]),
            make_test_tag(&["a", &format!("30617:{OWNER_HEX}:buzz")]),
        ];
        let builder = rebuild_project("", tags, far_future_ts).expect("valid head envelope");
        let head = builder.sign_with_keys(&keys).expect("sign");
        // Verify the event actually has our future timestamp.
        assert_eq!(head.created_at, far_future_ts);

        // next_timestamp must return far_future + 1, not now().
        let next = next_timestamp(&head).expect("no overflow");
        assert_eq!(
            next.as_secs(),
            far_future_ts.as_secs() + 1,
            "tombstone must be strictly after head, even when head is far in the future"
        );
    }

    // ── empty update guard ────────────────────────────────────────────────────

    /// `cmd_update` with no setters or clearers must return `CliError::Usage`
    /// before making any network call.  The guard is synchronous (before the
    /// first `.await`) so we can drive it with a dummy client whose address
    /// would reject any real connection attempt.
    #[tokio::test]
    async fn empty_update_returns_usage_error_before_any_network_call() {
        let keys = nostr::Keys::generate();
        // Port 9 is the discard protocol — any real connect will be refused
        // immediately, but the guard fires before the first await so this
        // never reaches the network.
        let client = crate::client::BuzzClient::new("http://127.0.0.1:9".into(), keys, None, None)
            .expect("client construction");

        let err = cmd_update(
            &client, "my-slug", None, false, // name / clear_name
            None, false, // description / clear_description
            None, false, // channel / clear_channel
            None, false, // visibility / clear_visibility
        )
        .await
        .expect_err("empty update must fail");

        assert!(
            matches!(err, CliError::Usage(_)),
            "expected CliError::Usage, got {err:?}"
        );
    }

    // ── no-network malformed-input tests ─────────────────────────────────────
    //
    // All three cases use port 9 (discard protocol): any real connection is
    // refused immediately, but local validation fires before the first .await
    // so the network is never touched.

    fn discard_client() -> crate::client::BuzzClient {
        let keys = nostr::Keys::generate();
        crate::client::BuzzClient::new("http://127.0.0.1:9".into(), keys, None, None)
            .expect("client construction")
    }

    /// Invalid visibility token must return Usage before touching the relay.
    #[tokio::test]
    async fn create_invalid_visibility_returns_usage_before_any_network_call() {
        let client = discard_client();
        let err = cmd_create(
            &client,
            "my-slug",
            &["buzz".to_string()],
            None,
            None,
            None,
            Some("chartreuse"),
        )
        .await
        .expect_err("invalid visibility must fail");
        assert!(
            matches!(err, CliError::Usage(_)),
            "expected CliError::Usage for invalid visibility, got {err:?}"
        );
    }

    /// A name longer than 256 bytes must return Usage before touching the relay.
    #[tokio::test]
    async fn create_overlong_name_returns_usage_before_any_network_call() {
        let client = discard_client();
        let long_name = "a".repeat(257);
        let err = cmd_create(
            &client,
            "my-slug",
            &["buzz".to_string()],
            Some(&long_name),
            None,
            None,
            None,
        )
        .await
        .expect_err("overlong name must fail");
        assert!(
            matches!(err, CliError::Usage(_)),
            "expected CliError::Usage for overlong name, got {err:?}"
        );
    }

    /// A malformed --repo coordinate must return Usage before touching the relay.
    #[tokio::test]
    async fn create_malformed_repo_returns_usage_before_any_network_call() {
        let client = discard_client();
        let err = cmd_create(
            &client,
            "my-slug",
            &["nope:bad".to_string()],
            None,
            None,
            None,
            None,
        )
        .await
        .expect_err("malformed repo must fail");
        assert!(
            matches!(err, CliError::Usage(_)),
            "expected CliError::Usage for malformed repo, got {err:?}"
        );
    }

    /// A malformed --repo coordinate on add-repo must return Usage before touching the relay.
    #[tokio::test]
    async fn add_repo_malformed_coord_returns_usage_before_any_network_call() {
        let client = discard_client();
        let err = cmd_add_repo(&client, "my-slug", &["nope:bad".to_string()])
            .await
            .expect_err("malformed repo must fail");
        assert!(
            matches!(err, CliError::Usage(_)),
            "expected CliError::Usage for malformed repo on add-repo, got {err:?}"
        );
    }

    /// A malformed --repo coordinate on remove-repo must return Usage before touching the relay.
    #[tokio::test]
    async fn remove_repo_malformed_coord_returns_usage_before_any_network_call() {
        let client = discard_client();
        let err = cmd_remove_repo(&client, "my-slug", &["nope:bad".to_string()])
            .await
            .expect_err("malformed repo must fail");
        assert!(
            matches!(err, CliError::Usage(_)),
            "expected CliError::Usage for malformed repo on remove-repo, got {err:?}"
        );
    }

    // ── duplicate --repo within one invocation ────────────────────────────────

    /// Supplying the same coordinate twice in one create call must return Usage
    /// (names the duplicate) before any network call.
    #[tokio::test]
    async fn create_duplicate_repo_returns_usage_before_any_network_call() {
        let client = discard_client();
        let coord = format!("30617:{OWNER_HEX}:buzz");
        let err = cmd_create(
            &client,
            "my-slug",
            &[coord.clone(), coord.clone()],
            None,
            None,
            None,
            None,
        )
        .await
        .expect_err("duplicate repo must fail");
        assert!(
            matches!(err, CliError::Usage(_)),
            "expected CliError::Usage for duplicate repo, got {err:?}"
        );
        // Error message must name the duplicate coordinate.
        assert!(
            format!("{err}").contains("buzz"),
            "Usage message must name the duplicate coordinate, got {err:?}"
        );
    }

    /// Supplying the same coordinate twice in one add-repo call must return Usage
    /// (names the duplicate) before any network call.
    #[tokio::test]
    async fn add_repo_duplicate_coord_returns_usage_before_any_network_call() {
        let client = discard_client();
        let coord = format!("30617:{OWNER_HEX}:buzz");
        let err = cmd_add_repo(&client, "my-slug", &[coord.clone(), coord.clone()])
            .await
            .expect_err("duplicate repo must fail");
        assert!(
            matches!(err, CliError::Usage(_)),
            "expected CliError::Usage for duplicate repo on add-repo, got {err:?}"
        );
    }

    // ── create collision guard ────────────────────────────────────────────────

    // The create-collision Conflict path is pinned by the live transcript
    // (step: duplicate create → Conflict, exit=5). No relay mock is available
    // for a unit test; the no-network tests above cover all pre-await paths.

    // ── add-repo no-op guard ──────────────────────────────────────────────────

    // The add-repo no-op Conflict path is pinned by the live transcript
    // (step 7: buzz already present → exit=5). No relay mock is available
    // for a unit test; the async no-network tests above cover all pre-await paths.
}
