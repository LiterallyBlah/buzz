use crate::client::BuzzClient;
use crate::commands::with_git_provenance;
use crate::error::CliError;
use crate::validate::{read_or_stdin, sdk_err, validate_hex64, validate_repo_id};
use buzz_sdk::{GitCommentMeta, GitIssueMeta, GitRepoCoord, GitStatusMeta};

pub async fn cmd_create_issue(
    client: &BuzzClient,
    repo_owner: &str,
    repo_id: &str,
    subject: &str,
    content: &str,
    labels: &[String],
    to: &[String],
) -> Result<(), CliError> {
    validate_hex64(repo_owner)?;
    validate_repo_id(repo_id)?;
    let body = read_or_stdin(content)?;

    let meta = GitIssueMeta {
        labels: labels.to_vec(),
        recipients: to.to_vec(),
    };

    let repo = GitRepoCoord {
        owner: repo_owner.to_string(),
        id: repo_id.to_string(),
    };

    let builder = with_git_provenance(
        buzz_sdk::build_git_issue(&repo, subject, &body, &meta).map_err(sdk_err)?,
    )?;
    let event = client.sign_event(builder)?;
    let event_id = event.id.to_hex();
    let resp = client.submit_event(event).await?;
    // `link` renders as a rich preview card in Buzz Desktop when included in
    // a chat message — agents announce issues with it (see base_prompt.md).
    let link = crate::links::issue_link(&event_id, repo_owner, repo_id);
    crate::client::print_create_response(&resp, "link", &link);
    Ok(())
}

#[allow(clippy::too_many_arguments)]
pub async fn cmd_comment_issue(
    client: &BuzzClient,
    repo_owner: &str,
    repo_id: &str,
    root: &str,
    reply_to: Option<&str>,
    content: &str,
    to: &[String],
) -> Result<(), CliError> {
    validate_hex64(repo_owner)?;
    validate_repo_id(repo_id)?;
    validate_hex64(root)?;
    if let Some(parent) = reply_to {
        validate_hex64(parent)?;
    }
    let body = read_or_stdin(content)?;

    let meta = GitCommentMeta {
        root_event: root.to_string(),
        parent_event: reply_to.map(str::to_string),
        recipients: to.to_vec(),
    };

    let repo = GitRepoCoord {
        owner: repo_owner.to_string(),
        id: repo_id.to_string(),
    };

    let builder = buzz_sdk::build_git_comment(&repo, &body, &meta).map_err(sdk_err)?;
    let event = client.sign_event(builder)?;
    let resp = client.submit_event(event).await?;
    println!("{resp}");
    Ok(())
}

pub async fn cmd_get_issue(client: &BuzzClient, event: &str) -> Result<(), CliError> {
    validate_hex64(event)?;
    let filter = serde_json::json!({
        "kinds": [1621],
        "ids": [event]
    });
    let resp = client.query(&filter).await?;
    println!("{resp}");
    Ok(())
}

pub async fn cmd_list_issues(
    client: &BuzzClient,
    repo_owner: &str,
    repo_id: &str,
    author: Option<&str>,
    label: Option<&str>,
    limit: Option<u32>,
) -> Result<(), CliError> {
    validate_hex64(repo_owner)?;
    validate_repo_id(repo_id)?;

    let a_value = format!("30617:{repo_owner}:{repo_id}");
    let mut filter = serde_json::json!({
        "kinds": [1621],
        "#a": [a_value]
    });

    if let Some(pk) = author {
        validate_hex64(pk)?;
        filter["authors"] = serde_json::json!([pk]);
    }
    if let Some(l) = label {
        filter["#t"] = serde_json::json!([l]);
    }
    if let Some(n) = limit {
        filter["limit"] = serde_json::json!(n);
    }

    let resp = client.query(&filter).await?;
    println!("{resp}");
    Ok(())
}

#[allow(clippy::too_many_arguments)]
pub async fn cmd_issue_status(
    client: &BuzzClient,
    issue: &str,
    status: &str,
    content: Option<&str>,
    repo_owner: Option<&str>,
    repo_id: Option<&str>,
    euc: Option<&str>,
    to: &[String],
) -> Result<(), CliError> {
    validate_hex64(issue)?;
    let status = crate::commands::patches::parse_status(status)?;
    let body = match content {
        Some(c) => read_or_stdin(c)?,
        None => String::new(),
    };

    let repo = match (repo_owner, repo_id) {
        (Some(owner), Some(id)) => {
            validate_hex64(owner)?;
            validate_repo_id(id)?;
            Some(GitRepoCoord {
                owner: owner.to_string(),
                id: id.to_string(),
            })
        }
        (None, None) => None,
        _ => {
            return Err(CliError::Usage(
                "--repo-owner and --repo-id must be given together".into(),
            ))
        }
    };

    // Mirrors `buzz patches status`: default a `p` tag to the repo owner
    // for discoverability, plus a `--to` escape hatch for the issue author
    // or anyone else who should be notified of the status change.
    let mut recipients = Vec::new();
    if let Some(ref repo) = repo {
        recipients.push(repo.owner.clone());
    }
    for recipient in to {
        validate_hex64(recipient)?;
        if !recipients.contains(recipient) {
            recipients.push(recipient.clone());
        }
    }

    let meta = GitStatusMeta {
        root_event: issue.to_string(),
        accepted_revision_root: None,
        repo,
        euc: euc.map(str::to_string),
        recipients,
        applied_patches: vec![],
        merge_commit: None,
        applied_as_commits: vec![],
    };

    let builder =
        with_git_provenance(buzz_sdk::build_git_status(status, &body, &meta).map_err(sdk_err)?)?;
    let event = client.sign_event(builder)?;
    let resp = client.submit_event(event).await?;
    println!("{resp}");
    Ok(())
}

