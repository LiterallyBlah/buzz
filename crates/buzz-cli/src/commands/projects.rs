//! `buzz projects` — the relay reads a project-routed agent runtime needs.
//!
//! An agent that holds conversations on issues and pull requests has two
//! questions no other command answers: which roots address me, and what has
//! happened on a root I am already enrolled in. Both are ordinary NIP-01
//! filters; what they are not is reachable through `issues get` (one root by
//! id, no thread) or `feed get` (`#p`-scoped, so it cannot see the untagged
//! status event that closed an issue).
//!
//! Every filter here is scoped and bounded. `roots` requires both a repository
//! set and the mentioned agent, and `history` requires the roots — the
//! unscoped forms are "every project event on the relay", which is not a
//! smaller version of the same question.

use buzz_core::kind::{
    KIND_GIT_ISSUE, KIND_GIT_PR_UPDATE, KIND_GIT_PULL_REQUEST, KIND_GIT_REPO_ANNOUNCEMENT,
    KIND_GIT_STATUS_CLOSED, KIND_GIT_STATUS_DRAFT, KIND_GIT_STATUS_MERGED, KIND_GIT_STATUS_OPEN,
    KIND_TEXT_NOTE,
};
use buzz_core::peer_call::{KIND_PEER_CALL, KIND_PEER_CALL_RESULT};
use serde_json::json;

use crate::client::BuzzClient;
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
fn canonical_coordinate(raw: &str) -> Result<String, CliError> {
    let parts: Vec<&str> = raw.split(':').collect();
    let [kind, owner, id] = parts[..] else {
        return Err(CliError::Usage(format!(
            "--project must be 30617:<owner>:<identifier> (got {raw:?})"
        )));
    };
    if kind.parse::<u32>().ok() != Some(KIND_GIT_REPO_ANNOUNCEMENT) {
        return Err(CliError::Usage(format!(
            "--project must start with {KIND_GIT_REPO_ANNOUNCEMENT}: (got {raw:?})"
        )));
    }
    validate_hex64(owner)?;
    validate_repo_id(id)?;
    Ok(format!(
        "{KIND_GIT_REPO_ANNOUNCEMENT}:{}:{id}",
        owner.to_ascii_lowercase()
    ))
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

pub async fn dispatch(command: ProjectsCmd, client: &BuzzClient) -> Result<(), CliError> {
    match command {
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
    }
}

/// The filters these commands submit, proved against a relay that records what
/// it was asked.
///
/// Every test drives the real `dispatch` and reads the request body the relay
/// received, because the filter *is* the product here: a `#e` where an `#E`
/// belongs, or a missing `#p`, returns a plausible-looking array that is
/// missing exactly the events the caller needed.
#[cfg(test)]
mod tests {
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
