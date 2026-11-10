// @formal evidence worker_drain.registry_transition_follows_local_fence
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
    deregistration_response: Arc<Mutex<Option<String>>>,
    registration_protocol: Arc<Mutex<Option<String>>>,
    registration_state: Arc<Mutex<String>>,
    initial_heartbeat_blocked: Arc<AtomicBool>,
    heartbeat_sequences: Arc<Mutex<Vec<u64>>>,
    applied_heartbeat_sequence: Arc<AtomicU64>,
    registration_lease_ttl_ms: Arc<AtomicU64>,
    next_lease_ttl_ms: Arc<AtomicU64>,
    accepted_connections: Arc<AtomicUsize>,
    thread: Option<JoinHandle<()>>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum HeartbeatFault {
    None,
    UnavailableOnce,
    UnavailableAfterInitial,
    DropResponseOnce,
    DelayInitial,
}

impl FakeWorkerUpstream {
    pub fn start() -> Self {
        Self::start_with_options(None, None, HeartbeatFault::None, 30_000, None)
    }

    pub fn start_at(address: std::net::SocketAddr) -> Self {
        Self::start_with_listener_options(
            TcpListener::bind(address).expect("bind delayed Worker upstream"),
            None,
            None,
            HeartbeatFault::None,
            30_000,
            None,
        )
    }

    pub fn start_with_expired_initial_receipt() -> Self {
        // Real HTTP must reach the fixture before the bounded request expires.
        // Lease arithmetic is covered with controlled clocks in its owner;
        // this row holds the response until exact withdrawal is observed.
        Self::start_with_options(None, None, HeartbeatFault::DelayInitial, 3_000, None)
    }

    #[allow(dead_code)] // Lifecycle startup admission tests only.
    pub fn start_with_zero_registry_ttl() -> Self {
        Self::start_with_options(None, None, HeartbeatFault::None, 0, None)
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
        Self::start_with_listener_options(
            listener,
            drain_release,
            applied_heartbeat_budget,
            heartbeat_fault,
            registry_ttl_ms,
            recovery_release,
        )
    }

