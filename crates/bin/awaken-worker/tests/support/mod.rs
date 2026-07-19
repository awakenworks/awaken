use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;
use std::time::Duration;

pub struct FakeWorkerUpstream {
    url: String,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl FakeWorkerUpstream {
    pub fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        listener.set_nonblocking(true).unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let thread_stop = stop.clone();
        let thread = std::thread::spawn(move || {
            while !thread_stop.load(Ordering::Acquire) {
                match listener.accept() {
                    Ok((stream, _)) => handle(stream),
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
            thread: Some(thread),
        }
    }

    pub fn url(&self) -> &str {
        &self.url
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

fn handle(mut stream: TcpStream) {
    let request = read_request(&mut stream);
    let header_end = request
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .map(|index| index + 4)
        .unwrap();
    let request_line = std::str::from_utf8(&request[..header_end])
        .unwrap()
        .lines()
        .next()
        .unwrap();
    let path = request_line.split_whitespace().nth(1).unwrap();
    let body = std::str::from_utf8(&request[header_end..]).unwrap();
    let response = match path {
        "/v1/worker/register" => {
            let worker_id = json_string_field(body, "worker_id");
            let incarnation_id = json_string_field(body, "incarnation_id");
            let manifest = json_object_field(body, "manifest");
            format!(
                r#"{{"worker":{{"snapshot":{{"identity":{{"worker_id":"{worker_id}","incarnation_id":"{incarnation_id}","generation":1}},"state":"starting","manifest":{manifest},"capability_fingerprint":"composition-test","in_flight":0,"expires_at_ms":60000}},"heartbeat_sequence":0,"registered_at_ms":0,"heartbeat_at_ms":0,"drain_deadline_ms":null}}}}"#
            )
        }
        "/v1/worker/heartbeat"
        | "/v1/worker/drain"
        | "/v1/worker/quiesced"
        | "/v1/worker/deregister" => r#"{"mutation":"applied"}"#.to_string(),
        "/v1/worker/dispatch/claim" => r#"{"claimed":null}"#.to_string(),
        _ => format!(r#"{{"error":"unexpected composition-test path: {path}"}}"#),
    };
    write!(
        stream,
        "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
        response.len(),
    )
    .unwrap();
    stream.write_all(response.as_bytes()).unwrap();
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

fn read_request(stream: &mut TcpStream) -> Vec<u8> {
    stream
        .set_read_timeout(Some(Duration::from_secs(2)))
        .unwrap();
    let mut request = Vec::new();
    let mut chunk = [0_u8; 4096];
    let mut expected = None;
    loop {
        let read = stream.read(&mut chunk).unwrap();
        if read == 0 {
            break;
        }
        request.extend_from_slice(&chunk[..read]);
        if expected.is_none()
            && let Some(header_end) = request.windows(4).position(|window| window == b"\r\n\r\n")
        {
            let headers = std::str::from_utf8(&request[..header_end]).unwrap();
            let content_length = headers
                .lines()
                .find_map(|line| {
                    let (name, value) = line.split_once(':')?;
                    name.eq_ignore_ascii_case("content-length")
                        .then(|| value.trim().parse::<usize>().unwrap())
                })
                .unwrap_or(0);
            expected = Some(header_end + 4 + content_length);
        }
        if expected.is_some_and(|length| request.len() >= length) {
            break;
        }
    }
    request
}
