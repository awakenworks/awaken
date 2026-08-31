use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::thread::JoinHandle;
use std::time::Duration;

pub struct FakeWorkerUpstream {
    url: String,
    stop: Arc<AtomicBool>,
    requests: Arc<Mutex<Vec<String>>>,
    #[allow(dead_code)] // Only the lifecycle integration test asserts client provenance.
    request_headers: Arc<Mutex<Vec<String>>>,
    environment_warmups: Arc<Mutex<String>>,
    heartbeat_sequences: Arc<Mutex<Vec<u64>>>,
    applied_heartbeat_sequence: Arc<AtomicU64>,
    thread: Option<JoinHandle<()>>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum HeartbeatFault {
    None,
    UnavailableOnce,
    UnavailableAfterInitial,
    DropResponseOnce,
}

impl FakeWorkerUpstream {
    pub fn start() -> Self {
        Self::start_with_options(None, None, HeartbeatFault::None, 30_000, None)
    }

    pub fn start_rejecting_periodic_heartbeat() -> Self {
        Self::start_with_options(None, Some(1), HeartbeatFault::None, 3_000, None)
    }

    pub fn start_transient_heartbeat_with_blocked_recovery() -> (Self, Arc<AtomicBool>) {
        let release = Arc::new(AtomicBool::new(false));
        (
            Self::start_with_options(
                None,
                None,
                HeartbeatFault::UnavailableOnce,
                3_000,
                Some(release.clone()),
            ),
            release,
        )
    }

    pub fn start_dropping_one_heartbeat_response() -> Self {
        Self::start_with_options(None, None, HeartbeatFault::DropResponseOnce, 3_000, None)
    }

    pub fn start_unavailable_after_initial_heartbeat() -> Self {
        Self::start_with_options(
            None,
            None,
            HeartbeatFault::UnavailableAfterInitial,
            600,
            None,
        )
    }

    pub fn start_with_short_registry_lease() -> Self {
        Self::start_with_options(None, None, HeartbeatFault::None, 600, None)
    }

    pub fn start_with_blocked_drain() -> (Self, Arc<AtomicBool>) {
        let release = Arc::new(AtomicBool::new(false));
        (
            Self::start_with_options(
                Some(release.clone()),
                None,
                HeartbeatFault::None,
                30_000,
                None,
            ),
            release,
        )
    }

    fn start_with_options(
        drain_release: Option<Arc<AtomicBool>>,
        applied_heartbeat_budget: Option<usize>,
        heartbeat_fault: HeartbeatFault,
        registry_ttl_ms: u64,
        recovery_release: Option<Arc<AtomicBool>>,
    ) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        listener.set_nonblocking(true).unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let requests = Arc::new(Mutex::new(Vec::new()));
        let request_headers = Arc::new(Mutex::new(Vec::new()));
        let environment_warmups = Arc::new(Mutex::new("[]".to_owned()));
        let heartbeat_sequences = Arc::new(Mutex::new(Vec::new()));
        let applied_heartbeat_sequence = Arc::new(AtomicU64::new(0));
        let thread_stop = stop.clone();
        let thread_requests = requests.clone();
        let thread_request_headers = request_headers.clone();
        let thread_environment_warmups = environment_warmups.clone();
        let thread_heartbeat_sequences = heartbeat_sequences.clone();
        let thread_applied_heartbeat_sequence = applied_heartbeat_sequence.clone();
        let thread_drain_release = drain_release;
        let thread_recovery_release = recovery_release;
        let heartbeat_count = AtomicUsize::new(0);
        let thread = std::thread::spawn(move || {
            while !thread_stop.load(Ordering::Acquire) {
                match listener.accept() {
                    Ok((stream, _)) => {
                        if thread_stop.load(Ordering::Acquire) {
                            break;
                        }
                        stream.set_nonblocking(false).unwrap();
                        handle(
                            stream,
                            HandleContext {
                                requests: &thread_requests,
                                request_headers: &thread_request_headers,
                                stop: &thread_stop,
                                drain_release: thread_drain_release.as_deref(),
                                applied_heartbeat_budget,
                                heartbeat_fault,
                                registry_ttl_ms,
                                recovery_release: thread_recovery_release.as_deref(),
                                heartbeat_count: &heartbeat_count,
                                heartbeat_sequences: &thread_heartbeat_sequences,
                                applied_heartbeat_sequence: &thread_applied_heartbeat_sequence,
                                environment_warmups: &thread_environment_warmups,
                            },
                        );
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(2));
                    }
                    Err(error) => panic!("fake worker upstream accept failed: {error}"),
                }
            }
        });
        Self {
            url: format!("http://{addr}"),
            stop,
            requests,
            request_headers,
            environment_warmups,
            heartbeat_sequences,
            applied_heartbeat_sequence,
            thread: Some(thread),
        }
    }

    pub fn url(&self) -> &str {
        &self.url
    }

    pub fn requests(&self) -> Vec<String> {
        self.requests.lock().unwrap().clone()
    }

    #[allow(dead_code)] // This shared fixture is also compiled by tests that need paths only.
    pub fn request_headers(&self) -> Vec<String> {
        self.request_headers.lock().unwrap().clone()
    }

    pub fn set_environment_warmups_json(&self, warmups: impl Into<String>) {
        *self.environment_warmups.lock().unwrap() = warmups.into();
    }

    pub fn heartbeat_sequences(&self) -> Vec<u64> {
        self.heartbeat_sequences.lock().unwrap().clone()
    }

    pub fn applied_heartbeat_sequence(&self) -> u64 {
        self.applied_heartbeat_sequence.load(Ordering::Acquire)
    }
}

