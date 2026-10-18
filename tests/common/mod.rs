//! Loopback-only HTTP fixtures for upload integration tests.

use serde_json::Value;
use std::collections::{HashMap, VecDeque};
use std::io::{Read, Write};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, Mutex,
};
use std::thread::{self, JoinHandle};
use std::time::Duration;

#[derive(Debug, Clone)]
pub struct CapturedRequest {
    pub method: String,
    pub path: String,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl CapturedRequest {
    pub fn json(&self) -> Value {
        serde_json::from_slice(&self.body).expect("mock request body should be JSON")
    }
}

struct MockResponse {
    status: u16,
    content_type: &'static str,
    body: Vec<u8>,
    headers: Vec<(String, String)>,
    delay: Option<Duration>,
}

impl MockResponse {
    fn json(status: u16, value: Value) -> Self {
        Self {
            status,
            content_type: "application/json",
            body: serde_json::to_vec(&value).expect("mock JSON should serialize"),
            headers: Vec::new(),
            delay: None,
        }
    }

    fn raw(status: u16, content_type: &'static str, body: Vec<u8>) -> Self {
        Self {
            status,
            content_type,
            body,
            headers: Vec::new(),
            delay: None,
        }
    }
}

type Requests = Arc<Mutex<Vec<CapturedRequest>>>;
type Responses = Arc<Mutex<HashMap<String, VecDeque<MockResponse>>>>;

/// A deterministic one-request-per-connection HTTP server on `127.0.0.1:0`.
pub struct MockEngine {
    base_url: String,
    listener_addr: String,
    requests: Requests,
    responses: Responses,
    stop: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
}

impl MockEngine {
    pub fn new() -> Self {
        let listener = TcpListener::bind(("127.0.0.1", 0)).expect("bind ephemeral loopback port");
        listener
            .set_nonblocking(true)
            .expect("set listener nonblocking");
        let listener_addr = listener
            .local_addr()
            .expect("read listener address")
            .to_string();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let responses = Arc::new(Mutex::new(HashMap::new()));
        let stop = Arc::new(AtomicBool::new(false));
        let thread_requests = Arc::clone(&requests);
        let thread_responses = Arc::clone(&responses);
        let thread_stop = Arc::clone(&stop);
        let worker = thread::spawn(move || {
            while !thread_stop.load(Ordering::Acquire) {
                match listener.accept() {
                    Ok((stream, _)) => {
                        handle_connection(stream, &thread_requests, &thread_responses)
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(1))
                    }
                    Err(_) => break,
                }
            }
        });
        Self {
            base_url: format!("http://{listener_addr}"),
            listener_addr,
            requests,
            responses,
            stop,
            worker: Some(worker),
        }
    }

    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    pub fn queue_json(&self, path: &str, status: u16, value: Value) {
        self.queue(path, MockResponse::json(status, value));
    }

    pub fn queue_raw(&self, path: &str, status: u16, content_type: &'static str, body: &[u8]) {
        self.queue(path, MockResponse::raw(status, content_type, body.to_vec()));
    }

    pub fn queue_delayed_json(&self, path: &str, status: u16, value: Value, delay: Duration) {
        let mut response = MockResponse::json(status, value);
        response.delay = Some(delay);
        self.queue(path, response);
    }

    pub fn queue_redirect(&self, path: &str, location: &str) {
        let mut response = MockResponse::raw(302, "text/plain", b"redirect".to_vec());
        response
            .headers
            .push(("Location".to_owned(), location.to_owned()));
        self.queue(path, response);
    }

    pub fn requests(&self) -> Vec<CapturedRequest> {
        self.requests.lock().expect("mock request lock").clone()
    }

    pub fn request_count(&self, method: &str, path: &str) -> usize {
        self.requests()
            .iter()
            .filter(|request| request.method == method && request.path == path)
            .count()
    }

    fn queue(&self, path: &str, response: MockResponse) {
        self.responses
            .lock()
            .expect("mock response lock")
            .entry(path.to_owned())
            .or_default()
            .push_back(response);
    }
}

