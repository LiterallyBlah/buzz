//! The fake relay the extension-data write tests publish against.
//!
//! Its own module because it is not a constant or a predicate: it is a threaded
//! HTTP server that serves **both** endpoints a publish uses — `POST /events`
//! to accept the submission and `POST /query` for the head read-back — and can
//! disturb host authority in either of the two windows between them. Keeping it
//! beside the two-line fixtures in [`super::extension_data_test_support`] would
//! misdescribe both.
//!
//! Extracted from [`super::extension_data_tests`] rather than shortened: that
//! module was 969 lines before this boundary's regression and had no room for
//! it, and cutting the rationale off the tests to fit a line count is the wrong
//! half to lose.

/// What the fake relay should return from the head query.
pub(super) enum HeadReply {
    /// Echo the event that was just submitted — the fresh-write case.
    EchoSubmitted,
    /// Serve a different event — the superseded-before-read-back case.
    ServeOther(String),
    /// Fail the query — the read-back-unavailable case.
    Fail,
}

pub(super) fn fake_relay(mode: HeadReply) -> String {
    fake_relay_with(mode, Disturb::default())
}

/// Where a fake relay should disturb host authority during a publish.
///
/// The two windows are different production branches, and only the relay can
/// tell them apart: `after_submit` fires once the write has committed but
/// before the confirmation query goes out, so the read-back's **pre-send**
/// recheck sees it; `before_head_reply` fires with the query already on the
/// wire, which only the **post-response** recheck can catch.
#[derive(Default)]
pub(super) struct Disturb {
    pub(super) after_submit: Option<Box<dyn Fn() + Send>>,
    pub(super) before_head_reply: Option<Box<dyn Fn() + Send>>,
}

pub(super) fn fake_relay_with(mode: HeadReply, disturb: Disturb) -> String {
    use std::io::{Read as _, Write as _};
    let Disturb {
        after_submit,
        before_head_reply,
    } = disturb;

    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("addr");

    std::thread::spawn(move || {
        let mut submitted: Option<String> = None;
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { break };
            let mut raw = Vec::new();
            let mut buf = [0u8; 8192];
            // Read headers plus the declared body.
            while let Ok(n) = stream.read(&mut buf) {
                if n == 0 {
                    break;
                }
                raw.extend_from_slice(&buf[..n]);
                let text = String::from_utf8_lossy(&raw).to_string();
                if let Some((head, body)) = text.split_once("\r\n\r\n") {
                    let want = head
                        .lines()
                        .find_map(|l| {
                            l.strip_prefix("Content-Length: ")
                                .or_else(|| l.strip_prefix("content-length: "))
                        })
                        .and_then(|v| v.trim().parse::<usize>().ok())
                        .unwrap_or(0);
                    if body.len() >= want {
                        break;
                    }
                }
            }
            let text = String::from_utf8_lossy(&raw).to_string();
            let (head, body) = text.split_once("\r\n\r\n").unwrap_or(("", ""));
            let is_query = head.starts_with("POST /query");

            let (status, payload) = if is_query {
                // The query is already on the wire; anything from here can only
                // be caught after the response.
                if let Some(hook) = &before_head_reply {
                    hook();
                }
                match &mode {
                    HeadReply::Fail => ("500 Internal Server Error", "{}".to_string()),
                    HeadReply::ServeOther(other) => ("200 OK", format!("[{other}]")),
                    HeadReply::EchoSubmitted => match &submitted {
                        Some(event) => ("200 OK", format!("[{event}]")),
                        None => ("200 OK", "[]".to_string()),
                    },
                }
            } else {
                // A submission: remember it so the head query can echo it back.
                submitted = Some(body.to_string());
                // The event is now committed as far as the host can tell. Any
                // disturbance from here lands strictly between the write and
                // its confirmation.
                if let Some(hook) = &after_submit {
                    hook();
                }
                let id = body
                    .split_once("\"id\":\"")
                    .and_then(|(_, rest)| rest.get(..64))
                    .unwrap_or_default()
                    .to_string();
                (
                    "200 OK",
                    format!(r#"{{"event_id":"{id}","accepted":true,"message":""}}"#),
                )
            };

            let response = format!(
                "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{payload}",
                payload.len()
            );
            let _ = stream.write_all(response.as_bytes());
            let _ = stream.flush();
        }
    });

    format!("http://{addr}")
}