pub async fn dispatch(cmd: crate::IssuesCmd, client: &BuzzClient) -> Result<(), CliError> {
    use crate::IssuesCmd;
    match cmd {
        IssuesCmd::Create {
            repo_owner,
            repo_id,
            title,
            content,
            label,
            to,
        } => cmd_create_issue(client, &repo_owner, &repo_id, &title, &content, &label, &to).await,
        IssuesCmd::Comment {
            repo_owner,
            repo_id,
            root,
            reply_to,
            content,
            to,
        } => {
            cmd_comment_issue(
                client,
                &repo_owner,
                &repo_id,
                &root,
                reply_to.as_deref(),
                &content,
                &to,
            )
            .await
        }
        IssuesCmd::Get { event } => cmd_get_issue(client, &event).await,
        IssuesCmd::List {
            repo_owner,
            repo_id,
            author,
            label,
            limit,
        } => {
            cmd_list_issues(
                client,
                &repo_owner,
                &repo_id,
                author.as_deref(),
                label.as_deref(),
                limit,
            )
            .await
        }
        IssuesCmd::Status {
            issue,
            status,
            content,
            repo_owner,
            repo_id,
            euc,
            to,
        } => {
            cmd_issue_status(
                client,
                &issue,
                &status,
                content.as_deref(),
                repo_owner.as_deref(),
                repo_id.as_deref(),
                euc.as_deref(),
                &to,
            )
            .await
        }
    }
}

/// Project replies, from the command an agent types to the event that reaches
/// the relay.
///
/// These drive the real `dispatch` for both surfaces against a local HTTP relay
/// and assert on the **submitted event body** — the bytes that were signed and
/// accepted, not a builder's return value. The builder is already covered in
/// `buzz-sdk`; what is not covered anywhere else is that the CLI reaches it with
/// the operator's arguments intact and submits the result, and that
/// `buzz pr comment` and `buzz issues comment` really are one path rather than
/// two that agree today.
#[cfg(test)]
mod project_reply_tests {
    use std::net::SocketAddr;
    use std::sync::{Arc, Mutex};

    use axum::extract::State;
    use axum::routing::post;
    use axum::{Json, Router};
    use nostr::Keys;
    use serde_json::Value;
    use tokio::net::TcpListener;

    use super::*;
    use crate::{IssuesCmd, PrCmd};

    type Captured = Arc<Mutex<Vec<Value>>>;

    /// A relay that accepts events and keeps what it was sent.
    async fn capturing_relay() -> (String, Captured) {
        let captured: Captured = Arc::new(Mutex::new(Vec::new()));
        let state = captured.clone();
        let app = Router::new()
            .route(
                "/events",
                post(|State(seen): State<Captured>, body: String| async move {
                    if let Ok(event) = serde_json::from_str::<Value>(&body) {
                        seen.lock().unwrap().push(event);
                    }
                    Json(serde_json::json!({"accepted": true, "message": ""}))
                }),
            )
            .with_state(state);

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr: SocketAddr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        (format!("http://{addr}"), captured)
    }

    fn tag_values(event: &Value, key: &str) -> Vec<String> {
        event["tags"]
            .as_array()
            .expect("tags")
            .iter()
            .filter_map(|t| {
                let arr = t.as_array()?;
                (arr.first()?.as_str()? == key).then(|| arr.get(1)?.as_str().map(str::to_owned))?
            })
            .collect()
    }

    fn has_marked_root(event: &Value, root: &str) -> bool {
        event["tags"].as_array().expect("tags").iter().any(|t| {
            let Some(arr) = t.as_array() else {
                return false;
            };
            arr.first().and_then(Value::as_str) == Some("e")
                && arr.get(1).and_then(Value::as_str) == Some(root)
                && arr.get(3).and_then(Value::as_str) == Some("root")
        })
    }