impl Drop for FakeWorkerUpstream {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(addr) = self.url.strip_prefix("http://") {
            let _ = TcpStream::connect(addr);
        }
        if let Some(thread) = self.thread.take() {
            thread.join().unwrap();
        }
    }
}

struct HandleContext<'a> {
    requests: &'a Mutex<Vec<String>>,
    request_headers: &'a Mutex<Vec<String>>,
    stop: &'a AtomicBool,
    drain_release: Option<&'a AtomicBool>,
    applied_heartbeat_budget: Option<usize>,
    heartbeat_fault: HeartbeatFault,
    registry_ttl_ms: u64,
    recovery_release: Option<&'a AtomicBool>,
    heartbeat_count: &'a AtomicUsize,
    heartbeat_sequences: &'a Mutex<Vec<u64>>,
    applied_heartbeat_sequence: &'a AtomicU64,
    environment_warmups: &'a Mutex<String>,
}

fn handle(mut stream: TcpStream, context: HandleContext<'_>) {
    let HandleContext {
        requests,
        request_headers,
        stop,
        drain_release,
        applied_heartbeat_budget,
        heartbeat_fault,
        registry_ttl_ms,
        recovery_release,
        heartbeat_count,
        heartbeat_sequences,
        applied_heartbeat_sequence,
        environment_warmups,
    } = context;
    let RequestRead::Complete(CompleteRequest {
        bytes: request,
        header_end,
    }) = read_request(&mut stream)
    else {
        return;
    };
    let request_line = std::str::from_utf8(&request[..header_end])
        .unwrap()
        .lines()
        .next()
        .unwrap();
    let path = request_line.split_whitespace().nth(1).unwrap();
    requests.lock().unwrap().push(path.to_string());
    request_headers.lock().unwrap().push(
        std::str::from_utf8(&request[..header_end])
            .unwrap()
            .to_owned(),
    );
    if path == "/v1/worker/drain"
        && let Some(release) = drain_release
    {
        while !release.load(Ordering::Acquire) && !stop.load(Ordering::Acquire) {
            std::thread::sleep(Duration::from_millis(2));
        }
    }
    let body = std::str::from_utf8(&request[header_end..]).unwrap();
    let response = match path {
        "/v1/worker/register" => {
            let worker_id = json_string_field(body, "worker_id");
            let incarnation_id = json_string_field(body, "incarnation_id");
            let manifest = json_object_field(body, "manifest");
            format!(
                r#"{{"worker":{{"snapshot":{{"identity":{{"worker_id":"{worker_id}","incarnation_id":"{incarnation_id}","generation":1}},"state":"starting","manifest":{manifest},"capability_fingerprint":"startup-test","in_flight":0,"expires_at_ms":{registry_ttl_ms}}},"heartbeat_sequence":0,"registered_at_ms":0,"heartbeat_at_ms":0,"drain_deadline_ms":null}},"lease_ttl_ms":{registry_ttl_ms}}}"#
            )
        }
        "/v1/worker/heartbeat" => {
            let ordinal = heartbeat_count.fetch_add(1, Ordering::AcqRel);
            let sequence = json_u64_field(body, "sequence");
            heartbeat_sequences.lock().unwrap().push(sequence);
            if ordinal == 1 && heartbeat_fault == HeartbeatFault::DropResponseOnce {
                applied_heartbeat_sequence.store(sequence, Ordering::Release);
                return;
            }
            if (ordinal == 1 && heartbeat_fault == HeartbeatFault::UnavailableOnce)
                || (ordinal >= 1 && heartbeat_fault == HeartbeatFault::UnavailableAfterInitial)
            {
                write_response(
                    &mut stream,
                    "503 Service Unavailable",
                    r#"{"error":"injected transient heartbeat failure"}"#,
                );
                return;
            }
            if ordinal >= 2
                && heartbeat_fault == HeartbeatFault::UnavailableOnce
                && let Some(release) = recovery_release
            {
                while !release.load(Ordering::Acquire) && !stop.load(Ordering::Acquire) {
                    std::thread::sleep(Duration::from_millis(2));
                }
            }
            if applied_heartbeat_budget.is_some_and(|budget| ordinal >= budget) {
                r#"{"mutation":"stale_incarnation"}"#.to_string()
            } else {
                applied_heartbeat_sequence.store(sequence, Ordering::Release);
                format!(r#"{{"mutation":"applied","lease_ttl_ms":{registry_ttl_ms}}}"#)
            }
        }
        "/v1/worker/environment/warmups" => {
            format!(r#"{{"warmups":{}}}"#, environment_warmups.lock().unwrap())
        }
        "/v1/worker/drain" | "/v1/worker/quiesced" | "/v1/worker/deregister" => {
            r#"{"mutation":"applied"}"#.to_string()
        }
        "/v1/worker/dispatch/claim" => r#"{"claimed":null}"#.to_string(),
        _ => format!(r#"{{"error":"unexpected startup-test path: {path}"}}"#),
    };
    write_response(&mut stream, "200 OK", &response);
}

fn write_response(stream: &mut TcpStream, status: &str, body: &str) {
    write!(
        stream,
        "HTTP/1.1 {status}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
        body.len(),
    )
    .unwrap();
    stream.write_all(body.as_bytes()).unwrap();
}

fn json_u64_field(body: &str, field: &str) -> u64 {
    let field = format!(r#""{field}""#);
    let suffix = body.split_once(&field).unwrap().1;
    suffix
        .split_once(':')
        .unwrap()
        .1
        .trim_start()
        .split(|character: char| !character.is_ascii_digit())
        .next()
        .unwrap()
        .parse()
        .unwrap()
}

fn json_string_field<'a>(body: &'a str, field: &str) -> &'a str {
    let field = format!(r#""{field}""#);
    let suffix = body.split_once(&field).unwrap().1;
    let value = suffix.split_once(':').unwrap().1.trim_start();
    let value = value.strip_prefix('"').unwrap();
    let end = value
        .as_bytes()
        .iter()
        .enumerate()
        .find_map(|(index, byte)| {
            (*byte == b'"'
                && value.as_bytes()[..index]
                    .iter()
                    .rev()
                    .take_while(|byte| **byte == b'\\')
                    .count()
                    % 2
                    == 0)
                .then_some(index)
        })
        .unwrap();
    &value[..end]
}

fn json_object_field<'a>(body: &'a str, field: &str) -> &'a str {
    let field = format!(r#""{field}""#);
    let suffix = body.split_once(&field).unwrap().1;
    let value = suffix.split_once(':').unwrap().1.trim_start();
    let mut depth = 0_u32;
    let mut quoted = false;
    let mut escaped = false;
    for (index, byte) in value.bytes().enumerate() {
        if quoted {
            if escaped {
                escaped = false;
            } else if byte == b'\\' {
                escaped = true;
            } else if byte == b'"' {
                quoted = false;
            }
            continue;
        }
        match byte {
            b'"' => quoted = true,
            b'{' => depth += 1,
            b'}' => {
                depth -= 1;
                if depth == 0 {
                    return &value[..=index];
                }
            }
            _ => {}
        }
    }
    panic!("registration manifest is not a JSON object")
}

struct CompleteRequest {
    bytes: Vec<u8>,
    header_end: usize,
}

enum RequestRead {
    Complete(CompleteRequest),
    PeerClosedBeforeCompleteRequest,
}

fn read_request(stream: &mut TcpStream) -> RequestRead {
    stream
        .set_read_timeout(Some(Duration::from_secs(2)))
        .unwrap();
    let mut request = Vec::new();
    let mut chunk = [0_u8; 4096];
    let mut complete_shape = None;
    loop {
        let read = stream.read(&mut chunk).unwrap();
        if read == 0 {
            return RequestRead::PeerClosedBeforeCompleteRequest;
        }
        request.extend_from_slice(&chunk[..read]);
        if complete_shape.is_none()
            && let Some(header_start) = request.windows(4).position(|window| window == b"\r\n\r\n")
        {
            let complete_header_end = header_start + 4;
            let headers = std::str::from_utf8(&request[..header_start]).unwrap();
            let content_length = headers
                .lines()
                .find_map(|line| {
                    let (name, value) = line.split_once(':')?;
                    name.eq_ignore_ascii_case("content-length")
                        .then(|| value.trim().parse::<usize>().unwrap())
                })
                .unwrap_or(0);
            complete_shape = Some((complete_header_end, complete_header_end + content_length));
        }
        if let Some((header_end, expected_length)) = complete_shape
            && request.len() >= expected_length
        {
            return RequestRead::Complete(CompleteRequest {
                bytes: request,
                header_end,
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn peer_close_before_http_headers_is_not_a_fixture_failure() {
        let upstream = FakeWorkerUpstream::start();
        let address = upstream
            .url()
            .strip_prefix("http://")
            .expect("fake worker upstream uses HTTP");
        let stream = TcpStream::connect(address).expect("connect to fake worker upstream");
        drop(stream);
        std::thread::sleep(Duration::from_millis(20));
        drop(upstream);
    }
}
