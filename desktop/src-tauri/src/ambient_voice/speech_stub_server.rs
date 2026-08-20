//! A speech server, for tests.
//!
//! The HTTP speech backends are a wire contract, and a mock built out of the
//! same beliefs as the code under test proves nothing about it. This is a real
//! socket speaking real HTTP/1.1 on loopback, so a request that names its part
//! wrongly, sends the wrong content type or posts to the wrong path fails here
//! the way it would fail against the server it was written for.
//!
//! Deliberately tiny: one connection at a time, `Content-Length` bodies only,
//! `Connection: close` on every reply. That is the whole of what the two
//! request functions send and receive.

use std::{
    io::{Read, Write},
    net::{TcpListener, TcpStream},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    thread,
    time::Duration,
};

/// One request the stub received, as the handler and the assertions see it.
#[derive(Debug, Clone)]
pub(crate) struct StubRequest {
    pub method: String,
    pub path: String,
    pub content_type: Option<String>,
    pub body: Vec<u8>,
}

impl StubRequest {
    /// The body as text, for the JSON and multipart assertions.
    pub fn body_text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }
}

/// What the stub answers with.
#[derive(Debug, Clone)]
pub(crate) struct StubReply {
    pub status: u16,
    pub content_type: &'static str,
    pub body: Vec<u8>,
    /// A `Location` header, for the redirect replies that prove the speech
    /// client does not chase a 307/308 to another host.
    pub location: Option<String>,
}

impl StubReply {
    pub fn json(body: &str) -> Self {
        Self {
            status: 200,
            content_type: "application/json",
            body: body.as_bytes().to_vec(),
            location: None,
        }
    }

    pub fn wav(body: Vec<u8>) -> Self {
        Self {
            status: 200,
            content_type: "audio/wav",
            body,
            location: None,
        }
    }

    pub fn status(status: u16, body: &str) -> Self {
        Self {
            status,
            content_type: "text/plain",
            body: body.as_bytes().to_vec(),
            location: None,
        }
    }

    /// A redirect answer: `status` (e.g. 307) pointing at `location`.
    pub fn redirect(status: u16, location: &str) -> Self {
        Self {
            status,
            content_type: "text/plain",
            body: Vec::new(),
            location: Some(location.to_string()),
        }
    }
}

pub(crate) struct StubSpeechServer {
    base_url: String,
    port: u16,
    received: Arc<Mutex<Vec<StubRequest>>>,
    shutdown: Arc<AtomicBool>,
    thread: Option<thread::JoinHandle<()>>,
}

impl StubSpeechServer {
    /// Start a server that answers every request through `handler`.
    pub fn start(handler: impl Fn(&StubRequest) -> StubReply + Send + 'static) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback");
        let port = listener.local_addr().expect("local addr").port();
        let received: Arc<Mutex<Vec<StubRequest>>> = Arc::new(Mutex::new(Vec::new()));
        let shutdown = Arc::new(AtomicBool::new(false));

        let served = Arc::clone(&received);
        let stopping = Arc::clone(&shutdown);
        let thread = thread::Builder::new()
            .name("speech-stub-server".into())
            .spawn(move || {
                for stream in listener.incoming() {
                    if stopping.load(Ordering::Acquire) {
                        break;
                    }
                    let Ok(stream) = stream else { continue };
                    if let Some(request) = read_request(&stream) {
                        let reply = handler(&request);
                        served
                            .lock()
                            .unwrap_or_else(|error| error.into_inner())
                            .push(request);
                        write_reply(stream, &reply);
                    }
                }
            })
            .expect("spawn stub server");

        Self {
            base_url: format!("http://127.0.0.1:{port}"),
            port,
            received,
            shutdown,
            thread: Some(thread),
        }
    }

    /// A server that answers the same thing to everything.
    pub fn always(reply: StubReply) -> Self {
        Self::start(move |_| reply.clone())
    }

    /// The base URL to configure a backend with.
    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    pub fn requests(&self) -> Vec<StubRequest> {
        self.received
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clone()
    }

    /// Wait until at least `count` requests have arrived, or give up.
    ///
    /// Returns what arrived either way so a failing assertion can print it.
    pub fn wait_for_requests(&self, count: usize, timeout: Duration) -> Vec<StubRequest> {
        let deadline = std::time::Instant::now() + timeout;
        loop {
            let requests = self.requests();
            if requests.len() >= count || std::time::Instant::now() >= deadline {
                return requests;
            }
            thread::sleep(Duration::from_millis(10));
        }
    }
}

impl Drop for StubSpeechServer {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::Release);
        // `incoming()` is blocked in accept; one connection wakes it so the
        // thread can see the flag and leave.
        let _ = TcpStream::connect(("127.0.0.1", self.port));
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// Read one HTTP/1.1 request: request line, headers, `Content-Length` body.
fn read_request(stream: &TcpStream) -> Option<StubRequest> {
    let mut stream = stream;
    let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
    let mut buffer = Vec::new();
    let mut chunk = [0u8; 4096];
    let head_end = loop {
        if let Some(position) = find_head_end(&buffer) {
            break position;
        }
        match stream.read(&mut chunk) {
            Ok(0) | Err(_) => return None,
            Ok(read) => buffer.extend_from_slice(&chunk[..read]),
        }
    };

    let head = String::from_utf8_lossy(&buffer[..head_end]).into_owned();
    let mut lines = head.lines();
    let mut request_line = lines.next()?.split_whitespace();
    let method = request_line.next()?.to_string();
    let path = request_line.next()?.to_string();

    let mut content_length = 0usize;
    let mut content_type = None;
    for line in lines {
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        let value = value.trim();
        if name.eq_ignore_ascii_case("content-length") {
            content_length = value.parse().unwrap_or(0);
        } else if name.eq_ignore_ascii_case("content-type") {
            content_type = Some(value.to_string());
        }
    }

    let mut body = buffer[head_end + 4..].to_vec();
    while body.len() < content_length {
        match stream.read(&mut chunk) {
            Ok(0) | Err(_) => break,
            Ok(read) => body.extend_from_slice(&chunk[..read]),
        }
    }
    body.truncate(content_length);
    Some(StubRequest {
        method,
        path,
        content_type,
        body,
    })
}

fn find_head_end(buffer: &[u8]) -> Option<usize> {
    buffer.windows(4).position(|window| window == b"\r\n\r\n")
}

fn write_reply(mut stream: TcpStream, reply: &StubReply) {
    let location = match &reply.location {
        Some(target) => format!("Location: {target}\r\n"),
        None => String::new(),
    };
    let head = format!(
        "HTTP/1.1 {} STUB\r\nContent-Type: {}\r\n{}Content-Length: {}\r\nConnection: close\r\n\r\n",
        reply.status,
        reply.content_type,
        location,
        reply.body.len()
    );
    let _ = stream.write_all(head.as_bytes());
    let _ = stream.write_all(&reply.body);
    let _ = stream.flush();
    let _ = stream.shutdown(std::net::Shutdown::Both);
}