    /// Everything the plan requires of a project reply, asserted on the event
    /// the relay accepted.
    fn assert_lands_on_root(event: &Value, owner: &str, repo_id: &str, root: &str, notify: &str) {
        assert_eq!(event["kind"].as_u64(), Some(1), "not a kind:1 comment");
        assert_eq!(
            tag_values(event, "a"),
            vec![format!("30617:{owner}:{repo_id}")],
            "without the repository coordinate the comment is outside the project's filter"
        );
        assert!(
            has_marked_root(event, root),
            "the reply is not attached to the originating root"
        );
        assert!(
            tag_values(event, "p").contains(&notify.to_string()),
            "nobody is notified, so the reply reaches no participant"
        );
        assert!(
            tag_values(event, "h").is_empty(),
            "an `h` scopes the reply to a channel and takes it out of the project"
        );
    }

    #[tokio::test]
    async fn an_issue_reply_lands_on_the_issue_root() {
        let owner = "a".repeat(64);
        let root = "b".repeat(64);
        let asker = "c".repeat(64);
        let (url, captured) = capturing_relay().await;
        let client = BuzzClient::new(url, Keys::generate(), None, None).unwrap();

        dispatch(
            IssuesCmd::Comment {
                repo_owner: owner.clone(),
                repo_id: "my-repo".into(),
                root: root.clone(),
                reply_to: None,
                content: "looking at it now".into(),
                to: vec![asker.clone()],
            },
            &client,
        )
        .await
        .expect("the issue reply must be accepted");

        let events = captured.lock().unwrap();
        assert_eq!(events.len(), 1, "exactly one event reached the relay");
        assert_lands_on_root(&events[0], &owner, "my-repo", &root, &asker);
        assert_eq!(events[0]["content"].as_str(), Some("looking at it now"));
    }

    /// The same assertions through `buzz pr comment`, on a pull-request root.
    ///
    /// A PR reply is not a different event, and this is where that stops being a
    /// claim in a doc comment: the accepted event is compared against the issue
    /// path's tag shape rather than merely being well-formed on its own.
    #[tokio::test]
    async fn a_pull_request_reply_lands_on_the_pull_request_root() {
        let owner = "a".repeat(64);
        let root = "d".repeat(64);
        let asker = "e".repeat(64);
        let (url, captured) = capturing_relay().await;
        let client = BuzzClient::new(url, Keys::generate(), None, None).unwrap();

        crate::commands::pr::dispatch(
            PrCmd::Comment {
                repo_owner: owner.clone(),
                repo_id: "my-repo".into(),
                root: root.clone(),
                reply_to: None,
                content: "the diff looks right".into(),
                to: vec![asker.clone()],
            },
            &client,
        )
        .await
        .expect("the pull-request reply must be accepted");

        let events = captured.lock().unwrap();
        assert_eq!(events.len(), 1);
        assert_lands_on_root(&events[0], &owner, "my-repo", &root, &asker);
        assert_eq!(events[0]["content"].as_str(), Some("the diff looks right"));
    }

    /// `--reply-to` answers a comment without detaching from the root.
    ///
    /// This is the failure that would leave a threaded reply visible in no
    /// issue at all: a client groups by the marked root, so a reply that carried
    /// only its parent would vanish from the conversation it belongs to.
    #[tokio::test]
    async fn a_threaded_reply_keeps_its_root_as_well_as_its_parent() {
        let owner = "a".repeat(64);
        let root = "b".repeat(64);
        let parent = "f".repeat(64);
        let asker = "c".repeat(64);
        let (url, captured) = capturing_relay().await;
        let client = BuzzClient::new(url, Keys::generate(), None, None).unwrap();

        dispatch(
            IssuesCmd::Comment {
                repo_owner: owner.clone(),
                repo_id: "my-repo".into(),
                root: root.clone(),
                reply_to: Some(parent.clone()),
                content: "answering that point".into(),
                to: vec![asker.clone()],
            },
            &client,
        )
        .await
        .expect("accepted");

        let events = captured.lock().unwrap();
        assert_lands_on_root(&events[0], &owner, "my-repo", &root, &asker);
        assert!(
            tag_values(&events[0], "e").contains(&parent),
            "the reply lost the comment it was answering"
        );
    }

    /// A malformed root is refused before anything is signed or submitted.
    #[tokio::test]
    async fn a_reply_to_no_valid_root_publishes_nothing() {
        let (url, captured) = capturing_relay().await;
        let client = BuzzClient::new(url, Keys::generate(), None, None).unwrap();

        dispatch(
            IssuesCmd::Comment {
                repo_owner: "a".repeat(64),
                repo_id: "my-repo".into(),
                root: "not-a-root".into(),
                reply_to: None,
                content: "hello".into(),
                to: vec![],
            },
            &client,
        )
        .await
        .expect_err("a malformed root must be refused");

        assert!(
            captured.lock().unwrap().is_empty(),
            "a refused reply still reached the relay"
        );
    }
}