    fn start_with_listener_options(
        listener: TcpListener,
        drain_release: Option<Arc<AtomicBool>>,
        applied_heartbeat_budget: Option<usize>,
        heartbeat_fault: HeartbeatFault,
        registry_ttl_ms: u64,
        recovery_release: Option<Arc<AtomicBool>>,
    ) -> Self {
        let addr = listener.local_addr().unwrap();
        listener.set_nonblocking(true).unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let requests = Arc::new(Mutex::new(Vec::new()));
        let request_headers = Arc::new(Mutex::new(Vec::new()));
        let environment_warmups = Arc::new(Mutex::new("[]".to_owned()));
        let deregistration_response =
            Arc::new(Mutex::new(Some(r#"{"mutation":"applied"}"#.to_owned())));
        let registration_protocol = Arc::new(Mutex::new(Some(
            awaken_worker_contract::CURRENT_RUNTIME_PROTOCOL_VERSION.to_string(),
        )));
        let registration_state = Arc::new(Mutex::new("starting".into()));
        let thread_registration_state = registration_state.clone();
        let heartbeat_sequences = Arc::new(Mutex::new(Vec::new()));
        let initial_heartbeat_blocked = Arc::new(AtomicBool::new(false));
        let applied_heartbeat_sequence = Arc::new(AtomicU64::new(0));
        let registration_lease_ttl_ms = Arc::new(AtomicU64::new(registry_ttl_ms));
        let thread_registration_lease_ttl_ms = registration_lease_ttl_ms.clone();
        let next_lease_ttl_ms = Arc::new(AtomicU64::new(registry_ttl_ms));
        let thread_next_lease_ttl_ms = next_lease_ttl_ms.clone();
        let accepted_connections = Arc::new(AtomicUsize::new(0));
        let thread_accepted_connections = accepted_connections.clone();
        let thread_stop = stop.clone();
        let thread_requests = requests.clone();
        let thread_request_headers = request_headers.clone();
        let thread_environment_warmups = environment_warmups.clone();
        let thread_deregistration_response = deregistration_response.clone();
        let thread_registration_protocol = registration_protocol.clone();
        let thread_heartbeat_sequences = heartbeat_sequences.clone();
        let thread_initial_heartbeat_blocked = initial_heartbeat_blocked.clone();
        let thread_applied_heartbeat_sequence = applied_heartbeat_sequence.clone();
        let thread_drain_release = drain_release;
        let thread_recovery_release = recovery_release;
        let heartbeat_count = AtomicUsize::new(0);
        let thread = std::thread::spawn(move || {
            std::thread::scope(|scope| {
                let mut handlers: Vec<std::thread::ScopedJoinHandle<'_, ()>> = Vec::new();
                let mut failure = None;
                while !thread_stop.load(Ordering::Acquire) {
                    for index in (0..handlers.len()).rev() {
                        if handlers[index].is_finished()
                            && let Err(error) = handlers.swap_remove(index).join()
                        {
                            thread_stop.store(true, Ordering::Release);
                            failure.get_or_insert(error);
                        }
                    }
                    if thread_stop.load(Ordering::Acquire) {
                        break;
                    }
                    match listener.accept() {
                        Ok((stream, _)) => {
                            if thread_stop.load(Ordering::Acquire) {
                                break;
                            }
                            stream.set_nonblocking(false).unwrap();
                            thread_accepted_connections.fetch_add(1, Ordering::Release);
                            let context = HandleContext {
                                requests: &thread_requests,
                                request_headers: &thread_request_headers,
                                stop: &thread_stop,
                                drain_release: thread_drain_release.as_deref(),
                                applied_heartbeat_budget,
                                heartbeat_fault,
                                registration_lease_ttl_ms: thread_registration_lease_ttl_ms
                                    .load(Ordering::Acquire),
                                registry_ttl_ms: thread_next_lease_ttl_ms.load(Ordering::Acquire),
                                recovery_release: thread_recovery_release.as_deref(),
                                heartbeat_count: &heartbeat_count,
                                heartbeat_sequences: &thread_heartbeat_sequences,
                                initial_heartbeat_blocked: &thread_initial_heartbeat_blocked,
                                applied_heartbeat_sequence: &thread_applied_heartbeat_sequence,
                                environment_warmups: &thread_environment_warmups,
                                deregistration_response: &thread_deregistration_response,
                                registration_protocol: &thread_registration_protocol,
                                registration_state: &thread_registration_state,
                            };
                            // An accepted, idle socket cannot hold unrelated
                            // registration/heartbeat requests behind its deadline.
                            // The scope owns and joins every connection handler.
                            handlers.push(scope.spawn(move || handle(stream, context)));
                        }
                        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                            std::thread::sleep(Duration::from_millis(2));
                        }
                        Err(error) => {
                            thread_stop.store(true, Ordering::Release);
                            panic!("fake worker upstream accept failed: {error}");
                        }
                    }
                }
                for handler in handlers {
                    if let Err(error) = handler.join() {
                        thread_stop.store(true, Ordering::Release);
                        failure.get_or_insert(error);
                    }
                }
                if let Some(error) = failure {
                    std::panic::resume_unwind(error);
                }
            });
        });
        Self {
            url: format!("http://{addr}"),
            stop,
            requests,
            request_headers,
            environment_warmups,
            deregistration_response,
            registration_protocol,
            registration_state,
            heartbeat_sequences,
            initial_heartbeat_blocked,
            applied_heartbeat_sequence,
            registration_lease_ttl_ms,
            next_lease_ttl_ms,
            accepted_connections,
            thread: Some(thread),
        }
    }

    pub fn set_lease_ttl_ms(&self, ttl: u64) {
        self.registration_lease_ttl_ms.store(ttl, Ordering::Release);
        self.next_lease_ttl_ms.store(ttl, Ordering::Release);
    }

    /// Startup assembly and an applied Ready heartbeat have separate receipts.
    /// Only phase-specific lifecycle tests override the former; ordinary tests
    /// retain the original shared TTL through `set_lease_ttl_ms`.
    #[allow(dead_code)] // Shared fixture also compiles in other integration binaries.
    pub fn set_registration_lease_ttl_ms(&self, ttl: u64) {
        self.registration_lease_ttl_ms.store(ttl, Ordering::Release);
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

    pub fn set_deregistration_response(&self, response: Option<String>) {
        *self.deregistration_response.lock().unwrap() = response;
    }

    #[allow(dead_code)] // Only lifecycle protocol tests inject legacy declarations.
    pub fn set_registration_state(&self, state: &str) {
        *self.registration_state.lock().unwrap() = state.into();
    }

    pub fn set_registration_protocol(&self, value: Option<&str>) {
        *self.registration_protocol.lock().unwrap() = value.map(str::to_owned);
    }

    pub fn block_initial_heartbeat(&self, blocked: bool) {
        self.initial_heartbeat_blocked
            .store(blocked, Ordering::Release);
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
        if let Some(thread) = self.thread.take()
            && let Err(error) = thread.join()
            && !std::thread::panicking()
        {
            // Keep a fixture fault fatal, but never abort the test process
            // with a second panic while its original failure unwinds.
            std::panic::resume_unwind(error);
        }
    }
}

struct HandleContext<'a> {
    initial_heartbeat_blocked: &'a AtomicBool,
    deregistration_response: &'a Mutex<Option<String>>,
    registration_protocol: &'a Mutex<Option<String>>,
    registration_state: &'a Mutex<String>,
    requests: &'a Mutex<Vec<String>>,
    request_headers: &'a Mutex<Vec<String>>,
    stop: &'a AtomicBool,
    drain_release: Option<&'a AtomicBool>,
    applied_heartbeat_budget: Option<usize>,
    heartbeat_fault: HeartbeatFault,
    registration_lease_ttl_ms: u64,
    registry_ttl_ms: u64,
    recovery_release: Option<&'a AtomicBool>,
    heartbeat_count: &'a AtomicUsize,
    heartbeat_sequences: &'a Mutex<Vec<u64>>,
    applied_heartbeat_sequence: &'a AtomicU64,
    environment_warmups: &'a Mutex<String>,
}

fn handle(mut stream: TcpStream, context: HandleContext<'_>) {
    let HandleContext {
        initial_heartbeat_blocked,
        deregistration_response,
        registration_protocol,
        registration_state,
        requests,
        request_headers,
        stop,
        drain_release,
        applied_heartbeat_budget,
        heartbeat_fault,
        registration_lease_ttl_ms,
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
            let registry_ttl_ms = registration_lease_ttl_ms;
            let worker_id = json_string_field(body, "worker_id");
            let incarnation_id = json_string_field(body, "incarnation_id");
            let manifest = json_object_field(body, "manifest");
            let state = registration_state.lock().unwrap().clone();
            let protocol = registration_protocol
                .lock()
                .unwrap()
                .as_ref()
                .map(|value| format!(",\"runtime_protocol_version\":{value}"))
                .unwrap_or_default();
            format!(
                r#"{{"worker":{{"snapshot":{{"identity":{{"worker_id":"{worker_id}","incarnation_id":"{incarnation_id}","generation":1}},"state":"{state}","manifest":{manifest},"capability_fingerprint":"startup-test","in_flight":0,"expires_at_ms":{registry_ttl_ms}}},"heartbeat_sequence":0,"registered_at_ms":0,"heartbeat_at_ms":0,"drain_deadline_ms":null}},"lease_ttl_ms":{registry_ttl_ms}{protocol}}}"#
            )
        }
        "/v1/worker/heartbeat" => {
            let ordinal = heartbeat_count.fetch_add(1, Ordering::AcqRel);
            let sequence = json_u64_field(body, "sequence");
            heartbeat_sequences.lock().unwrap().push(sequence);
            while ordinal == 0
                && initial_heartbeat_blocked.load(Ordering::Acquire)
                && !stop.load(Ordering::Acquire)
            {
                std::thread::sleep(Duration::from_millis(2));
            }
            if ordinal == 0 && heartbeat_fault == HeartbeatFault::DelayInitial {
                applied_heartbeat_sequence.store(sequence, Ordering::Release);
                while !stop.load(Ordering::Acquire)
                    && !requests
                        .lock()
                        .unwrap()
                        .iter()
                        .any(|path| path == "/v1/worker/deregister")
                {
                    std::thread::sleep(Duration::from_millis(2));
                }
            }
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
        "/v1/worker/deregister" => {
            let response = deregistration_response.lock().unwrap().clone();
            match response {
                Some(response) => response,
                None => {
                    while !stop.load(Ordering::Acquire) {
                        std::thread::sleep(Duration::from_millis(2));
                    }
                    return;
                }
            }
        }
        "/v1/worker/drain" | "/v1/worker/quiesced" => r#"{"mutation":"applied"}"#.to_string(),
        "/v1/worker/dispatch/claim" => r#"{"claimed":null}"#.to_string(),
        _ => format!(r#"{{"error":"unexpected startup-test path: {path}"}}"#),
    };
    write_response(&mut stream, "200 OK", &response);
}

fn write_response(mut stream: impl Write, status: &str, body: &str) {
    let result = write!(
        stream,
        "HTTP/1.1 {status}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
        body.len(),
    )
    .and_then(|()| stream.write_all(body.as_bytes()));
    if let Err(error) = result {
        // A lifecycle request may be cancelled after this fixture has read the
        // complete request but before it writes the response (for example when
        // provider evidence loss terminates the heartbeat future). That peer no
        // longer owns a response. Keep the fake upstream alive for the ensuing
        // drain/deregister calls, while every non-disconnect I/O fault remains a
        // fixture failure.
        if !matches!(
            error.kind(),
            std::io::ErrorKind::BrokenPipe
                | std::io::ErrorKind::ConnectionAborted
                | std::io::ErrorKind::ConnectionReset
                | std::io::ErrorKind::NotConnected
        ) {
            panic!("fake worker upstream response failed: {error}");
        }
    }
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
    read_request_bytes(stream)
}

fn read_request_bytes(reader: &mut impl Read) -> RequestRead {
    let mut request = Vec::new();
    let mut chunk = [0_u8; 4096];
    let mut complete_shape = None;
    loop {
        let read = match reader.read(&mut chunk) {
            Ok(read) => read,
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(error)
                if request.is_empty()
                    && matches!(
                        error.kind(),
                        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                    ) =>
            {
                // A cancelled client can leave an accepted, idle connection.
                // Unix and Windows report read deadlines with different kinds.
                return RequestRead::PeerClosedBeforeCompleteRequest;
            }
            Err(error) => panic!("fake worker upstream request read failed: {error}"),
        };
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
    fn idle_connection_cannot_block_a_live_control_request() {
        // F9 an idle accepted connection remains live while a second complete
        // request succeeds inside one second, below the unchanged two-second
        // read deadline. A serial listener is a negative control for this row.
        let fixture = FakeWorkerUpstream::start();
        let address = fixture.url().strip_prefix("http://").unwrap();
        let idle = TcpStream::connect(address).unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(1);
        while fixture.accepted_connections.load(Ordering::Acquire) == 0 {
            assert!(
                std::time::Instant::now() < deadline,
                "F9 idle socket accepted"
            );
            std::thread::yield_now();
        }
        let mut live = TcpStream::connect(address).unwrap();
        live.set_read_timeout(Some(Duration::from_secs(1))).unwrap();
        live.write_all(b"GET /v1/worker/environment/warmups HTTP/1.1\r\nHost: fixture\r\n\r\n")
            .unwrap();
        let mut response = String::new();
        live.read_to_string(&mut response).expect("F9 live request");
        assert!(response.starts_with("HTTP/1.1 200 OK\r\n"), "{response}");
        assert!(response.contains("\"warmups\":[]"), "{response}");
        assert_eq!(fixture.requests(), ["/v1/worker/environment/warmups"]);
        drop(idle);
    }

    #[test]
    fn fixture_drop_keeps_the_original_fault_without_a_second_unwind() {
        // F7 a failed server is joined and remains fatal during ordinary Drop;
        // F8 parent failure + failed server -> join, preserve the parent panic,
        // never abort the process with a destructor panic during unwinding.
        for parent_failed in [false, true] {
            let mut fixture = FakeWorkerUpstream::start();
            fixture.stop.store(true, Ordering::Release);
            fixture.thread.take().unwrap().join().unwrap();
            fixture.thread = Some(std::thread::spawn(|| panic!("server fault")));
            let error = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                let _fixture = fixture;
                if parent_failed {
                    panic!("parent fault");
                }
            }))
            .unwrap_err();
            assert_eq!(
                error.downcast_ref::<&str>().copied(),
                Some(if parent_failed {
                    "parent fault"
                } else {
                    "server fault"
                })
            );
        }
    }

    #[test]
    fn idle_read_deadline_retires_only_an_empty_connection() {
        // F4 no request bytes + either native timeout kind -> retire the idle
        // connection; F5 partial headers/body + timeout -> fail, never dispatch
        // incomplete input. F6 another I/O fault remains a fixture failure.
        struct DeadlineReader {
            prefix: std::io::Cursor<Vec<u8>>,
            error: std::io::ErrorKind,
        }
        impl Read for DeadlineReader {
            fn read(&mut self, bytes: &mut [u8]) -> std::io::Result<usize> {
                let read = self.prefix.read(bytes)?;
                if read == 0 {
                    Err(self.error.into())
                } else {
                    Ok(read)
                }
            }
        }
        for error in [std::io::ErrorKind::WouldBlock, std::io::ErrorKind::TimedOut] {
            let mut reader = DeadlineReader {
                prefix: std::io::Cursor::new(Vec::new()),
                error,
            };
            assert!(matches!(
                read_request_bytes(&mut reader),
                RequestRead::PeerClosedBeforeCompleteRequest
            ));
            for prefix in [
                b"GET / HTTP/1.1\r\n".as_slice(),
                b"POST / HTTP/1.1\r\nContent-Length: 2\r\n\r\n{",
            ] {
                let failure = std::panic::catch_unwind(|| {
                    read_request_bytes(&mut DeadlineReader {
                        prefix: std::io::Cursor::new(prefix.to_vec()),
                        error,
                    })
                });
                assert!(failure.is_err(), "F5 partial request must fail");
            }
        }
        assert!(
            std::panic::catch_unwind(|| {
                read_request_bytes(&mut DeadlineReader {
                    prefix: std::io::Cursor::new(Vec::new()),
                    error: std::io::ErrorKind::PermissionDenied,
                })
            })
            .is_err(),
            "F6 unexpected fault must fail"
        );
    }

    struct FailingWriter(std::io::ErrorKind);

    impl Write for FailingWriter {
        fn write(&mut self, _buffer: &[u8]) -> std::io::Result<usize> {
            Err(std::io::Error::from(self.0))
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn peer_close_before_http_headers_is_not_a_fixture_failure() {
        // Decision table F1: C1 peer closes before a complete request => E1
        // read-side handler returns; no response is owned and the listener stays
        // available. C2 complete request + connected peer is covered by every
        // lifecycle integration test below.
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

    #[test]
    fn peer_close_after_a_complete_request_does_not_kill_the_fixture() {
        // Response-write decision table: F2 C1 complete request + peer-disconnect
        // write error => E1 retire only that response and keep serving; F3 C1 +
        // any other I/O error => E2 fail the fixture. Constraint: cancellation
        // does not make arbitrary write faults successful. Real connected writes
        // are exercised by the lifecycle tests that use this one helper.
        for kind in [
            std::io::ErrorKind::BrokenPipe,
            std::io::ErrorKind::ConnectionAborted,
            std::io::ErrorKind::ConnectionReset,
            std::io::ErrorKind::NotConnected,
        ] {
            write_response(FailingWriter(kind), "200 OK", "{}");
        }
        assert!(
            std::panic::catch_unwind(|| {
                write_response(
                    FailingWriter(std::io::ErrorKind::PermissionDenied),
                    "200 OK",
                    "{}",
                );
            })
            .is_err(),
            "F3/E2 unexpected I/O faults remain visible"
        );
    }
}