impl Drop for MockEngine {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        let _ = TcpStream::connect(&self.listener_addr);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

fn handle_connection(mut stream: TcpStream, requests: &Requests, responses: &Responses) {
    // Windows can inherit the listener's nonblocking mode on accepted sockets.
    // The request reader uses blocking reads with a bounded timeout.
    stream
        .set_nonblocking(false)
        .expect("set accepted socket blocking");
    let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
    let _ = stream.set_write_timeout(Some(Duration::from_secs(2)));
    let Some(request) = read_request(&mut stream) else {
        let _ = stream.shutdown(Shutdown::Both);
        return;
    };
    let response = {
        let mut responses = responses.lock().expect("mock response lock");
        let route = request
            .path
            .split_once('?')
            .map(|(path, _)| path)
            .unwrap_or(&request.path);
        responses
            .get_mut(&request.path)
            .and_then(VecDeque::pop_front)
            .or_else(|| responses.get_mut(route).and_then(VecDeque::pop_front))
            .unwrap_or_else(|| MockResponse::raw(404, "text/plain", b"unconfigured route".to_vec()))
    };
    requests.lock().expect("mock request lock").push(request);
    if let Some(delay) = response.delay {
        thread::sleep(delay);
    }
    let reason = match response.status {
        200 => "OK",
        201 => "Created",
        202 => "Accepted",
        204 => "No Content",
        301 => "Moved Permanently",
        302 => "Found",
        400 => "Bad Request",
        404 => "Not Found",
        409 => "Conflict",
        500 => "Internal Server Error",
        _ => "Mock Response",
    };
    let mut head = format!(
        "HTTP/1.1 {} {}\r\nContent-Type: {}\r\nContent-Length: {}\r\nConnection: close\r\n",
        response.status,
        reason,
        response.content_type,
        response.body.len()
    );
    for (name, value) in response.headers {
        head.push_str(&name);
        head.push_str(": ");
        head.push_str(&value);
        head.push_str("\r\n");
    }
    head.push_str("\r\n");
    let _ = stream.write_all(head.as_bytes());
    let _ = stream.write_all(&response.body);
    let _ = stream.flush();
    // Send FIN after the full response, then let the client close its side.
    // Abruptly closing both directions can produce a Windows TCP reset while
    // parallel clients are still consuming the response.
    let _ = stream.shutdown(Shutdown::Write);
    let mut remaining = [0u8; 1024];
    while let Ok(count) = stream.read(&mut remaining) {
        if count == 0 {
            break;
        }
    }
}

fn read_request(stream: &mut TcpStream) -> Option<CapturedRequest> {
    let mut bytes = Vec::new();
    let header_end;
    loop {
        let mut chunk = [0u8; 1024];
        let count = stream.read(&mut chunk).ok()?;
        if count == 0 {
            return None;
        }
        bytes.extend_from_slice(&chunk[..count]);
        if let Some(position) = bytes.windows(4).position(|window| window == b"\r\n\r\n") {
            header_end = position + 4;
            break;
        }
        if bytes.len() > 64 * 1024 {
            return None;
        }
    }
    let text = std::str::from_utf8(&bytes[..header_end]).ok()?;
    let mut lines = text.split("\r\n");
    let mut request_line = lines.next()?.split_whitespace();
    let method = request_line.next()?.to_owned();
    let path = request_line.next()?.to_owned();
    let mut headers = Vec::new();
    let mut content_length = 0usize;
    for line in lines.filter(|line| !line.is_empty()) {
        let (name, value) = line.split_once(':')?;
        let value = value.trim().to_owned();
        if name.eq_ignore_ascii_case("content-length") {
            content_length = value.parse().ok()?;
        }
        headers.push((name.to_owned(), value));
    }
    let mut body = bytes[header_end..].to_vec();
    while body.len() < content_length {
        let mut chunk = [0u8; 4096];
        let count = stream.read(&mut chunk).ok()?;
        if count == 0 {
            return None;
        }
        body.extend_from_slice(&chunk[..count]);
    }
    body.truncate(content_length);
    Some(CapturedRequest {
        method,
        path,
        headers,
        body,
    })
}
